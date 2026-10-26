use crate::approval::ApprovalBundle;
use crate::launch::merged_env;
use crate::session::SessionSpec;
use crate::workspace::{self, EnvVar, IpcResponse, LaunchSpec, WorkspaceStartOptions};
use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceProfile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub session: SessionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_commands: Vec<ProfileSetupCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup_apps: Vec<ProfileStartupApp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProfileSetupCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProfileStartupApp {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileStore {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<WorkspaceProfile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileList {
    pub path: PathBuf,
    pub profiles: Vec<WorkspaceProfile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfilePath {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileDeleteResult {
    pub id: String,
    pub deleted: bool,
    pub would_delete: bool,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<WorkspaceProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfilePutResult {
    pub ok: bool,
    pub message: String,
    pub id: String,
    pub action: String,
    pub saved: bool,
    pub created: bool,
    pub replaced: bool,
    pub would_create: bool,
    pub would_replace: bool,
    pub replace: bool,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<WorkspaceProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_profile: Option<WorkspaceProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileExportResult {
    pub id: String,
    pub wrote: bool,
    pub would_write: bool,
    pub replace: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    pub profile: WorkspaceProfile,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileValidateResult {
    pub ok: bool,
    pub message: String,
    pub json_path: PathBuf,
    pub profile: WorkspaceProfile,
    pub check: ProfileCheck,
}

impl ProfilePutResult {
    pub fn error(profile: WorkspaceProfile, replace: bool, dry_run: bool, message: String) -> Self {
        Self {
            ok: false,
            message,
            id: profile.id.clone(),
            action: "error".to_string(),
            saved: false,
            created: false,
            replaced: false,
            would_create: false,
            would_replace: false,
            replace,
            dry_run,
            profile: Some(profile),
            existing_profile: None,
            agent_mode: None,
            recovery_hints: Vec::new(),
        }
    }

    pub fn import_error(replace: bool, dry_run: bool, message: String) -> Self {
        Self {
            ok: false,
            message,
            id: "import".to_string(),
            action: "error".to_string(),
            saved: false,
            created: false,
            replaced: false,
            would_create: false,
            would_replace: false,
            replace,
            dry_run,
            profile: None,
            existing_profile: None,
            agent_mode: None,
            recovery_hints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileCheck {
    pub profile: WorkspaceProfile,
    pub requires_hidden_workspace_ack: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileSetupRun {
    pub workspace_id: String,
    pub profile_id: String,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would_launch_all: Option<bool>,
    pub wait: bool,
    pub kill_on_timeout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub launched: Vec<IpcResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waited: Vec<IpcResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub killed: Vec<IpcResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileStartupRun {
    pub workspace_id: String,
    pub profile_id: String,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub would_launch_all: Option<bool>,
    pub wait_window: bool,
    pub screenshot_window: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_timeout_ms: Option<u64>,
    pub launched: Vec<IpcResponse>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileWorkspaceOpen {
    pub workspace_id: String,
    pub profile_id: String,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_succeeded: Option<bool>,
    pub startup_launched: bool,
    pub start: IpcResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<ProfileSetupRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup: Option<ProfileStartupRun>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileWorkspaceOpenPreview {
    pub workspace_id: String,
    pub profile_id: String,
    pub would_open: bool,
    pub start: IpcResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<ProfileSetupPreview>,
    pub startup: ProfileStartupPreview,
    pub message: String,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileSetupPreview {
    pub workspace_id: String,
    pub profile_id: String,
    pub would_run_setup: bool,
    pub wait: bool,
    pub kill_on_timeout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub command_count: usize,
    pub requires_running_daemon_for_launch_preview: bool,
    pub commands: Vec<ProfileLaunchPlan>,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileStartupPreview {
    pub workspace_id: String,
    pub profile_id: String,
    pub conditional_on_setup_success: bool,
    pub wait_window: bool,
    pub screenshot_window: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_timeout_ms: Option<u64>,
    pub app_count: usize,
    pub requires_running_daemon_for_launch_preview: bool,
    pub apps: Vec<ProfileLaunchPlan>,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProfileLaunchPlan {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileWorkspaceOpenOptions {
    pub run_setup: bool,
    pub setup: ProfileSetupOptions,
    pub startup: ProfileStartupOptions,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileSetupOptions {
    pub dry_run: bool,
    pub wait: bool,
    pub timeout_ms: Option<u64>,
    pub kill_on_timeout: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileStartupOptions {
    pub dry_run: bool,
    pub wait_window: bool,
    pub window_timeout_ms: Option<u64>,
    pub screenshot_window: bool,
}

pub fn profiles_path() -> PathBuf {
    config_dir().join("profiles.json")
}

pub fn profile_path() -> ProfilePath {
    ProfilePath {
        path: profiles_path(),
    }
}

pub fn list_profiles() -> Result<ProfileList> {
    let path = profiles_path();
    let store = read_store(&path)?;
    Ok(ProfileList {
        path,
        profiles: store.profiles,
    })
}

pub fn get_profile(id: &str) -> Result<WorkspaceProfile> {
    let id = sanitize_profile_id(id)?;
    read_store(&profiles_path())?
        .profiles
        .into_iter()
        .find(|profile| profile.id == id)
        .ok_or_else(|| anyhow::anyhow!("profile {id:?} was not found"))
}

pub fn export_profile(
    id: &str,
    output_path: Option<PathBuf>,
    replace: bool,
) -> Result<ProfileExportResult> {
    let profile = get_profile(id)?;
    let would_write = output_path.is_some();
    let output_path = output_path.map(|path| {
        if path.is_dir() {
            path.join(format!("{}.json", profile.id))
        } else {
            path
        }
    });
    if let Some(path) = &output_path {
        if path.exists() && !replace {
            bail!(
                "profile export output {} already exists; pass --replace or set replace=true to overwrite it",
                path.display()
            );
        }
        let content = serde_json::to_string_pretty(&profile)
            .context("failed to serialize profile for export")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, format!("{content}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(ProfileExportResult {
        id: profile.id.clone(),
        wrote: output_path.is_some(),
        would_write,
        replace,
        output_path,
        profile,
    })
}

pub fn read_profile_json_file(path: &Path) -> Result<WorkspaceProfile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read profile JSON from {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse profile JSON from {}", path.display()))
}

pub fn template_profile(
    kind: &str,
    id: Option<String>,
    host_path: Option<PathBuf>,
    browser_path: Option<PathBuf>,
    user_data_dir: Option<PathBuf>,
) -> Result<WorkspaceProfile> {
    let kind = kind.trim();
    let profile = match kind {
        "project-dev" | "project_dev" => {
            let id = id.unwrap_or_else(|| "project-dev".to_string());
            let host_path = match host_path {
                Some(path) => path,
                None => env::current_dir().context("failed to resolve template host path")?,
            };
            WorkspaceProfile {
                id,
                description: Some(format!(
                    "Project QA profile: apps start in {}.",
                    host_path.display()
                )),
                session: SessionSpec::default(),
                width: None,
                height: None,
                cwd: Some(host_path),
                env: Vec::new(),
                setup_commands: Vec::new(),
                startup_apps: Vec::new(),
            }
        }
        "chrome" => {
            let id = id.unwrap_or_else(|| "chrome".to_string());
            let browser = browser_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "google-chrome".to_string());
            WorkspaceProfile {
                id,
                description: Some(
                    "Chrome with a fresh profile in /tmp/agent-workspace-chrome. Exposes Chrome DevTools on an ephemeral loopback port for workspace-owned browser control.".to_string(),
                ),
                session: SessionSpec::default(),
                width: Some(1280),
                height: Some(800),
                cwd: Some(PathBuf::from("/tmp")),
                env: Vec::new(),
                setup_commands: Vec::new(),
                startup_apps: vec![ProfileStartupApp {
                    name: Some("chrome".to_string()),
                    command: chrome_command(browser, "/tmp/agent-workspace-chrome"),
                    cwd: Some(PathBuf::from("/tmp")),
                    env: Vec::new(),
                }],
            }
        }
        "browser-session" | "browser_session" | "authenticated-chrome" | "authenticated_chrome" => {
            let id = id.unwrap_or_else(|| "browser-session".to_string());
            let user_data_dir = user_data_dir
                .or(host_path)
                .context("browser-session template requires --user-data-dir PATH")?;
            let browser = browser_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "google-chrome".to_string());
            WorkspaceProfile {
                id,
                description: Some(format!(
                    "Chrome on the user-data directory {}, with its logins. Close the host browser or point to a copied profile first: Chrome opens one profile in one process. Exposes Chrome DevTools on an ephemeral loopback port for workspace-owned browser control.",
                    user_data_dir.display()
                )),
                session: SessionSpec::default(),
                width: Some(1280),
                height: Some(800),
                cwd: Some(PathBuf::from("/tmp")),
                env: Vec::new(),
                setup_commands: Vec::new(),
                startup_apps: vec![ProfileStartupApp {
                    name: Some("browser-session".to_string()),
                    command: chrome_command(browser, &user_data_dir.display().to_string()),
                    cwd: Some(PathBuf::from("/tmp")),
                    env: Vec::new(),
                }],
            }
        }
        _ => bail!(
            "unknown profile template {kind:?}. Expected: project-dev, chrome, or browser-session"
        ),
    };
    validate_profile(&profile)?;
    Ok(profile)
}

fn chrome_command(browser: String, user_data_dir: &str) -> Vec<String> {
    vec![
        browser,
        format!("--user-data-dir={user_data_dir}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--new-window".to_string(),
        "about:blank".to_string(),
    ]
}

pub fn check_profile(id: &str) -> Result<ProfileCheck> {
    let profile = get_profile(id)?;
    Ok(check_workspace_profile(profile))
}

fn check_workspace_profile(profile: WorkspaceProfile) -> ProfileCheck {
    ProfileCheck {
        profile,
        requires_hidden_workspace_ack: true,
    }
}

pub fn validate_profile_json_file(path: PathBuf) -> Result<ProfileValidateResult> {
    let profile = read_profile_json_file(&path)?;
    validate_profile(&profile)?;
    let check = check_workspace_profile(profile.clone());
    Ok(ProfileValidateResult {
        ok: true,
        message: "profile JSON is valid".to_string(),
        json_path: path,
        profile,
        check,
    })
}

pub fn put_profile(
    profile: WorkspaceProfile,
    replace: bool,
    dry_run: bool,
) -> Result<ProfilePutResult> {
    validate_profile(&profile)?;
    let path = profiles_path();
    let mut store = read_store(&path)?;
    let existing_index = store
        .profiles
        .iter()
        .position(|existing| existing.id == profile.id);
    let existing_profile = existing_index.map(|index| store.profiles[index].clone());
    let action = match (&existing_profile, replace) {
        (None, _) => "create",
        (Some(_), true) => "replace",
        (Some(_), false) => "reject_existing",
    };
    let would_create = existing_profile.is_none();
    let would_replace = existing_profile.is_some() && replace;

    if !dry_run {
        if let Some(index) = existing_index {
            if !replace {
                bail!(
                    "profile {:?} already exists; pass --replace or set replace=true to overwrite it",
                    profile.id
                );
            }
            store.profiles[index] = profile.clone();
        } else {
            store.profiles.push(profile.clone());
        }
        store.profiles.sort_by(|left, right| left.id.cmp(&right.id));
        write_store(&path, &store)?;
    }

    let saved = !dry_run && action != "reject_existing";
    let created = saved && action == "create";
    let replaced = saved && action == "replace";
    let message = match (dry_run, action) {
        (true, "create") => "profile put dry run: profile would be created",
        (true, "replace") => "profile put dry run: profile would be replaced",
        (true, "reject_existing") => {
            "profile put dry run: profile exists and requires replace=true to overwrite"
        }
        (false, "create") => "profile created",
        (false, "replace") => "profile replaced",
        _ => {
            bail!(
                "profile {:?} already exists; pass --replace or set replace=true to overwrite it",
                profile.id
            );
        }
    };

    Ok(ProfilePutResult {
        ok: true,
        message: message.to_string(),
        id: profile.id.clone(),
        action: action.to_string(),
        saved,
        created,
        replaced,
        would_create,
        would_replace,
        replace,
        dry_run,
        profile: Some(profile),
        existing_profile,
        agent_mode: None,
        recovery_hints: Vec::new(),
    })
}

pub fn delete_profile(id: &str, dry_run: bool) -> Result<ProfileDeleteResult> {
    let id = sanitize_profile_id(id)?;
    let path = profiles_path();
    let mut store = read_store(&path)?;
    let profile_index = store.profiles.iter().position(|profile| profile.id == id);
    let profile = profile_index.map(|index| store.profiles[index].clone());
    let would_delete = profile.is_some();
    let deleted = would_delete && !dry_run;
    if let Some(index) = profile_index.filter(|_| !dry_run) {
        store.profiles.remove(index);
        write_store(&path, &store)?;
    }
    Ok(ProfileDeleteResult {
        id,
        deleted,
        would_delete,
        dry_run,
        profile,
        agent_mode: None,
    })
}

pub fn validate_profile(profile: &WorkspaceProfile) -> Result<()> {
    sanitize_profile_id(&profile.id)?;
    if matches!(profile.width, Some(0)) {
        bail!("profile width must be greater than zero");
    }
    if matches!(profile.height, Some(0)) {
        bail!("profile height must be greater than zero");
    }
    validate_optional_profile_cwd(&profile.cwd, "profile cwd")?;
    validate_env_vars(&profile.env, "profile env")?;
    for setup in &profile.setup_commands {
        workspace::validate_optional_app_name(&setup.name)?;
        workspace::validate_command(&setup.command, "profile setup")?;
        validate_optional_profile_cwd(&setup.cwd, "profile setup cwd")?;
        validate_env_vars(&setup.env, "profile setup env")?;
    }
    for app in &profile.startup_apps {
        workspace::validate_optional_app_name(&app.name)?;
        workspace::validate_command(&app.command, "profile startup app")?;
        validate_optional_profile_cwd(&app.cwd, "profile startup app cwd")?;
        validate_env_vars(&app.env, "profile startup app env")?;
    }
    Ok(())
}

fn validate_optional_profile_cwd(cwd: &Option<PathBuf>, field: &str) -> Result<()> {
    let Some(cwd) = cwd else {
        return Ok(());
    };
    if cwd.as_os_str().is_empty() {
        bail!("{field} cannot be empty");
    }
    if !cwd.is_absolute() {
        bail!("{field} {} must be absolute", cwd.display());
    }
    if cwd
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("{field} {} cannot contain '..'", cwd.display());
    }
    Ok(())
}

pub fn apply_profile_to_start_options(
    profile_id: &str,
    options: &mut WorkspaceStartOptions,
    width_explicit: bool,
    height_explicit: bool,
) -> Result<WorkspaceProfile> {
    let profile = get_profile(profile_id)?;
    options.profile_id = Some(profile.id.clone());
    options.session = profile.session;
    options.profile_cwd = profile.cwd.clone();
    options.profile_env = profile.env.clone();
    if !width_explicit {
        if let Some(width) = profile.width {
            options.width = width;
        }
    }
    if !height_explicit {
        if let Some(height) = profile.height {
            options.height = height;
        }
    }
    Ok(profile)
}

pub fn apply_profile_to_launch_spec(
    profile_id: &str,
    spec: &mut LaunchSpec,
    cwd_explicit: bool,
) -> Result<WorkspaceProfile> {
    let profile = get_profile(profile_id)?;
    spec.profile_id = Some(profile.id.clone());
    if !cwd_explicit {
        if let Some(cwd) = profile.cwd.clone() {
            spec.cwd = Some(cwd);
        }
    }
    let explicit_env = std::mem::take(&mut spec.env);
    spec.env = merged_env(profile.env.clone(), explicit_env);
    Ok(profile)
}

pub fn setup_launch_specs(profile_id: &str) -> Result<Vec<LaunchSpec>> {
    let profile = get_profile(profile_id)?;
    profile
        .setup_commands
        .iter()
        .map(|setup| {
            if setup.command.is_empty() {
                bail!("profile setup command cannot be empty");
            }
            Ok(LaunchSpec {
                command: setup.command.clone(),
                name: setup.name.clone(),
                profile_id: Some(profile.id.clone()),
                cwd: setup.cwd.clone().or_else(|| profile.cwd.clone()),
                env: merged_env(profile.env.clone(), setup.env.clone()),
            })
        })
        .collect()
}

pub fn startup_app_launch_specs(profile_id: &str) -> Result<Vec<LaunchSpec>> {
    let profile = get_profile(profile_id)?;
    profile
        .startup_apps
        .iter()
        .map(|app| {
            if app.command.is_empty() {
                bail!("profile startup app command cannot be empty");
            }
            Ok(LaunchSpec {
                command: app.command.clone(),
                name: app.name.clone(),
                profile_id: Some(profile.id.clone()),
                cwd: app.cwd.clone().or_else(|| profile.cwd.clone()),
                env: merged_env(profile.env.clone(), app.env.clone()),
            })
        })
        .collect()
}

pub fn preview_open_profile_workspace(
    options: WorkspaceStartOptions,
    profile_id: &str,
    open_options: ProfileWorkspaceOpenOptions,
) -> Result<ProfileWorkspaceOpenPreview> {
    let workspace_id = options.id.clone();
    let start = workspace::preview_workspace_start(options)?;
    let start_preview = start
        .start_preview
        .as_ref()
        .context("workspace start preview response did not include start_preview")?;
    let setup = if open_options.run_setup {
        Some(preview_profile_setup(
            &workspace_id,
            profile_id,
            &open_options.setup,
        )?)
    } else {
        None
    };
    let startup = preview_profile_startup(
        &workspace_id,
        profile_id,
        &open_options.startup,
        open_options.run_setup,
    )?;
    let would_open = start_preview.ok_to_start;
    let message = if would_open {
        "profile workspace open dry run returned".to_string()
    } else {
        format!(
            "profile workspace open dry run blocked: {}",
            start_preview.message
        )
    };
    let mut approval = ApprovalBundle::new(
        "workspace_open_profile",
        format!("profile {profile_id} into workspace {workspace_id}"),
        would_open,
    );
    approval = approval.merge_child(&start_preview.approval);
    if let Some(setup) = &setup {
        approval = approval.merge_child(&setup.approval);
    }
    approval = approval.merge_child(&startup.approval);

    Ok(ProfileWorkspaceOpenPreview {
        workspace_id,
        profile_id: profile_id.to_string(),
        would_open,
        start,
        setup,
        startup,
        message,
        approval,
    })
}

fn preview_profile_setup(
    workspace_id: &str,
    profile_id: &str,
    options: &ProfileSetupOptions,
) -> Result<ProfileSetupPreview> {
    let wait = options.wait || options.timeout_ms.is_some() || options.kill_on_timeout;
    let commands = setup_launch_specs(profile_id)?
        .into_iter()
        .map(profile_launch_plan)
        .collect::<Vec<_>>();
    let approval = profile_plan_approval_bundle(
        "profile_setup",
        format!("profile {profile_id} setup"),
        &commands,
    );

    Ok(ProfileSetupPreview {
        workspace_id: workspace_id.to_string(),
        profile_id: profile_id.to_string(),
        would_run_setup: true,
        wait,
        kill_on_timeout: options.kill_on_timeout,
        timeout_ms: options.timeout_ms,
        command_count: commands.len(),
        requires_running_daemon_for_launch_preview: true,
        commands,
        approval,
    })
}

fn preview_profile_startup(
    workspace_id: &str,
    profile_id: &str,
    options: &ProfileStartupOptions,
    conditional_on_setup_success: bool,
) -> Result<ProfileStartupPreview> {
    let apps = startup_app_launch_specs(profile_id)?
        .into_iter()
        .map(profile_launch_plan)
        .collect::<Vec<_>>();
    let approval = profile_plan_approval_bundle(
        "profile_startup",
        format!("profile {profile_id} startup apps"),
        &apps,
    );

    Ok(ProfileStartupPreview {
        workspace_id: workspace_id.to_string(),
        profile_id: profile_id.to_string(),
        conditional_on_setup_success,
        wait_window: options.wait_window,
        screenshot_window: options.screenshot_window,
        window_timeout_ms: options.window_timeout_ms,
        app_count: apps.len(),
        requires_running_daemon_for_launch_preview: true,
        apps,
        approval,
    })
}

fn profile_launch_plan(spec: LaunchSpec) -> ProfileLaunchPlan {
    let subject = spec.name.clone().unwrap_or_else(|| spec.command.join(" "));
    ProfileLaunchPlan {
        command: spec.command,
        name: spec.name,
        cwd: spec.cwd,
        env: spec.env,
        profile_id: spec.profile_id,
        approval: ApprovalBundle::new("profile_launch_declaration", subject, true),
    }
}

fn profile_plan_approval_bundle(
    action: &str,
    subject: String,
    plans: &[ProfileLaunchPlan],
) -> ApprovalBundle {
    plans.iter().fold(
        ApprovalBundle::new(action, subject, true),
        |bundle, plan| bundle.merge_child(&plan.approval),
    )
}

pub fn launch_profile_setup(
    workspace_id: &str,
    profile_id: &str,
    options: ProfileSetupOptions,
) -> Result<ProfileSetupRun> {
    let wait = options.wait || options.timeout_ms.is_some() || options.kill_on_timeout;
    let specs = setup_launch_specs(profile_id)?;
    let mut launched = Vec::new();
    let mut waited = Vec::new();
    let mut killed = Vec::new();
    let mut app_ids = Vec::new();
    for spec in specs {
        let launch = if options.dry_run {
            workspace::preview_launch_app(workspace_id, spec, false, None, false)?
        } else {
            workspace::launch_app_with_spec(workspace_id, spec)?
        };
        if options.dry_run {
            launched.push(launch);
            continue;
        }
        if !launch.ok {
            launched.push(launch);
            break;
        }
        if wait {
            let app_id = launched_app_id(&launch)
                .context("profile setup launch did not return an app id")?;
            let wait =
                workspace::wait_app(workspace_id, app_id.clone(), options.timeout_ms, false)?;
            let should_kill =
                options.kill_on_timeout && !wait.ok && response_app_running(&wait, &app_id);
            if should_kill {
                killed.push(workspace::kill_app(workspace_id, app_id.clone(), false)?);
            }
            app_ids.push(app_id);
            waited.push(wait);
        }
        launched.push(launch);
    }
    let would_launch_all = options.dry_run.then(|| {
        launched.iter().all(|response| {
            response
                .launch_preview
                .as_ref()
                .is_some_and(|preview| preview.would_launch)
        })
    });
    let completed = (!options.dry_run && wait).then(|| {
        launched.iter().all(|response| response.ok)
            && waited.iter().all(|response| response.ok)
            && waited.len() == launched.len()
    });
    let succeeded = (!options.dry_run && wait).then(|| {
        completed.unwrap_or(false)
            && waited
                .iter()
                .zip(app_ids.iter())
                .all(|(response, app_id)| response_app_exit_code(response, app_id) == Some(0))
    });
    Ok(ProfileSetupRun {
        workspace_id: workspace_id.to_string(),
        profile_id: profile_id.to_string(),
        dry_run: options.dry_run,
        would_launch_all,
        wait,
        kill_on_timeout: options.kill_on_timeout,
        timeout_ms: options.timeout_ms,
        launched,
        waited,
        killed,
        completed,
        succeeded,
    })
}

pub fn launch_profile_startup_apps(
    workspace_id: &str,
    profile_id: &str,
    options: ProfileStartupOptions,
) -> Result<ProfileStartupRun> {
    let specs = startup_app_launch_specs(profile_id)?;
    let mut launched = Vec::new();
    for spec in specs {
        let response = if options.dry_run {
            workspace::preview_launch_app(
                workspace_id,
                spec,
                options.wait_window,
                options.window_timeout_ms,
                options.screenshot_window,
            )?
        } else {
            workspace::launch_app_with_options(
                workspace_id,
                spec,
                options.wait_window,
                options.window_timeout_ms,
                options.screenshot_window,
            )?
        };
        launched.push(response);
    }
    let would_launch_all = options.dry_run.then(|| {
        launched.iter().all(|response| {
            response
                .launch_preview
                .as_ref()
                .is_some_and(|preview| preview.would_launch)
        })
    });
    Ok(ProfileStartupRun {
        workspace_id: workspace_id.to_string(),
        profile_id: profile_id.to_string(),
        dry_run: options.dry_run,
        would_launch_all,
        wait_window: options.wait_window,
        screenshot_window: options.screenshot_window,
        window_timeout_ms: options.window_timeout_ms,
        launched,
    })
}

pub fn open_profile_workspace(
    options: WorkspaceStartOptions,
    profile_id: &str,
    open_options: ProfileWorkspaceOpenOptions,
) -> Result<ProfileWorkspaceOpen> {
    let workspace_id = options.id.clone();
    let start = workspace::start_workspace(options)?;
    let (setup, startup) = if start.ok {
        let setup = if open_options.run_setup {
            Some(launch_profile_setup(
                &workspace_id,
                profile_id,
                open_options.setup,
            )?)
        } else {
            None
        };
        let setup_succeeded = setup
            .as_ref()
            .and_then(|setup| setup.succeeded)
            .unwrap_or(true);
        let startup = if setup_succeeded {
            Some(launch_profile_startup_apps(
                &workspace_id,
                profile_id,
                open_options.startup,
            )?)
        } else {
            None
        };
        (setup, startup)
    } else {
        (None, None)
    };
    let setup_succeeded = setup.as_ref().and_then(|setup| setup.succeeded);
    let startup_launched = startup
        .as_ref()
        .is_some_and(|startup| startup.launched.iter().all(|response| response.ok));
    let ready = start.ok && setup_succeeded.unwrap_or(true) && startup_launched;
    Ok(ProfileWorkspaceOpen {
        workspace_id,
        profile_id: profile_id.to_string(),
        ready,
        setup_succeeded,
        startup_launched,
        start,
        setup,
        startup,
    })
}

fn launched_app_id(response: &IpcResponse) -> Option<String> {
    if let Some(app) = response.apps.as_ref().and_then(|apps| apps.last()) {
        return Some(app.id.clone());
    }
    response
        .status
        .as_ref()?
        .apps
        .last()
        .map(|app| app.id.clone())
}

fn response_app_running(response: &IpcResponse, app_id: &str) -> bool {
    response
        .apps
        .as_ref()
        .and_then(|apps| apps.iter().find(|app| app.id == app_id))
        .or_else(|| {
            response
                .status
                .as_ref()?
                .apps
                .iter()
                .find(|app| app.id == app_id || app.pid.to_string() == app_id)
        })
        .is_some_and(|app| app.running)
}

fn response_app_exit_code(response: &IpcResponse, app_id: &str) -> Option<i32> {
    if let Some(app) = response
        .apps
        .as_ref()
        .and_then(|apps| apps.iter().find(|app| app.id == app_id))
    {
        return app.exit_code;
    }
    response
        .status
        .as_ref()?
        .apps
        .iter()
        .find(|app| app.id == app_id || app.pid.to_string() == app_id)?
        .exit_code
}

fn read_store(path: &Path) -> Result<ProfileStore> {
    if !path.exists() {
        return Ok(ProfileStore::default());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(ProfileStore::default());
    }
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_store(path: &Path, store: &ProfileStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let temp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(store).context("failed to serialize profiles")?;
    fs::write(&temp_path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-workspace-linux")
}

fn sanitize_profile_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("profile id cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("profile id may only contain ASCII letters, numbers, '-' and '_'");
    }
    Ok(trimmed.to_string())
}

fn validate_env_var(env_var: &EnvVar) -> Result<()> {
    if env_var.name.is_empty() {
        bail!("environment variable name cannot be empty");
    }
    if env_var.name.contains('=') {
        bail!("environment variable name cannot contain '='");
    }
    if env_var.name.contains('\0') || env_var.value.contains('\0') {
        bail!("environment variable cannot contain NUL bytes");
    }
    Ok(())
}

fn validate_env_vars(env: &[EnvVar], subject: &str) -> Result<()> {
    let mut names = BTreeSet::new();
    for env_var in env {
        validate_env_var(env_var)?;
        if !names.insert(env_var.name.clone()) {
            bail!(
                "{subject} duplicates environment variable {:?}",
                env_var.name
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> WorkspaceProfile {
        WorkspaceProfile {
            id: "qa-local".to_string(),
            description: None,
            session: SessionSpec::default(),
            width: None,
            height: None,
            cwd: None,
            env: Vec::new(),
            setup_commands: Vec::new(),
            startup_apps: Vec::new(),
        }
    }

    #[test]
    fn legacy_profile_without_session_defaults_to_x11() {
        let profile: WorkspaceProfile =
            serde_json::from_str(r#"{"id":"legacy"}"#).expect("legacy profile should parse");

        assert_eq!(profile.session, SessionSpec::default());
    }

    #[test]
    fn profile_validation_rejects_duplicate_env_names() {
        let mut profile = test_profile();
        profile.env = vec![
            EnvVar {
                name: "DUP".to_string(),
                value: "one".to_string(),
            },
            EnvVar {
                name: "DUP".to_string(),
                value: "two".to_string(),
            },
        ];

        let error = validate_profile(&profile).expect_err("duplicate env names should fail");
        assert!(error
            .to_string()
            .contains("duplicates environment variable"));
    }

    #[test]
    fn profile_validation_rejects_relative_cwd() {
        let mut profile = test_profile();
        profile.cwd = Some(PathBuf::from("relative"));

        let error = validate_profile(&profile).expect_err("relative cwd should fail");
        assert!(error.to_string().contains("must be absolute"));
    }

    #[test]
    fn profile_validation_rejects_empty_startup_program() {
        let mut profile = test_profile();
        profile.startup_apps.push(ProfileStartupApp {
            name: Some("bad".to_string()),
            command: vec!["".to_string()],
            cwd: None,
            env: Vec::new(),
        });

        let error = validate_profile(&profile).expect_err("empty command program should fail");
        assert!(error
            .to_string()
            .contains("command program cannot be empty"));
    }

    #[test]
    fn profile_validate_file_returns_its_check_without_saving() {
        let path = env::temp_dir().join(format!(
            "agent-workspace-profile-validate-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{
  "id": "validate-smoke",
  "network": {"mode": "disabled"},
  "require_enforced_policy": true,
  "startup_apps": [{"name": "noop", "command": ["/bin/true"]}]
}"#,
        )
        .expect("profile json");

        let validation = validate_profile_json_file(path.clone()).expect("valid profile");

        assert!(validation.ok);
        assert_eq!(validation.json_path, path);
        assert_eq!(validation.profile.id, "validate-smoke");
        assert_eq!(validation.check.profile.id, "validate-smoke");
        assert!(validation.check.requires_hidden_workspace_ack);
        let _ = fs::remove_file(validation.json_path);
    }

    #[test]
    fn browser_session_template_requires_user_data_dir() {
        let error = template_profile(
            "browser-session",
            Some("session-smoke".to_string()),
            None,
            Some(PathBuf::from("/usr/bin/google-chrome")),
            None,
        )
        .expect_err("browser session template should require user-data dir");

        assert!(error.to_string().contains("--user-data-dir"));
    }

    #[test]
    fn templates_start_apps_on_the_users_own_paths() {
        let project = template_profile(
            "project-dev",
            None,
            Some(PathBuf::from("/home/me/src/app")),
            None,
            None,
        )
        .expect("project template");
        assert_eq!(project.cwd, Some(PathBuf::from("/home/me/src/app")));

        let chrome = template_profile("chrome", Some("c".to_string()), None, None, None)
            .expect("chrome template");
        let command = &chrome.startup_apps[0].command;
        assert!(command.iter().any(|arg| arg == "--remote-debugging-port=0"));
        assert!(!command.iter().any(|arg| arg == "--no-sandbox"));

        let session = template_profile(
            "browser-session",
            None,
            None,
            Some(PathBuf::from("/usr/bin/google-chrome")),
            Some(PathBuf::from("/home/me/.config/google-chrome")),
        )
        .expect("browser session template");
        let command = &session.startup_apps[0].command;
        assert_eq!(command[0], "/usr/bin/google-chrome");
        assert!(command
            .iter()
            .any(|arg| arg == "--user-data-dir=/home/me/.config/google-chrome"));
    }
}
