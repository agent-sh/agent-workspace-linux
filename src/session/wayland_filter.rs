//! Sandboxed apps reach a native compositor only through this filter.
//!
//! Hyprland and KWin offer capture, input injection, toplevel management,
//! layer shell, output and clipboard-manager globals to every client of their
//! main socket. A wp_security_context_v1 connection is not enough on its own:
//! Hyprland still offers virtual keyboard, virtual pointer and input method
//! there, and KWin hides only its short X-KDE-Wayland-Interfaces list. Each
//! sandboxed client is therefore relayed over a security-context connection,
//! and the relay forwards only allow-listed globals and refuses binds of any
//! other name. The daemon's own helpers keep the unfiltered main socket.
use anyhow::{bail, Context, Result};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    mem::{size_of, zeroed},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::WpSecurityContextManagerV1,
    wp_security_context_v1::WpSecurityContextV1,
};

/// The filtered socket, bound into sandboxes at the main socket's path.
pub(crate) const SOCKET: &str = "sandbox-wayland";
/// The compositor's security-context listener; never bound into sandboxes.
const UPSTREAM: &str = "sandbox-wayland-sc";
const ENGINE: &str = "org.agent_sh.workspace";
// Must not name a desktop entry: KWin grants X-KDE-Wayland-Interfaces by it.
const APP_ID: &str = "org.agent_sh.Workspace.Sandbox";
/// libwayland reads at most 28 file descriptors per recvmsg.
const MAX_FDS: usize = 28;

/// Globals an ordinary app window needs. Anything else, including protocols
/// a future compositor adds, stays hidden from sandboxed apps.
const ALLOWED: &[&str] = &[
    "wl_compositor",
    "wl_subcompositor",
    "wl_shm",
    "wl_seat",
    "wl_output",
    "wl_data_device_manager",
    "wl_fixes",
    "wl_drm",
    "wl_eglstream_controller",
    "xdg_wm_base",
    "xdg_activation_v1",
    "xdg_system_bell_v1",
    "xdg_toplevel_drag_manager_v1",
    "xdg_toplevel_icon_manager_v1",
    "xdg_toplevel_tag_manager_v1",
    "xdg_wm_dialog_v1",
    "zxdg_decoration_manager_v1",
    "zxdg_output_manager_v1",
    "zxdg_exporter_v1",
    "zxdg_exporter_v2",
    "zxdg_importer_v1",
    "zxdg_importer_v2",
    "wp_alpha_modifier_v1",
    "wp_color_manager_v1",
    "wp_color_representation_manager_v1",
    "wp_commit_timing_manager_v1",
    "wp_content_type_manager_v1",
    "wp_cursor_shape_manager_v1",
    "wp_fifo_manager_v1",
    "wp_fractional_scale_manager_v1",
    "wp_linux_drm_syncobj_manager_v1",
    "wp_pointer_warp_v1",
    "wp_presentation",
    "wp_single_pixel_buffer_manager_v1",
    "wp_tearing_control_manager_v1",
    "wp_viewporter",
    "frog_color_management_factory_v1",
    "zwp_idle_inhibit_manager_v1",
    "zwp_keyboard_shortcuts_inhibit_manager_v1",
    "zwp_linux_dmabuf_v1",
    "zwp_linux_explicit_synchronization_v1",
    "zwp_pointer_constraints_v1",
    "zwp_pointer_gestures_v1",
    "zwp_primary_selection_device_manager_v1",
    "zwp_relative_pointer_manager_v1",
    "zwp_tablet_manager_v2",
    "zwp_text_input_manager_v1",
    "zwp_text_input_manager_v2",
    "zwp_text_input_manager_v3",
    "org_kde_kwin_appmenu_manager",
    "org_kde_kwin_blur_manager",
    "org_kde_kwin_contrast_manager",
    "org_kde_kwin_server_decoration_manager",
    "org_kde_kwin_server_decoration_palette_manager",
    "org_kde_kwin_shadow_manager",
    "org_kde_kwin_slide_manager",
    "kde_output_device_v2",
    "kde_output_order_v1",
    "hyprland_surface_manager_v1",
];

/// The filtered socket and the app-visible main socket path it replaces.
pub(crate) fn sandbox_socket(runtime: &Path, main: &str) -> (PathBuf, PathBuf) {
    (runtime.join(SOCKET), runtime.join(main))
}

pub(crate) struct WaylandFilter {
    runtime: PathBuf,
    listener: Arc<UnixListener>,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<HashMap<u64, UnixStream>>>,
    accept: Option<JoinHandle<()>>,
    // The compositor stops accepting on the security context once this
    // write end of its close_fd pipe is gone.
    close: Option<OwnedFd>,
}

impl WaylandFilter {
    pub(crate) fn start(runtime: &Path, main: &Path) -> Result<Self> {
        for name in [SOCKET, UPSTREAM] {
            let path = runtime.join(name);
            if path.symlink_metadata().is_ok() {
                fs::remove_file(&path)?;
            }
        }
        let upstream = UnixListener::bind(runtime.join(UPSTREAM))
            .context("bind sandbox security-context socket")?;
        let mut pipe = [0; 2];
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let (close_read, close_write) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        let instance = runtime
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        security_context(main, &upstream, &close_read, &instance)
            .context("create the sandbox Wayland security context")?;
        // The compositor holds its own copies of both descriptors now.
        drop((upstream, close_read));
        let listener = Arc::new(
            UnixListener::bind(runtime.join(SOCKET)).context("bind sandbox Wayland socket")?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let accept = {
            let (listener, stop, clients) = (listener.clone(), stop.clone(), clients.clone());
            let upstream = runtime.join(UPSTREAM);
            std::thread::Builder::new()
                .name("sandbox-wayland".into())
                .spawn(move || accept_loop(&listener, &upstream, &stop, &clients))?
        };
        Ok(Self {
            runtime: runtime.to_path_buf(),
            listener,
            stop,
            clients,
            accept: Some(accept),
            close: Some(close_write),
        })
    }

    pub(crate) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocked accept(2) on Linux.
        unsafe {
            libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR);
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let clients = self
            .clients
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default();
        for client in clients.values() {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
        self.close.take();
        for name in [SOCKET, UPSTREAM] {
            let _ = fs::remove_file(self.runtime.join(name));
        }
    }
}

impl Drop for WaylandFilter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Registry;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Registry {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(Registry: ignore WpSecurityContextManagerV1);
wayland_client::delegate_noop!(Registry: ignore WpSecurityContextV1);

fn security_context(
    main: &Path,
    listen: &UnixListener,
    close: &OwnedFd,
    instance: &str,
) -> Result<()> {
    let socket = UnixStream::connect(main).context("connect compositor Wayland socket")?;
    // Bound every blocking read: a stalled compositor fails startup instead.
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    let connection = Connection::from_socket(socket)?;
    let (globals, mut queue) = registry_queue_init::<Registry>(&connection)?;
    let handle = queue.handle();
    let manager: WpSecurityContextManagerV1 = globals
        .bind(&handle, 1..=1, ())
        .context("compositor lacks wp_security_context_manager_v1")?;
    let context = manager.create_listener(listen.as_fd(), close.as_fd(), &handle, ());
    context.set_sandbox_engine(ENGINE.into());
    context.set_app_id(APP_ID.into());
    context.set_instance_id(instance.into());
    context.commit();
    context.destroy();
    manager.destroy();
    // A protocol error, such as rejected metadata, surfaces here.
    queue.roundtrip(&mut Registry)?;
    Ok(())
}

fn accept_loop(
    listener: &UnixListener,
    upstream: &Path,
    stop: &AtomicBool,
    clients: &Arc<Mutex<HashMap<u64, UnixStream>>>,
) {
    for (id, client) in (0u64..).zip(listener.incoming()) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(client) = client else {
            // Back off instead of spinning on a persistent accept error.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let Ok(server) = UnixStream::connect(upstream) else {
            continue;
        };
        let Ok(copy) = client.try_clone() else {
            continue;
        };
        if let Ok(mut list) = clients.lock() {
            list.insert(id, copy);
        }
        let forget = {
            let clients = clients.clone();
            move || {
                if let Ok(mut list) = clients.lock() {
                    list.remove(&id);
                }
            }
        };
        if relay(client, server, forget).is_err() {
            if let Ok(mut list) = clients.lock() {
                list.remove(&id);
            }
        }
    }
}

#[derive(Default)]
struct State {
    registries: HashSet<u32>,
    names: HashSet<u32>,
}

/// Relays one client in two threads. Either direction ending closes both.
fn relay(
    client: UnixStream,
    server: UnixStream,
    finished: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    let state = Arc::new(Mutex::new(State::default()));
    let close = |streams: &[&UnixStream; 2]| {
        for stream in streams {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    };
    let (client_in, server_out) = (client.try_clone()?, server.try_clone()?);
    let requests = state.clone();
    std::thread::Builder::new()
        .name("sandbox-wayland-requests".into())
        .spawn(move || {
            let _ = pump(&client_in, &server_out, |buffer| {
                filter_requests(buffer, &requests)
            });
            close(&[&client_in, &server_out]);
        })?;
    std::thread::Builder::new()
        .name("sandbox-wayland-events".into())
        .spawn(move || {
            let _ = pump(&server, &client, |buffer| filter_events(buffer, &state));
            close(&[&client, &server]);
            finished();
        })?;
    Ok(())
}

/// Moves complete messages from `from` to `to`. `filter` takes the buffer,
/// returns the bytes to forward and leaves any partial message in place.
fn pump(
    from: &UnixStream,
    to: &UnixStream,
    mut filter: impl FnMut(&mut Vec<u8>) -> io::Result<Vec<u8>>,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    let mut fds = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        let read = receive(from.as_raw_fd(), &mut chunk, &mut fds)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let out = filter(&mut buffer)?;
        if !out.is_empty() {
            transmit(to.as_raw_fd(), &out, &mut fds)?;
        }
    }
}

fn word(buffer: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(buffer[offset..offset + 4].try_into().unwrap())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Calls `visit` for each complete message; it returns whether to forward
/// it. The complete messages leave the buffer.
fn messages(
    buffer: &mut Vec<u8>,
    mut visit: impl FnMut(u32, u16, &[u8]) -> io::Result<bool>,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(buffer.len());
    let mut offset = 0;
    while buffer.len() - offset >= 8 {
        let header = word(buffer, offset + 4);
        let size = (header >> 16) as usize;
        if size < 8 || !size.is_multiple_of(4) {
            return Err(invalid("malformed Wayland message"));
        }
        if buffer.len() - offset < size {
            break;
        }
        let message = &buffer[offset..offset + size];
        if visit(word(message, 0), header as u16, &message[8..])? {
            out.extend_from_slice(message);
        }
        offset += size;
    }
    buffer.drain(..offset);
    Ok(out)
}

/// A Wayland string argument: length with NUL, then padded bytes.
fn string(body: &[u8], offset: usize) -> Option<&str> {
    let length = *body.get(offset..offset + 4)?.first_chunk::<4>()?;
    let length = u32::from_ne_bytes(length) as usize;
    let bytes = body.get(offset + 4..offset + 4 + length.checked_sub(1)?)?;
    std::str::from_utf8(bytes).ok()
}

fn filter_requests(buffer: &mut Vec<u8>, state: &Mutex<State>) -> io::Result<Vec<u8>> {
    messages(buffer, |object, opcode, body| {
        let mut state = state.lock().map_err(|_| invalid("filter state poisoned"))?;
        if object == 1 && opcode == 1 {
            // wl_display.get_registry(new_id)
            let id = body.get(..4).ok_or_else(|| invalid("short get_registry"))?;
            state
                .registries
                .insert(u32::from_ne_bytes(id.try_into().unwrap()));
        } else if opcode == 0 && state.registries.contains(&object) {
            // wl_registry.bind(name, interface, version, new_id): only names
            // this client was shown, under the advertised interface.
            let name = body.get(..4).ok_or_else(|| invalid("short bind"))?;
            let name = u32::from_ne_bytes(name.try_into().unwrap());
            let interface = string(body, 4).ok_or_else(|| invalid("bad bind"))?;
            if !state.names.contains(&name) || !ALLOWED.contains(&interface) {
                return Err(invalid("sandboxed client bound a hidden global"));
            }
        }
        Ok(true)
    })
}

fn filter_events(buffer: &mut Vec<u8>, state: &Mutex<State>) -> io::Result<Vec<u8>> {
    messages(buffer, |object, opcode, body| {
        let mut state = state.lock().map_err(|_| invalid("filter state poisoned"))?;
        if !state.registries.contains(&object) || opcode > 1 {
            return Ok(true);
        }
        let name = body
            .get(..4)
            .ok_or_else(|| invalid("short registry event"))?;
        let name = u32::from_ne_bytes(name.try_into().unwrap());
        if opcode == 0 {
            // wl_registry.global(name, interface, version)
            let interface = string(body, 4).ok_or_else(|| invalid("bad global"))?;
            if !ALLOWED.contains(&interface) {
                return Ok(false);
            }
            state.names.insert(name);
            Ok(true)
        } else {
            // wl_registry.global_remove(name): only for names shown.
            Ok(state.names.remove(&name))
        }
    })
}

fn receive(fd: RawFd, buffer: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut control = [0u64; 64];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of::<[u64; 64]>();
    let read = loop {
        let read = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
        if read >= 0 {
            break read as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / size_of::<libc::c_int>();
                for index in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(invalid("file descriptors truncated"));
    }
    Ok(read)
}

/// Sends `data`, attaching queued descriptors at most MAX_FDS per sendmsg.
/// A descriptor may arrive before the message that uses it, never after.
fn transmit(fd: RawFd, data: &[u8], fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    let mut sent = 0;
    while sent < data.len() {
        let batch: Vec<OwnedFd> = fds.drain(..fds.len().min(MAX_FDS)).collect();
        // Carry each descriptor batch with a single byte while more remain.
        let end = if fds.is_empty() { data.len() } else { sent + 1 };
        let mut iov = libc::iovec {
            iov_base: data[sent..end].as_ptr() as *mut _,
            iov_len: end - sent,
        };
        let mut control = [0u64; 64];
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !batch.is_empty() {
            let bytes = (batch.len() * size_of::<libc::c_int>()) as u32;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE(bytes) } as usize;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(bytes) as usize;
                let target = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for (index, fd) in batch.iter().enumerate() {
                    target.add(index).write_unaligned(fd.as_raw_fd());
                }
            }
        }
        let written = loop {
            let written = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
            if written >= 0 {
                break written as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        sent += written;
        // The kernel duplicated the batch into the peer; ours close here.
        drop(batch);
    }
    if !fds.is_empty() {
        return Err(invalid("descriptors left without data"));
    }
    Ok(())
}

/// Fails closed when a backend's filter socket is missing.
pub(crate) fn require_socket(socket: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if !fs::symlink_metadata(socket).is_ok_and(|meta| meta.file_type().is_socket()) {
        bail!(
            "sandbox Wayland filter is not serving {}; refusing to launch a sandboxed app",
            socket.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(object: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
        let mut out = object.to_ne_bytes().to_vec();
        out.extend_from_slice(&((((body.len() + 8) as u32) << 16) | opcode as u32).to_ne_bytes());
        out.extend_from_slice(body);
        out
    }

    fn wl_string(value: &str) -> Vec<u8> {
        let mut out = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out.push(0);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    fn global(registry: u32, name: u32, interface: &str) -> Vec<u8> {
        let mut body = name.to_ne_bytes().to_vec();
        body.extend(wl_string(interface));
        body.extend_from_slice(&1u32.to_ne_bytes());
        message(registry, 0, &body)
    }

    fn bind(registry: u32, name: u32, interface: &str, id: u32) -> Vec<u8> {
        let mut body = name.to_ne_bytes().to_vec();
        body.extend(wl_string(interface));
        body.extend_from_slice(&1u32.to_ne_bytes());
        body.extend_from_slice(&id.to_ne_bytes());
        message(registry, 0, &body)
    }

    #[test]
    fn hides_privileged_globals_and_refuses_their_names() {
        let state = Mutex::new(State::default());
        let mut requests = message(1, 1, &2u32.to_ne_bytes());
        assert_eq!(
            filter_requests(&mut requests.clone(), &state).unwrap(),
            requests
        );
        let mut events = [
            global(2, 1, "wl_compositor"),
            global(2, 7, "zwlr_screencopy_manager_v1"),
            global(2, 9, "zwp_virtual_keyboard_manager_v1"),
            message(2, 1, &7u32.to_ne_bytes()),
            message(2, 1, &1u32.to_ne_bytes()),
        ]
        .concat();
        // A partial trailing message waits for the rest of its bytes.
        let tail = global(2, 3, "xdg_wm_base");
        events.extend_from_slice(&tail[..6]);
        let forwarded = filter_events(&mut events, &state).unwrap();
        assert_eq!(
            forwarded,
            [
                global(2, 1, "wl_compositor"),
                message(2, 1, &1u32.to_ne_bytes())
            ]
            .concat()
        );
        assert_eq!(events, tail[..6]);
        events.extend_from_slice(&tail[6..]);
        assert_eq!(filter_events(&mut events, &state).unwrap(), tail);

        requests = bind(2, 3, "xdg_wm_base", 3);
        assert!(filter_requests(&mut requests, &state).is_ok());
        // Guessing a hidden name, or relabelling a shown one, ends the client.
        for request in [
            bind(2, 9, "zwp_virtual_keyboard_manager_v1", 4),
            bind(2, 3, "zwlr_screencopy_manager_v1", 4),
            bind(2, 1, "wl_compositor", 4),
        ] {
            assert!(filter_requests(&mut request.clone(), &state).is_err());
        }
        // Objects other than registries pass untouched.
        let mut other = message(5, 0, &[0; 8]);
        assert_eq!(filter_events(&mut other.clone(), &state).unwrap(), other);
        assert!(filter_requests(&mut other, &state).is_ok());
    }

    #[test]
    fn relays_descriptors_with_their_messages() {
        let (client, client_peer) = UnixStream::pair().unwrap();
        let (server_peer, server) = UnixStream::pair().unwrap();
        relay(client_peer, server_peer, || {}).unwrap();
        let (read, mut write) = {
            let mut pipe = [0; 2];
            assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
            unsafe {
                (
                    OwnedFd::from_raw_fd(pipe[0]),
                    std::fs::File::from_raw_fd(pipe[1]),
                )
            }
        };
        let request = message(5, 3, &[0; 4]);
        transmit(client.as_raw_fd(), &request, &mut vec![read]).unwrap();
        let mut buffer = vec![0u8; 64];
        let mut fds = Vec::new();
        let count = receive(server.as_raw_fd(), &mut buffer, &mut fds).unwrap();
        assert_eq!(&buffer[..count], request.as_slice());
        assert_eq!(fds.len(), 1);
        use std::io::{Read, Write};
        write.write_all(b"ok").unwrap();
        drop(write);
        let mut text = String::new();
        std::fs::File::from(fds.pop().unwrap())
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "ok");
        // A bind of a name the client was never shown closes both directions.
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let registry = message(1, 1, &2u32.to_ne_bytes());
        transmit(client.as_raw_fd(), &registry, &mut Vec::new()).unwrap();
        let count = receive(server.as_raw_fd(), &mut buffer, &mut fds).unwrap();
        assert_eq!(&buffer[..count], registry.as_slice());
        let hidden = bind(2, 40, "zwlr_layer_shell_v1", 3);
        transmit(client.as_raw_fd(), &hidden, &mut Vec::new()).unwrap();
        let mut rest = Vec::new();
        (&server).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }
}
