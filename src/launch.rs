use crate::workspace::{EnvVar, WorkspaceStatus};
use std::{
    env,
    ffi::OsString,
    fs,
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

/// Variables of the user's desktop session that would make a workspace
/// process use the user's desktop instead of the workspace: its buses,
/// accessibility, input, sound and login session. Compositors and helpers run
/// without them. Apps then get the workspace's display and buses, and back
/// from the user's session what acts for the user: agents, sound and the
/// XDG directories (`use_the_users_session`). PATH and HOME are left intact.
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

/// The environment of an app launched into the workspace: the workspace's
/// display, buses and runtime directory, the user's own session, then the
/// launch's own variables.
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
    use_the_users_session(command, |name| env::var_os(name));
    for env_var in launch_env {
        if is_user_session_var(&env_var.name) {
            command.env(&env_var.name, &env_var.value);
        }
    }
}

/// What a workspace app keeps of the user's own session. Native compositors
/// keep their settings in private XDG directories; apps read the user's.
const USER_DIRECTORY_ENV_VARS: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
];
const USER_AGENT_ENV_VARS: &[&str] = &["SSH_AUTH_SOCK", "SSH_AGENT_PID", "GPG_AGENT_INFO"];

fn is_user_session_var(name: &str) -> bool {
    USER_DIRECTORY_ENV_VARS.contains(&name)
        || USER_AGENT_ENV_VARS.contains(&name)
        || name == "PULSE_SERVER"
}

/// Apps act for the user: their config and data, SSH and GPG agents and
/// audio. The keyring reaches them through the workspace bus instead.
fn use_the_users_session(command: &mut Command, user: impl Fn(&str) -> Option<OsString>) {
    for name in USER_DIRECTORY_ENV_VARS {
        match user(name) {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    for name in USER_AGENT_ENV_VARS {
        if let Some(value) = user(name) {
            command.env(name, value);
        }
    }
    // The workspace's runtime directory hides the user's sound server socket.
    let pulse = user("PULSE_SERVER").or_else(|| {
        let socket = PathBuf::from(user("XDG_RUNTIME_DIR")?).join("pulse/native");
        socket
            .exists()
            .then(|| OsString::from(format!("unix:{}", socket.display())))
    });
    if let Some(pulse) = pulse {
        command.env("PULSE_SERVER", pulse);
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

    #[test]
    fn apps_get_the_users_directories_agents_and_sound() {
        let runtime = env::temp_dir().join(format!(
            "agent-workspace-user-session-{}",
            std::process::id()
        ));
        fs::create_dir_all(runtime.join("pulse")).expect("runtime dir");
        fs::write(runtime.join("pulse/native"), "").expect("pulse socket stand-in");
        let user = |name: &str| match name {
            "XDG_DATA_HOME" => Some(OsString::from("/home/me/.local/share")),
            "SSH_AUTH_SOCK" => Some(OsString::from("/run/user/1000/ssh-agent.socket")),
            "XDG_RUNTIME_DIR" => Some(runtime.clone().into_os_string()),
            _ => None,
        };
        let mut command = Command::new("true");
        command
            .env("XDG_CONFIG_HOME", "/run/workspace/session-config")
            .env("XDG_DATA_HOME", "/run/workspace/session-data");

        use_the_users_session(&mut command, user);

        let envs = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        // The compositor's private config is gone; the user's default applies.
        assert_eq!(envs["XDG_CONFIG_HOME"], None);
        assert_eq!(
            envs["XDG_DATA_HOME"].as_deref(),
            Some("/home/me/.local/share")
        );
        assert_eq!(
            envs["SSH_AUTH_SOCK"].as_deref(),
            Some("/run/user/1000/ssh-agent.socket")
        );
        assert_eq!(
            envs["PULSE_SERVER"].as_deref(),
            Some(format!("unix:{}/pulse/native", runtime.display()).as_str())
        );
        fs::remove_dir_all(runtime).ok();
    }
}
