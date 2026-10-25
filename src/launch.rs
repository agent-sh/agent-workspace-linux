use crate::policy::{AppliedWorkspacePolicy, NetworkMode};
use crate::workspace::{EnvVar, WorkspaceStatus};
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn merged_env(mut base: Vec<EnvVar>, overrides: Vec<EnvVar>) -> Vec<EnvVar> {
    for override_var in overrides {
        if let Some(existing) = base
            .iter_mut()
            .find(|base_var| base_var.name == override_var.name)
        {
            *existing = override_var;
        } else {
            base.push(override_var);
        }
    }
    base
}

/// Host-session environment variables that must not leak from the launching
/// process into apps started inside the isolated workspace. These point at the
/// host user's live desktop session services (D-Bus, accessibility, input,
/// PulseAudio, SSH/GPG agents, keyring, login session) and would let a workspace
/// app reach back into the host session. We remove them with targeted
/// `env_remove` only; PATH/HOME and the session-specific display handling
/// applied afterward are deliberately left intact.
const HOST_SESSION_ENV_VARS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_STARTER_ADDRESS",
    "DBUS_STARTER_BUS_TYPE",
    "AT_SPI_BUS_ADDRESS",
    "YDOTOOL_SOCKET",
    "XDG_RUNTIME_DIR",
    "PULSE_SERVER",
    "PULSE_COOKIE",
    "PULSE_RUNTIME_PATH",
    "PIPEWIRE_REMOTE",
    "PIPEWIRE_RUNTIME_DIR",
    "LIBEI_SOCKET",
    "EIS_SOCKET",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "DESKTOP_SESSION",
    "XDG_SESSION_ID",
    "GNOME_KEYRING_CONTROL",
    "GPG_AGENT_INFO",
    "IBUS_ADDRESS",
    "IBUS_DAEMON_SOCKET",
    "SESSION_MANAGER",
];

pub(crate) fn scrub_host_session_environment(command: &mut Command) {
    for name in HOST_SESSION_ENV_VARS {
        command.env_remove(name);
    }
}

pub(crate) fn workspace_environment_variables(status: &WorkspaceStatus) -> Vec<EnvVar> {
    let mut variables = vec![
        EnvVar {
            name: "DISPLAY".to_string(),
            value: status.display.clone(),
        },
        EnvVar {
            name: "XAUTHORITY".to_string(),
            value: status.xauthority_path.display().to_string(),
        },
        EnvVar {
            name: "AGENT_WORKSPACE_ID".to_string(),
            value: status.id.clone(),
        },
        EnvVar {
            name: "AGENT_WORKSPACE_SESSION_ID".to_string(),
            value: status.session_id.clone(),
        },
        EnvVar {
            name: "AGENT_WORKSPACE_RUNTIME_DIR".to_string(),
            value: status.runtime_dir.display().to_string(),
        },
        EnvVar {
            name: "AGENT_WORKSPACE_SOCKET".to_string(),
            value: status.socket_path.display().to_string(),
        },
    ];
    let mut command = Command::new("true");
    status.session.configure_process_environment(
        &mut command,
        &status.display,
        &status.xauthority_path,
        &status.runtime_dir,
    );
    configure_application_bus(&mut command, status);
    for (name, value) in command.get_envs() {
        let name = name.to_string_lossy().to_string();
        variables.retain(|entry| entry.name != name);
        if let Some(value) = value {
            variables.push(EnvVar {
                name,
                value: value.to_string_lossy().to_string(),
            });
        }
    }
    variables
}

pub(crate) fn configure_launch_environment(
    command: &mut Command,
    status: &WorkspaceStatus,
    launch_env: &[EnvVar],
) {
    scrub_host_session_environment(command);
    for env_var in launch_env {
        command.env(&env_var.name, &env_var.value);
    }
    scrub_host_session_environment(command);
    for env_var in workspace_environment_variables(status) {
        command.env(env_var.name, env_var.value);
    }
    status.session.configure_process_environment(
        command,
        &status.display,
        &status.xauthority_path,
        &status.runtime_dir,
    );
    configure_application_bus(command, status);
}

fn configure_application_bus(command: &mut Command, status: &WorkspaceStatus) {
    if status.session.backend.requires_client_application_bus() {
        let bus = status.runtime_dir.join("client-bus");
        command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            crate::session::private::socket_address(&bus),
        );
    }
}

pub(crate) struct SessionSandboxResources {
    pub(crate) read_only_binds: Vec<PathBuf>,
    pub(crate) read_write_binds: Vec<PathBuf>,
}

pub(crate) struct BubblewrapSandbox {
    pub(crate) args: Vec<String>,
    pub(crate) mount_isolation: String,
    pub(crate) network_isolation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaunchNetworkPlan {
    Host,
    BubblewrapUnshareNet,
    BubblewrapLoopbackOnly,
}

impl LaunchNetworkPlan {
    fn uses_bubblewrap(self) -> bool {
        matches!(
            self,
            Self::BubblewrapUnshareNet | Self::BubblewrapLoopbackOnly
        )
    }

    fn unshare_net(self) -> bool {
        matches!(
            self,
            Self::BubblewrapUnshareNet | Self::BubblewrapLoopbackOnly
        )
    }

    fn isolation_label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::BubblewrapUnshareNet => "bubblewrap_unshare_net",
            Self::BubblewrapLoopbackOnly => "bubblewrap_loopback_only",
        }
    }
}

pub(crate) fn bubblewrap_sandbox_for_launch(
    status: &WorkspaceStatus,
    policy: Option<&AppliedWorkspacePolicy>,
    cwd: Option<&Path>,
) -> Result<Option<BubblewrapSandbox>> {
    let network = launch_network_plan(policy);
    let mounts = uses_bubblewrap_mount_isolation(policy);
    if !network.uses_bubblewrap() && !mounts {
        return Ok(None);
    }

    if mounts {
        Ok(Some(BubblewrapSandbox {
            args: restricted_mount_namespace_args(status, policy, cwd, network)?,
            mount_isolation: "bubblewrap_mount_namespace".to_string(),
            network_isolation: network.isolation_label().to_string(),
        }))
    } else {
        let mut args = vec![
            "--bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--unshare-pid".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
        ];
        // Host desktop/system IPC is not part of ordinary host file access.
        // Rebind only this workspace's display endpoints below.
        for path in [
            PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })),
            PathBuf::from("/run/dbus"),
            PathBuf::from("/tmp/.X11-unix"),
        ] {
            if path.is_dir() {
                args.extend(["--tmpfs".to_string(), path.display().to_string()]);
            }
        }
        // Keep desktop control/configuration private even for network-only
        // profiles whose ordinary host files remain visible. Fresh procfs in the
        // app PID namespace also prevents /proc/<host-pid>/root/fd bypasses.
        // Bubblewrap does not traverse symlink parents for mount targets. Mask
        // the real directory so short runtime aliases see the same private view.
        let runtime_target =
            fs::canonicalize(&status.runtime_dir).unwrap_or_else(|_| status.runtime_dir.clone());
        let target_path = |path: &Path| {
            path.strip_prefix(&status.runtime_dir)
                .map(|relative| runtime_target.join(relative))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        args.extend(["--tmpfs".to_string(), runtime_target.display().to_string()]);
        let resources = app_session_resources(status)?;
        let mut dirs = BTreeSet::new();
        for path in resources
            .read_only_binds
            .iter()
            .chain(&resources.read_write_binds)
        {
            add_parent_dirs(&mut dirs, path);
        }
        for dir in dirs {
            if dir.starts_with(&status.runtime_dir) && dir != status.runtime_dir {
                args.extend(["--dir".to_string(), target_path(&dir).display().to_string()]);
            }
        }
        for (flag, paths) in [
            ("--ro-bind", resources.read_only_binds),
            ("--bind", resources.read_write_binds),
        ] {
            for path in paths {
                args.extend([
                    flag.to_string(),
                    path.display().to_string(),
                    target_path(&path).display().to_string(),
                ]);
            }
        }
        if network.unshare_net() {
            args.push("--unshare-net".to_string());
        }
        if let Some(cwd) = cwd {
            args.push("--chdir".to_string());
            args.push(cwd.display().to_string());
        }
        Ok(Some(BubblewrapSandbox {
            args,
            mount_isolation: "host".to_string(),
            network_isolation: network.isolation_label().to_string(),
        }))
    }
}

pub(crate) fn launch_isolation_labels_from_sandbox(
    sandbox: Option<&BubblewrapSandbox>,
) -> (String, String) {
    sandbox
        .map(|sandbox| {
            (
                sandbox.mount_isolation.clone(),
                sandbox.network_isolation.clone(),
            )
        })
        .unwrap_or_else(|| ("host".to_string(), "host".to_string()))
}

pub(crate) fn launch_isolation_labels_for_policy(
    policy: Option<&AppliedWorkspacePolicy>,
) -> (String, String) {
    let mount_isolation = if uses_bubblewrap_mount_isolation(policy) {
        "bubblewrap_mount_namespace"
    } else {
        "host"
    };
    (
        mount_isolation.to_string(),
        launch_network_plan(policy).isolation_label().to_string(),
    )
}

pub(crate) fn launch_network_plan(policy: Option<&AppliedWorkspacePolicy>) -> LaunchNetworkPlan {
    if policy.is_some_and(|policy| {
        matches!(policy.network.mode, NetworkMode::Disabled)
            && policy.enforcement.network.enforced
            && policy.runtime_capabilities.bubblewrap.ok
    }) {
        LaunchNetworkPlan::BubblewrapUnshareNet
    } else if policy.is_some_and(|policy| {
        matches!(policy.network.mode, NetworkMode::LocalOnly)
            && policy.enforcement.network.enforced
            && policy.runtime_capabilities.bubblewrap.ok
    }) {
        LaunchNetworkPlan::BubblewrapLoopbackOnly
    } else {
        LaunchNetworkPlan::Host
    }
}

pub(crate) fn uses_bubblewrap_mount_isolation(policy: Option<&AppliedWorkspacePolicy>) -> bool {
    policy.is_some_and(|policy| {
        !policy.mounts.is_empty()
            && policy.enforcement.mounts.enforced
            && policy.runtime_capabilities.bubblewrap.ok
    })
}

fn app_session_resources(status: &WorkspaceStatus) -> Result<SessionSandboxResources> {
    let mut resources = status.session.sandbox_resources(
        &status.xauthority_path,
        &status.runtime_dir,
        &status.display,
    );
    let control_bus = status.runtime_dir.join("session-bus");
    let client_bus = status.runtime_dir.join("client-bus");
    if status.session.backend.requires_client_application_bus() && !client_bus.exists() {
        bail!(
            "{:?} client bus is unavailable at {}; refusing to launch a sandboxed app",
            status.session.backend,
            client_bus.display()
        );
    }
    for path in resources
        .read_only_binds
        .iter_mut()
        .chain(&mut resources.read_write_binds)
    {
        if *path == control_bus {
            *path = client_bus.clone();
        }
    }
    let client = status.runtime_dir.join("client");
    if client.is_dir() {
        resources.read_write_binds.push(client);
    }
    Ok(resources)
}

pub(crate) fn restricted_mount_namespace_args(
    status: &WorkspaceStatus,
    policy: Option<&AppliedWorkspacePolicy>,
    cwd: Option<&Path>,
    network: LaunchNetworkPlan,
) -> Result<Vec<String>> {
    let resources = app_session_resources(status)?;
    restricted_mount_namespace_args_with_resources(policy, cwd, network, resources)
}

fn restricted_mount_namespace_args_with_resources(
    policy: Option<&AppliedWorkspacePolicy>,
    cwd: Option<&Path>,
    network: LaunchNetworkPlan,
    resources: SessionSandboxResources,
) -> Result<Vec<String>> {
    let policy = policy.context("mount namespace requested without an applied policy")?;
    let mut args = Vec::new();
    let mut dirs = BTreeSet::new();
    let mut extra_read_only_binds = BTreeSet::new();
    let mut add_dir = |path: &Path| {
        if path != Path::new("/") {
            dirs.insert(path.to_path_buf());
        }
    };
    add_dir(Path::new("/tmp"));
    for path in resources
        .read_only_binds
        .iter()
        .chain(&resources.read_write_binds)
    {
        add_parent_dirs(&mut dirs, path);
    }
    for mount in &policy.mounts {
        if !mount.workspace_path.is_absolute() {
            bail!(
                "profile mount workspace_path {} must be absolute for bubblewrap enforcement",
                mount.workspace_path.display()
            );
        }
        if !mount.host_path.exists() {
            bail!(
                "profile mount host_path {} does not exist",
                mount.host_path.display()
            );
        }
        add_parent_dirs(&mut dirs, &mount.workspace_path);
    }
    if let Some(parent) =
        external_symlink_target_parent(Path::new("/etc/resolv.conf"), Path::new("/etc"))
    {
        add_parent_dirs(&mut dirs, &parent);
        extra_read_only_binds.insert(parent);
    }

    for path in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/opt"] {
        if Path::new(path).exists() {
            args.push("--ro-bind".to_string());
            args.push(path.to_string());
            args.push(path.to_string());
        }
    }
    args.push("--unshare-pid".to_string());
    args.push("--proc".to_string());
    args.push("/proc".to_string());
    args.push("--dev".to_string());
    args.push("/dev".to_string());

    for dir in dirs {
        args.push("--dir".to_string());
        args.push(dir.display().to_string());
    }
    for path in extra_read_only_binds {
        args.push("--ro-bind".to_string());
        args.push(path.display().to_string());
        args.push(path.display().to_string());
    }
    for path in resources.read_only_binds {
        args.push("--ro-bind".to_string());
        args.push(path.display().to_string());
        args.push(path.display().to_string());
    }
    for path in resources.read_write_binds {
        args.push("--bind".to_string());
        args.push(path.display().to_string());
        args.push(path.display().to_string());
    }

    for mount in &policy.mounts {
        let canonical_host_path = fs::canonicalize(&mount.host_path).with_context(|| {
            format!(
                "failed to canonicalize profile mount host_path {}",
                mount.host_path.display()
            )
        })?;
        args.push(match mount.mode {
            crate::policy::MountMode::ReadOnly => "--ro-bind".to_string(),
            crate::policy::MountMode::ReadWrite => "--bind".to_string(),
        });
        args.push(canonical_host_path.display().to_string());
        args.push(mount.workspace_path.display().to_string());
    }
    if network.unshare_net() {
        args.push("--unshare-net".to_string());
    }
    args.push("--chdir".to_string());
    args.push(
        cwd.unwrap_or_else(|| Path::new("/tmp"))
            .display()
            .to_string(),
    );
    Ok(args)
}

pub(crate) fn external_symlink_target_parent(path: &Path, covered_root: &Path) -> Option<PathBuf> {
    let _link_target = fs::read_link(path).ok()?;
    let target = fs::canonicalize(path).ok()?;
    let covered_root =
        fs::canonicalize(covered_root).unwrap_or_else(|_| covered_root.to_path_buf());
    if target.starts_with(covered_root) {
        return None;
    }
    target.parent().map(Path::to_path_buf)
}

fn add_parent_dirs(dirs: &mut BTreeSet<PathBuf>, path: &Path) {
    let mut parents = Vec::new();
    let mut current = path.parent();
    while let Some(parent) = current {
        if parent == Path::new("/") {
            break;
        }
        parents.push(parent.to_path_buf());
        current = parent.parent();
    }
    for parent in parents.into_iter().rev() {
        dirs.insert(parent);
    }
}

pub(crate) fn resolve_command_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

pub(crate) fn resolve_executable_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable_file(&path).then_some(path);
    }
    resolve_command_path(program)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_status(
        runtime_dir: &Path,
        backend: crate::session::SessionBackend,
    ) -> WorkspaceStatus {
        WorkspaceStatus {
            id: "native-launch-test".to_string(),
            session: crate::session::SessionSpec {
                backend,
                scale: 1.0,
            },
            session_id: "session-test".to_string(),
            purpose: None,
            profile_id: None,
            applied_policy: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            user_acknowledged_unenforced_policy: true,
            ready: true,
            started_at_unix: 1,
            display: ":90".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: runtime_dir.to_path_buf(),
            socket_path: runtime_dir.join("control.sock"),
            xauthority_path: runtime_dir.join("Xauthority"),
            daemon_pid: None,
            x_server_pid: 0,
            window_manager_pid: None,
            last_event_sequence: 0,
            apps: Vec::new(),
        }
    }

    fn sandbox_policy(
        mounts: Vec<crate::policy::ProfileMount>,
        mode: NetworkMode,
    ) -> AppliedWorkspacePolicy {
        AppliedWorkspacePolicy::new_with_capabilities(
            "native-launch-test".to_string(),
            mounts,
            crate::policy::NetworkPolicy {
                mode,
                allow_hosts: Vec::new(),
            },
            false,
            0,
            crate::policy::PolicyRuntimeCapabilities {
                bubblewrap: crate::policy::PolicyToolCheck {
                    ok: true,
                    detail: "test capability".to_string(),
                },
                ..Default::default()
            },
        )
    }

    fn test_runtime(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "agent-workspace-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn assert_environment_uses_client_bus(backend: crate::session::SessionBackend) {
        let runtime = test_runtime(&format!("missing-client-env-{backend:?}"));
        let status = native_status(&runtime, backend);
        let mut command = Command::new("true");
        command.env("DBUS_SESSION_BUS_ADDRESS", "host-dbus");

        configure_launch_environment(&mut command, &status, &[]);

        let address = command
            .get_envs()
            .find(|(name, _)| *name == "DBUS_SESSION_BUS_ADDRESS")
            .and_then(|(_, value)| value)
            .expect("client bus address");
        assert_eq!(
            address.to_string_lossy(),
            crate::session::private::socket_address(&runtime.join("client-bus"))
        );
    }

    fn assert_sandboxes_reject_missing_client_bus(backend: crate::session::SessionBackend) {
        let runtime = test_runtime(&format!("missing-client-sandbox-{backend:?}"));
        let mount_host = runtime.join("mount-host");
        fs::create_dir_all(&mount_host).expect("runtime and mount host");
        let status = native_status(&runtime, backend);
        let policies = [
            sandbox_policy(Vec::new(), NetworkMode::Disabled),
            sandbox_policy(
                vec![crate::policy::ProfileMount {
                    host_path: mount_host,
                    workspace_path: PathBuf::from("/workspace/data"),
                    mode: crate::policy::MountMode::ReadOnly,
                }],
                NetworkMode::InheritHost,
            ),
        ];

        for policy in &policies {
            let error = match bubblewrap_sandbox_for_launch(&status, Some(policy), None) {
                Err(error) => error,
                Ok(_) => panic!("missing client bus must fail closed"),
            };
            assert!(error.to_string().contains("client bus is unavailable"));
        }

        fs::remove_dir_all(runtime).expect("remove test runtime");
    }

    fn assert_sandboxes_bind_client_bus(backend: crate::session::SessionBackend) {
        let runtime = test_runtime(&format!("working-client-sandbox-{backend:?}"));
        let mount_host = runtime.join("mount-host");
        fs::create_dir_all(&mount_host).expect("runtime and mount host");
        let client_bus = runtime.join("client-bus");
        let listener =
            std::os::unix::net::UnixListener::bind(&client_bus).expect("working client bus socket");
        let status = native_status(&runtime, backend);
        let policies = [
            sandbox_policy(Vec::new(), NetworkMode::Disabled),
            sandbox_policy(
                vec![crate::policy::ProfileMount {
                    host_path: mount_host,
                    workspace_path: PathBuf::from("/workspace/data"),
                    mode: crate::policy::MountMode::ReadOnly,
                }],
                NetworkMode::InheritHost,
            ),
        ];
        // Bind targets use the canonical runtime, which differs from the
        // source path under a symlinked TMPDIR, so compare resolved paths.
        let client = fs::canonicalize(&client_bus).expect("canonical client bus");
        let resolves_to_client =
            |path: &str| fs::canonicalize(path).is_ok_and(|path| path == client);
        let controls = [
            runtime.join("session-bus"),
            fs::canonicalize(&runtime)
                .expect("canonical runtime")
                .join("session-bus"),
        ]
        .map(|path| path.display().to_string());

        for policy in &policies {
            let sandbox = bubblewrap_sandbox_for_launch(&status, Some(policy), None)
                .expect("working client bus sandbox")
                .expect("sandbox");
            assert!(sandbox.args.windows(3).any(|window| {
                window[0] == "--bind"
                    && resolves_to_client(&window[1])
                    && resolves_to_client(&window[2])
            }));
            assert!(!sandbox.args.windows(3).any(|window| {
                matches!(window[0].as_str(), "--bind" | "--ro-bind")
                    && (controls.contains(&window[1]) || controls.contains(&window[2]))
            }));
        }

        drop(listener);
        fs::remove_dir_all(runtime).expect("remove test runtime");
    }

    #[test]
    fn hyprland_crash_then_x11_same_runtime_ignores_stale_client_bus() {
        let runtime = test_runtime("native-crash-x11-same-runtime");
        fs::create_dir_all(&runtime).expect("create reused runtime");
        let client_bus = runtime.join("client-bus");
        let listener =
            std::os::unix::net::UnixListener::bind(&client_bus).expect("native client bus socket");
        let native = native_status(&runtime, crate::session::SessionBackend::Hyprland);
        let mut native_command = Command::new("true");
        configure_launch_environment(&mut native_command, &native, &[]);
        assert!(native_command
            .get_envs()
            .find(|(name, _)| *name == "DBUS_SESSION_BUS_ADDRESS")
            .and_then(|(_, value)| value)
            .is_some());

        drop(listener);
        let x11 = native_status(&runtime, crate::session::SessionBackend::X11);
        let mut command = Command::new("true");
        command.env("DBUS_SESSION_BUS_ADDRESS", "host-dbus");
        configure_launch_environment(&mut command, &x11, &[]);

        let address = command
            .get_envs()
            .find(|(name, _)| *name == "DBUS_SESSION_BUS_ADDRESS")
            .expect("host bus scrubbed");
        assert_eq!(address.1, None);
        let policy = sandbox_policy(Vec::new(), NetworkMode::Disabled);
        let sandbox = bubblewrap_sandbox_for_launch(&x11, Some(&policy), None)
            .expect("X11 sandbox after native crash")
            .expect("network sandbox");
        let client_bus = fs::canonicalize(&runtime)
            .expect("canonical runtime")
            .join("client-bus")
            .display()
            .to_string();
        assert!(!sandbox.args.windows(3).any(|window| {
            matches!(window[0].as_str(), "--bind" | "--ro-bind")
                && (window[1] == client_bus || window[2] == client_bus)
        }));

        fs::remove_dir_all(runtime).expect("remove reused runtime");
    }

    #[test]
    fn kwin_environment_uses_client_bus_even_when_socket_is_missing() {
        assert_environment_uses_client_bus(crate::session::SessionBackend::Kwin);
    }

    #[test]
    fn gnome_environment_uses_client_bus_even_when_socket_is_missing() {
        assert_environment_uses_client_bus(crate::session::SessionBackend::Gnome);
    }

    #[test]
    fn hyprland_environment_uses_client_bus_even_when_socket_is_missing() {
        assert_environment_uses_client_bus(crate::session::SessionBackend::Hyprland);
    }

    #[test]
    fn kwin_sandboxes_reject_missing_client_bus() {
        assert_sandboxes_reject_missing_client_bus(crate::session::SessionBackend::Kwin);
    }

    #[test]
    fn gnome_sandboxes_reject_missing_client_bus() {
        assert_sandboxes_reject_missing_client_bus(crate::session::SessionBackend::Gnome);
    }

    #[test]
    fn hyprland_sandboxes_reject_missing_client_bus() {
        assert_sandboxes_reject_missing_client_bus(crate::session::SessionBackend::Hyprland);
    }

    #[test]
    fn kwin_sandboxes_bind_client_bus_not_control_bus() {
        assert_sandboxes_bind_client_bus(crate::session::SessionBackend::Kwin);
    }

    #[test]
    fn gnome_sandboxes_bind_client_bus_not_control_bus() {
        assert_sandboxes_bind_client_bus(crate::session::SessionBackend::Gnome);
    }

    #[test]
    fn hyprland_sandboxes_bind_client_bus_not_control_bus() {
        assert_sandboxes_bind_client_bus(crate::session::SessionBackend::Hyprland);
    }

    #[test]
    fn merged_env_preserves_non_session_values_and_override_order() {
        let merged = merged_env(
            vec![
                EnvVar {
                    name: "FIRST".to_string(),
                    value: "base".to_string(),
                },
                EnvVar {
                    name: "OVERRIDE".to_string(),
                    value: "base".to_string(),
                },
            ],
            vec![EnvVar {
                name: "OVERRIDE".to_string(),
                value: "launch".to_string(),
            }],
        );

        assert_eq!(merged[0].name, "FIRST");
        assert_eq!(merged[0].value, "base");
        assert_eq!(merged[1].name, "OVERRIDE");
        assert_eq!(merged[1].value, "launch");
    }

    #[test]
    fn host_session_scrub_rejects_accessibility_input_and_runtime_overrides() {
        let mut command = Command::new("true");
        for name in ["AT_SPI_BUS_ADDRESS", "YDOTOOL_SOCKET", "XDG_RUNTIME_DIR"] {
            command.env(name, "host-value");
        }
        scrub_host_session_environment(&mut command);
        for name in ["AT_SPI_BUS_ADDRESS", "YDOTOOL_SOCKET", "XDG_RUNTIME_DIR"] {
            command.env(name, "launch-override");
        }
        scrub_host_session_environment(&mut command);

        let envs = command.get_envs().collect::<Vec<_>>();
        for name in ["AT_SPI_BUS_ADDRESS", "YDOTOOL_SOCKET", "XDG_RUNTIME_DIR"] {
            assert!(envs
                .iter()
                .any(|(key, value)| *key == name && value.is_none()));
        }
    }

    #[test]
    fn session_read_write_resources_bind_only_the_exact_runtime_socket() {
        let policy = AppliedWorkspacePolicy::new_with_capabilities(
            "qa".to_string(),
            Vec::new(),
            crate::policy::NetworkPolicy::default(),
            false,
            0,
            crate::policy::PolicyRuntimeCapabilities::default(),
        );
        let xauthority = PathBuf::from("/tmp/workspace/Xauthority");
        let socket = PathBuf::from("/run/user/1000/agent-workspace/wayland-0");
        let args = restricted_mount_namespace_args_with_resources(
            Some(&policy),
            Some(Path::new("/tmp")),
            LaunchNetworkPlan::Host,
            SessionSandboxResources {
                read_only_binds: vec![xauthority.clone()],
                read_write_binds: vec![socket.clone()],
            },
        )
        .expect("mount namespace args");

        let socket = socket.display().to_string();
        assert!(args
            .windows(3)
            .any(|window| window == ["--bind", socket.as_str(), socket.as_str()]));
        for broad_path in ["/run", "/run/user", "/run/user/1000"] {
            assert!(!args.windows(3).any(|window| {
                matches!(window[0].as_str(), "--bind" | "--ro-bind")
                    && window[1] == broad_path
                    && window[2] == broad_path
            }));
        }
        assert!(args
            .windows(2)
            .any(|window| window == ["--dir", "/run/user/1000/agent-workspace"]));
        let xauthority = xauthority.display().to_string();
        assert!(args
            .windows(3)
            .any(|window| window == ["--ro-bind", xauthority.as_str(), xauthority.as_str()]));
    }

    #[test]
    fn executable_resolution_preserves_permission_and_strict_path_semantics() {
        let path = env::temp_dir().join(format!(
            "agent-workspace-non-executable-{}",
            std::process::id()
        ));
        fs::write(&path, "not executable").expect("write non-executable file");

        assert_eq!(
            resolve_command_path(path.to_str().expect("utf-8 path")),
            Some(path.clone())
        );
        assert_eq!(
            resolve_executable_path(path.to_str().expect("utf-8 path")),
            None
        );
        assert_eq!(
            resolve_executable_path("/bin/sh"),
            Some(PathBuf::from("/bin/sh"))
        );
        assert!(resolve_command_path("sh").is_some());

        fs::remove_file(path).expect("remove non-executable file");
    }
}
