//! Native, read-only toplevel export. No activation or output cropping.
use anyhow::{bail, Context, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};
use wayland_client::{
    protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, QueueHandle, WEnum,
};
mod foreign {
    #[allow(clippy::single_component_path_imports)]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "protocols/wlr-foreign-toplevel-management-unstable-v1.xml"
        );
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!(
        "protocols/wlr-foreign-toplevel-management-unstable-v1.xml"
    );
}
mod protocol {
    use super::foreign::*;
    #[allow(clippy::single_component_path_imports)]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use super::super::foreign::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}
use protocol::{
    hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1 as Frame},
    hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1 as Manager,
};
#[derive(Default)]
struct State {
    manager: Option<Manager>,
    shm: Option<wl_shm::WlShm>,
    layout: Option<(wl_shm::Format, u32, u32, u32)>,
    enumerated: bool,
    ready: bool,
    failed: bool,
    inverted: bool,
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
            version: _,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "hyprland_toplevel_export_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<Frame, ()> for State {
    fn event(
        state: &mut Self,
        _: &Frame,
        event: hyprland_toplevel_export_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use hyprland_toplevel_export_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => state.layout = Some((format, width, height, stride)),
            Event::BufferDone => state.enumerated = true,
            Event::Ready { .. } => state.ready = true,
            Event::Failed => state.failed = true,
            Event::Flags { flags } => {
                let bits: u32 = flags.into();
                state.inverted = bits & 1 != 0;
            }
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore Manager);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);

pub(super) fn capture(runtime: &Path, address: u64) -> Result<image::RgbaImage> {
    let socket = UnixStream::connect(runtime.join(crate::session::hyprland::WAYLAND_SOCKET))?;
    let connection = Connection::from_socket(socket)?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut state = State::default();
    connection.display().get_registry(&qh, ());
    // All event waits, including registry discovery and capture, share one deadline.
    let deadline = Instant::now() + Duration::from_secs(5);
    let pump = |queue: &mut wayland_client::EventQueue<State>, state: &mut State| -> Result<()> {
        queue.dispatch_pending(state)?;
        connection.flush()?;
        if Instant::now() >= deadline {
            bail!("Hyprland window capture timed out")
        }
        if let Some(guard) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(5000) as i32;
            let ready = unsafe { libc::poll(&mut fd, 1, ms) };
            if ready < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if ready == 0 {
                bail!("Hyprland window capture timed out")
            }
            guard.read()?;
        }
        queue.dispatch_pending(state)?;
        Ok(())
    };
    while state.manager.is_none() || state.shm.is_none() {
        pump(&mut queue, &mut state)?;
    }
    // Hyprland's public uint handle is the low32 bits of the clients address.
    let frame = state
        .manager
        .as_ref()
        .unwrap()
        .capture_toplevel(0, address as u32, &qh, ());
    while !state.enumerated && !state.failed {
        pump(&mut queue, &mut state)?;
    }
    if state.failed {
        bail!("Hyprland refused native toplevel capture")
    }
    let (format, width, height, stride) = state.layout.context("Hyprland supplied no shm frame")?;
    if !matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Abgr8888
            | wl_shm::Format::Xbgr8888
    ) || width == 0
        || height == 0
        || stride < width.checked_mul(4).context("capture width overflow")?
    {
        bail!("unsupported Hyprland shm frame {format:?} {width}x{height}/{stride}")
    }
    let length = stride
        .checked_mul(height)
        .filter(|n| *n <= i32::MAX as u32)
        .context("capture size overflow")?;
    let name = c"agent-workspace-capture";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_len(length as u64)?;
    let pool = state
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), length as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );
    frame.copy(&buffer, 1);
    while !state.ready && !state.failed {
        pump(&mut queue, &mut state)?;
    }
    frame.destroy();
    buffer.destroy();
    pool.destroy();
    connection.flush()?;
    if state.failed {
        bail!("Hyprland native window capture failed")
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    let mut image = image::RgbaImage::new(width, height);
    for y in 0..height {
        let source_y = if state.inverted { height - 1 - y } else { y };
        for x in 0..width {
            let at = (source_y * stride + x * 4) as usize;
            let p = &bytes[at..at + 4];
            let rgba = if matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
                [p[2], p[1], p[0], p[3]]
            } else {
                [p[0], p[1], p[2], p[3]]
            };
            let mut rgba = rgba;
            if matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888) {
                rgba[3] = 255;
            }
            image.put_pixel(x, y, image::Rgba(rgba));
        }
    }
    Ok(image)
}
