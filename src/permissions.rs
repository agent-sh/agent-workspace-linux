use crate::launch::resolve_command_path;
use crate::profile::WorkspaceProfile;
use crate::workspace::{LaunchSpec, WorkspaceStartOptions};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// What the user accepts from agents through this MCP server. The app
/// allowlist is the only dimension: the workspace is not a sandbox, and its
/// apps run as the user with the user's files and network.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct McpPermissionCeiling {
    #[serde(default)]
    pub apps: AppPermissionCeiling,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AppPermissionCeiling {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpPermissionState {
    pub configured: bool,
    pub restricted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    #[serde(default)]
    pub ceiling: McpPermissionCeiling,
    pub message: String,
    /// Non-blocking advisories about allowlist entries whose enforcement is
    /// weaker than it appears (shell/interpreter entries, bare command names).
    /// They do not narrow or widen the ceiling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<String>,
}

impl Default for McpPermissionState {
    fn default() -> Self {
        Self::from_ceiling(None, McpPermissionCeiling::default())
    }
}

impl McpPermissionState {
    pub fn from_ceiling(source: Option<PathBuf>, ceiling: McpPermissionCeiling) -> Self {
        let configured = source.is_some();
        let ceiling = if configured {
            ceiling
        } else {
            McpPermissionCeiling::default()
        };
        let restricted = ceiling.is_restricted();
        let advisories = ceiling.allowlist_advisories();
        let message = match (configured, restricted) {
            (false, _) => {
                "No MCP permission ceiling is configured; the host/client session controls workspace permissions, including full-access sessions after hidden-workspace approval."
                    .to_string()
            }
            (true, false) => {
                "MCP permission ceiling file is configured but does not restrict app launches; the host/client session controls them."
                    .to_string()
            }
            (true, true) => {
                "MCP permission ceiling is active; clients may narrow these permissions but cannot broaden them."
                    .to_string()
            }
        };
        Self {
            configured,
            restricted,
            source,
            ceiling,
            message,
            advisories,
        }
    }

    pub fn validate_profile(&self, profile: &WorkspaceProfile) -> Result<()> {
        self.ceiling.validate_profile(profile)
    }

    pub fn validate_start_options(&self, _options: &WorkspaceStartOptions) -> Result<()> {
        Ok(())
    }

    pub fn validate_launch_spec(&self, spec: &LaunchSpec) -> Result<()> {
        self.ceiling
            .validate_command(&spec.command, "workspace launch command")
    }
}

impl McpPermissionCeiling {
    fn is_restricted(&self) -> bool {
        !self.apps.allow.is_empty()
    }

    /// Surface non-blocking caveats about app allowlist entries. The app
    /// allowlist matches only the launched program (argv[0]), never its
    /// arguments, and bare command names are resolved against `$PATH`.
    /// These advisories explain where that enforcement is weaker than it looks.
    fn allowlist_advisories(&self) -> Vec<String> {
        let mut advisories = Vec::new();
        for entry in &self.apps.allow {
            let display = entry.display();
            if let Some(name) = command_basename(entry) {
                if is_shell_or_interpreter(&name) {
                    advisories.push(format!(
                        "App allowlist entry {display} resolves to {name:?}, a shell/interpreter/socket tool. The allowlist matches only the launched program, not its arguments, so allowing it effectively permits launching arbitrary programs (e.g. `{name} -c ...`). This entry is advisory, not a hard limit on what runs."
                    ));
                }
            }
            if is_bare_command_name(entry) {
                advisories.push(format!(
                    "App allowlist entry {display} is a bare command name; matching depends on $PATH at validation time vs. the launched app's $PATH at execution time. Prefer an absolute path to pin which binary is allowed."
                ));
            }
        }
        advisories
    }

    fn validate_config(&self) -> Result<()> {
        for command in &self.apps.allow {
            validate_allowed_command(command)?;
        }
        Ok(())
    }

    fn validate_profile(&self, profile: &WorkspaceProfile) -> Result<()> {
        for setup in &profile.setup_commands {
            self.validate_command(&setup.command, "profile setup command")?;
        }
        for app in &profile.startup_apps {
            self.validate_command(&app.command, "profile startup app")?;
        }
        Ok(())
    }

    fn validate_command(&self, command: &[String], context: &str) -> Result<()> {
        if self.apps.allow.is_empty() {
            return Ok(());
        }
        let Some(program) = command.first().filter(|program| !program.trim().is_empty()) else {
            bail!("{context} is empty and cannot be matched against MCP app allowlist");
        };
        if self
            .apps
            .allow
            .iter()
            .any(|allowed| command_matches_allow_entry(program, allowed))
        {
            return Ok(());
        }
        let allowed = self
            .apps
            .allow
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("{context} program {program:?} is not allowed by the MCP app allowlist [{allowed}]");
    }
}

pub fn load_mcp_permission_state(path: PathBuf) -> Result<McpPermissionState> {
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read MCP permissions from {}", path.display()))?;
    let ceiling: McpPermissionCeiling = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse MCP permissions from {}", path.display()))?;
    ceiling.validate_config()?;
    Ok(McpPermissionState::from_ceiling(Some(path), ceiling))
}

/// A starter ceiling: `open`, with an optional app allowlist.
pub fn template_permission_ceiling(kind: &str, apps: Vec<PathBuf>) -> Result<McpPermissionCeiling> {
    if kind != "open" {
        bail!("unknown permissions template {kind:?}. Expected: open");
    }
    let ceiling = McpPermissionCeiling {
        apps: AppPermissionCeiling { allow: apps },
    };
    ceiling.validate_config()?;
    Ok(ceiling)
}

fn validate_allowed_command(command: &Path) -> Result<()> {
    if command.as_os_str().is_empty() {
        bail!("MCP app allowlist entries cannot be empty");
    }
    if command.is_absolute() {
        validate_absolute_path(command, "MCP app allowlist entry")?;
        return Ok(());
    }
    let mut components = command.components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        return Ok(());
    }
    bail!(
        "MCP app allowlist entry {} must be an absolute path or a single command name",
        command.display()
    );
}

/// Known shell/interpreter/socket basenames whose presence in the app
/// allowlist effectively delegates arbitrary execution, because the allowlist
/// only matches argv[0] and never inspects the arguments handed to them.
const SHELL_OR_INTERPRETER_BASENAMES: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "env", "python", "python3", "ruby", "perl", "php",
    "lua", "tclsh", "node", "socat", "nc", "ncat", "busybox",
];

/// Extract the file name (basename) of an allowlist entry, lowercased for
/// case-insensitive comparison against the known-tool list.
fn command_basename(entry: &Path) -> Option<String> {
    entry
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
}

fn is_shell_or_interpreter(basename: &str) -> bool {
    SHELL_OR_INTERPRETER_BASENAMES.contains(&basename)
}

/// A bare command name has no path separators: not absolute and a single
/// normal path component (e.g. `bash`, not `/bin/bash` or `./bash`).
fn is_bare_command_name(entry: &Path) -> bool {
    if entry.is_absolute() {
        return false;
    }
    let mut components = entry.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn validate_absolute_path(path: &Path, field: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{field} cannot be empty");
    }
    if !path.is_absolute() {
        bail!("{field} {} must be absolute", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{field} {} cannot contain '..'", path.display());
    }
    Ok(())
}

fn command_matches_allow_entry(program: &str, allowed: &Path) -> bool {
    let program_path = Path::new(program);
    if allowed.is_absolute() {
        if program_path.is_absolute() && paths_equivalent(program_path, allowed) {
            return true;
        }
        if let Some(resolved) = resolve_command_path(program) {
            return paths_equivalent(&resolved, allowed);
        }
        return false;
    }
    program == allowed.to_string_lossy()
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::EnvVar;

    fn launch_spec(command: &[&str], _profile: Option<()>) -> LaunchSpec {
        LaunchSpec {
            command: command.iter().map(|part| part.to_string()).collect(),
            name: None,
            profile_id: None,
            cwd: None,
            env: Vec::<EnvVar>::new(),
        }
    }

    fn configured_state(ceiling: McpPermissionCeiling) -> McpPermissionState {
        McpPermissionState::from_ceiling(Some(PathBuf::from("/tmp/mcp-permissions.json")), ceiling)
    }

    #[test]
    fn the_open_template_carries_the_app_allowlist() {
        let open =
            template_permission_ceiling("open", vec![PathBuf::from("/usr/bin/xterm")]).unwrap();
        assert_eq!(open.apps.allow, vec![PathBuf::from("/usr/bin/xterm")]);
        assert!(open.is_restricted());
        assert!(template_permission_ceiling("closed", Vec::new()).is_err());
    }

    #[test]
    fn default_ceiling_allows_unprofiled_launch() {
        let state = McpPermissionState::default();
        state
            .validate_launch_spec(&launch_spec(&["sh", "-c", "true"], None))
            .unwrap();
    }

    #[test]
    fn configured_empty_ceiling_reports_open_and_allows_launch() {
        let ceiling: McpPermissionCeiling = serde_json::from_str("{}").unwrap();
        let state = configured_state(ceiling);

        assert!(state.configured);
        assert!(!state.restricted);
        state
            .validate_launch_spec(&launch_spec(&["bash", "-lc", "true"], None))
            .unwrap();
    }

    #[test]
    fn app_allowlist_limits_launch_programs() {
        let state = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("xterm")],
            },
        });

        state
            .validate_launch_spec(&launch_spec(&["xterm"], None))
            .unwrap();
        assert!(state
            .validate_launch_spec(&launch_spec(&["bash"], None))
            .is_err());
    }

    #[test]
    fn absolute_and_bare_app_allow_entries_keep_distinct_matching_semantics() {
        let absolute = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("/bin/sh")],
            },
        });
        absolute
            .validate_launch_spec(&launch_spec(&["/bin/sh"], None))
            .expect("absolute path should match itself");
        absolute
            .validate_launch_spec(&launch_spec(&["sh"], None))
            .expect("bare command should resolve through PATH for an absolute allow entry");

        let bare = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("sh")],
            },
        });
        bare.validate_launch_spec(&launch_spec(&["sh"], None))
            .expect("bare command should match the same bare allow entry");
        assert!(
            bare.validate_launch_spec(&launch_spec(&["/bin/sh"], None))
                .is_err(),
            "a bare allow entry must not start matching arbitrary absolute spellings"
        );
    }

    #[test]
    fn shell_allowlist_entry_produces_arbitrary_execution_advisory() {
        let state = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("bash")],
            },
        });

        assert!(
            !state.advisories.is_empty(),
            "expected an advisory for a shell allowlist entry"
        );
        assert!(
            state
                .advisories
                .iter()
                .any(|advisory| advisory.contains("arbitrary programs")),
            "advisories did not mention arbitrary execution: {:?}",
            state.advisories
        );
    }

    #[test]
    fn bare_name_allow_entry_produces_recommend_absolute_advisory() {
        // `xterm` is a bare name but not a shell/interpreter, so only the
        // bare-name advisory should fire.
        let state = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("xterm")],
            },
        });

        assert!(
            state
                .advisories
                .iter()
                .any(|advisory| advisory.contains("absolute path")),
            "expected a recommend-absolute advisory: {:?}",
            state.advisories
        );

        // An absolute, non-shell entry produces no advisories at all.
        let absolute = configured_state(McpPermissionCeiling {
            apps: AppPermissionCeiling {
                allow: vec![PathBuf::from("/usr/bin/xterm")],
            },
        });
        assert!(
            absolute.advisories.is_empty(),
            "absolute non-shell entry should have no advisories: {:?}",
            absolute.advisories
        );
    }
}
