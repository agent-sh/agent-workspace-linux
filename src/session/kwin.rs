//! A private KWin virtual compositor, with its own bus, runtime and settings.
//! No parent X11/Wayland display and no host input device is used.
pub(crate) use super::private::bus_address;
use super::private::{prepare_child, wait_for, PrivateSession};
use crate::{
    launch::SessionSandboxResources,
    workspace::{cleanup_stale_process_pid, command_path_check, DaemonOptions},
};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

pub(crate) const WAYLAND_SOCKET: &str = "workspace-wayland";

pub(crate) fn blockers() -> Vec<String> {
    use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
    let mut blockers: Vec<String> = ["kwin_wayland", "Xwayland", "dbus-daemon", "wl-copy", "wl-paste", "kbuildsycoca6", "kscreen-doctor", "timeout"]
        .into_iter().filter(|name| !command_path_check(name).ok)
        .map(|name| format!("Install {name} for native KWin Wayland workspaces (KWin 6.6 or newer, including kwin-common)."))
        .collect();
    let render_node = fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name();
            if !name
                .to_string_lossy()
                .strip_prefix("renderD")
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
                })
            {
                return false;
            }
            if !entry.file_type().is_ok_and(|kind| kind.is_char_device()) {
                return false;
            }
            let Ok(path) = std::ffi::CString::new(entry.path().as_os_str().as_bytes()) else {
                return false;
            };
            unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
        });
    if !render_node {
        blockers.push("Native KWin capture requires access to a DRM render node (/dev/dri/renderD*); no host display or input devices are needed.".to_owned());
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
        .env("XDG_RUNTIME_DIR", runtime)
        .env("PIPEWIRE_RUNTIME_DIR", runtime)
        .env("PIPEWIRE_REMOTE", "pipewire-0")
        .env("DBUS_SESSION_BUS_ADDRESS", bus_address(runtime))
        .env("WAYLAND_DISPLAY", WAYLAND_SOCKET)
        .env_remove("WAYLAND_SOCKET")
        .env("XDG_SESSION_TYPE", "wayland")
        .env("XDG_CURRENT_DESKTOP", "KDE")
        .env("XDG_SESSION_DESKTOP", "KDE")
        .env("XDG_MENU_PREFIX", "agent-workspace-")
        .env("KDE_FULL_SESSION", "true")
        .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
        .env("NO_AT_BRIDGE", "0")
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("CLUTTER_BACKEND", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .env("XDG_CONFIG_HOME", runtime.join("session-config"))
        .env("XDG_CACHE_HOME", runtime.join("session-cache"))
        .env("XDG_DATA_HOME", runtime.join("session-data"));
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

pub(crate) fn sandbox_resources(
    runtime: &Path,
    xauthority: &Path,
    display: &str,
) -> SessionSandboxResources {
    let mut read_only_binds = Vec::new();
    if xauthority.is_file() {
        read_only_binds.push(xauthority.to_path_buf());
    }
    let mut read_write_binds = vec![runtime.join(WAYLAND_SOCKET), runtime.join("session-bus")];
    if let Some(number) = display
        .strip_prefix(':')
        .and_then(|s| s.split('.').next())
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
    {
        let socket = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        if socket.exists() {
            read_write_binds.push(socket);
        }
    }
    let atspi = runtime.join("at-spi/bus");
    if atspi.exists() {
        read_write_binds.push(atspi);
    }
    SessionSandboxResources {
        read_only_binds,
        read_write_binds,
    }
}

pub(crate) struct KwinSession {
    private: PrivateSession,
    filter: Option<super::wayland_filter::WaylandFilter>,
}
impl KwinSession {
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
        ] {
            fs::create_dir_all(runtime.join(directory))?;
        }
        let menus = runtime.join("session-config/menus");
        fs::create_dir_all(&menus)?;
        fs::write(menus.join("agent-workspace-applications.menu"), "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \"http://www.freedesktop.org/standards/menu-spec/1.0/menu.dtd\">\n<Menu><Name>Applications</Name><DefaultAppDirs/><DefaultDirectoryDirs/><Include><All/></Include></Menu>\n")?;
        // Authorize this executable through KWin's own desktop-entry checks.
        // The entry and its service cache belong only to this private session.
        let applications = runtime.join("session-data/applications");
        fs::create_dir_all(&applications)?;
        let executable = fs::canonicalize(crate::workspace::daemon_executable_path()?)?;
        let executable = executable
            .to_str()
            .context("KWin control executable path must be UTF-8")?;
        if executable.contains(['\n', '\r']) {
            bail!("invalid KWin executable path");
        }
        let executable = executable.replace('\\', "\\\\").replace('"', "\\\"");
        fs::write(applications.join("org.agent_sh.Workspace.desktop"), format!("[Desktop Entry]\nType=Application\nName=Agent Workspace Linux\nNoDisplay=true\nExec=\"{executable}\"\nX-KDE-Wayland-Interfaces=org_kde_kwin_fake_input\nX-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n"))?;
        let mut cache = Command::new("timeout");
        configure_process_environment(&mut cache, runtime, "", &options.xauthority_path);
        let output = cache
            .env("HOME", runtime.join("session-home"))
            .args(["5s", "kbuildsycoca6", "--noincremental"])
            .output()
            .context("build private KWin desktop service cache")?;
        fs::write(runtime.join("desktop-cache.log"), &output.stderr)?;
        if !output.status.success() {
            bail!(
                "could not build private KWin desktop service cache: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut session = Self {
            private: PrivateSession::start(
                runtime,
                |command| {
                    configure_process_environment(command, runtime, "", &options.xauthority_path)
                },
                "",
            )?,
            filter: None,
        };
        let client_env = runtime.join("kwin-client.json");
        if client_env.exists() {
            fs::remove_file(&client_env)?;
        }
        // KWin treats each positional argument as a separate application,
        // rather than an argv vector. Quote one command for KShell::splitArgs.
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let helper = format!(
            "{} __kwin-client-env {}",
            quote(&crate::workspace::daemon_executable_path()?),
            quote(&client_env)
        );
        // KWin's virtual backend accepts logical size and constructs its pixel
        // mode from size * scale. Keep the public workspace bounds in pixels.
        let logical_width = (options.width as f64 / options.session.scale)
            .round()
            .max(1.0) as u32;
        let logical_height = (options.height as f64 / options.session.scale)
            .round()
            .max(1.0) as u32;
        options.width = (logical_width as f64 * options.session.scale).round() as u32;
        options.height = (logical_height as f64 * options.session.scale).round() as u32;
        let mut command = Command::new("kwin_wayland");
        configure_process_environment(&mut command, runtime, "", &options.xauthority_path);
        command
            .env("KWIN_COMPOSE", "O2")
            .env(
                "QT_LOGGING_RULES",
                "kwin_core.debug=true;KWIN_UTILS.debug=true",
            )
            .env("QT_QUICK_BACKEND", "software")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("HOME", runtime.join("session-home"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .args(["--virtual", "--socket", WAYLAND_SOCKET, "--width"])
            .arg(logical_width.to_string())
            .arg("--height")
            .arg(logical_height.to_string())
            .arg("--scale")
            .arg(options.session.scale.to_string())
            .args([
                "--output-count",
                "1",
                "--no-lockscreen",
                "--no-global-shortcuts",
                "--no-kactivities",
                "--xwayland",
            ])
            .arg(&helper);
        prepare_child(&mut command);
        command.stdout(Stdio::from(fs::File::create(runtime.join("kwin.log"))?));
        command.stderr(Stdio::from(
            fs::OpenOptions::new()
                .append(true)
                .open(runtime.join("kwin.log"))?,
        ));
        session.private.compositor = Some(
            command
                .spawn()
                .context("start private KWin virtual compositor")?,
        );
        wait_for(
            Duration::from_secs(15),
            || {
                let pid = session.private.compositor.as_ref().unwrap().id();
                if let super::private::UnreapedExit::Exited(status) =
                    super::private::unreaped_exit(pid)
                {
                    bail!(
                        "KWin exited with {status}; see {}",
                        runtime.join("kwin.log").display()
                    );
                }
                Ok(runtime.join(WAYLAND_SOCKET).exists() && client_env.exists())
            },
            "KWin Wayland compositor",
        )?;
        let env: serde_json::Value = serde_json::from_slice(&fs::read(&client_env)?)?;
        options.display = env["DISPLAY"].as_str().unwrap_or_default().to_string();
        if options.display.is_empty() {
            bail!("KWin did not provide its XWayland display; check the private X11 socket setup in kwin.log");
        }
        if let Some(path) = env["XAUTHORITY"].as_str().filter(|s| !s.is_empty()) {
            options.xauthority_path = path.into();
        }
        // KWin's output configuration store can replace --scale after virtual
        // output creation. Apply and verify scale through the native output API.
        let mut query = Command::new("timeout");
        configure_process_environment(
            &mut query,
            runtime,
            &options.display,
            &options.xauthority_path,
        );
        let output = query.args(["5s", "kscreen-doctor", "--json"]).output()?;
        if !output.status.success() {
            bail!(
                "query KWin output configuration: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let config: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("decode KWin output configuration")?;
        let outputs = config["outputs"]
            .as_array()
            .context("KWin output list missing")?;
        if outputs.len() != 1 {
            bail!("private KWin session must have exactly one output");
        }
        let id = outputs[0]["id"]
            .as_u64()
            .context("KWin output id missing")?;
        let mut apply = Command::new("timeout");
        configure_process_environment(
            &mut apply,
            runtime,
            &options.display,
            &options.xauthority_path,
        );
        let output = apply
            .args(["5s", "kscreen-doctor"])
            .arg(format!("output.{id}.scale.{}", options.session.scale))
            .output()?;
        fs::write(runtime.join("output-config.log"), &output.stderr)?;
        if !output.status.success() {
            bail!(
                "apply KWin output scale: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let verified = query.output()?;
        if !verified.status.success() {
            bail!(
                "verify KWin output scale: {}",
                String::from_utf8_lossy(&verified.stderr)
            );
        }
        fs::write(runtime.join("output-config.json"), &verified.stdout)?;
        let verified: serde_json::Value = serde_json::from_slice(&verified.stdout)?;
        let actual_scale = verified["outputs"][0]["scale"]
            .as_f64()
            .context("KWin output scale missing")?;
        let actual = crate::session::SessionSpec {
            backend: crate::session::SessionBackend::Kwin,
            scale: actual_scale,
        };
        actual.validate()?;
        // KDE output-management sends scale as Wayland 24.8 fixed point.
        if !options.session.matches(actual) {
            bail!(
                "KWin output scale is {actual_scale}, requested {}",
                options.session.scale
            );
        }
        options.session = actual;
        let (width, height) = crate::desktop::kwin::probe(&options.runtime_dir)?;
        options.width = width;
        options.height = height;
        // Sandboxed apps connect through the filter, never the main socket.
        session.filter = Some(super::wayland_filter::WaylandFilter::start(
            runtime,
            &runtime.join(WAYLAND_SOCKET),
        )?);
        Ok(session)
    }
    pub(crate) fn pid(&self) -> Option<u32> {
        self.private.pid()
    }
    pub(crate) fn shutdown(&mut self) {
        self.filter = None;
        self.private.shutdown();
    }
}
pub(crate) fn cleanup_stale_processes(pid: Option<u32>, dry_run: bool, actions: &mut Vec<String>) {
    if let Some(pid) = pid {
        cleanup_stale_process_pid(pid, "KWin compositor", &["kwin_wayland"], dry_run, actions);
    }
}
