//! Private native Mutter display with a restricted GNOME Shell session mode.
use super::private::{bus_address, prepare_child, socket_address, wait_for, PrivateSession};
use crate::workspace::{command_path_check, DaemonOptions};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

pub(crate) const WAYLAND_SOCKET: &str = "workspace-wayland";

/// The workspace session bus. Apps on it do not get capture, remote input,
/// privileged Wayland or Shell control; those stay with the daemon. The daemon itself uses
/// only org.gnome.Mutter.DisplayConfig and org.agent_sh.Workspace.Gnome.
const CONTROL_BUS_POLICY: &str = r#"<deny send_interface="org.gnome.Shell"/>
<deny send_interface="org.gnome.Shell.Extensions"/>
<deny send_interface="org.gnome.Shell.Screenshot"/>
<deny send_interface="org.gnome.Shell.Screencast"/>
<deny send_interface="org.gnome.Shell.Introspect"/>
<deny send_interface="org.gnome.Shell.PortalHelper"/>
<deny send_interface="org.gnome.Shell.PerfHelper"/>
<deny send_interface="org.gnome.Mutter.RemoteDesktop"/>
<deny send_interface="org.gnome.Mutter.RemoteDesktop.Session"/>
<deny send_interface="org.gnome.Mutter.ScreenCast"/>
<deny send_interface="org.gnome.Mutter.ScreenCast.Session"/>
<deny send_interface="org.gnome.Mutter.ScreenCast.Stream"/>
<deny send_interface="org.gnome.Mutter.InputCapture"/>
<deny send_interface="org.gnome.Mutter.InputCapture.Session"/>
<deny send_interface="org.gnome.Mutter.ServiceChannel"/>
<deny send_interface="org.gnome.Mutter.InputMapping"/>
<deny send_interface="org.gnome.Mutter.DebugControl"/>
<deny send_interface="org.gnome.Mutter.Devkit"/>
<deny send_interface="org.gnome.Mutter.X11"/>
<deny send_interface="org.gnome.Mutter.IdleMonitor"/>
<deny send_interface="org.gnome.Mutter.ColorManager"/>
<deny send_interface="org.gnome.Mutter.ColorManager.Calibration"/>
<deny send_interface="org.freedesktop.Application"/>
<deny send_interface="org.gtk.Application"/>"#;

pub(crate) fn blockers() -> Vec<String> {
    blockers_for(None)
}

pub(crate) fn blockers_for(version: Option<super::runtime::Version>) -> Vec<String> {
    let mut blockers: Vec<String> = [
        "gnome-shell",
        "Xwayland",
        "bwrap",
        "dbus-daemon",
        "gsettings",
        "timeout",
    ]
    .into_iter()
    .filter(|name| !command_path_check(name).ok)
    .map(|name| {
        format!("Install {name} for native GNOME workspaces (GNOME Shell/Mutter 50 or 51).")
    })
    .collect();
    if command_path_check("gnome-shell").ok {
        let compatible = Command::new("timeout")
            .args(["5s", "gnome-shell", "--version"])
            .output()
            .ok()
            .filter(|r| r.status.success())
            .and_then(|r| String::from_utf8(r.stdout).ok())
            .and_then(|v| v.split_whitespace().last().map(str::to_owned))
            .is_some_and(|v| match version {
                Some(version) => version.matches(&v),
                None => matches!(v.split('.').next(), Some("50" | "51")),
            });
        if !compatible {
            blockers.push(match version {
                Some(version) => format!("Prepared GNOME runtime does not provide requested Shell {version}."),
                None => "Native GNOME workspaces require GNOME Shell 50 or 51 and matching Mutter libraries.".into(),
            });
        }
    }
    blockers
}

pub(crate) fn configure_process_environment(
    command: &mut Command,
    runtime: &Path,
    display: &str,
    xauthority: &Path,
) {
    crate::launch::scrub_host_session_environment(command);
    command
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("GNOME_SETUP_DISPLAY")
        .env_remove("IBUS_ADDRESS")
        .env_remove("IBUS_DAEMON_SOCKET")
        .env_remove("GDK_SCALE")
        .env_remove("GDK_DPI_SCALE")
        .env_remove("QT_SCALE_FACTOR")
        .env_remove("QT_SCREEN_SCALE_FACTORS")
        .env("GTK_IM_MODULE", "wayland")
        .env("QT_IM_MODULE", "wayland")
        .env("XMODIFIERS", "@im=none")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address(runtime))
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            socket_address(&runtime.join("system-bus")),
        )
        .env("WAYLAND_DISPLAY", WAYLAND_SOCKET)
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_CURRENT_DESKTOP", "GNOME")
        .env("XDG_SESSION_DESKTOP", "agent-workspace")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("CLUTTER_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
        .env("NO_AT_BRIDGE", "0")
        .env("GSETTINGS_BACKEND", "keyfile")
        .env("XDG_CONFIG_HOME", runtime.join("session-config"))
        .env("XDG_CACHE_HOME", runtime.join("session-cache"))
        .env("XDG_DATA_HOME", runtime.join("session-data"))
        .env("XDG_DATA_DIRS", "/usr/local/share:/usr/share");
    if !display.is_empty() {
        command
            .env("DISPLAY", display)
            .env("XAUTHORITY", xauthority);
    }
    if let Ok(address) = fs::read_to_string(runtime.join("atspi-address")) {
        command.env("AT_SPI_BUS_ADDRESS", address.trim());
    }
}

pub(crate) struct GnomeSession {
    private: PrivateSession,
    runtime: std::path::PathBuf,
}
impl GnomeSession {
    pub(crate) fn start(options: &mut DaemonOptions) -> Result<Self> {
        if options.width == 0 || options.height == 0 {
            bail!("workspace dimensions must be positive");
        }
        let runtime = &options.runtime_dir;
        for directory in [
            "session-config",
            "session-cache",
            "session-data",
            "session-home",
            "session-xdg",
            "at-spi",
        ] {
            fs::create_dir_all(runtime.join(directory))?;
        }
        let extension = runtime.join("session-share/gnome-shell/extensions/workspace@agent-sh");
        fs::create_dir_all(&extension)?;
        fs::write(
            extension.join("extension.js"),
            include_str!("../../assets/gnome/extension.js"),
        )?;
        fs::write(
            extension.join("metadata.json"),
            include_str!("../../assets/gnome/metadata.json"),
        )?;
        let modes = runtime.join("session-share/gnome-shell/modes");
        fs::create_dir_all(&modes)?;
        fs::write(
            modes.join("agent-workspace.json"),
            include_str!("../../assets/gnome/agent-workspace.json"),
        )?;
        let mut session = Self {
            runtime: runtime.clone(),
            private: PrivateSession::start(
                runtime,
                |command| {
                    configure_process_environment(command, runtime, "", &options.xauthority_path)
                },
                CONTROL_BUS_POLICY,
            )?,
        };
        session.private.start_system_bus(runtime, |command| {
            configure_process_environment(command, runtime, "", &options.xauthority_path)
        })?;
        let mut settings = vec![
            ("org.gnome.desktop.interface", "enable-animations", "false"),
            ("org.gnome.desktop.session", "idle-delay", "0"),
            ("org.gnome.desktop.lockdown", "disable-command-line", "true"),
            ("org.gnome.desktop.lockdown", "disable-lock-screen", "true"),
            ("org.gnome.shell", "disable-user-extensions", "true"),
            ("org.gnome.shell", "enabled-extensions", "[]"),
            ("org.gnome.shell", "disabled-extensions", "[]"),
        ];
        if options.session.version.is_some() {
            settings.push((
                "org.gnome.mutter",
                "experimental-features",
                "['scale-monitor-framebuffer']",
            ));
        }
        for (schema, key, value) in settings {
            let mut command = Command::new("timeout");
            configure_process_environment(&mut command, runtime, "", &options.xauthority_path);
            let result = command
                .args(["5s", "gsettings", "set", schema, key, value])
                .output()?;
            if !result.status.success() {
                bail!(
                    "configure private GNOME settings: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
        // Shell may otherwise auto-start ibus or desktop helpers outside app
        // launch policy. Allow only XWayland and the sandbox launcher used by
        // GNOME 50's Glycin PNG encoder; omit ibus and desktop launch helpers.
        let binaries = runtime.join("session-bin");
        if binaries.exists() {
            fs::remove_dir_all(&binaries)?;
        }
        fs::create_dir_all(&binaries)?;
        for name in ["Xwayland", "bwrap"] {
            std::os::unix::fs::symlink(command_path_check(name).detail, binaries.join(name))?;
        }
        let mut command = Command::new(command_path_check("gnome-shell").detail);
        configure_process_environment(&mut command, runtime, "", &options.xauthority_path);
        command
            .env("PATH", &binaries)
            .env("HOME", runtime.join("session-home"))
            .env(
                "XDG_DATA_DIRS",
                format!(
                    "{}:/usr/local/share:/usr/share",
                    runtime.join("session-share").display()
                ),
            )
            .env(
                "AGENT_WORKSPACE_CONTROLLER_PID",
                std::process::id().to_string(),
            )
            // No system autostart or Xwayland-session.d hooks: those run
            // helpers such as ibus-x11 by absolute path outside launch policy.
            .env("XDG_CONFIG_DIRS", runtime.join("session-xdg"))
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env_remove("WAYLAND_DISPLAY")
            .args([
                "--headless",
                "--wayland-display",
                WAYLAND_SOCKET,
                "--mode",
                "agent-workspace",
                "--virtual-monitor",
            ])
            .arg(format!("{}x{}", options.width, options.height));
        prepare_child(&mut command);
        command.stdout(Stdio::from(fs::File::create(runtime.join("gnome.log"))?));
        command.stderr(Stdio::from(
            fs::OpenOptions::new()
                .append(true)
                .open(runtime.join("gnome.log"))?,
        ));
        session.private.compositor = Some(
            command
                .spawn()
                .context("start private GNOME headless compositor")?,
        );
        let compositor = session.pid().context("GNOME compositor PID missing")?;
        let mut last_error = String::new();
        wait_for(
            Duration::from_secs(20),
            || {
                let pid = session.private.compositor.as_ref().unwrap().id();
                if let super::private::UnreapedExit::Exited(status) =
                    super::private::unreaped_exit(pid)
                {
                    bail!(
                        "GNOME exited with {status}; see {}",
                        runtime.join("gnome.log").display()
                    );
                }
                match crate::desktop::gnome::call(
                    runtime,
                    compositor,
                    serde_json::json!({"op":"probe"}),
                ) {
                    Ok(probe) => Ok(probe["ready"] == true),
                    Err(error) => {
                        last_error = error.to_string();
                        Ok(false)
                    }
                }
            },
            "GNOME native control extension",
        )
        .with_context(|| last_error)?;
        crate::desktop::gnome::apply_scale(runtime, compositor, options.session.scale)?;
        let probe =
            crate::desktop::gnome::call(runtime, compositor, serde_json::json!({"op":"probe"}))?;
        fs::write(
            runtime.join("output-config.json"),
            serde_json::to_vec_pretty(&probe)?,
        )?;
        let actual = probe["scale"]
            .as_f64()
            .context("GNOME monitor scale missing")?;
        if (actual - options.session.scale).abs() > 0.001 {
            bail!(
                "GNOME monitor scale is {actual}, requested {}",
                options.session.scale
            );
        }
        if probe["monitors"] != 1
            || probe["session_mode"] != "agent-workspace"
            || probe["has_run_dialog"] != false
            || probe["has_overview"] != false
        {
            bail!("GNOME private session mode or output contract is unavailable: {probe}");
        }
        // A mode change can publish its geometry before the first frame is
        // painted. Readiness requires an actual decodable native capture.
        let mut dimensions = None;
        let mut capture_error = String::new();
        wait_for(
            Duration::from_secs(5),
            || match crate::desktop::gnome::probe_capture(runtime, compositor) {
                Ok(size) => {
                    dimensions = Some(size);
                    Ok(true)
                }
                Err(error) => {
                    capture_error = format!("{error:#}");
                    Ok(false)
                }
            },
            "GNOME first native frame",
        )
        .with_context(|| capture_error)?;
        let (width, height) = dimensions.context("GNOME capture dimensions missing")?;
        if probe["width"].as_u64() != Some(u64::from(width))
            || probe["height"].as_u64() != Some(u64::from(height))
        {
            bail!("GNOME screenshot dimensions {width}x{height} do not match the physical monitor: {probe}");
        }
        options.width = width;
        options.height = height;
        options.display = probe["display"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("GNOME did not publish its private XWayland display")?
            .to_string();
        options.xauthority_path = probe["xauthority"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("GNOME did not publish XWayland authority")?
            .into();
        options.session.scale = actual;
        Ok(session)
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.private.pid()
    }
    pub(crate) fn shutdown(&mut self) {
        use std::os::fd::AsRawFd;
        use std::os::unix::{fs::MetadataExt, net::UnixStream};
        let socket = self.runtime.join(WAYLAND_SOCKET);
        let lock = self.runtime.join(format!("{WAYLAND_SOCKET}.lock"));
        let mut owned = Vec::new();
        if let Some(child) = self.private.compositor.as_mut() {
            // Record only this compositor's bound socket. Never remove a
            // replacement socket left by another process after shutdown.
            if let Ok(stream) = UnixStream::connect(&socket) {
                let mut cred = unsafe { std::mem::zeroed::<libc::ucred>() };
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
                if ok == 0 && cred.pid == child.id() as i32 {
                    for path in [&socket, &lock] {
                        if let Ok(m) = path.symlink_metadata() {
                            owned.push((path.clone(), m.dev(), m.ino()));
                        }
                    }
                }
            }
            // GNOME treats loss of mandatory XWayland as fatal. Give Shell
            // SIGTERM first so it can stop its child and unlink sockets itself.
            // Do not reap it here: the unreaped leader keeps its process group
            // ID reserved until PrivateSession signals and reaps the group.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if exited_unreaped(child.id()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.private.shutdown();
        for (path, dev, ino) in owned {
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.dev() == dev && m.ino() == ino)
            {
                let _ = fs::remove_file(path);
            }
        }
    }
}
impl Drop for GnomeSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// True once `pid` (a child of this process) has exited, without reaping it.
fn exited_unreaped(pid: u32) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // ECHILD means it was already reaped elsewhere.
    result != 0 || unsafe { info.si_pid() } != 0
}

fn split_nul(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect()
}

fn single_env<'a>(environ: &'a [u8], key: &str) -> Result<&'a [u8], String> {
    let prefix = format!("{key}=");
    let mut values = split_nul(environ)
        .into_iter()
        .filter_map(|entry| entry.strip_prefix(prefix.as_bytes()));
    match (values.next(), values.next()) {
        (Some(value), None) => Ok(value),
        (None, _) => Err(format!("{key} is not set")),
        _ => Err(format!("{key} is set more than once")),
    }
}

fn has_args(args: &[&[u8]], expected: &[&[u8]]) -> bool {
    args.windows(expected.len())
        .any(|window| window == expected)
}

/// Exact markers of a compositor started by GnomeSession::start: the
/// private session mode, the private Wayland socket, the runtime it was
/// given and the PID of the daemon that started it.
fn compositor_markers(cmdline: &[u8], environ: &[u8]) -> Result<(PathBuf, u32), String> {
    use std::os::unix::ffi::OsStrExt;
    let args = split_nul(cmdline);
    let program = args.first().ok_or("empty command line")?;
    if Path::new(std::ffi::OsStr::from_bytes(program)).file_name()
        != Some(std::ffi::OsStr::new("gnome-shell"))
    {
        return Err("not gnome-shell".into());
    }
    if !has_args(&args, &[b"--mode", b"agent-workspace"])
        || !has_args(&args, &[b"--wayland-display", WAYLAND_SOCKET.as_bytes()])
        || !args.contains(&b"--headless".as_slice())
    {
        return Err("not the private workspace session mode".into());
    }
    let runtime = PathBuf::from(std::ffi::OsStr::from_bytes(single_env(
        environ,
        "XDG_RUNTIME_DIR",
    )?));
    let controller = std::str::from_utf8(single_env(environ, "AGENT_WORKSPACE_CONTROLLER_PID")?)
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or("invalid AGENT_WORKSPACE_CONTROLLER_PID")?;
    if !runtime.is_absolute() {
        return Err("XDG_RUNTIME_DIR is not absolute".into());
    }
    Ok((runtime, controller))
}

/// The runtime's own manifest must name this compositor and its daemon.
fn manifest_records(manifest: &serde_json::Value, pid: u32, controller: u32) -> bool {
    manifest["session"]["backend"] == "gnome"
        && manifest["window_manager_pid"].as_u64() == Some(u64::from(pid))
        && manifest["daemon_pid"].as_u64() == Some(u64::from(controller))
}

/// Whether a process command line is the workspace daemon for `runtime`.
fn is_daemon_for(cmdline: &[u8], runtime: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let args = split_nul(cmdline);
    args.get(1) == Some(&b"daemon".as_slice())
        && has_args(&args, &[b"--runtime-dir", runtime.as_os_str().as_bytes()])
}

fn live_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next())
        })
        .is_some_and(|state| state != 'Z' && state != 'X')
}

/// A stale compositor is one this backend started for a runtime whose
/// manifest records it and whose daemon is gone. A host gnome-shell, another
/// GNOME helper or a live workspace's compositor never matches.
fn stale_compositor(pid: u32) -> Result<(), String> {
    let read = |name: &str| {
        fs::read(format!("/proc/{pid}/{name}")).map_err(|error| format!("read {name}: {error}"))
    };
    let (runtime, controller) = compositor_markers(&read("cmdline")?, &read("environ")?)?;
    let manifest = fs::read(runtime.join("workspace.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| format!("no workspace manifest in {}", runtime.display()))?;
    if !manifest_records(&manifest, pid, controller) {
        return Err(format!(
            "manifest in {} does not record this compositor",
            runtime.display()
        ));
    }
    if live_process(controller)
        && fs::read(format!("/proc/{controller}/cmdline"))
            .is_ok_and(|cmdline| is_daemon_for(&cmdline, &runtime))
    {
        return Err(format!("its workspace daemon pid {controller} is running"));
    }
    Ok(())
}

fn pidfd_signal(fd: &std::os::fd::OwnedFd, signal: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    let error = std::io::Error::last_os_error();
    if result == 0 || error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn pidfd_exited(fd: &std::os::fd::OwnedFd, timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe { libc::poll(&mut poll, 1, millis) > 0 && poll.revents & libc::POLLIN != 0 }
}

pub(crate) fn cleanup_stale_processes(pid: Option<u32>, dry_run: bool, actions: &mut Vec<String>) {
    use std::os::fd::{FromRawFd, OwnedFd};
    let Some(pid) = pid.filter(|pid| *pid > 0 && *pid <= i32::MAX as u32) else {
        return;
    };
    // Hold a pidfd across the identity check so the signal cannot reach a
    // process that reused the PID after the check.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if raw < 0 {
        return;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    if !live_process(pid) {
        actions.push(format!(
            "skipped GNOME compositor pid {pid}: process is already defunct"
        ));
        return;
    }
    if let Err(reason) = stale_compositor(pid) {
        actions.push(format!("skipped GNOME compositor pid {pid}: {reason}"));
        return;
    }
    if dry_run {
        actions.push(format!("would terminate GNOME compositor pid {pid}"));
        return;
    }
    let result = pidfd_signal(&fd, libc::SIGTERM).and_then(|()| {
        if pidfd_exited(&fd, Duration::from_secs(5)) {
            return Ok(());
        }
        pidfd_signal(&fd, libc::SIGKILL)?;
        pidfd_exited(&fd, Duration::from_secs(5));
        Ok(())
    });
    actions.push(match result {
        Ok(()) => format!("terminated GNOME compositor pid {pid}"),
        Err(error) => format!("failed to terminate GNOME compositor pid {pid}: {error}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &[u8] = b"/usr/bin/gnome-shell\0--headless\0--wayland-display\0workspace-wayland\0--mode\0agent-workspace\0--virtual-monitor\x001200x900\0";

    #[test]
    fn compositor_markers_accept_the_private_workspace_compositor() {
        let environ = b"PATH=/x\0XDG_RUNTIME_DIR=/run/w/agent-workspace-linux/a\0AGENT_WORKSPACE_CONTROLLER_PID=42\0";
        assert_eq!(
            compositor_markers(OURS, environ),
            Ok((PathBuf::from("/run/w/agent-workspace-linux/a"), 42))
        );
    }

    #[test]
    fn compositor_markers_reject_host_gnome_processes() {
        let host_env = b"XDG_RUNTIME_DIR=/run/user/1000\0DISPLAY=:0\0";
        for cmdline in [
            b"/usr/bin/gnome-shell\0--mode=ubuntu\0".as_slice(),
            b"/usr/bin/gnome-shell\0".as_slice(),
            b"/usr/libexec/gnome-shell-calendar-server\0".as_slice(),
            b"/usr/bin/gjs\0-m\0/usr/share/gnome-shell/org.gnome.Shell.Notifications\0".as_slice(),
        ] {
            assert!(
                compositor_markers(cmdline, host_env).is_err(),
                "{cmdline:?}"
            );
        }
        // Our argv with the host environment still lacks the workspace markers.
        assert!(compositor_markers(OURS, host_env).is_err());
        // A duplicated marker is ambiguous and rejected.
        assert!(compositor_markers(
            OURS,
            b"XDG_RUNTIME_DIR=/a\0XDG_RUNTIME_DIR=/b\0AGENT_WORKSPACE_CONTROLLER_PID=1\0"
        )
        .is_err());
    }

    #[test]
    fn manifest_must_record_this_compositor_and_daemon() {
        let manifest = serde_json::json!({
            "session": {"backend": "gnome"},
            "window_manager_pid": 7,
            "daemon_pid": 42,
        });
        assert!(manifest_records(&manifest, 7, 42));
        assert!(!manifest_records(&manifest, 8, 42));
        assert!(!manifest_records(&manifest, 7, 43));
        let kwin = serde_json::json!({
            "session": {"backend": "kwin"},
            "window_manager_pid": 7,
            "daemon_pid": 42,
        });
        assert!(!manifest_records(&kwin, 7, 42));
    }

    #[test]
    fn daemon_detection_requires_the_same_runtime() {
        let runtime = Path::new("/run/w/agent-workspace-linux/a");
        let daemon = b"/bin/agent-workspace-linux\0daemon\0--id\0a\0--runtime-dir\0/run/w/agent-workspace-linux/a\0";
        assert!(is_daemon_for(daemon, runtime));
        assert!(!is_daemon_for(
            daemon,
            Path::new("/run/w/agent-workspace-linux/b")
        ));
        assert!(!is_daemon_for(b"/usr/bin/sleep\x00100\0", runtime));
    }
}
