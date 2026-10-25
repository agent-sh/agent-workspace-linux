//! Private native Mutter display with a restricted GNOME Shell session mode.
use super::private::{bus_address, prepare_child, socket_address, wait_for, PrivateSession};
use crate::workspace::{command_path_check, DaemonOptions};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

pub(crate) const WAYLAND_SOCKET: &str = "workspace-wayland";

pub(crate) fn blockers() -> Vec<String> {
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
            .and_then(|v| {
                v.split_whitespace()
                    .last()
                    .and_then(|v| v.split('.').next())
                    .map(str::to_owned)
            })
            .is_some_and(|v| matches!(v.as_str(), "50" | "51"));
        if !compatible {
            blockers.push("Native GNOME workspaces require GNOME Shell 50 or 51 and matching Mutter libraries.".into());
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

pub(crate) fn sandbox_resources(
    runtime: &Path,
    xauthority: &Path,
    display: &str,
) -> crate::launch::SessionSandboxResources {
    super::kwin::sandbox_resources(runtime, xauthority, display)
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
                r#"<deny send_interface="org.gnome.Shell"/>
<deny send_interface="org.gnome.Shell.Extensions"/>
<deny send_interface="org.gnome.Shell.Screenshot"/>
<deny send_interface="org.freedesktop.Application"/>
<deny send_interface="org.gtk.Application"/>"#,
            )?,
        };
        session.private.start_system_bus(runtime, |command| {
            configure_process_environment(command, runtime, "", &options.xauthority_path)
        })?;
        for (schema, key, value) in [
            ("org.gnome.desktop.interface", "enable-animations", "false"),
            ("org.gnome.desktop.session", "idle-delay", "0"),
            ("org.gnome.desktop.lockdown", "disable-command-line", "true"),
            ("org.gnome.desktop.lockdown", "disable-lock-screen", "true"),
            ("org.gnome.shell", "disable-user-extensions", "true"),
            ("org.gnome.shell", "enabled-extensions", "[]"),
            ("org.gnome.shell", "disabled-extensions", "[]"),
        ] {
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
        let mut last_error = String::new();
        wait_for(
            Duration::from_secs(20),
            || {
                if let Some(status) = session.private.compositor.as_mut().unwrap().try_wait()? {
                    bail!(
                        "GNOME exited with {status}; see {}",
                        runtime.join("gnome.log").display()
                    );
                }
                match crate::desktop::gnome::call(runtime, serde_json::json!({"op":"probe"})) {
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
        crate::desktop::gnome::apply_scale(runtime, options.session.scale)?;
        let probe = crate::desktop::gnome::call(runtime, serde_json::json!({"op":"probe"}))?;
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
            || match crate::desktop::gnome::probe_capture(runtime) {
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
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
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

pub(crate) fn cleanup_stale_processes(pid: Option<u32>, dry_run: bool, actions: &mut Vec<String>) {
    if let Some(pid) = pid {
        crate::workspace::cleanup_stale_process_pid(
            pid,
            "GNOME compositor",
            &["gnome-shell"],
            dry_run,
            actions,
        );
    }
}
