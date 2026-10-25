//! Native COSMIC protocol client. Called only in a deadline-bound helper process.
//! Explicit socket paths prevent fallback to any host desktop.
use anyhow::{bail, Context, Result};
use cosmic_client_toolkit::{
    screencopy::{
        CaptureCursorSession, CaptureFrame, CaptureOptions, CaptureSession, CaptureSource,
        FailureReason, Formats, Frame, ScreencopyCursorSessionData, ScreencopyFrameData,
        ScreencopyFrameDataExt, ScreencopyHandler, ScreencopySessionData, ScreencopyState,
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    toplevel_management::{ToplevelManagerHandler, ToplevelManagerState},
};
use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::State as WindowState;
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1;
use sctk::{
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shm::{raw::RawPool, Shm, ShmHandler},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    os::unix::net::UnixStream,
    path::Path,
    sync::Mutex,
};
use wayland_client::{
    delegate_noop,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_output, wl_pointer, wl_seat},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::WpSecurityContextManagerV1,
    wp_security_context_v1::WpSecurityContextV1,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};

#[derive(Default)]
struct Head {
    enabled: bool,
    scale: f64,
    mode: Option<zwlr_output_mode_v1::ZwlrOutputModeV1>,
}
struct State {
    registry: RegistryState,
    output: OutputState,
    windows: ToplevelInfoState,
    manager: ToplevelManagerState,
    capture: ScreencopyState,
    shm: Shm,
    seat: wl_seat::WlSeat,
    output_manager: zwlr_output_manager_v1::ZwlrOutputManagerV1,
    heads: HashMap<zwlr_output_head_v1::ZwlrOutputHeadV1, Head>,
    modes: HashMap<zwlr_output_mode_v1::ZwlrOutputModeV1, (i32, i32)>,
    serial: Option<u32>,
    pending_windows: HashSet<wayland_client::backend::ObjectId>,
    capabilities: Vec<u32>,
    pixels: Option<(u32, u32, Vec<u8>, WEnum<wl_output::Transform>)>,
    failure: Option<String>,
    configured: Option<bool>,
    pointer: Option<(i32, i32)>,
}
impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    sctk::registry_handlers!(OutputState);
}
impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl ToplevelInfoHandler for State {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.windows
    }
    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
    }
    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
    }
    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ExtForeignToplevelHandleV1,
    ) {
    }
}
impl ToplevelManagerHandler for State {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        &mut self.manager
    }
    fn capabilities(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        v: Vec<WEnum<ZcosmicToplelevelManagementCapabilitiesV1>>,
    ) {
        self.capabilities = v.into_iter().map(u32::from).collect();
    }
}
impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
struct Buffer {
    data: ScreencopyFrameData,
    pool: Mutex<RawPool>,
    width: u32,
    height: u32,
}
impl ScreencopyFrameDataExt for Buffer {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.data
    }
}
impl ScreencopyHandler for State {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.capture
    }
    fn init_done(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        let (w, h) = formats.buffer_size;
        if w == 0
            || h == 0
            || w > 32768
            || h > 32768
            || !formats
                .shm_formats
                .contains(&wayland_client::protocol::wl_shm::Format::Abgr8888)
        {
            self.failure = Some("unsupported or invalid COSMIC SHM capture format".into());
            return;
        }
        let result = (|| -> Result<()> {
            let mut pool = RawPool::new(w as usize * h as usize * 4, &self.shm)?;
            let buffer = pool.create_buffer(
                0,
                w as i32,
                h as i32,
                w as i32 * 4,
                wayland_client::protocol::wl_shm::Format::Abgr8888,
                (),
                qh,
            );
            session.capture(
                &buffer,
                &[],
                qh,
                Buffer {
                    data: ScreencopyFrameData::default(),
                    pool: Mutex::new(pool),
                    width: w,
                    height: h,
                },
            );
            Ok(())
        })();
        if let Err(e) = result {
            self.failure = Some(e.to_string());
        }
    }
    fn stopped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &CaptureSession) {
        self.failure = Some("COSMIC capture session stopped".into());
    }
    fn ready(&mut self, _: &Connection, _: &QueueHandle<Self>, frame: &CaptureFrame, info: Frame) {
        let data = frame.data::<Buffer>().expect("owned capture buffer");
        let mut pool = data.pool.lock().unwrap();
        self.pixels = Some((
            data.width,
            data.height,
            pool.mmap().to_vec(),
            info.transform,
        ));
    }
    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &CaptureFrame,
        why: WEnum<FailureReason>,
    ) {
        self.failure = Some(format!("COSMIC capture failed: {why:?}"));
    }
    fn cursor_position(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &CaptureCursorSession,
        x: i32,
        y: i32,
    ) {
        self.pointer = Some((x, y));
    }
}
impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        e: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_manager_v1::Event::Head { head } => {
                s.heads.insert(head, Head::default());
            }
            zwlr_output_manager_v1::Event::Done { serial } => s.serial = Some(serial),
            _ => {}
        }
    }
    wayland_client::event_created_child!(State,zwlr_output_manager_v1::ZwlrOutputManagerV1,[0 => (zwlr_output_head_v1::ZwlrOutputHeadV1,())]);
}
impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for State {
    fn event(
        s: &mut Self,
        p: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        e: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let h = s.heads.entry(p.clone()).or_default();
        match e {
            zwlr_output_head_v1::Event::Enabled { enabled } => h.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Scale { scale } => h.scale = scale,
            zwlr_output_head_v1::Event::CurrentMode { mode } => h.mode = Some(mode),
            _ => {}
        }
    }
    wayland_client::event_created_child!(State,zwlr_output_head_v1::ZwlrOutputHeadV1,[3 => (zwlr_output_mode_v1::ZwlrOutputModeV1,())]);
}
impl Dispatch<zwlr_output_mode_v1::ZwlrOutputModeV1, ()> for State {
    fn event(
        s: &mut Self,
        p: &zwlr_output_mode_v1::ZwlrOutputModeV1,
        e: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_mode_v1::Event::Size { width, height } = e {
            s.modes.insert(p.clone(), (width, height));
        }
    }
}
impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        e: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_configuration_v1::Event::Succeeded => s.configured = Some(true),
            zwlr_output_configuration_v1::Event::Failed
            | zwlr_output_configuration_v1::Event::Cancelled => s.configured = Some(false),
            _ => {}
        }
    }
}
sctk::delegate_output!(State);
sctk::delegate_registry!(State);
sctk::delegate_shm!(State);
use cosmic_client_toolkit::{toplevel_info::ToplevelUserData, GlobalData};
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
};
wayland_client::delegate_dispatch!(State:[ZcosmicToplevelInfoV1:GlobalData]=>ToplevelInfoState);
wayland_client::delegate_dispatch!(State:[ZcosmicToplevelHandleV1:GlobalData]=>ToplevelInfoState);
wayland_client::delegate_dispatch!(State:[ExtForeignToplevelListV1:GlobalData]=>ToplevelInfoState);
impl Dispatch<ExtForeignToplevelHandleV1, ToplevelUserData> for State {
    fn event(
        s: &mut Self,
        p: &ExtForeignToplevelHandleV1,
        e: ext_foreign_toplevel_handle_v1::Event,
        d: &ToplevelUserData,
        c: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if matches!(e, ext_foreign_toplevel_handle_v1::Event::Closed) {
            s.pending_windows.remove(&p.id());
        } else {
            s.pending_windows.insert(p.id());
        }
        <ToplevelInfoState as Dispatch<ExtForeignToplevelHandleV1, ToplevelUserData, Self>>::event(
            s, p, e, d, c, q,
        );
    }
}
cosmic_client_toolkit::delegate_toplevel_manager!(State);
cosmic_client_toolkit::delegate_screencopy!(State);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1);

/// Registers the restricted application socket with COSMIC.
///
/// COSMIC hides capture, toplevel management, output configuration, virtual
/// keyboard, input method, layer shell, session lock, data control and
/// keyboard layout globals from clients with a security context. The listener
/// and close descriptors are inherited from the workspace daemon, which keeps
/// the close descriptor's write end open for the session lifetime.
struct SecurityContextClient;
impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, GlobalListContents>
    for SecurityContextClient
{
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(SecurityContextClient: ignore WpSecurityContextManagerV1);
delegate_noop!(SecurityContextClient: ignore WpSecurityContextV1);

fn create_security_context(connection: &Connection, args: &Value) -> Result<Value> {
    let descriptor = |name: &str| -> Result<std::os::fd::BorrowedFd<'static>> {
        let fd = args[name]
            .as_i64()
            .and_then(|fd| i32::try_from(fd).ok())
            .filter(|fd| *fd > 2)
            .with_context(|| format!("COSMIC security context {name} missing"))?;
        // The daemon passed these descriptors to this short-lived helper.
        Ok(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) })
    };
    let instance = args["instance_id"]
        .as_str()
        .context("COSMIC security context instance missing")?;
    let (globals, mut queue) = registry_queue_init::<SecurityContextClient>(connection)?;
    let qh = queue.handle();
    let manager: WpSecurityContextManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("COSMIC security context protocol unavailable")?;
    let context =
        manager.create_listener(descriptor("listen_fd")?, descriptor("close_fd")?, &qh, ());
    context.set_sandbox_engine(SANDBOX_ENGINE.to_owned());
    context.set_instance_id(instance.to_owned());
    context.commit();
    context.destroy();
    manager.destroy();
    queue.roundtrip(&mut SecurityContextClient)?;
    Ok(json!({"sandbox_engine": SANDBOX_ENGINE, "instance_id": instance}))
}

pub(crate) const SANDBOX_ENGINE: &str = "agent-workspace-linux";

/// Every invocation must run under a process deadline, including registry setup.
pub(crate) fn run(socket: &Path, operation: &str, args: &Value) -> Result<Value> {
    let connection = Connection::from_socket(
        UnixStream::connect(socket).context("connect private COSMIC display")?,
    )?;
    if operation == "security-context" {
        return create_security_context(&connection, args);
    }
    if operation == "registry" {
        let (globals, _queue) = registry_queue_init::<SecurityContextClient>(&connection)?;
        let mut interfaces = globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|global| global.interface)
            .collect::<Vec<_>>();
        interfaces.sort();
        interfaces.dedup();
        return Ok(json!({ "interfaces": interfaces }));
    }
    let (globals, mut queue) = registry_queue_init(&connection)?;
    let qh = queue.handle();
    let registry = RegistryState::new(&globals);
    let windows = ToplevelInfoState::try_new(&registry, &qh)
        .context("COSMIC toplevel protocol unavailable")?;
    if windows.cosmic_toplevel_info.is_none() {
        bail!("COSMIC geometry protocol unavailable");
    }
    let manager = ToplevelManagerState::try_new(&registry, &qh)
        .context("COSMIC management protocol unavailable")?;
    let mut state = State {
        output: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh)?,
        capture: ScreencopyState::new(&globals, &qh),
        seat: globals.bind(&qh, 1..=9, ())?,
        output_manager: globals.bind(&qh, 4..=4, ())?,
        registry,
        windows,
        manager,
        heads: HashMap::new(),
        modes: HashMap::new(),
        serial: None,
        pending_windows: HashSet::new(),
        capabilities: vec![],
        pixels: None,
        failure: None,
        configured: None,
        pointer: None,
    };
    queue.roundtrip(&mut state)?;
    while state.serial.is_none() || state.windows.toplevels().count() < state.pending_windows.len()
    {
        queue.blocking_dispatch(&mut state)?;
    }
    let output = state
        .output
        .outputs()
        .next()
        .context("COSMIC has no output")?;
    let (head, head_state) = state
        .heads
        .iter()
        .find(|(_, h)| h.enabled)
        .context("COSMIC has no enabled output")?;
    let mode = *head_state
        .mode
        .as_ref()
        .and_then(|m| state.modes.get(m))
        .context("COSMIC output mode missing")?;
    let scale = head_state.scale;
    if !scale.is_finite() || scale <= 0. {
        bail!("invalid COSMIC output scale");
    }
    if operation == "scale" {
        let requested = args["scale"].as_f64().context("scale missing")?;
        if !requested.is_finite() || requested <= 0. {
            bail!("scale must be positive");
        }
        let config = state
            .output_manager
            .create_configuration(state.serial.unwrap(), &qh, ());
        let change = config.enable_head(head, &qh, ());
        change.set_scale(requested);
        config.apply();
        while state.configured.is_none() {
            queue.blocking_dispatch(&mut state)?;
        }
        if state.configured != Some(true) {
            bail!("COSMIC rejected requested output scale");
        }
        return Ok(json!({"applied":true}));
    }
    if operation == "snapshot" {
        let windows=state.windows.toplevels().map(|w|->Result<Value>{
            let g=w.geometry.get(&output).context("COSMIC window geometry not ready")?;
            Ok(json!({"id":w.identifier,"title":w.title,"app_id":null,"pid":null,"wm_class":w.app_id,"wm_instance":w.app_id,"visible":!w.state.contains(&WindowState::Minimized),"focused":w.state.contains(&WindowState::Activated),"geometry":{"x":(f64::from(g.x)*scale).round() as i32,"y":(f64::from(g.y)*scale).round() as i32,"width":(f64::from(g.width)*scale).round() as u32,"height":(f64::from(g.height)*scale).round() as u32,"screen":0}}))
        }).collect::<Result<Vec<_>>>()?;
        return Ok(
            json!({"width":mode.0,"height":mode.1,"scale":scale,"windows":windows,"capabilities":state.capabilities}),
        );
    }
    let id = args["id"].as_str();
    let target = id
        .map(|id| {
            state
                .windows
                .toplevels()
                .find(|w| w.identifier == id)
                .cloned()
                .context("COSMIC window no longer exists")
        })
        .transpose()?;
    match operation {
        "capture" => {
            let source = target
                .as_ref()
                .map(|w| CaptureSource::Toplevel(w.foreign_toplevel.clone()))
                .unwrap_or(CaptureSource::Output(output));
            let _session = state.capture.capturer().create_session(
                &source,
                CaptureOptions::empty(),
                &qh,
                ScreencopySessionData::default(),
            )?;
            while state.pixels.is_none() && state.failure.is_none() {
                queue.blocking_dispatch(&mut state)?;
            }
            if let Some(f) = state.failure {
                bail!(f);
            }
            let (w, h, pixels, transform) = state.pixels.context("capture returned no image")?;
            let raw =
                image::RgbaImage::from_raw(w, h, pixels).context("invalid image dimensions")?;
            use image::imageops as op;
            let image = match transform {
                WEnum::Value(wl_output::Transform::Normal) => raw,
                WEnum::Value(wl_output::Transform::_90) => op::rotate270(&raw),
                WEnum::Value(wl_output::Transform::_180) => op::rotate180(&raw),
                WEnum::Value(wl_output::Transform::_270) => op::rotate90(&raw),
                WEnum::Value(wl_output::Transform::Flipped) => op::flip_horizontal(&raw),
                WEnum::Value(wl_output::Transform::Flipped180) => op::flip_vertical(&raw),
                _ => bail!("unsupported COSMIC capture transform {transform:?}"),
            };
            let path = args["path"].as_str().context("capture path missing")?;
            image.save(path)?;
            Ok(json!({"width":image.width(),"height":image.height()}))
        }
        "pointer" => {
            let pointer = state.seat.get_pointer(&qh, ());
            let _cursor = state.capture.capturer().create_cursor_session(
                &CaptureSource::Output(output.clone()),
                &pointer,
                &qh,
                ScreencopyCursorSessionData::default(),
            )?;
            while state.pointer.is_none() {
                queue.blocking_dispatch(&mut state)?;
            }
            let (x, y) = state.pointer.unwrap();
            let info = state
                .output
                .info(&output)
                .context("COSMIC output info missing")?;
            let (x, y) = match info.transform {
                wl_output::Transform::Normal => (x, y),
                wl_output::Transform::Flipped180 => (x, mode.1 - y),
                _ => bail!("unsupported COSMIC pointer transform"),
            };
            Ok(json!({"x":x,"y":y,"screen":0}))
        }
        "focus" | "close" | "minimize" | "show" => {
            let target = target.context("window id missing")?;
            let handle = target.cosmic_toplevel.context("COSMIC handle missing")?;
            match operation {
                "focus" => state.manager.manager.activate(&handle, &state.seat),
                "close" => state.manager.manager.close(&handle),
                "minimize" => state.manager.manager.set_minimized(&handle),
                "show" => state.manager.manager.unset_minimized(&handle),
                _ => unreachable!(),
            }
            queue.roundtrip(&mut state)?;
            Ok(json!({"sent":true}))
        }
        _ => bail!("unknown COSMIC operation {operation}"),
    }
}
