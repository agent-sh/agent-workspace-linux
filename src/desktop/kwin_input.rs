use anyhow::{bail, Context, Result};
use std::{
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};
use wayland_client::{
    protocol::{wl_callback, wl_registry},
    Connection, Dispatch, EventQueue, QueueHandle,
};

mod protocol {
    // The scanner resolves this import through super::wayland_client.
    #[allow(clippy::single_component_path_imports)]
    use wayland_client;
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}
use protocol::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

#[derive(Default)]
struct State {
    input: Option<OrgKdeKwinFakeInput>,
    done: bool,
    advertised_version: u32,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "org_kde_kwin_fake_input" {
                state.advertised_version = version;
            }
            if interface == "org_kde_kwin_fake_input" && version >= 6 {
                state.input = Some(registry.bind(name, 6, qh, ()));
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.done = true;
    }
}
impl Dispatch<OrgKdeKwinFakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &OrgKdeKwinFakeInput,
        _: protocol::org_kde_kwin_fake_input::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub(super) struct Input {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}
impl Input {
    pub fn connect(runtime: &Path) -> Result<Self> {
        // An explicit private socket is mandatory. Never use connect_to_env.
        let socket = UnixStream::connect(runtime.join(crate::session::kwin::WAYLAND_SOCKET))
            .context("connect to workspace KWin input socket")?;
        let connection = Connection::from_socket(socket)?;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut this = Self {
            connection,
            queue,
            state: State::default(),
        };
        this.sync()?;
        this.state
            .input
            .as_ref()
            .with_context(|| format!("KWin fake-input v6 is unavailable (advertised version {}); check private desktop-entry authorization and KWin version", this.state.advertised_version))?
            .authenticate(
                "Agent Workspace Linux".into(),
                "Control this private workspace".into(),
            );
        this.sync()?;
        Ok(this)
    }
    pub fn pointer(&mut self, x: f64, y: f64) -> Result<()> {
        self.device().pointer_motion_absolute(x, y);
        self.sync()
    }
    pub fn button(&mut self, button: u32, pressed: bool) -> Result<()> {
        self.device().button(button, u32::from(pressed));
        self.sync()
    }
    pub fn axis(&mut self, horizontal: bool, amount: f64) -> Result<()> {
        self.device().axis(u32::from(horizontal), amount);
        self.sync()
    }
    pub fn keysym(&mut self, sym: u32, pressed: bool) -> Result<()> {
        self.device().keyboard_keysym(sym, u32::from(pressed));
        self.sync()
    }
    fn device(&self) -> &OrgKdeKwinFakeInput {
        self.state.input.as_ref().expect("input was authenticated")
    }
    fn sync(&mut self) -> Result<()> {
        self.state.done = false;
        self.connection.display().sync(&self.queue.handle(), ());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.state.done {
            self.queue.dispatch_pending(&mut self.state)?;
            if self.state.done {
                break;
            }
            self.connection.flush()?;
            if Instant::now() >= deadline {
                bail!("workspace KWin input timed out");
            }
            if let Some(guard) = self.queue.prepare_read() {
                let mut fd = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ms = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as i32;
                let ready = unsafe { libc::poll(&mut fd, 1, ms) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error.into());
                }
                if ready == 0 {
                    bail!("workspace KWin input timed out");
                }
                guard.read()?;
            }
        }
        Ok(())
    }
}

pub(super) fn keysyms(key: &str) -> Result<Vec<u32>> {
    use xkbcommon::xkb;
    let names = key
        .split('+')
        .map(|name| match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Control_L".to_owned(),
            "shift" => "Shift_L".to_owned(),
            "alt" => "Alt_L".to_owned(),
            "super" | "meta" | "win" => "Super_L".to_owned(),
            "enter" | "return" => "Return".to_owned(),
            "escape" | "esc" => "Escape".to_owned(),
            "backspace" => "BackSpace".to_owned(),
            "space" => "space".to_owned(),
            _ => name.to_owned(),
        });
    names
        .map(|name| {
            let sym = xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
            if sym == 0 {
                bail!("unknown key name {name:?}");
            }
            Ok(sym)
        })
        .collect()
}
