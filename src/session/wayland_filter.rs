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
//!
//! Compositors take a window's pid from the credentials of its connection.
//! The relay's upstream connection is therefore made by a small holder
//! process forked per client, and window pids are mapped from that holder
//! back to the sandboxed client, so no window reports the daemon's pid.
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
    mem::{size_of, zeroed},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
/// libwayland's MAX_FDS_OUT: a peer reads at most 28 descriptors per recvmsg.
const MAX_FDS: usize = 28;
/// libwayland's WL_MAX_MESSAGE_SIZE; both ends drop larger messages.
const MAX_MESSAGE: usize = 4096;
/// A libwayland peer's whole descriptor ring (WL_BUFFER_DEFAULT_MAX_SIZE
/// bytes of 4-byte fds). libwayland sends queued descriptors 28 at a time with
/// one byte each before the messages that carry them, so a well-behaved
/// client can have this many queued while its next message is incomplete.
const MAX_QUEUED_FDS: usize = 1024;
/// Each sandboxed Wayland client process holds one connection; an agent
/// workspace runs a handful of GUI apps. Every connection costs three
/// descriptors, two threads and a holder process.
const MAX_CONNECTIONS: usize = 64;
/// The holder's process name (at most 15 bytes), so a holder pid without a
/// live mapping is never reported as a window's pid.
const HOLDER_NAME: &str = "awl-wl-holder";

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

/// Holder pid -> sandboxed client pid, for every live relayed connection.
static PEERS: Mutex<BTreeMap<u32, u32>> = Mutex::new(BTreeMap::new());

/// The pid a window should report, given the pid its compositor reports.
/// Relayed windows map to their sandboxed client; the daemon itself and
/// holders without a live mapping report none.
pub(crate) fn window_pid(pid: Option<u32>) -> Option<u32> {
    let pid = pid?;
    if pid == std::process::id() {
        return None;
    }
    if let Some(client) = PEERS.lock().ok().and_then(|peers| peers.get(&pid).copied()) {
        return (client != 0).then_some(client);
    }
    let holder = fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim_end() == HOLDER_NAME);
    (!holder).then_some(pid)
}

/// Descriptor budgets, from the daemon's soft RLIMIT_NOFILE, so relayed
/// clients cannot exhaust the descriptors the daemon needs for its own IPC,
/// spawns and capture.
#[derive(Clone, Copy)]
struct Limits {
    connections: usize,
    queued_fds: usize,
    /// Descriptors one connection may queue without their messages.
    per_connection_fds: usize,
}

impl Limits {
    fn from_rlimit() -> Self {
        let mut limit: libc::rlimit = unsafe { zeroed() };
        let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
            limit.rlim_cur as usize
        } else {
            1024
        };
        // At most a quarter of the descriptors for queued client fds and a
        // sixteenth (three each) for connections; the rest stays the daemon's.
        Self {
            connections: MAX_CONNECTIONS.min(soft / 48).max(1),
            queued_fds: MAX_QUEUED_FDS.min(soft / 4),
            per_connection_fds: MAX_QUEUED_FDS,
        }
    }
}

pub(crate) struct WaylandFilter {
    runtime: PathBuf,
    listener: Arc<UnixListener>,
    stop: Arc<AtomicBool>,
    links: Arc<Mutex<HashMap<u64, Arc<Link>>>>,
    accept: Option<JoinHandle<()>>,
    // The compositor stops accepting on the security context once this
    // write end of its close_fd pipe is gone.
    close: Option<OwnedFd>,
}

impl WaylandFilter {
    /// Creates a security context on `main` and relays sandboxed clients over it.
    pub(crate) fn start(runtime: &Path, main: &Path) -> Result<Self> {
        let upstream_path = runtime.join(UPSTREAM);
        if upstream_path.symlink_metadata().is_ok() {
            fs::remove_file(&upstream_path)?;
        }
        let upstream =
            UnixListener::bind(&upstream_path).context("bind sandbox security-context socket")?;
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
        let mut filter = Self::relay(runtime, &upstream_path)?;
        filter.close = Some(close_write);
        Ok(filter)
    }

    /// Relays sandboxed clients to an existing security-context socket.
    pub(crate) fn relay(runtime: &Path, upstream: &Path) -> Result<Self> {
        let path = runtime.join(SOCKET);
        if path.symlink_metadata().is_ok() {
            fs::remove_file(&path)?;
        }
        let address = SocketAddress::new(upstream)?;
        let listener = Arc::new(UnixListener::bind(&path).context("bind sandbox Wayland socket")?);
        let stop = Arc::new(AtomicBool::new(false));
        let links = Arc::new(Mutex::new(HashMap::new()));
        let accept = {
            let (listener, stop, links) = (listener.clone(), stop.clone(), links.clone());
            std::thread::Builder::new()
                .name("sandbox-wayland".into())
                .spawn(move || {
                    accept_loop(&listener, &address, &stop, &links, Limits::from_rlimit())
                })?
        };
        Ok(Self {
            runtime: runtime.to_path_buf(),
            listener,
            stop,
            links,
            accept: Some(accept),
            close: None,
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
        let links = self
            .links
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default();
        for link in links.values() {
            link.close();
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

/// A sockaddr_un built before fork, so the holder needs no allocation.
struct SocketAddress {
    address: libc::sockaddr_un,
    length: libc::socklen_t,
}

impl SocketAddress {
    fn new(path: &Path) -> Result<Self> {
        let mut address: libc::sockaddr_un = unsafe { zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= address.sun_path.len() {
            bail!(
                "sandbox Wayland upstream path is too long: {}",
                path.display()
            );
        }
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as libc::c_char;
        }
        let length = (size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
        Ok(Self { address, length })
    }
}

/// The holder process whose credentials the compositor sees for one client.
struct Holder {
    pid: libc::pid_t,
    control: Option<UnixStream>,
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Ok(mut peers) = PEERS.lock() {
            peers.remove(&(self.pid as u32));
        }
        // A holder still blocked in connect(2) never reads its control
        // socket, so end it outright before reaping.
        self.control.take();
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
        }
    }
}

/// Forks a holder that connects to `upstream` and passes the connection back.
fn connect_through_holder(
    upstream: &SocketAddress,
    client_pid: u32,
) -> io::Result<(UnixStream, Holder)> {
    let (control, child) = UnixStream::pair()?;
    let child_fd = child.as_raw_fd();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe { holder(child_fd, upstream) }
    }
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(child);
    let holder = Holder {
        pid,
        control: Some(control),
    };
    // Register before the holder connects, then let it connect.
    PEERS
        .lock()
        .map_err(|_| invalid("relay pid map poisoned"))?
        .insert(pid as u32, client_pid);
    let control = holder.control.as_ref().unwrap();
    let go = [1u8];
    if unsafe {
        libc::send(
            control.as_raw_fd(),
            go.as_ptr().cast(),
            1,
            libc::MSG_NOSIGNAL,
        )
    } != 1
    {
        return Err(io::Error::last_os_error());
    }
    control.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut fds = Vec::new();
    let mut byte = [0u8; 1];
    let read = receive(control.as_raw_fd(), &mut byte, &mut fds)?;
    match (read, fds.pop()) {
        (1, Some(fd)) if fds.is_empty() => Ok((UnixStream::from(fd), holder)),
        _ => Err(invalid("sandbox Wayland holder did not connect")),
    }
}

/// The forked holder. The daemon is multi-threaded, so this uses only
/// async-signal-safe system calls and never returns.
unsafe fn holder(control: RawFd, upstream: &SocketAddress) -> ! {
    // Keep only the control socket: inherited copies of other relays'
    // descriptors would hold their connections open.
    // close_range needs Linux 5.9; without it, fail rather than keep copies.
    if (control > 0 && libc::close_range(0, control as libc::c_uint - 1, 0) != 0)
        || libc::close_range(control as libc::c_uint + 1, libc::c_uint::MAX, 0) != 0
    {
        libc::_exit(126);
    }
    let mut name = [0u8; 16];
    name[..HOLDER_NAME.len()].copy_from_slice(HOLDER_NAME.as_bytes());
    libc::prctl(libc::PR_SET_NAME, name.as_ptr());
    // Daemon signal handlers must not run in the holder.
    let default: libc::sigaction = zeroed();
    for signal in 1..32 {
        if signal != libc::SIGKILL && signal != libc::SIGSTOP {
            libc::sigaction(signal, &default, std::ptr::null_mut());
        }
    }
    // Connect only after the parent has recorded this holder's mapping, so
    // the compositor never sees a connection without one.
    let mut go = [0u8; 1];
    loop {
        match libc::read(control, go.as_mut_ptr().cast(), 1) {
            1 => break,
            -1 if *libc::__errno_location() == libc::EINTR => {}
            _ => libc::_exit(1),
        }
    }
    let socket = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
    if socket < 0
        || libc::connect(
            socket,
            (&upstream.address as *const libc::sockaddr_un).cast(),
            upstream.length,
        ) < 0
    {
        libc::_exit(1);
    }
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control_buffer = [0u64; 4];
    let mut message: libc::msghdr = zeroed();
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control_buffer.as_mut_ptr().cast();
    message.msg_controllen = libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) as usize;
    let header = libc::CMSG_FIRSTHDR(&message);
    (*header).cmsg_level = libc::SOL_SOCKET;
    (*header).cmsg_type = libc::SCM_RIGHTS;
    (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as usize;
    libc::CMSG_DATA(header)
        .cast::<libc::c_int>()
        .write_unaligned(socket);
    if libc::sendmsg(control, &message, libc::MSG_NOSIGNAL) != 1 {
        libc::_exit(1);
    }
    libc::close(socket);
    // Stay alive, so the pid the compositor recorded stays this process,
    // until the daemon closes the control socket or exits.
    loop {
        let read = libc::read(control, byte.as_mut_ptr().cast(), 1);
        if read == 0 || (read < 0 && *libc::__errno_location() != libc::EINTR) {
            libc::_exit(0);
        }
    }
}

fn peer_pid(stream: &UnixStream) -> u32 {
    let mut credentials: libc::ucred = unsafe { zeroed() };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    // Credentials are translated into the daemon's pid namespace.
    if ok == 0 {
        credentials.pid as u32
    } else {
        0
    }
}

fn accept_loop(
    listener: &UnixListener,
    upstream: &SocketAddress,
    stop: &AtomicBool,
    links: &Arc<Mutex<HashMap<u64, Arc<Link>>>>,
    limits: Limits,
) {
    let budget = Arc::new(Budget {
        queued: AtomicUsize::new(0),
        limit: limits.queued_fds,
        per_connection: limits.per_connection_fds,
    });
    for (id, client) in (0u64..).zip(listener.incoming()) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(client) = client else {
            // Back off instead of spinning on a persistent accept error.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if links
            .lock()
            .map_or(true, |list| list.len() >= limits.connections)
        {
            eprintln!(
                "sandbox Wayland relay refused a client: {} connections already open",
                limits.connections
            );
            continue;
        }
        let Ok((server, holder)) = connect_through_holder(upstream, peer_pid(&client)) else {
            continue;
        };
        let link = Arc::new(Link { client, server });
        if let Ok(mut list) = links.lock() {
            list.insert(id, link.clone());
        }
        let forget = {
            let links = links.clone();
            move || {
                if let Ok(mut list) = links.lock() {
                    list.remove(&id);
                }
            }
        };
        if relay(link, holder, budget.clone(), forget).is_err() {
            if let Ok(mut list) = links.lock() {
                list.remove(&id);
            }
        }
    }
}

/// Queued client descriptors across every connection of one filter.
struct Budget {
    queued: AtomicUsize,
    limit: usize,
    per_connection: usize,
}

struct Link {
    client: UnixStream,
    server: UnixStream,
}

impl Link {
    fn close(&self) {
        let _ = self.client.shutdown(std::net::Shutdown::Both);
        let _ = self.server.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Default)]
struct State {
    /// Names each registry was shown, with their interfaces. A removed name
    /// stays bindable: a bind can race global_remove, and the compositor
    /// gives such a bind an inert object.
    registries: HashMap<u32, HashMap<u32, String>>,
    /// A registry only ends through wl_fixes.destroy_registry, so no
    /// delete_id can unfilter a live one. Ids of destroyed registries keep
    /// dropping hidden `global` events still in flight to them until a new
    /// registry takes the id; other objects reusing the id are unaffected.
    former_registries: HashSet<u32>,
    /// Bound wl_fixes objects, the only way to destroy a registry.
    fixes: HashSet<u32>,
}

/// Relays one client in two threads. Either direction ending closes both.
fn relay(
    link: Arc<Link>,
    holder: Holder,
    budget: Arc<Budget>,
    finished: impl FnOnce() + Send + 'static,
) -> io::Result<()> {
    let state = Arc::new(Mutex::new(State::default()));
    let (requests_link, requests) = (link.clone(), state.clone());
    let thread = |name: &str| {
        std::thread::Builder::new()
            .name(name.into())
            .stack_size(256 * 1024)
    };
    thread("sandbox-wayland-requests").spawn(move || {
        let _ = pump(
            &requests_link.client,
            &requests_link.server,
            Some(&budget),
            |buffer| filter_requests(buffer, &requests),
        );
        requests_link.close();
    })?;
    thread("sandbox-wayland-events").spawn(move || {
        let _ = pump(&link.server, &link.client, None, |buffer| {
            filter_events(buffer, &state)
        });
        link.close();
        drop(holder);
        finished();
    })?;
    Ok(())
}

/// Moves complete messages from `from` to `to`. `filter` takes the buffer,
/// returns the bytes to forward and leaves any partial message in place.
/// Queued descriptors count against `budget`, when given, and the
/// per-connection cap.
fn pump(
    from: &UnixStream,
    to: &UnixStream,
    budget: Option<&Budget>,
    mut filter: impl FnMut(&mut Vec<u8>) -> io::Result<Vec<u8>>,
) -> io::Result<()> {
    let mut buffer = Vec::new();
    let mut fds = Vec::new();
    let mut counted = 0;
    let result = (|| {
        let mut chunk = vec![0u8; 65536];
        loop {
            let read = receive(from.as_raw_fd(), &mut chunk, &mut fds)?;
            if let Some(budget) = budget {
                let added = fds.len() - counted;
                counted = fds.len();
                if budget.queued.fetch_add(added, Ordering::SeqCst) + added > budget.limit {
                    return Err(invalid("sandboxed clients queued too many descriptors"));
                }
            }
            if fds.len() > budget.map_or(MAX_QUEUED_FDS, |budget| budget.per_connection) {
                return Err(invalid(
                    "too many descriptors queued without their messages",
                ));
            }
            if read == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..read]);
            let out = filter(&mut buffer)?;
            if !out.is_empty() {
                transmit(to.as_raw_fd(), &out, &mut fds)?;
                if let Some(budget) = budget {
                    budget
                        .queued
                        .fetch_sub(counted - fds.len(), Ordering::SeqCst);
                    counted = fds.len();
                }
            }
        }
    })();
    if let Some(budget) = budget {
        budget.queued.fetch_sub(counted, Ordering::SeqCst);
    }
    result
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
        if size < 8 || !size.is_multiple_of(4) || size > MAX_MESSAGE {
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

/// The interface of a body that is exactly wl_registry.global(name,
/// interface, version), with nothing before or after.
fn exact_global(body: &[u8]) -> Option<&str> {
    let interface = string(body, 4)?;
    (body.len() == 4 + 4 + (interface.len() + 1).next_multiple_of(4) + 4).then_some(interface)
}

fn name(body: &[u8]) -> io::Result<u32> {
    let name = body
        .get(..4)
        .ok_or_else(|| invalid("short registry message"))?;
    Ok(u32::from_ne_bytes(name.try_into().unwrap()))
}

fn filter_requests(buffer: &mut Vec<u8>, state: &Mutex<State>) -> io::Result<Vec<u8>> {
    messages(buffer, |object, opcode, body| {
        let mut state = state.lock().map_err(|_| invalid("filter state poisoned"))?;
        if object == 1 && opcode == 1 {
            // wl_display.get_registry(new_id)
            let id = name(body)?;
            state.former_registries.remove(&id);
            state.registries.insert(id, HashMap::new());
        } else if let Some(shown) = state.registries.get(&object) {
            // wl_registry.bind(name, interface, version, new_id): only names
            // this registry was shown, under that interface.
            let interface = string(body, 4).ok_or_else(|| invalid("bad bind"))?;
            if opcode != 0 || shown.get(&name(body)?).map(String::as_str) != Some(interface) {
                return Err(invalid("sandboxed client bound a hidden global"));
            }
            if interface == "wl_fixes" {
                let id_offset = 4 + 4 + (interface.len() + 1).next_multiple_of(4) + 4;
                let id = body
                    .get(id_offset..id_offset + 4)
                    .ok_or_else(|| invalid("bad bind"))?;
                state
                    .fixes
                    .insert(u32::from_ne_bytes(id.try_into().unwrap()));
            }
        } else if state.fixes.contains(&object) {
            match opcode {
                // wl_fixes.destroy: the id may name anything afterwards.
                0 => {
                    state.fixes.remove(&object);
                }
                // wl_fixes.destroy_registry(registry)
                1 => {
                    let registry = name(body)?;
                    if state.registries.remove(&registry).is_some() {
                        state.former_registries.insert(registry);
                    }
                }
                _ => {}
            }
        }
        Ok(true)
    })
}

fn filter_events(buffer: &mut Vec<u8>, state: &Mutex<State>) -> io::Result<Vec<u8>> {
    messages(buffer, |object, opcode, body| {
        let mut state = state.lock().map_err(|_| invalid("filter state poisoned"))?;
        let Some(shown) = state.registries.get_mut(&object) else {
            // A destroyed registry's in-flight globals stay filtered; an
            // object reusing the id never sends a message of exactly this
            // shape with a hidden interface name.
            if opcode == 0 && state.former_registries.contains(&object) {
                if let Some(interface) = exact_global(body) {
                    return Ok(ALLOWED.contains(&interface));
                }
            }
            return Ok(true);
        };
        match opcode {
            0 => {
                // wl_registry.global(name, interface, version)
                let interface = string(body, 4).ok_or_else(|| invalid("bad global"))?;
                if !ALLOWED.contains(&interface) {
                    return Ok(false);
                }
                shown.insert(name(body)?, interface.to_owned());
                Ok(true)
            }
            // wl_registry.global_remove(name): only for names this registry showed.
            1 => Ok(shown.contains_key(&name(body)?)),
            _ => Ok(true),
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

/// Sends `data`, attaching queued descriptors at most MAX_FDS per sendmsg,
/// with one byte per batch while more data follows, as libwayland does. A
/// descriptor may arrive before the message that uses it, never after, so
/// descriptors left when the data runs out stay queued for later messages.
fn transmit(fd: RawFd, data: &[u8], fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    let mut sent = 0;
    while sent < data.len() {
        let batch: Vec<OwnedFd> = fds.drain(..fds.len().min(MAX_FDS)).collect();
        let end = if !fds.is_empty() && data.len() - sent > 1 {
            sent + 1
        } else {
            data.len()
        };
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

    fn send_fds(stream: &UnixStream, data: &[u8], count: usize) -> io::Result<()> {
        let null = fs::File::open("/dev/null").unwrap();
        let mut fds = (0..count)
            .map(|_| OwnedFd::from(null.try_clone().unwrap()))
            .collect::<Vec<_>>();
        transmit(stream.as_raw_fd(), data, &mut fds)
    }

    /// An accept loop relaying to a local upstream listener.
    struct Harness {
        dir: PathBuf,
        upstream: UnixListener,
        listener: Arc<UnixListener>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn new(name: &str, limits: Limits) -> Self {
            let dir = std::env::temp_dir().join(format!("awl-wf-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let upstream = UnixListener::bind(dir.join("up")).unwrap();
            let listener = Arc::new(UnixListener::bind(dir.join("in")).unwrap());
            let address = SocketAddress::new(&dir.join("up")).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (listener, stop) = (listener.clone(), stop.clone());
                let links = Arc::new(Mutex::new(HashMap::new()));
                std::thread::spawn(move || accept_loop(&listener, &address, &stop, &links, limits))
            };
            Self {
                dir,
                upstream,
                listener,
                stop,
                thread: Some(thread),
            }
        }

        fn connect(&self) -> (UnixStream, UnixStream) {
            let client = UnixStream::connect(self.dir.join("in")).unwrap();
            let (server, _) = self.upstream.accept().unwrap();
            for stream in [&client, &server] {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
            }
            (client, server)
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            unsafe {
                libc::shutdown(self.listener.as_raw_fd(), libc::SHUT_RDWR);
            }
            let _ = self.thread.take().unwrap().join();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const OPEN: Limits = Limits {
        connections: MAX_CONNECTIONS,
        queued_fds: MAX_QUEUED_FDS,
        per_connection_fds: MAX_QUEUED_FDS,
    };

    /// Waits until the relay has read everything `stream` sent, so the
    /// descriptors it carried are counted before the test goes on. The pump
    /// counts them just after the read; the next step (a new connection
    /// through a forked holder) is far slower than that gap.
    fn drained(stream: &UnixStream) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut queued: libc::c_int = 0;
            let ok = unsafe { libc::ioctl(stream.as_raw_fd(), libc::TIOCOUTQ, &mut queued) };
            assert_eq!(ok, 0, "SIOCOUTQ");
            if queued == 0 {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "relay did not read");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[track_caller]
    fn closed(stream: &UnixStream) -> Vec<u8> {
        use std::io::Read;
        let mut rest = Vec::new();
        // Closing a socket with unread data resets it; that is closed too.
        match (&*stream).read_to_end(&mut rest) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            Err(error) => panic!("reading a closed relay connection: {error}"),
        }
        rest
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
        ]
        .concat();
        // A partial trailing message waits for the rest of its bytes.
        let tail = global(2, 3, "xdg_wm_base");
        events.extend_from_slice(&tail[..6]);
        let forwarded = filter_events(&mut events, &state).unwrap();
        assert_eq!(forwarded, global(2, 1, "wl_compositor"));
        assert_eq!(events, tail[..6]);
        events.extend_from_slice(&tail[6..]);
        assert_eq!(filter_events(&mut events, &state).unwrap(), tail);

        requests = bind(2, 3, "xdg_wm_base", 3);
        assert!(filter_requests(&mut requests, &state).is_ok());
        // Guessing a hidden name, or relabelling a shown one, ends the client.
        for request in [
            bind(2, 9, "zwp_virtual_keyboard_manager_v1", 4),
            bind(2, 3, "zwlr_screencopy_manager_v1", 4),
        ] {
            assert!(filter_requests(&mut request.clone(), &state).is_err());
        }
        // Objects other than registries pass untouched.
        let mut other = message(5, 0, &[0; 8]);
        assert_eq!(filter_events(&mut other.clone(), &state).unwrap(), other);
        assert!(filter_requests(&mut other, &state).is_ok());
    }

    #[test]
    fn tracks_names_per_registry_and_tolerates_removal_races() {
        let state = Mutex::new(State::default());
        for registry in [2u32, 3] {
            filter_requests(&mut message(1, 1, &registry.to_ne_bytes()), &state).unwrap();
        }
        let mut events = [global(2, 1, "wl_output"), global(3, 1, "wl_output")].concat();
        assert_eq!(filter_events(&mut events.clone(), &state).unwrap(), events);
        // An output unplug reaches every registry that showed it.
        let removes = [
            message(2, 1, &1u32.to_ne_bytes()),
            message(3, 1, &1u32.to_ne_bytes()),
        ]
        .concat();
        assert_eq!(
            filter_events(&mut removes.clone(), &state).unwrap(),
            removes
        );
        // A bind that raced the removal stays legal; the compositor makes it inert.
        assert!(filter_requests(&mut bind(3, 1, "wl_output", 4), &state).is_ok());
        // A name is bindable only on the registry that showed it.
        events = global(2, 5, "wl_seat");
        filter_events(&mut events, &state).unwrap();
        assert!(filter_requests(&mut bind(3, 5, "wl_seat", 5), &state).is_err());
        assert!(filter_requests(&mut bind(2, 5, "wl_seat", 5), &state).is_ok());
    }

    #[test]
    fn no_delete_id_unfilters_a_live_registry() {
        let state = Mutex::new(State::default());
        // wl_display.sync(new_id 4), then get_registry(new_id 4) at once:
        // the callback's delete_id(4) arrives while registry 4 is live.
        filter_requests(&mut message(1, 0, &4u32.to_ne_bytes()), &state).unwrap();
        filter_requests(&mut message(1, 1, &4u32.to_ne_bytes()), &state).unwrap();
        filter_events(&mut message(1, 1, &4u32.to_ne_bytes()), &state).unwrap();
        let mut hidden = global(4, 9, "zwlr_virtual_pointer_manager_v1");
        assert!(filter_events(&mut hidden, &state).unwrap().is_empty());
        assert!(filter_requests(
            &mut bind(4, 9, "zwlr_virtual_pointer_manager_v1", 5),
            &state
        )
        .is_err());
    }

    #[test]
    fn destroyed_registries_end_only_through_wl_fixes() {
        let state = Mutex::new(State::default());
        filter_requests(&mut message(1, 1, &2u32.to_ne_bytes()), &state).unwrap();
        filter_events(&mut global(2, 1, "wl_fixes"), &state).unwrap();
        filter_requests(&mut bind(2, 1, "wl_fixes", 3), &state).unwrap();
        // sync(4), get_registry(4), destroy_registry(4) in one write: the
        // callback's delete_id(4) precedes the registry's globals.
        filter_requests(&mut message(1, 0, &4u32.to_ne_bytes()), &state).unwrap();
        filter_requests(&mut message(1, 1, &4u32.to_ne_bytes()), &state).unwrap();
        filter_requests(&mut message(3, 1, &4u32.to_ne_bytes()), &state).unwrap();
        filter_events(&mut message(1, 1, &4u32.to_ne_bytes()), &state).unwrap();
        let mut late = global(4, 9, "zwlr_virtual_pointer_manager_v1");
        assert!(filter_events(&mut late, &state).unwrap().is_empty());
        let allowed = global(4, 1, "wl_compositor");
        assert_eq!(
            filter_events(&mut allowed.clone(), &state).unwrap(),
            allowed
        );
        // After its delete_id a wl_callback reusing the id works.
        filter_events(&mut message(1, 1, &4u32.to_ne_bytes()), &state).unwrap();
        let done = message(4, 0, &7u32.to_ne_bytes());
        assert_eq!(filter_events(&mut done.clone(), &state).unwrap(), done);
        assert!(filter_requests(&mut done.clone(), &state).is_ok());
        // After wl_fixes.destroy, its id names something else: opcode 1 on
        // it no longer destroys registries.
        filter_requests(&mut message(3, 0, &[]), &state).unwrap();
        filter_requests(&mut message(3, 1, &2u32.to_ne_bytes()), &state).unwrap();
        let mut hidden = global(2, 9, "zwlr_virtual_pointer_manager_v1");
        assert!(filter_events(&mut hidden, &state).unwrap().is_empty());
    }

    #[test]
    fn relays_descriptors_and_reports_the_client_pid_for_the_holder() {
        let harness = Harness::new("relay", OPEN);
        let (client, server) = harness.connect();
        let holder = peer_pid(&server);
        assert_ne!(holder, std::process::id());
        assert_eq!(window_pid(Some(holder)), Some(std::process::id()));
        assert_eq!(window_pid(Some(std::process::id())), None);
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
        let registry = message(1, 1, &2u32.to_ne_bytes());
        transmit(client.as_raw_fd(), &registry, &mut Vec::new()).unwrap();
        let count = receive(server.as_raw_fd(), &mut buffer, &mut fds).unwrap();
        assert_eq!(&buffer[..count], registry.as_slice());
        transmit(
            client.as_raw_fd(),
            &bind(2, 40, "zwlr_layer_shell_v1", 3),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(closed(&server).is_empty());
        // The holder is unmapped and reaped with its connection.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Path::new(&format!("/proc/{holder}")).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "holder {holder} survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!PEERS.lock().unwrap().contains_key(&holder));
    }

    #[test]
    fn forwards_libwaylands_descriptor_bursts() {
        // libwayland sends 28 descriptors with one byte while more remain,
        // before the bytes of the messages that carry them.
        let harness = Harness::new("burst", OPEN);
        let (client, server) = harness.connect();
        let messages = [0, 1, 2, 3].map(|n| message(5, n, &[0; 4])).concat();
        for byte in 0..3 {
            send_fds(&client, &messages[byte..byte + 1], 28).unwrap();
        }
        send_fds(&client, &messages[3..], 16).unwrap();
        let mut received = Vec::new();
        let mut fds = Vec::new();
        let mut buffer = vec![0u8; 256];
        while received.len() < messages.len() {
            let count = receive(server.as_raw_fd(), &mut buffer, &mut fds).unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(received, messages);
        assert_eq!(fds.len(), 100);
    }

    #[test]
    fn closes_clients_that_queue_descriptors_without_messages() {
        // Descriptors in flight over Unix sockets count against a per-user
        // limit on the host (ETOOMANYREFS past it), which a desktop session
        // shares: a flood of them here crashed a running terminal. Keep every
        // case to a few dozen descriptors with small caps.
        let partial = &message(5, 0, &[0; 4088])[..8];
        // Past the per-connection cap.
        let harness = Harness::new(
            "flood",
            Limits {
                connections: 4,
                queued_fds: MAX_QUEUED_FDS,
                per_connection_fds: 56,
            },
        );
        let (client, server) = harness.connect();
        let mut sent = 0;
        let mut result = send_fds(&client, partial, 28);
        while result.is_ok() && sent <= 56 {
            sent += 28;
            result = send_fds(&client, &[0], 28);
        }
        assert!(closed(&server).is_empty());
        assert!(sent > 56, "closed before the cap: {sent}");
        // Past the budget shared by every connection of one filter.
        let harness = Harness::new(
            "budget",
            Limits {
                connections: 4,
                queued_fds: 56,
                per_connection_fds: 56,
            },
        );
        let (first, first_server) = harness.connect();
        send_fds(&first, partial, 28).unwrap();
        drained(&first);
        let (second, second_server) = harness.connect();
        send_fds(&second, partial, 28).unwrap();
        let _ = send_fds(&second, &[0], 28);
        assert!(closed(&second_server).is_empty());
        // The budget comes back when the flooding connection ends.
        let (third, third_server) = harness.connect();
        send_fds(&third, &message(5, 0, &[0; 4]), 28).unwrap();
        let mut buffer = vec![0u8; 64];
        let mut fds = Vec::new();
        assert_eq!(
            receive(third_server.as_raw_fd(), &mut buffer, &mut fds).unwrap(),
            12
        );
        assert_eq!(fds.len(), 28);
        drop((first, first_server));
    }

    #[test]
    fn closes_oversized_messages_and_connections_past_the_cap() {
        let harness = Harness::new(
            "cap",
            Limits {
                connections: 1,
                queued_fds: MAX_QUEUED_FDS,
                per_connection_fds: MAX_QUEUED_FDS,
            },
        );
        let (client, server) = harness.connect();
        let refused = UnixStream::connect(harness.dir.join("in")).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert!(closed(&refused).is_empty());
        // A header longer than WL_MAX_MESSAGE_SIZE ends the connection.
        let mut oversized = message(5, 0, &[0; 4]);
        oversized[4..8].copy_from_slice(&(8192u32 << 16).to_ne_bytes());
        transmit(client.as_raw_fd(), &oversized, &mut Vec::new()).unwrap();
        assert!(closed(&server).is_empty());
    }
}
