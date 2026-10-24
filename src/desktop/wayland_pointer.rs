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
        wayland_scanner::generate_interfaces!("protocols/wlr-virtual-pointer-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/wlr-virtual-pointer-unstable-v1.xml");
}
use protocol::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct State {
    input: Option<ZwlrVirtualPointerV1>,
    manager: Option<ZwlrVirtualPointerManagerV1>,
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
            if interface == "zwlr_virtual_pointer_manager_v1" {
                state.advertised_version = version;
            }
            if interface == "zwlr_virtual_pointer_manager_v1" && version >= 1 {
                state.manager = Some(registry.bind(name, 1, qh, ()));
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
impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: protocol::zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub(crate) struct Input {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}
impl Input {
    pub fn connect(socket_path: &Path) -> Result<Self> {
        // An explicit private socket is mandatory. Never use connect_to_env.
        let socket = UnixStream::connect(socket_path)
            .context("connect to workspace Wayland input socket")?;
        let connection = Connection::from_socket(socket)?;
        let queue = connection.new_event_queue();
        connection.display().get_registry(&queue.handle(), ());
        let mut this = Self {
            connection,
            queue,
            state: State::default(),
        };
        this.sync()?;
        this.state.input = Some(
            this.state
                .manager
                .as_ref()
                .context("workspace wlr virtual pointer unavailable")?
                .create_virtual_pointer(None, &this.queue.handle(), ()),
        );
        this.sync()?;
        Ok(this)
    }
    pub fn pointer(&mut self, x: f64, y: f64, width: u32, height: u32) -> Result<()> {
        self.device()
            .motion_absolute(0, x.round() as u32, y.round() as u32, width, height);
        self.device().frame();
        self.sync()
    }
    pub fn button(&mut self, button: u32, pressed: bool) -> Result<()> {
        use wayland_client::protocol::wl_pointer::ButtonState;
        self.device().button(
            0,
            button,
            if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        );
        self.device().frame();
        self.sync()
    }
    pub fn axis(&mut self, horizontal: bool, amount: f64) -> Result<()> {
        use wayland_client::protocol::wl_pointer::Axis;
        self.device().axis(
            0,
            if horizontal {
                Axis::HorizontalScroll
            } else {
                Axis::VerticalScroll
            },
            amount,
        );
        self.device().frame();
        self.sync()
    }
    fn device(&self) -> &ZwlrVirtualPointerV1 {
        self.state.input.as_ref().expect("input connected")
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
                bail!("workspace Wayland input timed out");
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
                    bail!("workspace Wayland input timed out");
                }
                guard.read()?;
            }
        }
        Ok(())
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: protocol::zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
