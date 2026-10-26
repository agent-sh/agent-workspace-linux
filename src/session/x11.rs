use crate::workspace::{cleanup_stale_process_pid, first_available_command_name, DaemonOptions};
use anyhow::{bail, Context, Result};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) const CHROME_OZONE_PLATFORM: &str = "x11";

pub(crate) struct X11Session {
    x_server: Option<Child>,
    window_manager: Option<Child>,
    bus: Option<Child>,
    user_services: Vec<super::host_services::Relay>,
}

impl X11Session {
    pub(crate) fn start(options: &mut DaemonOptions) -> Result<Self> {
        let mut session = Self {
            x_server: None,
            window_manager: None,
            bus: None,
            user_services: Vec::new(),
        };
        // The display is picked before the daemon starts, so a concurrent start
        // can take it first. Our Xvfb then exits; move to the next free display
        // with a fresh cookie. The display range bounds the retries.
        loop {
            let mut x_server = spawn_xvfb(options)?;
            match wait_for_display(&mut x_server, &options.display, &options.xauthority_path)? {
                DisplayWait::Ready => {
                    session.x_server = Some(x_server);
                    break;
                }
                DisplayWait::TakenByAnotherServer => {
                    let _ = x_server.wait();
                    options.display = crate::workspace::pick_display()?;
                    crate::workspace::create_xauthority(
                        &options.display,
                        &options.xauthority_path,
                    )?;
                }
            }
        }
        // Apps get a workspace session bus, so single-instance apps open here
        // and not on the user's desktop, with the user's keyring relayed in.
        session.bus = Some(super::private::start_session_bus(
            &options.runtime_dir,
            |command| {
                crate::launch::scrub_host_session_environment(command);
                configure_process_environment(command, &options.display, &options.xauthority_path);
            },
        )?);
        session.user_services = super::private::user_services(&options.runtime_dir);
        session.window_manager = if options.session.backend == crate::session::SessionBackend::I3 {
            Some(super::i3::spawn(options)?)
        } else {
            spawn_window_manager(options)?
        };
        Ok(session)
    }

    pub(crate) fn x_server_pid(&self) -> u32 {
        self.x_server.as_ref().expect("X server is running").id()
    }

    pub(crate) fn window_manager_pid(&self) -> Option<u32> {
        self.window_manager.as_ref().map(Child::id)
    }

    pub(crate) fn shutdown(&mut self) {
        self.user_services.clear();
        for slot in [&mut self.window_manager, &mut self.x_server, &mut self.bus] {
            if let Some(mut child) = slot.take() {
                // Let i3/Xvfb unlink private sockets on normal shutdown. The child
                // remains unreaped until try_wait, so its PID cannot be reused here.
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                let exited = loop {
                    if child.try_wait().ok().flatten().is_some() {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    thread::sleep(Duration::from_millis(20));
                };
                if !exited {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }
}

impl Drop for X11Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn configure_runtime_environment(command: &mut Command, runtime_dir: &Path) {
    command.env("XDG_RUNTIME_DIR", runtime_dir);
}

pub(crate) fn configure_process_environment(
    command: &mut Command,
    display: &str,
    xauthority: &Path,
) {
    if let Some(runtime_dir) = xauthority
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        configure_runtime_environment(command, runtime_dir);
        if runtime_dir.join("session-bus").exists() {
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                super::private::bus_address(runtime_dir),
            );
        }
    } else {
        command.env_remove("XDG_RUNTIME_DIR");
    }
    command
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauthority)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("I3SOCK")
        .env_remove("SWAYSOCK")
        .env("XDG_SESSION_TYPE", "x11")
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("SDL_VIDEODRIVER", "x11")
        .env("CLUTTER_BACKEND", "x11")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "x11");
}

pub(crate) fn cleanup_stale_processes(
    window_manager_pid: Option<u32>,
    x_server_pid: Option<u32>,
    dry_run: bool,
    actions: &mut Vec<String>,
) {
    if let Some(pid) = window_manager_pid {
        cleanup_stale_process_pid(pid, "window manager", &["openbox"], dry_run, actions);
    }
    if let Some(pid) = x_server_pid {
        cleanup_stale_process_pid(pid, "X server", &["Xvfb"], dry_run, actions);
    }
}

fn spawn_xvfb(options: &DaemonOptions) -> Result<Child> {
    let mut command = Command::new("Xvfb");
    command
        .arg(&options.display)
        .args(["-screen", "0"])
        .arg(format!("{}x{}x24", options.width, options.height))
        .args(["-nolisten", "tcp"])
        .arg("-auth")
        .arg(&options.xauthority_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Die with the daemon, like the native compositors, so a killed or crashed
    // daemon never leaves the display held.
    super::private::prepare_child(&mut command);
    command.spawn().context("failed to start Xvfb")
}

fn spawn_window_manager(options: &DaemonOptions) -> Result<Option<Child>> {
    let Some(command) = first_available_command_name(&["openbox", "i3", "fluxbox"]) else {
        bail!("missing window manager: install openbox, i3, or fluxbox");
    };
    let mut child = Command::new(command);
    child
        .env("DISPLAY", &options.display)
        .env("XAUTHORITY", &options.xauthority_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::private::prepare_child(&mut child);
    let child = child
        .spawn()
        .with_context(|| format!("failed to start {command}"))?;
    Ok(Some(child))
}

enum DisplayWait {
    Ready,
    TakenByAnotherServer,
}

fn wait_for_display(x_server: &mut Child, display: &str, xauthority: &Path) -> Result<DisplayWait> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(status) = x_server.try_wait()? {
            if display_held_by_other_server(display, x_server.id()) {
                return Ok(DisplayWait::TakenByAnotherServer);
            }
            bail!("Xvfb exited on display {display} with {status}");
        }
        // Xvfb may keep retrying a contested lock instead of exiting.
        if display_held_by_other_server(display, x_server.id()) {
            let _ = x_server.kill();
            return Ok(DisplayWait::TakenByAnotherServer);
        }
        let ok = Command::new("xdpyinfo")
            .arg("-display")
            .arg(display)
            .env("XAUTHORITY", xauthority)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        // Another user's server may answer on this number before ours has
        // failed; only our own lock proves the display is ours.
        if ok && display_lock_pid(display) == Some(x_server.id()) {
            return Ok(DisplayWait::Ready);
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for X display {display}")
}

/// True when the display's X lock names a live server other than ours.
fn display_held_by_other_server(display: &str, ours: u32) -> bool {
    display_lock_pid(display)
        .is_some_and(|pid| pid != ours && Path::new(&format!("/proc/{pid}")).exists())
}

fn display_lock_pid(display: &str) -> Option<u32> {
    let number = display.strip_prefix(':')?.parse::<u32>().ok()?;
    std::fs::read_to_string(format!("/tmp/.X{number}-lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn environment(command: &Command) -> BTreeMap<String, Option<String>> {
        command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn x11_environment_uses_workspace_runtime_instead_of_host_runtime() {
        let mut command = Command::new("true");
        command.env("XDG_RUNTIME_DIR", "/run/user/1000");

        configure_process_environment(&mut command, ":90", Path::new("/tmp/workspace/Xauthority"));

        let envs = environment(&command);
        assert_eq!(
            envs.get("XDG_RUNTIME_DIR")
                .and_then(|value| value.as_deref()),
            Some("/tmp/workspace")
        );
        assert_eq!(
            envs.get("DISPLAY").and_then(|value| value.as_deref()),
            Some(":90")
        );
        assert!(matches!(envs.get("WAYLAND_DISPLAY"), Some(None)));
    }

    #[test]
    fn x11_environment_does_not_leak_runtime_without_workspace_parent() {
        let mut command = Command::new("true");
        command.env("XDG_RUNTIME_DIR", "/run/user/1000");

        configure_process_environment(&mut command, ":90", Path::new("Xauthority"));

        assert!(matches!(
            environment(&command).get("XDG_RUNTIME_DIR"),
            Some(None)
        ));
    }

    #[test]
    fn drop_reaps_session_processes_after_startup_failure() {
        let x_server = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn test X server process");
        let x_server_pid = x_server.id();
        let session = X11Session {
            x_server: Some(x_server),
            window_manager: None,
            bus: None,
            user_services: Vec::new(),
        };

        drop(session);

        assert!(!Path::new(&format!("/proc/{x_server_pid}")).exists());
    }

    #[test]
    fn shutdown_reaps_session_processes_and_drop_is_idempotent() {
        let x_server = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn test X server process");
        let window_manager = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn test window manager process");
        let x_server_pid = x_server.id();
        let window_manager_pid = window_manager.id();
        let mut session = X11Session {
            x_server: Some(x_server),
            window_manager: Some(window_manager),
            bus: None,
            user_services: Vec::new(),
        };

        session.shutdown();
        drop(session);

        assert!(!Path::new(&format!("/proc/{x_server_pid}")).exists());
        assert!(!Path::new(&format!("/proc/{window_manager_pid}")).exists());
    }
}
