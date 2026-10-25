use crate::workspace::{cleanup_stale_process_pid, command_path_check, DaemonOptions};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub(crate) fn socket_path(runtime: &Path) -> PathBuf {
    runtime.join("i3.sock")
}

pub(crate) fn blockers() -> Vec<String> {
    let mut blockers = Vec::new();
    for name in [
        "Xvfb", "xauth", "xdpyinfo", "xprop", "xdotool", "i3", "import",
    ] {
        if !command_path_check(name).ok {
            blockers.push(format!("Install {name} for the private i3 workspace."));
        }
    }
    blockers
}

pub(crate) fn spawn(options: &DaemonOptions) -> Result<Child> {
    let config = options.runtime_dir.join("i3.config");
    // No inherited config, executable bindings, bar, startup apps, or config wizard.
    // Borderless floating windows make requested client geometry exact; new windows
    // retain i3 tiling until the caller explicitly moves or resizes one.
    fs::write(&config, "font pango:monospace 10\nfocus_follows_mouse no\nmouse_warping none\nfocus_on_window_activation focus\ndefault_border none\ndefault_floating_border none\nfloating_minimum_size 1 x 1\nfloating_maximum_size -1 x -1\n")?;
    let mut command = Command::new("i3");
    crate::launch::scrub_host_session_environment(&mut command);
    super::x11::configure_process_environment(
        &mut command,
        &options.display,
        &options.xauthority_path,
    );
    command
        .args(["--no-autostart", "--config"])
        .arg(config)
        .arg("--disable-signalhandler")
        .env("I3SOCK", socket_path(&options.runtime_dir))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-agent-workspace-bus",
        )
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(
            options.runtime_dir.join("i3.log"),
        )?));
    super::private::prepare_child(&mut command);
    let mut child = command.spawn().context("start private i3 window manager")?;
    let ready = super::private::wait_for(
        Duration::from_secs(5),
        || Ok(super::super::desktop::i3::request(&options.runtime_dir, child.id(), 7, "").is_ok()),
        "i3 IPC",
    );
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
            "{error}; see {}",
            options.runtime_dir.join("i3.log").display()
        );
    }
    Ok(child)
}

pub(crate) fn cleanup_stale_processes(
    wm: Option<u32>,
    x: Option<u32>,
    dry_run: bool,
    actions: &mut Vec<String>,
) {
    if let Some(pid) = wm {
        cleanup_stale_process_pid(pid, "i3 window manager", &["i3"], dry_run, actions);
    }
    if let Some(pid) = x {
        cleanup_stale_process_pid(pid, "X server", &["Xvfb"], dry_run, actions);
    }
}
