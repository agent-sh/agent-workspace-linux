use crate::workspace::{EnvVar, WorkspaceStatus};
use std::{
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
/// process into apps started inside the workspace. These point at the
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

/// The attach environment for trusted external controllers (the agent host,
/// computer-use-linux). It keeps the compositor-facing session bus; only apps
/// launched into the workspace are switched to the client bus.
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

/// The environment of an app launched into the workspace: its display, buses
/// and runtime directory, then the launch's own variables.
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
