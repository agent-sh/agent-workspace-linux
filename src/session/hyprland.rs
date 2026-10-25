//! A private Hyprland instance nested in a private KWin virtual renderer.
//! Aquamarine requires a dma-buf allocator; neither compositor uses a host display.
use super::private::{prepare_child, wait_for, PrivateSession};
use crate::{
    launch::SessionSandboxResources,
    workspace::{command_path_check, DaemonOptions},
};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    time::Duration,
};
pub(crate) const WAYLAND_SOCKET: &str = "workspace-wayland";

pub(crate) fn blockers() -> Vec<String> {
    use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
    let mut blockers: Vec<String> = ["Hyprland", "kwin_wayland", "Xwayland", "xauth", "xdpyinfo", "dbus-daemon", "grim", "wtype", "wl-copy", "wl-paste", "timeout"]
        .into_iter().filter(|name| !command_path_check(name).ok)
        .map(|name| format!("Install {name} for native Hyprland workspaces (Hyprland 0.53, KWin 6.6+ private renderer)."))
        .collect();
    let render_node = fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            if !entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("renderD")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
                || !entry.file_type().is_ok_and(|kind| kind.is_char_device())
            {
                return false;
            }
            let Ok(path) = std::ffi::CString::new(entry.path().as_os_str().as_bytes()) else {
                return false;
            };
            unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
        });
    if !render_node {
        blockers.push("Hyprland's private renderer requires an accessible DRM render node (/dev/dri/renderD*); no host display, card or input device is needed.".into());
    }
    blockers
}

pub(crate) fn configure_process_environment(
    command: &mut Command,
    runtime: &Path,
    display: &str,
    xauthority: &Path,
) {
    super::kwin::configure_process_environment(command, runtime, display, xauthority);
    command
        .env("XDG_CURRENT_DESKTOP", "Hyprland")
        .env("XDG_SESSION_DESKTOP", "Hyprland")
        .env_remove("KDE_FULL_SESSION")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .env_remove("HYPRLAND_CMD")
        .env_remove("HYPRLAND_CONFIG")
        .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent");
}
pub(crate) fn sandbox_resources(
    runtime: &Path,
    xauthority: &Path,
    display: &str,
) -> SessionSandboxResources {
    // KWin's generic resources include only display/auth/private bus/a11y,
    // never the privileged Hyprland IPC or private renderer socket.
    super::kwin::sandbox_resources(runtime, xauthority, display)
}
/// XWayland's compositor-owned wrapper adds authentication before it starts.
/// Hyprland itself does not pass -auth, including for its abstract X socket.
pub(crate) fn xwayland(args: &[String]) -> Result<()> {
    let display = args.first().context("missing private XWayland display")?;
    if !display
        .strip_prefix(':')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
    {
        bail!("invalid XWayland display");
    }
    let runtime = PathBuf::from(
        std::env::var_os("AGENT_WORKSPACE_RUNTIME_DIR").context("missing private runtime")?,
    );
    let executable =
        std::env::var_os("AGENT_WORKSPACE_REAL_XWAYLAND").context("missing XWayland executable")?;
    let authority = runtime.join("Xauthority");
    crate::workspace::create_xauthority(display, &authority)?;
    fs::set_permissions(&authority, fs::Permissions::from_mode(0o600))?;
    fs::write(
        runtime.join("hyprland-client.staging"),
        serde_json::to_vec(&serde_json::json!({"DISPLAY":display}))?,
    )?;
    fs::rename(
        runtime.join("hyprland-client.staging"),
        runtime.join("hyprland-client.json"),
    )?;
    let error = Command::new(executable)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args(args)
        .stderr(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(runtime.join("hyprland-xwayland.log"))?,
        )
        .args(["-nolisten", "tcp", "-auth"])
        .arg(authority)
        .exec();
    Err(error.into())
}
/// Read only the IPC path published after validating its peer against our child.
pub(crate) fn request(runtime: &Path, command: &str) -> Result<String> {
    let path = fs::read_to_string(runtime.join("hyprland-ipc"))?;
    let path = PathBuf::from(path.trim());
    if !path.starts_with(runtime.join("hypr"))
        || path.file_name().and_then(|s| s.to_str()) != Some(".socket.sock")
    {
        bail!("invalid workspace Hyprland control socket");
    }
    request_socket(&path, command)
}
fn connect_socket(path: &Path) -> Result<UnixStream> {
    let directory = fs::File::open(path.parent().context("IPC socket directory missing")?)?;
    let name = path.file_name().context("IPC socket name missing")?;
    // The actual filesystem path may exceed sockaddr_un.sun_path. An owned
    // directory fd gives the same socket an unambiguous short Linux path.
    let short = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    Ok(UnixStream::connect(short)?)
}
fn request_socket(path: &Path, command: &str) -> Result<String> {
    let mut stream = connect_socket(path).context("connect private Hyprland IPC")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(command.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut data = String::new();
    stream.take(4 * 1024 * 1024).read_to_string(&mut data)?;
    Ok(data)
}
fn own_socket(runtime: &Path, pid: u32) -> Result<Option<PathBuf>> {
    let Ok(entries) = fs::read_dir(runtime.join("hypr")) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path().join(".socket.sock");
        if let Ok(stream) = connect_socket(&path) {
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let ok = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut length,
                )
            };
            if ok == 0 && cred.pid == pid as i32 && cred.uid == unsafe { libc::geteuid() } {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

pub(crate) struct HyprlandSession {
    runtime: PathBuf,
    ipc_ready: bool,
    compositor: Option<Child>,
    parent: Option<Child>,
    private: PrivateSession,
}
impl HyprlandSession {
    pub(crate) fn start(options: &mut DaemonOptions) -> Result<Self> {
        let runtime = &options.runtime_dir;
        for dir in [
            "session-home",
            "session-config",
            "session-data",
            "session-cache",
        ] {
            fs::create_dir_all(runtime.join(dir))?;
        }
        let private = PrivateSession::start(
            runtime,
            |command| configure_process_environment(command, runtime, "", &options.xauthority_path),
            "",
        )?;
        let mut session = Self {
            runtime: runtime.to_path_buf(),
            ipc_ready: false,
            compositor: None,
            parent: None,
            private,
        };
        let mut parent = Command::new("kwin_wayland");
        configure_process_environment(&mut parent, runtime, "", &options.xauthority_path);
        parent
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env("HOME", runtime.join("session-home"))
            .env("QT_QUICK_BACKEND", "software")
            .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
            .args([
                "--virtual",
                "--socket",
                "hyprland-parent",
                "--width",
                "1280",
                "--height",
                "960",
                "--output-count",
                "1",
                "--no-lockscreen",
                "--no-global-shortcuts",
                "--no-kactivities",
            ]);
        prepare_child(&mut parent);
        // Runtimes are reused by id; a crashed renderer leaves its socket
        // behind, which must not pass for the new renderer's readiness.
        let parent_socket = runtime.join("hyprland-parent");
        if parent_socket.symlink_metadata().is_ok() {
            fs::remove_file(&parent_socket)?;
        }
        let log = fs::File::create(runtime.join("hyprland-parent.log"))?;
        parent.stdout(log.try_clone()?).stderr(log);
        session.parent = Some(
            parent
                .spawn()
                .context("start Hyprland private KWin renderer")?,
        );
        wait_for(
            Duration::from_secs(15),
            || {
                if let Some(exit) = session.parent.as_mut().unwrap().try_wait()? {
                    bail!("private Hyprland renderer exited {exit}");
                }
                Ok(connect_socket(&parent_socket).is_ok())
            },
            "Hyprland private renderer",
        )?;
        let config = runtime.join("session-config/hyprland.conf");
        // No source, exec, bind, portal or systemd activation. Window operations
        // are explicit IPC calls and apps are launched only by workspace policy.
        fs::write(&config, include_str!("../../assets/hyprland.conf"))?;
        let socket_path = runtime.join(WAYLAND_SOCKET);
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        let fd = listener.as_raw_fd();
        let runtime_directory = fs::File::open(runtime)?;
        let runtime_fd = runtime_directory.as_raw_fd();
        let wrappers = runtime.join("session-bin");
        fs::create_dir_all(&wrappers)?;
        let wrapper = wrappers.join("Xwayland");
        if wrapper.symlink_metadata().is_ok() {
            fs::remove_file(&wrapper)?;
        }
        // Runtime filesystems may be noexec. Execute the installed binary via
        // a symlink, with argv[0] selecting the internal XWayland launcher.
        std::os::unix::fs::symlink(
            fs::canonicalize(crate::workspace::daemon_executable_path()?)?,
            &wrapper,
        )?;
        let real_xwayland = command_path_check("Xwayland");
        if !real_xwayland.ok {
            bail!("private XWayland executable unavailable");
        }
        let real_xwayland = fs::canonicalize(real_xwayland.detail)?;
        let client_env = runtime.join("hyprland-client.json");
        if client_env.exists() {
            fs::remove_file(&client_env)?;
        }
        let mut compositor = Command::new("Hyprland");
        configure_process_environment(&mut compositor, runtime, "", &options.xauthority_path);
        compositor
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    wrappers.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            // Hyprland appends a long hash-based IPC path. Preserve an owned
            // directory fd across exec instead of exceeding Unix socket limits.
            .env("XDG_RUNTIME_DIR", format!("/proc/self/fd/{runtime_fd}"))
            .env("AGENT_WORKSPACE_XWAYLAND_BACKEND", "hyprland")
            .env("AGENT_WORKSPACE_RUNTIME_DIR", runtime)
            .env("AGENT_WORKSPACE_REAL_XWAYLAND", &real_xwayland)
            .env("XAUTHORITY", &options.xauthority_path)
            .env("HOME", runtime.join("session-home"))
            .env("WAYLAND_DISPLAY", "hyprland-parent")
            .env("AQ_DRM_DEVICES", "/dev/null")
            .env("LIBSEAT_BACKEND", "noop")
            .env("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent")
            .env("HYPRLAND_NO_SD_NOTIFY", "1")
            .env("HYPRLAND_NO_RT", "1")
            .env("HYPRLAND_NO_CRASHREPORTER", "1")
            .arg("--config")
            .arg(config)
            .args(["--socket", WAYLAND_SOCKET, "--wayland-fd"])
            .arg(fd.to_string());
        prepare_child(&mut compositor);
        unsafe {
            compositor.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0
                    || libc::fcntl(runtime_fd, libc::F_SETFD, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let log = fs::File::create(runtime.join("hyprland.log"))?;
        compositor.stdout(log.try_clone()?).stderr(log);
        session.compositor = Some(compositor.spawn().context("start private Hyprland")?);
        drop(listener);
        drop(runtime_directory);
        wait_for(
            Duration::from_secs(15),
            || {
                let child = session.compositor.as_mut().unwrap();
                if let Some(exit) = child.try_wait()? {
                    bail!("Hyprland exited {exit}; see hyprland.log");
                }
                if let Some(path) = own_socket(runtime, child.id())? {
                    fs::write(
                        runtime.join("hyprland-ipc"),
                        path.as_os_str().as_encoded_bytes(),
                    )?;
                    return Ok(true);
                }
                Ok(false)
            },
            "Hyprland private IPC",
        )?;
        session.ipc_ready = true;
        for query in ["j/configerrors", "j/binds"] {
            let value: serde_json::Value = serde_json::from_str(&request(runtime, query)?)?;
            let clean = value.as_array().is_some_and(|items| {
                if query == "j/configerrors" {
                    items.iter().all(|item| {
                        item.as_str()
                            .is_some_and(|message| message.trim().is_empty())
                    })
                } else {
                    items.is_empty()
                }
            });
            if !clean {
                bail!("Hyprland private configuration must have no errors or bindings: {query}: {value}");
            }
        }
        let initial: serde_json::Value = serde_json::from_str(&request(runtime, "j/monitors")?)?;
        let result = request(runtime, "output create headless workspace")?;
        if result.trim() != "ok" {
            bail!("Hyprland output setup: {result}");
        }
        for monitor in initial
            .as_array()
            .context("Hyprland initial outputs missing")?
        {
            let name = monitor["name"]
                .as_str()
                .context("Hyprland output name missing")?;
            if !name.starts_with("WAYLAND-")
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                bail!("unexpected Hyprland parent output {name}");
            }
            let result = request(runtime, &format!("output remove {name}"))?;
            if result.trim() != "ok" {
                bail!("Hyprland parent output removal: {result}");
            }
        }
        // Remove the temporary parent output before positioning the headless
        // output, avoiding overlap and invalid fractional-mode notifications.
        let result = request(
            runtime,
            &format!(
                "keyword monitor workspace,{}x{}@60,0x0,{}",
                options.width, options.height, options.session.scale
            ),
        )?;
        if result.trim() != "ok" {
            bail!("Hyprland output geometry: {result}");
        }
        wait_for(
            Duration::from_secs(5),
            || {
                let monitors: serde_json::Value =
                    serde_json::from_str(&request(runtime, "j/monitors")?)?;
                fs::write(
                    runtime.join("output-config.json"),
                    serde_json::to_vec_pretty(&monitors)?,
                )?;
                let list = monitors.as_array().context("Hyprland monitors missing")?;
                if list.len() != 1 {
                    return Ok(false);
                }
                let monitor = &list[0];
                Ok(monitor["name"] == "workspace"
                    && monitor["width"].as_u64() == Some(options.width as u64)
                    && monitor["height"].as_u64() == Some(options.height as u64)
                    && monitor["scale"]
                        .as_f64()
                        .is_some_and(|s| (s - options.session.scale).abs() <= 0.002))
            },
            "Hyprland requested output geometry (see output-config.json)",
        )?;
        crate::desktop::hyprland::prepare_input(runtime)?;
        wait_for(
            Duration::from_secs(10),
            || Ok(client_env.exists()),
            "Hyprland XWayland display",
        )?;
        let env: serde_json::Value = serde_json::from_slice(&fs::read(client_env)?)?;
        options.display = env["DISPLAY"]
            .as_str()
            .context("private XWayland display missing")?
            .to_owned();
        let mut check = Command::new("timeout");
        configure_process_environment(
            &mut check,
            runtime,
            &options.display,
            &options.xauthority_path,
        );
        let output = check.args(["5s", "xdpyinfo"]).output()?;
        if !output.status.success() {
            bail!(
                "private authenticated XWayland did not become ready: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(session)
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.compositor.as_ref().map(Child::id)
    }
    pub(crate) fn shutdown(&mut self) {
        // Do not signal while Hyprland is already in its IPC-triggered cleanup.
        // Keep its renderer alive until the child has completed teardown.
        if self.ipc_ready
            && self
                .compositor
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_none())
        {
            let _ = request(&self.runtime, "dispatch exit");
            if let Some(child) = &mut self.compositor {
                let _ = wait_for(
                    Duration::from_secs(5),
                    || Ok(child.try_wait()?.is_some()),
                    "Hyprland graceful exit",
                );
            }
        }
        self.ipc_ready = false;
        for slot in [&mut self.compositor, &mut self.parent] {
            if let Some(mut child) = slot.take() {
                let group = -(child.id() as i32);
                if child.try_wait().ok().flatten().is_none() {
                    unsafe {
                        libc::kill(group, libc::SIGTERM);
                    }
                    let _ = wait_for(
                        Duration::from_secs(5),
                        || Ok(child.try_wait()?.is_some()),
                        "native compositor shutdown",
                    );
                }
                unsafe {
                    libc::kill(group, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.private.shutdown();
        // wl_display_add_socket_fd leaves unlinking to the socket owner.
        let _ = fs::remove_file(self.runtime.join(WAYLAND_SOCKET));
    }
}
impl Drop for HyprlandSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub(crate) fn cleanup_stale_processes(pid: Option<u32>, dry_run: bool, actions: &mut Vec<String>) {
    if let Some(pid) = pid {
        crate::workspace::cleanup_stale_process_pid(
            pid,
            "Hyprland compositor",
            &["Hyprland"],
            dry_run,
            actions,
        );
    }
}
