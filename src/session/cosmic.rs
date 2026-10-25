//! Native COSMIC under a private, software-only Weston headless parent.
//! Kiosk mode disables launch shortcuts; its immutable PATH contains only the
//! authenticated XWayland wrapper and xrdb, never desktop launchers or shells.
use super::private::{bus_address, prepare_child, wait_for, PrivateSession};
use crate::{
    launch::SessionSandboxResources,
    workspace::{cleanup_stale_process_pid, command_path_check, DaemonOptions},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
const ENV_FILE: &str = "cosmic-client.json";
/// COSMIC gives clients of this socket a security context, so it withholds
/// its privileged globals from them. Sandboxed applications see the Wayland
/// filter at this path, which relays to this socket and forwards only its
/// allow-listed globals, so a global a cosmic-comp update adds without a
/// security-context gate stays hidden too. The
/// compositor socket named in `ENV_FILE` stays with the daemon's helpers,
/// attached controllers and unsandboxed launches. Those run as the same user
/// with full file access and could open it by path anyway, and desktop
/// services such as portals need capture, as with the control bus.
pub(crate) const APP_SOCKET: &str = "wayland-apps";
/// Globals COSMIC 8cb1802 filters with `client_not_sandboxed` or
/// `client_has_no_security_context`. Startup checks that the security
/// context is in effect by their absence; the filter's allow-list, not this
/// list, is what bounds what a sandboxed app can see.
pub(crate) const PRIVILEGED_GLOBALS: &[&str] = &[
    "zcosmic_toplevel_info_v1",
    "zcosmic_toplevel_manager_v1",
    "zcosmic_workspace_manager_v2",
    "ext_workspace_manager_v1",
    "ext_image_copy_capture_manager_v1",
    "ext_output_image_capture_source_manager_v1",
    "ext_foreign_toplevel_image_capture_source_manager_v1",
    "zcosmic_workspace_image_capture_source_manager_v1",
    "zwlr_output_manager_v1",
    "zwlr_output_power_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwp_input_method_manager_v2",
    "zwlr_layer_shell_v1",
    "ext_session_lock_manager_v1",
    "cosmic_session_lock_layer_manager_v1",
    "zcosmic_keyboard_layout_manager_v1",
    "ext_data_control_manager_v1",
    "zwlr_data_control_manager_v1",
    "cosmic_a11y_manager_v1",
    "wp_security_context_manager_v1",
];
pub(crate) fn blockers() -> Vec<String> {
    ["cosmic-comp",
        "bwrap","weston","Xwayland","xrdb","xauth","dbus-daemon","wl-copy","wl-paste","timeout"].into_iter().filter(|n|!command_path_check(n).ok).map(|n|format!("Install {n} for native COSMIC workspaces (COSMIC 1.0 with EI text input, Weston headless kiosk shell). ")).collect()
}
fn client_environment(runtime: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(runtime.join(ENV_FILE))?)?)
}
pub(crate) fn wayland_socket(runtime: &Path) -> Result<PathBuf> {
    let env = client_environment(runtime)?;
    let socket = env["WAYLAND_DISPLAY"]
        .as_str()
        .context("COSMIC did not publish a Wayland socket")?;
    if socket.contains('/') || !socket.starts_with("wayland-") {
        bail!("invalid private COSMIC socket name");
    }
    Ok(runtime.join(socket))
}
pub(crate) fn configure_process_environment(
    command: &mut Command,
    runtime: &Path,
    display: &str,
    xauthority: &Path,
) {
    crate::launch::scrub_host_session_environment(command);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address(runtime))
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            super::private::socket_address(&runtime.join("system-bus")),
        )
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_CURRENT_DESKTOP", "COSMIC")
        .env("XDG_SESSION_DESKTOP", "COSMIC")
        .env("XDG_CONFIG_HOME", runtime.join("session-config"))
        .env("XDG_DATA_HOME", runtime.join("session-data"))
        .env("XDG_CACHE_HOME", runtime.join("session-cache"))
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("CLUTTER_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
        .env("NO_AT_BRIDGE", "0")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("COSMIC_SESSION_SOCK")
        .env_remove("AGENT_WORKSPACE_COSMIC_GUARD_FD")
        .env_remove("AGENT_WORKSPACE_XWAYLAND_BACKEND")
        .env_remove("NOTIFY_SOCKET");
    if let Ok(socket) = wayland_socket(runtime) {
        command.env("WAYLAND_DISPLAY", socket.file_name().unwrap());
    } else {
        command.env_remove("WAYLAND_DISPLAY");
    }
    if let Ok(address) = fs::read_to_string(runtime.join("atspi-address")) {
        command.env("AT_SPI_BUS_ADDRESS", address.trim());
    }
    if display.is_empty() {
        command.env_remove("DISPLAY").env_remove("XAUTHORITY");
    } else {
        command
            .env("DISPLAY", display)
            .env("XAUTHORITY", xauthority);
    }
}
/// Sandboxed launches use the restricted socket, the only one bound into them.
pub(crate) fn configure_sandboxed_environment(command: &mut Command) {
    command.env("WAYLAND_DISPLAY", APP_SOCKET);
}
pub(crate) fn sandbox_resources(
    runtime: &Path,
    xauthority: &Path,
    display: &str,
) -> SessionSandboxResources {
    let mut read_only_binds = vec![];
    if xauthority.is_file() {
        read_only_binds.push(xauthority.to_owned());
    }
    let mut read_write_binds = vec![runtime.join("session-bus")];
    // Never the compositor socket: sandboxed clients must have a security context.
    let apps = runtime.join(APP_SOCKET);
    if apps.exists() {
        read_write_binds.push(apps);
    }
    if let Some(number) = display
        .strip_prefix(':')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        let path = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        if path.exists() {
            read_write_binds.push(path);
        }
    }
    let a11y = runtime.join("at-spi/bus");
    if a11y.exists() {
        read_write_binds.push(a11y);
    }
    SessionSandboxResources {
        read_only_binds,
        read_write_binds,
    }
}
pub(crate) fn control(runtime: &Path, operation: &str, args: &Value) -> Result<Value> {
    let endpoint = if operation == "input" {
        bus_address(runtime)
    } else {
        wayland_socket(runtime)?.to_string_lossy().into_owned()
    };
    run_helper(runtime, &endpoint, operation, args, |_| {})
}
/// Whether keyboard focus is on an XWayland client.
pub(crate) fn x11_keyboard_focus(runtime: &Path, display: &str, xauthority: &Path) -> Result<bool> {
    if display.is_empty() {
        return Ok(false);
    }
    run_helper(runtime, "xwayland", "x11-focus", &json!({}), |command| {
        command
            .env("DISPLAY", display)
            .env("XAUTHORITY", xauthority);
    })?["x11"]
        .as_bool()
        .context("COSMIC XWayland focus unavailable")
}
fn run_helper(
    runtime: &Path,
    endpoint: &str,
    operation: &str,
    args: &Value,
    prepare: impl FnOnce(&mut Command),
) -> Result<Value> {
    let mut command = Command::new("timeout");
    configure_process_environment(&mut command, runtime, "", &runtime.join("Xauthority"));
    command
        .args(["--kill-after=1s", "8s"])
        .arg(crate::workspace::daemon_executable_path()?)
        .args([
            "__cosmic-control",
            endpoint,
            operation,
            &serde_json::to_string(args)?,
        ]);
    prepare(&mut command);
    let result = command.output().context("run COSMIC control helper")?;
    if !result.status.success() {
        bail!(
            "COSMIC {operation} failed ({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(serde_json::from_slice(&result.stdout)?)
}
pub(crate) fn helper(args: &[String]) -> Result<()> {
    let endpoint = args.first().context("COSMIC endpoint missing")?;
    let operation = args.get(1).context("COSMIC operation missing")?;
    let args: Value = serde_json::from_str(args.get(2).context("COSMIC arguments missing")?)?;
    let value = if operation == "input" {
        crate::desktop::cosmic_input::run(endpoint, &args)?
    } else if operation == "x11-focus" {
        crate::desktop::cosmic::x11_focus_probe()?
    } else {
        crate::desktop::cosmic_protocol::run(Path::new(endpoint), operation, &args)?
    };
    println!("{}", value);
    Ok(())
}
pub(crate) fn session_guard(path: &Path) -> Result<()> {
    let private_path = std::env::var_os("PATH").context("private COSMIC PATH missing")?;
    let private_path = Path::new(&private_path);
    if std::env::split_paths(private_path).count() != 1 {
        bail!("COSMIC PATH must contain only its private launcher directory");
    }
    let probe = private_path.join("cosmic-settings");
    if fs::write(&probe, b"launcher isolation probe").is_ok() {
        let _ = fs::remove_file(&probe);
        bail!("COSMIC launcher directory is writable inside the compositor namespace");
    }
    if Command::new("/bin/sh")
        .args(["-c", "cosmic-settings accessibility-magnifier"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success()
    {
        bail!("COSMIC magnifier settings launcher escaped the private PATH");
    }
    let value = json!({"WAYLAND_DISPLAY":std::env::var("WAYLAND_DISPLAY")?,"DISPLAY":std::env::var("DISPLAY").unwrap_or_default(),"launcher_restricted":true});
    let staging = path.with_extension("staging");
    fs::write(&staging, serde_json::to_vec(&value)?)?;
    fs::rename(staging, path)?;
    // Closing the parent endpoint exits the kiosk child. COSMIC then follows
    // its normal shutdown path and releases its Wayland and XWayland sockets.
    let fd = std::env::var("AGENT_WORKSPACE_COSMIC_GUARD_FD")?.parse::<i32>()?;
    if fd < 3 {
        bail!("invalid COSMIC lifetime descriptor");
    }
    let mut lifetime = unsafe { UnixStream::from_raw_fd(fd) };
    let mut byte = [0u8; 1];
    if lifetime.read(&mut byte)? != 0 {
        bail!("unexpected COSMIC lifetime message");
    }
    Ok(())
}
fn executable(name: &str) -> Result<PathBuf> {
    let check = command_path_check(name);
    if !check.ok {
        bail!("missing COSMIC dependency {name}");
    }
    Ok(fs::canonicalize(check.detail)?)
}
const XWAYLAND_CONFIG_FD: i32 = 198;
fn xwayland_config() -> Result<(PathBuf, Value)> {
    let path = format!("/proc/self/fd/{XWAYLAND_CONFIG_FD}");
    let config: Value = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read COSMIC launcher config from {path}"))?,
    )?;
    let authority = config["authority"]
        .as_str()
        .context("COSMIC Xauthority path missing")?;
    let runtime = Path::new(authority)
        .parent()
        .context("COSMIC runtime directory missing")?
        .to_owned();
    Ok((runtime, config))
}
pub(crate) fn xwayland() -> Result<()> {
    let log = xwayland_config()
        .ok()
        .map(|(runtime, _)| runtime.join("xwayland.log"));
    let result = xwayland_exec();
    if let Err(error) = result {
        if let Some(log) = log {
            use std::io::Write as _;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log) {
                let _ = writeln!(file, "COSMIC XWayland dispatch failed: {error:#}");
            }
        }
        return Err(error);
    }
    Ok(())
}
fn xwayland_exec() -> Result<()> {
    if std::env::var("AGENT_WORKSPACE_XWAYLAND_BACKEND").is_ok_and(|v| v != "cosmic") {
        bail!("Xwayland argv0 dispatch has the wrong backend discriminator");
    }
    let (runtime, config) = xwayland_config()?;
    if config["backend"] != "cosmic" {
        bail!("Xwayland launcher is not configured for COSMIC");
    }
    let authority = config["authority"].as_str().unwrap();
    if std::env::var("XAUTHORITY").is_ok_and(|value| value != authority) {
        bail!("COSMIC Xauthority does not match the launcher configuration");
    }
    let executable = config["executable"]
        .as_str()
        .context("COSMIC XWayland executable missing")?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "-ac" || arg == "-auth") {
        bail!("COSMIC XWayland authorization cannot be overridden");
    }
    // An X server given an unreadable or empty -auth file has no cookie to
    // check. Refuse to start rather than run a display without authorization.
    let metadata = fs::metadata(authority).context("COSMIC Xauthority file is unreadable")?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("COSMIC Xauthority file must be a non-empty private file owned by the workspace");
    }
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runtime.join("xwayland.log"))?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .arg("-auth")
        .arg(authority)
        .stderr(Stdio::from(stderr));
    // The compositor does not pass its environment on to XWayland.
    super::private::use_mesa_egl(&mut command);
    let error = command.exec();
    Err(error).context("exec authenticated COSMIC XWayland")
}
fn set_cookie(authority: &Path, display: &str, cookie: &str) -> Result<()> {
    let result = Command::new("xauth")
        .args(["-f"])
        .arg(authority)
        .args(["add", display, "MIT-MAGIC-COOKIE-1", cookie])
        .output()?;
    if !result.status.success() {
        bail!(
            "initialize COSMIC XWayland authorization: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    fs::set_permissions(authority, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
/// Create the restricted application socket and register it with COSMIC.
/// Returns the write end of the close descriptor; COSMIC stops accepting
/// connections on the socket when it is closed.
fn create_application_socket(runtime: &Path, instance: &str) -> Result<OwnedFd> {
    let path = runtime.join(APP_SOCKET);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (close_read, close_write) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let (listen_fd, close_fd) = (listener.as_raw_fd(), close_read.as_raw_fd());
    run_helper(
        runtime,
        &wayland_socket(runtime)?.to_string_lossy(),
        "security-context",
        &json!({"listen_fd": listen_fd, "close_fd": close_fd, "instance_id": instance}),
        |command| unsafe {
            command.pre_exec(move || {
                for fd in [listen_fd, close_fd] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        },
    )?;
    drop((listener, close_read));
    // Fail closed: apps must never see a socket COSMIC treats as unsandboxed.
    let registry = run_helper(
        runtime,
        &path.to_string_lossy(),
        "registry",
        &json!({}),
        |_| {},
    )?;
    let interfaces = registry["interfaces"]
        .as_array()
        .context("COSMIC application registry missing")?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !interfaces.contains(&"wl_compositor") || !interfaces.contains(&"xdg_wm_base") {
        bail!("COSMIC application socket lacks core globals: {interfaces:?}");
    }
    let exposed = PRIVILEGED_GLOBALS
        .iter()
        .filter(|name| interfaces.contains(name))
        .collect::<Vec<_>>();
    if !exposed.is_empty() {
        bail!("COSMIC application socket exposes privileged globals: {exposed:?}");
    }
    Ok(close_write)
}
/// One append-mode file for both streams, so neither overwrites the other.
fn log_file(path: &Path) -> Result<(Stdio, Stdio)> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.set_len(0)?;
    Ok((Stdio::from(file.try_clone()?), Stdio::from(file)))
}
/// Copy the allowlisted launcher into a separate, read-only mount. A network-only
/// client can write host files, so permissions on the host PATH directory alone
/// are insufficient. Neither chmod nor replacing that directory changes this copy.
fn compositor_namespace(runtime: &Path, wrapper: Option<&Path>) -> Result<Command> {
    let mut command = Command::new(executable("bwrap")?);
    command.args([
        "--bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--die-with-parent",
    ]);
    let destination = runtime.join(if wrapper.is_some() {
        "cosmic-bin"
    } else {
        "parent-bin"
    });
    if wrapper.is_none() {
        // --die-with-parent stops Weston only from inside a PID namespace;
        // without one Weston outlives a crashed daemon. cosmic-comp keeps the
        // host PID view for client credentials and exits with its guard socket.
        command.arg("--unshare-pid");
    }
    command.arg("--tmpfs").arg(&destination);
    if let Some(wrapper) = wrapper {
        let file = fs::File::open(wrapper)?;
        let config = fs::File::open(wrapper.with_file_name("xwayland.json"))?;
        command
            .args(["--perms", "0500", "--ro-bind-data"])
            .arg(file.as_raw_fd().to_string())
            .arg(destination.join("Xwayland"))
            .args(["--perms", "0400", "--ro-bind-data"])
            .arg(config.as_raw_fd().to_string())
            .arg(destination.join("xwayland.json"));
        command
            .arg("--ro-bind")
            .arg(executable("xrdb")?)
            .arg(destination.join("xrdb"));
        unsafe {
            command.pre_exec(move || {
                for fd in [file.as_raw_fd(), config.as_raw_fd()] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::dup2(config.as_raw_fd(), XWAYLAND_CONFIG_FD) < 0
                    || libc::fcntl(XWAYLAND_CONFIG_FD, libc::F_SETFD, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.arg("--remount-ro").arg(&destination).arg("--");
    Ok(command)
}

pub(crate) struct CosmicSession {
    private: PrivateSession,
    parent: Option<Child>,
    lifetime: Option<UnixStream>,
    runtime: PathBuf,
    stopped: bool,
    xwayland_files: Vec<(PathBuf, u64, u64)>,
    xwayland_lock_pid: Option<u32>,
    application_socket: Option<OwnedFd>,
    filter: Option<super::wayland_filter::WaylandFilter>,
}
impl CosmicSession {
    pub(crate) fn start(options: &mut DaemonOptions) -> Result<Self> {
        if options.width == 0 || options.height == 0 {
            bail!("workspace dimensions must be positive");
        }
        let runtime = &options.runtime_dir;
        for name in [
            "session-home",
            "session-config",
            "session-data",
            "session-cache",
            "cosmic-bin",
            "parent-bin",
        ] {
            fs::create_dir_all(runtime.join(name))?;
        }
        let env_file = runtime.join(ENV_FILE);
        if env_file.exists() {
            fs::remove_file(&env_file)?;
        }
        if options.xauthority_path.exists() {
            fs::remove_file(&options.xauthority_path)?;
        }
        let mut random = [0u8; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let cookie = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        set_cookie(&options.xauthority_path, ":0", &cookie)?;
        let bin = runtime.join("cosmic-bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
        // This directory is never a client sandbox resource. Clear only owned files.
        for entry in fs::read_dir(&bin)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                bail!("unexpected directory in private COSMIC PATH");
            }
            fs::remove_file(entry.path())?;
        }
        let xwayland = executable("Xwayland")?;
        std::os::unix::fs::symlink(
            crate::workspace::daemon_executable_path()?,
            bin.join("Xwayland"),
        )?;
        std::os::unix::fs::symlink(executable("xrdb")?, bin.join("xrdb"))?;
        fs::write(
            bin.join("xwayland.json"),
            serde_json::to_vec(&json!({
                "backend": "cosmic",
                "executable": xwayland,
                "authority": options.xauthority_path,
            }))?,
        )?;
        fs::set_permissions(bin.join("xwayland.json"), fs::Permissions::from_mode(0o400))?;
        // The compositor never uses this directory directly: Bubblewrap copies
        // its files into a read-only mount. It stays writable by the owner so
        // stop and stale cleanup can remove the runtime.
        // No cosmic-session, panels, settings service, portal launcher or system bus.
        let mut private = PrivateSession::start(
            runtime,
            |c| configure_process_environment(c, runtime, "", &options.xauthority_path),
            "",
        )?;
        private.start_system_bus(runtime, |c| {
            configure_process_environment(c, runtime, "", &options.xauthority_path)
        })?;
        let mut session = Self {
            private,
            parent: None,
            lifetime: None,
            runtime: runtime.to_owned(),
            stopped: false,
            xwayland_files: vec![],
            xwayland_lock_pid: None,
            application_socket: None,
            filter: None,
        };
        let parent_socket = runtime.join("cosmic-parent");
        if parent_socket.exists() {
            fs::remove_file(&parent_socket)?;
        }
        let mut parent = compositor_namespace(runtime, None)?;
        parent.arg(executable("weston")?);
        configure_process_environment(&mut parent, runtime, "", &options.xauthority_path);
        parent
            .env("HOME", runtime.join("session-home"))
            .env("PATH", runtime.join("parent-bin"))
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env_remove("WAYLAND_DISPLAY")
            .args([
                "--backend=headless-backend.so",
                "--renderer=gl",
                "--shell=kiosk-shell.so",
                "--idle-time=0",
                "--no-config",
                "--socket=cosmic-parent",
            ])
            .arg(format!("--width={}", options.width))
            .arg(format!("--height={}", options.height));
        super::private::use_mesa_egl(&mut parent);
        prepare_child(&mut parent);
        let (stdout, stderr) = log_file(&runtime.join("cosmic-parent.log"))?;
        parent.stdout(stdout).stderr(stderr);
        session.parent = Some(parent.spawn()?);
        wait_for(
            Duration::from_secs(10),
            || {
                // Observe without reaping, so shutdown's group signals cannot
                // reach a recycled process group.
                if let super::private::UnreapedExit::Exited(status) =
                    super::private::unreaped_exit(session.parent.as_ref().unwrap().id())
                {
                    bail!("private COSMIC parent exited: {status}");
                }
                Ok(parent_socket.exists())
            },
            "private COSMIC Wayland parent",
        )?;
        let mut compositor = compositor_namespace(runtime, Some(&bin.join("Xwayland")))?;
        compositor.arg(executable("cosmic-comp")?);
        configure_process_environment(&mut compositor, runtime, "", &options.xauthority_path);
        compositor
            .env("HOME", runtime.join("session-home"))
            .env("PATH", &bin)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("COSMIC_BACKEND", "winit")
            .env("WINIT_UNIX_BACKEND", "wayland")
            .env("WAYLAND_DISPLAY", "cosmic-parent")
            .env("XAUTHORITY", &options.xauthority_path)
            .env("AGENT_WORKSPACE_XWAYLAND_BACKEND", "cosmic")
            .arg(crate::workspace::daemon_executable_path()?)
            .arg("__cosmic-session-guard")
            .arg(&env_file);
        let (lifetime, guard) = UnixStream::pair()?;
        compositor.env(
            "AGENT_WORKSPACE_COSMIC_GUARD_FD",
            guard.as_raw_fd().to_string(),
        );
        unsafe {
            compositor.pre_exec(move || {
                if libc::fcntl(guard.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        session.lifetime = Some(lifetime);
        super::private::use_mesa_egl(&mut compositor);
        prepare_child(&mut compositor);
        let (stdout, stderr) = log_file(&runtime.join("cosmic.log"))?;
        compositor.stdout(stdout).stderr(stderr);
        session.private.compositor = Some(compositor.spawn()?);
        wait_for(
            Duration::from_secs(20),
            || {
                let pid = session.private.compositor.as_ref().unwrap().id();
                if let super::private::UnreapedExit::Exited(status) =
                    super::private::unreaped_exit(pid)
                {
                    bail!("private COSMIC compositor exited: {status}; inspect cosmic.log");
                }
                Ok(env_file.exists())
            },
            "native COSMIC readiness",
        )?;
        options.display = client_environment(runtime)?["DISPLAY"]
            .as_str()
            .context("COSMIC client display missing")?
            .to_owned();
        if options.display.is_empty() {
            bail!("COSMIC did not start authenticated XWayland");
        }
        set_cookie(&options.xauthority_path, &options.display, &cookie)?;
        let number = options
            .display
            .strip_prefix(':')
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .context("invalid COSMIC XWayland display")?;
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        session.xwayland_lock_pid = fs::read_to_string(&lock)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        for path in [PathBuf::from(format!("/tmp/.X11-unix/X{number}")), lock] {
            let metadata = fs::symlink_metadata(&path)?;
            session
                .xwayland_files
                .push((path, metadata.dev(), metadata.ino()));
        }

        control(runtime, "scale", &json!({"scale":options.session.scale}))?;
        let snapshot = control(runtime, "snapshot", &json!({}))?;
        let scale = snapshot["scale"]
            .as_f64()
            .context("COSMIC scale unavailable")?;
        if (scale - options.session.scale).abs() > 0.5 / 256. {
            bail!("COSMIC did not apply requested output scale");
        }
        options.session.scale = scale;
        options.width = snapshot["width"]
            .as_u64()
            .context("COSMIC width unavailable")? as u32;
        options.height = snapshot["height"]
            .as_u64()
            .context("COSMIC height unavailable")? as u32;
        // Probe input before advertising a ready desktop. No synthesized events.
        control(runtime, "input", &json!([]))?;
        session.application_socket = Some(create_application_socket(runtime, &options.session_id)?);
        // cosmic-comp's security context hides the globals it gates; the
        // filter's allow-list also hides any it adds later without a gate.
        session.filter = Some(super::wayland_filter::WaylandFilter::relay(
            runtime,
            &runtime.join(APP_SOCKET),
        )?);
        Ok(session)
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.private.pid()
    }
    pub(crate) fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.filter.take();
        self.application_socket.take();
        let _ = fs::remove_file(self.runtime.join(APP_SOCKET));
        self.lifetime.take();
        let socket = wayland_socket(&self.runtime).ok();
        if let Some(pid) = self
            .private
            .compositor
            .as_ref()
            .map(std::process::Child::id)
        {
            // Leave the leader unreaped so the group signal in
            // `private.shutdown` cannot reach a recycled process group.
            let _ = wait_for(
                Duration::from_secs(3),
                || {
                    if let Some(socket) = socket.as_ref() {
                        let _ = UnixStream::connect(socket);
                    }
                    Ok(!matches!(
                        super::private::unreaped_exit(pid),
                        super::private::UnreapedExit::Running
                    ))
                },
                "COSMIC graceful stop",
            );
        }
        self.private.shutdown();
        if let Some(mut parent) = self.parent.take() {
            // Signal the group: Weston survives the death of its bwrap
            // supervisor despite --die-with-parent. The unreaped leader keeps
            // the group number from being recycled before the SIGKILL.
            let group = -(parent.id() as i32);
            unsafe {
                libc::kill(group, libc::SIGTERM);
            }
            let _ = wait_for(
                Duration::from_secs(3),
                || {
                    Ok(!matches!(
                        super::private::unreaped_exit(parent.id()),
                        super::private::UnreapedExit::Running
                    ))
                },
                "COSMIC parent stop",
            );
            unsafe {
                libc::kill(group, libc::SIGKILL);
            }
            let _ = parent.kill();
            let _ = parent.wait();
        }
        // Smithay owns these paths rather than XWayland itself. A compositor
        // terminated through its namespace supervisor can leave them behind.
        // Never remove a reused display or any socket outside this exact lease.
        let owner_gone = self.xwayland_lock_pid.is_some_and(|pid| {
            let gone = (unsafe { libc::kill(pid as i32, 0) < 0 })
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            let exited = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|s| {
                    s.rsplit_once(") ").is_some_and(|(_, tail)| {
                        matches!(tail.as_bytes().first(), Some(b'Z' | b'X'))
                    })
                });
            gone || exited
        });
        if owner_gone {
            for (path, dev, ino) in self.xwayland_files.drain(..) {
                if fs::symlink_metadata(&path).is_ok_and(|m| m.dev() == dev && m.ino() == ino) {
                    let _ = fs::remove_file(path);
                }
            }
        }
        // bwrap may terminate the nested process before its socket destructors
        // run. These paths belong only to this stopped workspace, never /tmp.
        for path in socket
            .into_iter()
            .chain(std::iter::once(self.runtime.join("cosmic-parent")))
        {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("lock"));
        }
    }
}
impl Drop for CosmicSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub(crate) fn cleanup_stale_processes(pid: Option<u32>, dry_run: bool, actions: &mut Vec<String>) {
    if let Some(pid) = pid {
        cleanup_stale_process_pid(
            pid,
            "COSMIC compositor",
            &["cosmic-comp", "bwrap"],
            dry_run,
            actions,
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn compositor_log_keeps_interleaved_stdout_and_stderr() {
        let dir = std::env::temp_dir().join(format!("awl-cosmic-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cosmic.log");
        std::fs::write(&path, "previous session\n").unwrap();
        let (stdout, stderr) = super::log_file(&path).unwrap();
        let status = std::process::Command::new("sh")
            .args(["-c", "echo out1; echo err1 >&2; echo out2; echo err2 >&2"])
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .unwrap();
        assert!(status.success());
        let log = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(log, "out1\nerr1\nout2\nerr2\n");
    }
}
