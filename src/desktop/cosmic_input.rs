//! COSMIC's native libei connection, restricted to the workspace bus.
use anyhow::{bail, Context, Result};
use reis::{
    ei,
    event::{Device, DeviceCapability, EiEvent, EiEventConverter},
    PendingRequestResult,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Seek, SeekFrom},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

struct Input {
    context: ei::Context,
    events: EiEventConverter,
    devices: Vec<Device>,
    deadline: Instant,
}
impl Input {
    fn pump(&mut self) -> Result<()> {
        while let Some(event) = self.context.pending_event() {
            match event {
                PendingRequestResult::Request(e) => self.events.handle_event(e)?,
                PendingRequestResult::ParseError(e) => bail!("invalid COSMIC EI event: {e}"),
                PendingRequestResult::InvalidObject(_) => bail!("invalid COSMIC EI object"),
            }
        }
        while let Some(event) = self.events.next_event() {
            match event {
                EiEvent::SeatAdded(e) => {
                    e.seat.bind_capabilities(
                        DeviceCapability::PointerAbsolute
                            | DeviceCapability::Keyboard
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll
                            | DeviceCapability::Text,
                    );
                    self.context.flush()?;
                }
                EiEvent::DeviceResumed(e) => {
                    if !self.devices.contains(&e.device) {
                        self.devices.push(e.device);
                    }
                }
                EiEvent::DeviceRemoved(e) => self.devices.retain(|d| d != &e.device),
                EiEvent::Disconnected(e) => bail!("COSMIC EI disconnected: {e:?}"),
                _ => {}
            }
        }
        Ok(())
    }
    fn wait(&mut self) -> Result<()> {
        if Instant::now() >= self.deadline {
            bail!("timed out waiting for COSMIC native input");
        }
        self.context.flush()?;
        let timeout = self
            .deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(i32::MAX as u128) as i32;
        let mut fd = libc::pollfd {
            fd: self.context.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, timeout) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if result == 0 {
            bail!("timed out waiting for COSMIC native input");
        }
        self.context.read()?;
        self.pump()
    }
    fn sync(&mut self) -> Result<()> {
        let callback = self.events.connection().connection().sync(1);
        let done = Arc::new(AtomicBool::new(false));
        let result = done.clone();
        self.events.add_callback_handler(callback, move |_| {
            result.store(true, Ordering::Relaxed);
        });
        while !done.load(Ordering::Relaxed) {
            self.wait()?;
        }
        Ok(())
    }
    fn device<T: ei::Interface>(&self) -> Result<(Device, T)> {
        self.devices
            .iter()
            .find_map(|d| d.interface::<T>().map(|i| (d.clone(), i)))
            .with_context(|| format!("COSMIC input capability {} unavailable", T::NAME))
    }
}
/// Caller applies an additional process deadline, including D-Bus and handshake.
pub(crate) fn run(bus_address: &str, actions: &Value) -> Result<Value> {
    let bus = zbus::blocking::connection::Builder::address(bus_address)?
        .method_timeout(Duration::from_secs(3))
        .name("com.system76.CosmicOSK")?
        .build()?;
    let proxy = zbus::blocking::Proxy::new(
        &bus,
        "com.system76.CosmicComp",
        "/com/system76/CosmicComp/Ei",
        "com.system76.CosmicComp.Ei",
    )?;
    let fd: zbus::zvariant::OwnedFd = proxy.call("GetSenderSocket", &(3_u32,))?;
    let context = ei::Context::new(UnixStream::from(std::os::fd::OwnedFd::from(fd)))?;
    let handshake = reis::handshake::ei_handshake_blocking(
        &context,
        "Agent Workspace Linux",
        ei::handshake::ContextType::Sender,
    )?;
    let events = EiEventConverter::new(&context, handshake);
    let mut input = Input {
        context,
        events,
        devices: vec![],
        deadline: Instant::now() + Duration::from_secs(4),
    };
    input.pump()?;
    while input.device::<ei::PointerAbsolute>().is_err()
        || input.device::<ei::Keyboard>().is_err()
        || input.device::<ei::Text>().is_err()
    {
        input.wait()?;
    }
    let regions=input.device::<ei::PointerAbsolute>()?.0.regions().iter().map(|r|json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height,"scale":r.scale,"mapping_id":r.mapping_id})).collect::<Vec<_>>();
    let serial = input.events.connection().serial();
    for d in &input.devices {
        d.device().start_emulating(serial, 1);
    }
    input.sync()?;
    for action in actions
        .as_array()
        .context("input actions must be an array")?
    {
        let kind = action["kind"]
            .as_str()
            .context("input action kind missing")?;
        let device = match kind {
            "motion" => {
                let (d, i) = input.device::<ei::PointerAbsolute>()?;
                i.motion_absolute(
                    action["x"].as_f64().context("x missing")? as f32,
                    action["y"].as_f64().context("y missing")? as f32,
                );
                d
            }
            "button" => {
                let (d, i) = input.device::<ei::Button>()?;
                i.button(
                    action["button"].as_u64().context("button missing")? as u32,
                    if action["pressed"] == true {
                        ei::button::ButtonState::Press
                    } else {
                        ei::button::ButtonState::Released
                    },
                );
                d
            }
            "scroll" => {
                let (d, i) = input.device::<ei::Scroll>()?;
                i.scroll_discrete(
                    action["x"].as_i64().unwrap_or(0) as i32,
                    action["y"].as_i64().unwrap_or(0) as i32,
                );
                d
            }
            "text" => {
                let (d, i) = input.device::<ei::Text>()?;
                i.utf8(action["text"].as_str().context("text missing")?);
                d
            }
            "key" => {
                let (d, i) = input.device::<ei::Keyboard>()?;
                let keymap = d.keymap().context("COSMIC EI keyboard has no keymap")?;
                let mut file = std::fs::File::from(keymap.fd.try_clone()?);
                file.seek(SeekFrom::Start(0))?;
                let mut text = String::new();
                file.read_to_string(&mut text)?;
                text.truncate(text.trim_end_matches('\0').len());
                let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
                let map = xkbcommon::xkb::Keymap::new_from_string(
                    &context,
                    text,
                    xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
                .context("invalid EI keymap")?;
                let sym = xkbcommon::xkb::Keysym::new(
                    action["keysym"].as_u64().context("keysym missing")? as u32,
                );
                let (code, level) = (0..=1)
                    .find_map(|level| {
                        (map.min_keycode().raw()..=map.max_keycode().raw())
                            .find(|c| {
                                map.key_get_syms_by_level(
                                    xkbcommon::xkb::Keycode::new(*c),
                                    0,
                                    level,
                                )
                                .contains(&sym)
                            })
                            .map(|code| (code, level))
                    })
                    .context("keysym not in COSMIC keyboard layout")?;
                let pressed = action["pressed"] == true;
                let shift = if level == 1 {
                    let symbol = xkbcommon::xkb::Keysym::new(0xffe1);
                    Some(
                        (map.min_keycode().raw()..=map.max_keycode().raw())
                            .find(|c| {
                                map.key_get_syms_by_level(xkbcommon::xkb::Keycode::new(*c), 0, 0)
                                    .contains(&symbol)
                            })
                            .context("COSMIC layout has no Shift key")?
                            .checked_sub(8)
                            .context("invalid Shift keycode")?,
                    )
                } else {
                    None
                };
                if pressed {
                    if let Some(shift) = shift {
                        i.key(shift, ei::keyboard::KeyState::Press);
                    }
                }
                i.key(
                    code.checked_sub(8).context("invalid keycode")?,
                    if pressed {
                        ei::keyboard::KeyState::Press
                    } else {
                        ei::keyboard::KeyState::Released
                    },
                );
                if !pressed {
                    if let Some(shift) = shift {
                        i.key(shift, ei::keyboard::KeyState::Released);
                    }
                }
                d
            }
            _ => bail!("unknown input action {kind}"),
        };
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        device.device().frame(
            input.events.connection().serial(),
            ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000,
        );
        input.sync()?;
    }
    for d in &input.devices {
        d.device()
            .stop_emulating(input.events.connection().serial());
    }
    input.sync()?;
    input.events.connection().connection().disconnect();
    input.context.flush()?;
    Ok(json!({"delivered":true,"regions":regions}))
}
