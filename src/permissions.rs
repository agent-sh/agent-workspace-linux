use crate::launch::resolve_command_path;
use crate::policy::{AppliedWorkspacePolicy, MountMode, NetworkMode, NetworkPolicy, ProfileMount};
use crate::profile::WorkspaceProfile;
use crate::workspace::{LaunchSpec, WorkspaceStartOptions};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::env;
use std::{
    collections::{BTreeSet, VecDeque},
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct McpPermissionCeiling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<ProfileMount>,
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
    /// These do not narrow or widen the ceiling; they surface caveats so the
    /// host/client can decide whether the configured allowlist is sufficient.
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
                "MCP permission ceiling file is configured but does not restrict network, mounts, or app launches; the host/client session controls open dimensions."
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

    pub fn validate_start_options(&self, options: &WorkspaceStartOptions) -> Result<()> {
        if let Some(policy) = &options.applied_policy {
            self.ceiling
                .validate_applied_policy(policy, "workspace start profile")?;
        }
        Ok(())
    }

    pub fn validate_launch_spec(&self, spec: &LaunchSpec) -> Result<()> {
        self.ceiling
            .validate_command(&spec.command, "workspace launch command")?;
        if let Some(policy) = &spec.applied_policy {
            self.ceiling
                .validate_applied_policy(policy, "workspace launch profile")?;
        } else {
            self.ceiling
                .validate_mounts(&[], "workspace launch without profile")?;
            self.ceiling.validate_network(
                &NetworkPolicy::default(),
                "workspace launch without profile",
            )?;
        }
        Ok(())
    }
}

impl McpPermissionCeiling {
    fn is_restricted(&self) -> bool {
        self.network
            .as_ref()
            .is_some_and(|network| !matches!(network.mode, NetworkMode::InheritHost))
            || !self.mounts.is_empty()
            || !self.apps.allow.is_empty()
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
        if let Some(network) = &self.network {
            validate_network_hosts(network, "MCP permission network")?;
        }
        for mount in &self.mounts {
            validate_absolute_path(&mount.host_path, "MCP permission mount host_path")?;
            validate_absolute_path(&mount.workspace_path, "MCP permission mount workspace_path")?;
        }
        for command in &self.apps.allow {
            validate_allowed_command(command)?;
        }
        Ok(())
    }

    fn validate_profile(&self, profile: &WorkspaceProfile) -> Result<()> {
        self.validate_network(&profile.network, "profile network")?;
        self.validate_mounts(&profile.mounts, "profile mounts")?;
        for setup in &profile.setup_commands {
            self.validate_command(&setup.command, "profile setup command")?;
        }
        for app in &profile.startup_apps {
            self.validate_command(&app.command, "profile startup app")?;
        }
        Ok(())
    }

    fn validate_applied_policy(
        &self,
        policy: &AppliedWorkspacePolicy,
        context: &str,
    ) -> Result<()> {
        self.validate_network(&policy.network, context)?;
        self.validate_mounts(&policy.mounts, context)?;
        Ok(())
    }

    fn validate_network(&self, requested: &NetworkPolicy, context: &str) -> Result<()> {
        let Some(ceiling) = &self.network else {
            return Ok(());
        };
        validate_network_hosts(requested, context)?;
        if network_within_ceiling(ceiling, requested) {
            return Ok(());
        }
        bail!(
            "{context} requests network.mode={} with allow_hosts=[{}], which exceeds MCP permission ceiling network.mode={} with allow_hosts=[{}]",
            network_mode_name(&requested.mode),
            requested.allow_hosts.join(", "),
            network_mode_name(&ceiling.mode),
            ceiling.allow_hosts.join(", ")
        );
    }

    fn validate_mounts(&self, requested: &[ProfileMount], context: &str) -> Result<()> {
        if self.mounts.is_empty() {
            return Ok(());
        }
        if requested.is_empty() {
            bail!(
                "{context} does not request a mount policy, but the MCP permission ceiling limits file access"
            );
        }
        for mount in requested {
            validate_absolute_path(&mount.host_path, "requested mount host_path")?;
            validate_absolute_path(&mount.workspace_path, "requested mount workspace_path")?;
            // Resolve the requested host path through symlinks (longest existing
            // prefix) so a mount whose real target escapes the ceiling cannot
            // slip past the lexical subset check.
            let requested_real = effective_host_path(&mount.host_path).with_context(|| {
                format!(
                    "{context} cannot resolve mount host_path {} for ceiling containment check",
                    mount.host_path.display()
                )
            })?;
            let mut matched = false;
            for allowed in &self.mounts {
                let allowed_real = effective_host_path(&allowed.host_path).with_context(|| {
                    format!(
                        "{context} cannot resolve MCP permission ceiling mount host_path {}",
                        allowed.host_path.display()
                    )
                })?;
                if mount_within(allowed, &allowed_real, mount, &requested_real) {
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
            bail!(
                "{context} requests mount {} -> {} ({}) outside the MCP permission ceiling",
                mount.host_path.display(),
                mount.workspace_path.display(),
                mount_mode_name(&mount.mode)
            );
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

pub fn template_permission_ceiling(
    kind: &str,
    allow_hosts: Vec<String>,
    mounts: Vec<ProfileMount>,
    apps: Vec<PathBuf>,
) -> Result<McpPermissionCeiling> {
    let network = match kind {
        "open" => {
            if !allow_hosts.is_empty() {
                bail!("permissions template open does not accept --allow-host");
            }
            None
        }
        "closed" | "disabled" => {
            if !allow_hosts.is_empty() {
                bail!("permissions template closed does not accept --allow-host");
            }
            Some(NetworkPolicy {
                mode: NetworkMode::Disabled,
                allow_hosts: Vec::new(),
            })
        }
        "local" | "local-only" | "local_only" => Some(NetworkPolicy {
            mode: NetworkMode::LocalOnly,
            allow_hosts,
        }),
        unknown => {
            bail!("unknown permissions template {unknown:?}. Expected: open, closed, or local")
        }
    };
    let ceiling = McpPermissionCeiling {
        network,
        mounts,
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

fn validate_network_hosts(network: &NetworkPolicy, context: &str) -> Result<()> {
    if matches!(network.mode, NetworkMode::LocalOnly)
        && network
            .allow_hosts
            .iter()
            .any(|host| host.trim().is_empty())
    {
        bail!("{context} contains an empty network allow_hosts entry");
    }
    Ok(())
}

fn network_within_ceiling(ceiling: &NetworkPolicy, requested: &NetworkPolicy) -> bool {
    match ceiling.mode {
        NetworkMode::InheritHost => true,
        NetworkMode::Disabled => matches!(requested.mode, NetworkMode::Disabled),
        NetworkMode::LocalOnly => {
            matches!(requested.mode, NetworkMode::Disabled)
                || (matches!(requested.mode, NetworkMode::LocalOnly)
                    && host_subset(&requested.allow_hosts, &ceiling.allow_hosts))
        }
    }
}

fn host_subset(requested: &[String], ceiling: &[String]) -> bool {
    let ceiling = normalized_hosts(ceiling);
    normalized_hosts(requested)
        .iter()
        .all(|host| ceiling.contains(host))
}

fn normalized_hosts(hosts: &[String]) -> BTreeSet<String> {
    hosts
        .iter()
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect()
}

fn mount_within(
    allowed: &ProfileMount,
    allowed_host_real: &Path,
    requested: &ProfileMount,
    requested_host_real: &Path,
) -> bool {
    // Host side: compare symlink-resolved real paths so the containment test
    // reflects where the mount actually lands on the host, not its lexical
    // spelling. Workspace side stays lexical because that path lives in the
    // container namespace and does not resolve on the host at validate time.
    path_is_same_or_child(requested_host_real, allowed_host_real)
        && path_is_same_or_child(&requested.workspace_path, &allowed.workspace_path)
        && mount_mode_within(&allowed.mode, &requested.mode)
}

fn path_is_same_or_child(child: &Path, parent: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

/// Resolve a host path to its real location for containment checks, even when
/// the path (or its ultimate target) does not yet exist.
///
/// Walks the path one component at a time from the root. Each existing
/// component is followed through symlinks (including symlinks whose targets do
/// not exist), so the result reflects where the mount actually lands on the
/// host. The first component that does not exist as a filesystem entry, and
/// everything below it, is appended lexically (these are not-yet-created paths
/// the runtime would still place under the resolved real prefix).
///
/// A dangling symlink is still followed: a symlink inside the allowed root that
/// points outside it is an escape regardless of whether the target exists yet,
/// so its target path is what gets compared against the ceiling.
///
/// Returns an error only when a component cannot be stat'd for a reason other
/// than "not found" (e.g. permission/IO failure) or when symlink resolution
/// exceeds a loop guard. Callers treat that as a rejection, never a silent pass.
fn effective_host_path(path: &Path) -> Result<PathBuf> {
    // Caller has already run `validate_absolute_path`, so `path` is absolute and
    // contains no `..` of its own. Symlink *targets* may still contain `..`,
    // which is resolved lexically against the accumulated real path below.
    let mut resolved = PathBuf::from("/");
    let mut pending: VecDeque<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_os_string()),
            _ => None,
        })
        .collect();
    // Bound the number of symlinks we will follow to avoid infinite loops.
    let mut symlink_budget = 40usize;

    while let Some(name) = pending.pop_front() {
        if name == ".." {
            // A `..` introduced by a symlink target: step up the real path.
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if symlink_budget == 0 {
                    bail!(
                        "symlink resolution limit exceeded while resolving {}",
                        path.display()
                    );
                }
                symlink_budget -= 1;
                let target = fs::read_link(&candidate)
                    .with_context(|| format!("failed to read symlink {}", candidate.display()))?;
                // Splice the target's components ahead of the remaining ones.
                // An absolute target resets resolution to the root; a relative
                // target is resolved against the directory holding the symlink.
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                let mut spliced: VecDeque<OsString> = target
                    .components()
                    .filter_map(|component| match component {
                        Component::Normal(part) => Some(part.to_os_string()),
                        Component::ParentDir => Some(OsString::from("..")),
                        _ => None,
                    })
                    .collect();
                spliced.append(&mut pending);
                pending = spliced;
            }
            Ok(_) => {
                resolved = candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // First non-existing component: it and the rest are appended
                // lexically as the not-yet-created suffix.
                resolved = candidate;
                for rest in pending.drain(..) {
                    resolved.push(rest);
                }
                return Ok(resolved);
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "failed to inspect {} while resolving {}",
                    candidate.display(),
                    path.display()
                )));
            }
        }
    }
    Ok(resolved)
}

fn mount_mode_within(allowed: &MountMode, requested: &MountMode) -> bool {
    matches!(allowed, MountMode::ReadWrite) || matches!(requested, MountMode::ReadOnly)
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

fn network_mode_name(mode: &NetworkMode) -> &'static str {
    match mode {
        NetworkMode::InheritHost => "inherit_host",
        NetworkMode::Disabled => "disabled",
        NetworkMode::LocalOnly => "local_only",
    }
}

fn mount_mode_name(mode: &MountMode) -> &'static str {
    match mode {
        MountMode::ReadOnly => "read_only",
        MountMode::ReadWrite => "read_write",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{AppliedWorkspacePolicy, PolicyRuntimeCapabilities};
    use crate::workspace::EnvVar;

    fn launch_spec(command: &[&str], policy: Option<AppliedWorkspacePolicy>) -> LaunchSpec {
        LaunchSpec {
            command: command.iter().map(|part| part.to_string()).collect(),
            name: None,
            profile_id: policy.as_ref().map(|policy| policy.profile_id.clone()),
            applied_policy: policy,
            user_acknowledged_unenforced_policy: false,
            cwd: None,
            env: Vec::<EnvVar>::new(),
        }
    }

    fn policy(mounts: Vec<ProfileMount>, network: NetworkPolicy) -> AppliedWorkspacePolicy {
        AppliedWorkspacePolicy::new_with_capabilities(
            "test-profile".to_string(),
            mounts,
            network,
            false,
            0,
            PolicyRuntimeCapabilities::default(),
        )
    }

    fn mount(host_path: &str, workspace_path: &str, mode: MountMode) -> ProfileMount {
        ProfileMount {
            host_path: PathBuf::from(host_path),
            workspace_path: PathBuf::from(workspace_path),
            mode,
        }
    }

    fn configured_state(ceiling: McpPermissionCeiling) -> McpPermissionState {
        McpPermissionState::from_ceiling(Some(PathBuf::from("/tmp/mcp-permissions.json")), ceiling)
    }

    #[test]
    fn permission_templates_cover_open_closed_and_local() {
        let open = template_permission_ceiling("open", Vec::new(), Vec::new(), Vec::new()).unwrap();
        assert!(open.network.is_none());
        assert!(!open.is_restricted());

        let closed = template_permission_ceiling(
            "closed",
            Vec::new(),
            Vec::new(),
            vec![PathBuf::from("sh")],
        )
        .unwrap();
        assert!(matches!(
            closed.network.as_ref().map(|network| &network.mode),
            Some(NetworkMode::Disabled)
        ));
        assert_eq!(closed.apps.allow, vec![PathBuf::from("sh")]);
        assert!(closed.is_restricted());

        let local = template_permission_ceiling(
            "local",
            vec!["localhost:3000".to_string()],
            vec![mount(
                "/home/me/project",
                "/workspace/project",
                MountMode::ReadWrite,
            )],
            Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            local.network.as_ref().map(|network| &network.mode),
            Some(NetworkMode::LocalOnly)
        ));
        assert_eq!(
            local.network.unwrap().allow_hosts,
            vec!["localhost:3000".to_string()]
        );
        assert_eq!(local.mounts.len(), 1);
    }

    #[test]
    fn permission_template_rejects_hosts_for_non_local_modes() {
        let error = template_permission_ceiling(
            "closed",
            vec!["localhost:3000".to_string()],
            Vec::new(),
            Vec::new(),
        )
        .expect_err("closed template should reject allow-host")
        .to_string();
        assert!(error.contains("does not accept --allow-host"));
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
    fn unconfigured_state_drops_accidental_ceiling() {
        let state = McpPermissionState::from_ceiling(
            None,
            McpPermissionCeiling {
                network: Some(NetworkPolicy {
                    mode: NetworkMode::Disabled,
                    allow_hosts: Vec::new(),
                }),
                mounts: vec![mount(
                    "/home/me/project",
                    "/workspace/project",
                    MountMode::ReadOnly,
                )],
                apps: AppPermissionCeiling {
                    allow: vec![PathBuf::from("xterm")],
                },
            },
        );

        assert!(!state.configured);
        assert!(!state.restricted);
        assert!(state.ceiling.network.is_none());
        assert!(state.ceiling.mounts.is_empty());
        assert!(state.ceiling.apps.allow.is_empty());
        state
            .validate_launch_spec(&launch_spec(&["bash", "-lc", "true"], None))
            .unwrap();
    }

    #[test]
    fn disabled_network_ceiling_rejects_unprofiled_launch() {
        let state = configured_state(McpPermissionCeiling {
            network: Some(NetworkPolicy {
                mode: NetworkMode::Disabled,
                allow_hosts: Vec::new(),
            }),
            mounts: Vec::new(),
            apps: AppPermissionCeiling::default(),
        });

        let error = state
            .validate_launch_spec(&launch_spec(&["curl", "https://example.com"], None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds MCP permission ceiling"));
    }

    #[test]
    fn disabled_network_ceiling_allows_disabled_profile_launch() {
        let state = configured_state(McpPermissionCeiling {
            network: Some(NetworkPolicy {
                mode: NetworkMode::Disabled,
                allow_hosts: Vec::new(),
            }),
            mounts: Vec::new(),
            apps: AppPermissionCeiling::default(),
        });
        let applied = policy(
            Vec::new(),
            NetworkPolicy {
                mode: NetworkMode::Disabled,
                allow_hosts: Vec::new(),
            },
        );

        state
            .validate_launch_spec(&launch_spec(
                &["curl", "https://example.com"],
                Some(applied),
            ))
            .unwrap();
    }

    #[test]
    fn local_only_ceiling_rejects_inherited_network() {
        let state = configured_state(McpPermissionCeiling {
            network: Some(NetworkPolicy {
                mode: NetworkMode::LocalOnly,
                allow_hosts: vec!["localhost:3000".to_string()],
            }),
            mounts: Vec::new(),
            apps: AppPermissionCeiling::default(),
        });

        let error = state
            .validate_launch_spec(&launch_spec(&["curl", "https://example.com"], None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("network.mode=inherit_host"));
    }

    #[test]
    fn local_only_ceiling_requires_requested_hosts_to_be_subset() {
        let ceiling = McpPermissionCeiling {
            network: Some(NetworkPolicy {
                mode: NetworkMode::LocalOnly,
                allow_hosts: vec!["localhost:3000".to_string()],
            }),
            mounts: Vec::new(),
            apps: AppPermissionCeiling::default(),
        };
        let allowed = NetworkPolicy {
            mode: NetworkMode::LocalOnly,
            allow_hosts: vec!["LOCALHOST:3000".to_string()],
        };
        let blocked = NetworkPolicy {
            mode: NetworkMode::LocalOnly,
            allow_hosts: vec!["localhost:9999".to_string()],
        };

        ceiling.validate_network(&allowed, "test").unwrap();
        assert!(ceiling.validate_network(&blocked, "test").is_err());
    }

    #[test]
    fn mount_ceiling_rejects_missing_or_broader_mount_policy() {
        let state = configured_state(McpPermissionCeiling {
            network: None,
            mounts: vec![mount(
                "/home/me/project",
                "/workspace/project",
                MountMode::ReadOnly,
            )],
            apps: AppPermissionCeiling::default(),
        });

        assert!(state
            .validate_launch_spec(&launch_spec(&["ls"], None))
            .unwrap_err()
            .to_string()
            .contains("does not request a mount policy"));

        let broader = policy(
            vec![mount("/home/me", "/workspace", MountMode::ReadOnly)],
            NetworkPolicy::default(),
        );
        assert!(state
            .validate_launch_spec(&launch_spec(&["ls"], Some(broader)))
            .is_err());
    }

    #[test]
    fn mount_ceiling_allows_child_read_only_mount() {
        let state = configured_state(McpPermissionCeiling {
            network: None,
            mounts: vec![mount(
                "/home/me/project",
                "/workspace/project",
                MountMode::ReadOnly,
            )],
            apps: AppPermissionCeiling::default(),
        });
        let narrowed = policy(
            vec![mount(
                "/home/me/project/src",
                "/workspace/project/src",
                MountMode::ReadOnly,
            )],
            NetworkPolicy::default(),
        );

        state
            .validate_launch_spec(&launch_spec(&["ls"], Some(narrowed)))
            .unwrap();
    }

    #[test]
    fn mount_ceiling_rejects_read_write_when_ceiling_is_read_only() {
        let ceiling = McpPermissionCeiling {
            network: None,
            mounts: vec![mount(
                "/home/me/project",
                "/workspace/project",
                MountMode::ReadOnly,
            )],
            apps: AppPermissionCeiling::default(),
        };
        let requested = vec![mount(
            "/home/me/project",
            "/workspace/project",
            MountMode::ReadWrite,
        )];

        assert!(ceiling.validate_mounts(&requested, "test").is_err());
    }

    #[test]
    fn app_allowlist_limits_launch_programs() {
        let state = configured_state(McpPermissionCeiling {
            network: None,
            mounts: Vec::new(),
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
            network: None,
            mounts: Vec::new(),
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
            network: None,
            mounts: Vec::new(),
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

    /// Create a unique scratch directory under the system temp dir without
    /// pulling in a temp-file crate (none is a dependency of this crate).
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "agentws-perm-test-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn mount_ceiling_rejects_symlink_escaping_ceiling_root() {
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("symlink-escape");
        // canonicalize so comparisons are robust on hosts where temp_dir is
        // itself a symlink (e.g. /tmp -> /private/tmp).
        let base = fs::canonicalize(&base).unwrap();
        let allowed_root = base.join("allowed");
        let outside = base.join("outside");
        let secret = outside.join("secret");
        fs::create_dir_all(&allowed_root).unwrap();
        fs::create_dir_all(&secret).unwrap();

        // A symlink that lives INSIDE the allowed root but points OUTSIDE it.
        let escape_link = allowed_root.join("escape");
        symlink(&secret, &escape_link).unwrap();

        let ceiling = McpPermissionCeiling {
            network: None,
            mounts: vec![ProfileMount {
                host_path: allowed_root.clone(),
                workspace_path: PathBuf::from("/workspace/project"),
                mode: MountMode::ReadWrite,
            }],
            apps: AppPermissionCeiling::default(),
        };

        // Requesting the symlink as the mount host_path passes the lexical
        // subset check (it starts_with allowed_root) but its real target
        // escapes the ceiling, so it must be rejected.
        let escaping = vec![ProfileMount {
            host_path: escape_link.clone(),
            workspace_path: PathBuf::from("/workspace/project/escape"),
            mode: MountMode::ReadWrite,
        }];
        let error = ceiling
            .validate_mounts(&escaping, "symlink test")
            .expect_err("symlink escaping the ceiling root must be rejected")
            .to_string();
        assert!(
            error.contains("outside the MCP permission ceiling"),
            "unexpected error: {error}"
        );

        // A legitimate same-or-child mount (a real subdir of the allowed root)
        // still passes.
        let real_child = allowed_root.join("sub");
        fs::create_dir_all(&real_child).unwrap();
        let legitimate = vec![ProfileMount {
            host_path: real_child.clone(),
            workspace_path: PathBuf::from("/workspace/project/sub"),
            mode: MountMode::ReadWrite,
        }];
        ceiling
            .validate_mounts(&legitimate, "symlink test")
            .expect("a real same-or-child mount must still pass");

        // A DANGLING symlink inside the allowed root that points outside it is
        // still an escape: the runtime bind-mount would resolve the link to its
        // (out-of-ceiling) target regardless of whether that target exists yet.
        let dangling_target = outside.join("not-created-yet");
        let dangling_link = allowed_root.join("dangling");
        symlink(&dangling_target, &dangling_link).unwrap();
        assert!(
            !dangling_link.exists(),
            "test setup: target must not exist for the dangling case"
        );
        let dangling = vec![ProfileMount {
            host_path: dangling_link.clone(),
            workspace_path: PathBuf::from("/workspace/project/dangling"),
            mode: MountMode::ReadWrite,
        }];
        let error = ceiling
            .validate_mounts(&dangling, "symlink test")
            .expect_err("a dangling symlink escaping the ceiling root must be rejected")
            .to_string();
        assert!(
            error.contains("outside the MCP permission ceiling"),
            "unexpected error for dangling symlink: {error}"
        );

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn shell_allowlist_entry_produces_arbitrary_execution_advisory() {
        let state = configured_state(McpPermissionCeiling {
            network: None,
            mounts: Vec::new(),
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
            network: None,
            mounts: Vec::new(),
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
            network: None,
            mounts: Vec::new(),
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
