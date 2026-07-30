use crate::policy::ProfileMount;
use crate::workspace::{self, WorkspaceApp, WorkspaceStatus};
use anyhow::{anyhow, bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_BROWSER_OPEN_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_BROWSER_WINDOW_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_BROWSER_URL: &str = "about:blank";

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserTargets {
    pub ok: bool,
    pub message: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_user_data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_user_data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_active_port_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<BrowserTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browser_target_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserOpen {
    pub ok: bool,
    pub message: String,
    pub id: String,
    pub url: String,
    pub browser_path: PathBuf,
    pub user_data_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<WorkspaceApp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<workspace::WorkspaceWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<WorkspaceBrowserTargets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl WorkspaceBrowserOpen {
    pub fn error(id: String, url: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        Self {
            ok: false,
            message: message.clone(),
            id,
            url,
            browser_path: PathBuf::new(),
            user_data_dir: PathBuf::new(),
            app: None,
            app_id: None,
            app_pid: None,
            windows: Vec::new(),
            targets: None,
            agent_mode: None,
            target_handles: None,
            recovery_hints: browser_recovery_hints(&message),
            warnings: Vec::new(),
        }
    }
}

impl WorkspaceBrowserTargets {
    pub fn error(id: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        let recovery_hints = browser_recovery_hints(&message);
        let target_handles = Some(browser_target_handles(Some(id.clone()), None, Vec::new()));
        Self {
            ok: false,
            message,
            id,
            app_id: None,
            app_pid: None,
            workspace_user_data_dir: None,
            host_user_data_dir: None,
            devtools_active_port_path: None,
            devtools_endpoint: None,
            targets: Vec::new(),
            browser_target_ids: Vec::new(),
            agent_mode: None,
            target_handles,
            recovery_hints,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserSnapshot {
    pub ok: bool,
    pub message: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BrowserTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<BrowserPageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl WorkspaceBrowserSnapshot {
    pub fn error(id: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        let recovery_hints = browser_recovery_hints(&message);
        let target_handles = Some(browser_target_handles(Some(id.clone()), None, Vec::new()));
        Self {
            ok: false,
            message,
            id,
            app_id: None,
            app_pid: None,
            devtools_endpoint: None,
            target: None,
            browser_target_id: None,
            page: None,
            agent_mode: None,
            target_handles,
            recovery_hints,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserSearchResults {
    pub ok: bool,
    pub message: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BrowserTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<BrowserSearchResultsPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl WorkspaceBrowserSearchResults {
    pub fn error(id: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        let recovery_hints = browser_recovery_hints(&message);
        let target_handles = Some(browser_target_handles(Some(id.clone()), None, Vec::new()));
        Self {
            ok: false,
            message,
            id,
            app_id: None,
            app_pid: None,
            devtools_endpoint: None,
            target: None,
            browser_target_id: None,
            page: None,
            agent_mode: None,
            target_handles,
            recovery_hints,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserNavigate {
    pub ok: bool,
    pub message: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BrowserTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_target_id: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<BrowserNavigationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<BrowserPageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserClickMatch {
    Selector,
    Text,
    Viewport,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserClickResult {
    pub match_kind: BrowserClickMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport_y: Option<i32>,
    #[serde(default)]
    pub tag_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub clicked_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<BrowserElementRect>,
    #[serde(default)]
    pub viewport_width: u32,
    #[serde(default)]
    pub viewport_height: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserElementRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceBrowserClick {
    pub ok: bool,
    pub message: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<BrowserTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click: Option<BrowserClickResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<BrowserPageSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl WorkspaceBrowserClick {
    pub fn error(id: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        let recovery_hints = browser_recovery_hints(&message);
        let target_handles = Some(browser_target_handles(Some(id.clone()), None, Vec::new()));
        Self {
            ok: false,
            message,
            id,
            app_id: None,
            app_pid: None,
            devtools_endpoint: None,
            target: None,
            browser_target_id: None,
            click: None,
            page: None,
            agent_mode: None,
            target_handles,
            recovery_hints,
            warnings: Vec::new(),
        }
    }
}

impl WorkspaceBrowserNavigate {
    pub fn error(id: String, url: String, error: anyhow::Error) -> Self {
        let message = error.to_string();
        let recovery_hints = browser_recovery_hints(&message);
        let target_handles = Some(browser_target_handles(Some(id.clone()), None, Vec::new()));
        Self {
            ok: false,
            message,
            id,
            app_id: None,
            app_pid: None,
            devtools_endpoint: None,
            target: None,
            browser_target_id: None,
            url,
            navigation: None,
            page: None,
            agent_mode: None,
            target_handles,
            recovery_hints,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BrowserTarget {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(
        default,
        rename = "webSocketDebuggerUrl",
        skip_serializing_if = "Option::is_none"
    )]
    pub web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserPageSnapshot {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub text_chars: usize,
    #[serde(default)]
    pub text_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<BrowserPageLink>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserPageLink {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub href: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserSearchResultsPage {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub result_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<BrowserSearchResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserSearchResult {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_count: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seller_or_brand: Option<String>,
    #[serde(default)]
    pub text_excerpt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserNavigationResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
}

fn browser_target_handles(
    workspace_id: Option<String>,
    app_id: Option<String>,
    browser_target_ids: Vec<String>,
) -> crate::agent::AgentTargetHandles {
    let mut handles = crate::agent::AgentTargetHandles {
        workspace_id,
        ..Default::default()
    };
    if let Some(app_id) = app_id {
        handles.app_ids.push(app_id);
    }
    handles.browser_target_ids = browser_target_ids;
    handles
}

fn browser_recovery_hints(message: &str) -> Vec<String> {
    let mut hints = vec![
        "Call workspace_list_apps with running=true to choose the browser app_id.".to_string(),
        "Call workspace_browser_targets with that app_id to discover browser_target_id values."
            .to_string(),
    ];
    let lower = message.to_ascii_lowercase();
    if lower.contains("devtools")
        || lower.contains("remote-debugging")
        || lower.contains("user-data-dir")
    {
        hints.push(
            "Call workspace_open_browser to launch Chrome/Chromium with --user-data-dir and loopback DevTools flags."
                .to_string(),
        );
    }
    if lower.contains("target") || lower.contains("tab") || lower.contains("page") {
        hints.push(
            "Pass target_id, title_contains, or url_contains from workspace_browser_targets when multiple pages are open."
                .to_string(),
        );
    }
    hints.push(
        "Use workspace_observe or workspace_screenshot_window for visual fallback if DevTools is unavailable."
            .to_string(),
    );
    hints
}

#[derive(Debug, Clone)]
struct BrowserAppSelection {
    app: WorkspaceApp,
    workspace_user_data_dir: Option<PathBuf>,
    host_user_data_dir: Option<PathBuf>,
    remote_debugging_port: Option<u16>,
    remote_debugging_address: Option<String>,
}

#[derive(Debug, Clone)]
struct DevToolsActivePort {
    port: u16,
    browser_path: Option<String>,
}

#[derive(Debug, Clone)]
struct SelectedBrowserTarget {
    targets: WorkspaceBrowserTargets,
    target: BrowserTarget,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceBrowserOpenPlan {
    pub spec: workspace::LaunchSpec,
    pub browser_path: PathBuf,
    pub user_data_dir: PathBuf,
    pub url: String,
}

pub fn workspace_browser_open_plan(
    id: &str,
    browser_path: Option<PathBuf>,
    user_data_dir: Option<PathBuf>,
    url: Option<String>,
) -> Result<WorkspaceBrowserOpenPlan> {
    let status = workspace::status_workspace(id)?;
    let browser_path = browser_path
        .or_else(|| env::var_os("BROWSER_BIN").map(PathBuf::from))
        .or_else(find_browser_executable)
        .context("Chrome/Chromium not found; pass browser_path or set BROWSER_BIN")?;
    let user_data_dir = user_data_dir.unwrap_or_else(|| status.runtime_dir.join("browser-profile"));
    let url = url.unwrap_or_else(|| DEFAULT_BROWSER_URL.to_string());
    validate_navigation_url(&url)?;
    let command = workspace_browser_command(&browser_path, &user_data_dir, &url);
    Ok(WorkspaceBrowserOpenPlan {
        spec: workspace::LaunchSpec {
            command,
            name: Some("workspace-browser".to_string()),
            profile_id: None,
            applied_policy: None,
            user_acknowledged_unenforced_policy: false,
            cwd: None,
            env: Vec::new(),
        },
        browser_path,
        user_data_dir,
        url,
    })
}

pub fn workspace_open_browser(
    id: &str,
    plan: WorkspaceBrowserOpenPlan,
    wait_window: bool,
    window_timeout_ms: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserOpen> {
    fs::create_dir_all(&plan.user_data_dir).with_context(|| {
        format!(
            "failed to create browser user-data-dir {}",
            plan.user_data_dir.display()
        )
    })?;
    let launch = workspace::launch_app_with_options(
        id,
        plan.spec,
        wait_window,
        window_timeout_ms.or(Some(DEFAULT_BROWSER_WINDOW_TIMEOUT_MS)),
        false,
    )?;
    if !launch.ok {
        bail!(launch.message);
    }
    let app = launch
        .apps
        .as_ref()
        .and_then(|apps| apps.first())
        .cloned()
        .context("workspace browser launch did not return an app")?;
    let targets = workspace_browser_targets(
        id,
        Some(app.id.clone()),
        Some(plan.user_data_dir.clone()),
        Some(timeout_ms.unwrap_or(DEFAULT_BROWSER_OPEN_TIMEOUT_MS)),
    )?;
    let target_handles = Some(browser_target_handles(
        Some(id.to_string()),
        Some(app.id.clone()),
        targets.browser_target_ids.clone(),
    ));
    let mut warnings = Vec::new();
    warnings.extend(targets.warnings.clone());
    Ok(WorkspaceBrowserOpen {
        ok: true,
        message: "workspace browser opened with loopback Chrome DevTools".to_string(),
        id: id.to_string(),
        url: plan.url,
        browser_path: plan.browser_path,
        user_data_dir: plan.user_data_dir,
        app_id: Some(app.id.clone()),
        app_pid: Some(app.pid),
        app: Some(app),
        windows: launch.windows.unwrap_or_default(),
        targets: Some(targets),
        agent_mode: None,
        target_handles,
        recovery_hints: Vec::new(),
        warnings,
    })
}

pub fn workspace_browser_targets(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserTargets> {
    match workspace::browser_targets(id, app_id.clone(), user_data_dir.clone(), timeout_ms) {
        Ok(response) => browser_targets_from_ipc_response(response),
        Err(error) if should_use_direct_browser_compat(&error) => {
            let status = workspace::status_workspace(id)?;
            let mut response =
                workspace_browser_targets_from_status(&status, app_id, user_data_dir, timeout_ms)?;
            response
                .warnings
                .push(browser_ipc_compat_warning("browser_targets", &error));
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn workspace_browser_targets_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserTargets> {
    let selection = select_browser_app(status, app_id.as_deref(), user_data_dir.as_deref())?;
    let app = selection.app.clone();
    let mut warnings = Vec::new();

    if let Some(address) = selection.remote_debugging_address.as_deref() {
        if !is_loopback_devtools_address(address) {
            bail!(
                "workspace browser app {} exposes DevTools on non-loopback address {address:?}; refusing to attach",
                app.id
            );
        }
    } else {
        warnings.push(
            "remote debugging address was not explicit; treating Chrome's endpoint as loopback-only because it was derived from the workspace app user-data directory"
                .to_string(),
        );
    }

    let host_user_data_dir = selection.host_user_data_dir.clone().ok_or_else(|| {
        anyhow!(
            "workspace browser app {} does not expose a resolvable --user-data-dir; relaunch Chrome with --user-data-dir and --remote-debugging-port=0",
            app.id
        )
    })?;
    let active_port_path = host_user_data_dir.join("DevToolsActivePort");
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(0).min(30_000));
    let (endpoint, targets, mut endpoint_warnings) =
        read_targets_with_optional_wait(&selection, &active_port_path, timeout)?;
    warnings.append(&mut endpoint_warnings);
    let browser_target_ids = targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    let app_id = app.id.clone();
    let target_handles = Some(browser_target_handles(
        Some(status.id.clone()),
        Some(app_id.clone()),
        browser_target_ids.clone(),
    ));

    Ok(WorkspaceBrowserTargets {
        ok: true,
        message: format!(
            "workspace browser targets returned for app {} through workspace-owned Chrome DevTools",
            app_id
        ),
        id: status.id.clone(),
        app_id: Some(app_id),
        app_pid: Some(app.pid),
        workspace_user_data_dir: selection.workspace_user_data_dir,
        host_user_data_dir: Some(host_user_data_dir),
        devtools_active_port_path: Some(active_port_path),
        devtools_endpoint: Some(endpoint),
        targets,
        browser_target_ids,
        agent_mode: None,
        target_handles,
        recovery_hints: Vec::new(),
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn workspace_browser_snapshot(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserSnapshot> {
    match workspace::browser_snapshot(
        id,
        app_id.clone(),
        user_data_dir.clone(),
        target_id.clone(),
        title_contains.clone(),
        url_contains.clone(),
        max_text_chars,
        timeout_ms,
    ) {
        Ok(response) => browser_snapshot_from_ipc_response(response),
        Err(error) if should_use_direct_browser_compat(&error) => {
            let status = workspace::status_workspace(id)?;
            let mut response = workspace_browser_snapshot_from_status(
                &status,
                app_id,
                user_data_dir,
                target_id,
                title_contains,
                url_contains,
                max_text_chars,
                timeout_ms,
            )?;
            response
                .warnings
                .push(browser_ipc_compat_warning("browser_snapshot", &error));
            if let Some(warning) = record_browser_event(
                &response.id,
                "browser_snapshot",
                browser_snapshot_event_detail(&response),
            ) {
                response.warnings.push(warning);
            }
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn workspace_browser_snapshot_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserSnapshot> {
    let selected = select_browser_target_from_status(
        status,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        timeout_ms,
    )?;
    let page = read_page_snapshot(
        &selected.target,
        max_text_chars.unwrap_or(12_000).min(200_000),
        cdp_timeout(timeout_ms),
    )?;
    let mut warnings = selected.targets.warnings.clone();
    warnings.extend(selected.warnings.clone());
    let response = WorkspaceBrowserSnapshot {
        ok: true,
        message: "workspace browser page snapshot captured through workspace-owned Chrome DevTools"
            .to_string(),
        id: selected.targets.id.clone(),
        app_id: selected.targets.app_id.clone(),
        app_pid: selected.targets.app_pid,
        devtools_endpoint: selected.targets.devtools_endpoint.clone(),
        target: Some(selected.target.clone()),
        browser_target_id: Some(selected.target.id.clone()),
        page: Some(page),
        agent_mode: None,
        target_handles: Some(browser_target_handles(
            Some(selected.targets.id.clone()),
            selected.targets.app_id.clone(),
            vec![selected.target.id.clone()],
        )),
        recovery_hints: Vec::new(),
        warnings,
    };
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
pub fn workspace_browser_search_results(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_results: Option<usize>,
    min_vram_gb: Option<u32>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserSearchResults> {
    match workspace::browser_search_results(
        id,
        app_id.clone(),
        user_data_dir.clone(),
        target_id.clone(),
        title_contains.clone(),
        url_contains.clone(),
        max_results,
        min_vram_gb,
        timeout_ms,
    ) {
        Ok(response) => browser_search_results_from_ipc_response(response),
        Err(error) if should_use_direct_browser_compat(&error) => {
            let status = workspace::status_workspace(id)?;
            let mut response = workspace_browser_search_results_from_status(
                &status,
                app_id,
                user_data_dir,
                target_id,
                title_contains,
                url_contains,
                max_results,
                min_vram_gb,
                timeout_ms,
            )?;
            response
                .warnings
                .push(browser_ipc_compat_warning("browser_search_results", &error));
            if let Some(warning) = record_browser_event(
                &response.id,
                "browser_search_results",
                browser_search_results_event_detail(&response, min_vram_gb),
            ) {
                response.warnings.push(warning);
            }
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn workspace_browser_search_results_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_results: Option<usize>,
    min_vram_gb: Option<u32>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserSearchResults> {
    let selected = select_browser_target_from_status(
        status,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        timeout_ms,
    )?;
    let page = read_search_results(
        &selected.target,
        max_results.unwrap_or(20).clamp(1, 100),
        min_vram_gb,
        cdp_timeout(timeout_ms),
    )?;
    let mut warnings = selected.targets.warnings.clone();
    warnings.extend(selected.warnings.clone());
    if page.results.is_empty() {
        warnings.push(
            "no structured product/search-result cards were found; fall back to workspace_browser_snapshot or visual observation"
                .to_string(),
        );
    }
    let response = WorkspaceBrowserSearchResults {
        ok: true,
        message:
            "workspace browser search results extracted through workspace-owned Chrome DevTools"
                .to_string(),
        id: selected.targets.id.clone(),
        app_id: selected.targets.app_id.clone(),
        app_pid: selected.targets.app_pid,
        devtools_endpoint: selected.targets.devtools_endpoint.clone(),
        target: Some(selected.target.clone()),
        browser_target_id: Some(selected.target.id.clone()),
        page: Some(page),
        agent_mode: None,
        target_handles: Some(browser_target_handles(
            Some(selected.targets.id.clone()),
            selected.targets.app_id.clone(),
            vec![selected.target.id.clone()],
        )),
        recovery_hints: Vec::new(),
        warnings,
    };
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
pub fn workspace_browser_navigate(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    url: String,
    wait_ms: Option<u64>,
    snapshot: bool,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserNavigate> {
    validate_navigation_url(&url)?;
    match workspace::browser_navigate(
        id,
        app_id.clone(),
        user_data_dir.clone(),
        target_id.clone(),
        title_contains.clone(),
        url_contains.clone(),
        url.clone(),
        wait_ms,
        snapshot,
        max_text_chars,
        timeout_ms,
    ) {
        Ok(response) => browser_navigate_from_ipc_response(response),
        Err(error) if should_use_direct_browser_compat(&error) => {
            let status = workspace::status_workspace(id)?;
            let mut response = workspace_browser_navigate_from_status(
                &status,
                app_id,
                user_data_dir,
                target_id,
                title_contains,
                url_contains,
                url,
                wait_ms,
                snapshot,
                max_text_chars,
                timeout_ms,
            )?;
            response
                .warnings
                .push(browser_ipc_compat_warning("browser_navigate", &error));
            if let Some(warning) = record_browser_event(
                &response.id,
                "browser_navigate",
                browser_navigate_event_detail(&response),
            ) {
                response.warnings.push(warning);
            }
            Ok(response)
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn workspace_browser_navigate_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    url: String,
    wait_ms: Option<u64>,
    snapshot: bool,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserNavigate> {
    validate_navigation_url(&url)?;
    let selected = select_browser_target_from_status(
        status,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        timeout_ms,
    )?;
    let timeout = cdp_timeout(timeout_ms);
    let navigation = navigate_target(&selected.target, &url, timeout)?;
    std::thread::sleep(Duration::from_millis(wait_ms.unwrap_or(750).min(30_000)));
    // The target metadata was captured before navigating, so its title/url still
    // describe the previous page. Refresh it so `target` and `page` agree.
    let mut navigated_target = selected.target.clone();
    let mut warnings = selected.targets.warnings.clone();
    warnings.extend(selected.warnings.clone());
    match refresh_target_document(&selected.target, timeout) {
        Ok((title, current_url)) => {
            navigated_target.title = title;
            navigated_target.url = current_url;
        }
        Err(error) => {
            warnings.push(format!(
                "could not refresh browser target metadata after navigation; target.title/target.url may describe the previous page: {error}"
            ));
        }
    }
    let page = if snapshot {
        Some(read_page_snapshot(
            &selected.target,
            max_text_chars.unwrap_or(12_000).min(200_000),
            timeout,
        )?)
    } else {
        None
    };
    let response = WorkspaceBrowserNavigate {
        ok: true,
        message: "workspace browser navigated through workspace-owned Chrome DevTools".to_string(),
        id: selected.targets.id.clone(),
        app_id: selected.targets.app_id.clone(),
        app_pid: selected.targets.app_pid,
        devtools_endpoint: selected.targets.devtools_endpoint.clone(),
        browser_target_id: Some(navigated_target.id.clone()),
        target: Some(navigated_target),
        url,
        navigation: Some(navigation),
        page,
        agent_mode: None,
        target_handles: Some(browser_target_handles(
            Some(selected.targets.id.clone()),
            selected.targets.app_id.clone(),
            vec![selected.target.id.clone()],
        )),
        recovery_hints: Vec::new(),
        warnings,
    };
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
pub fn workspace_browser_click(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    selector: Option<String>,
    text: Option<String>,
    viewport_x: Option<i32>,
    viewport_y: Option<i32>,
    wait_ms: Option<u64>,
    snapshot: bool,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserClick> {
    let status = workspace::status_workspace(id)?;
    let mut response = workspace_browser_click_from_status(
        &status,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        selector,
        text,
        viewport_x,
        viewport_y,
        wait_ms,
        snapshot,
        max_text_chars,
        timeout_ms,
    )?;
    if let Some(warning) = record_browser_event(
        &response.id,
        "browser_click",
        browser_click_event_detail(&response),
    ) {
        response.warnings.push(warning);
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn workspace_browser_click_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    selector: Option<String>,
    text: Option<String>,
    viewport_x: Option<i32>,
    viewport_y: Option<i32>,
    wait_ms: Option<u64>,
    snapshot: bool,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<WorkspaceBrowserClick> {
    validate_browser_click_request(selector.as_deref(), text.as_deref(), viewport_x, viewport_y)?;
    let selected = select_browser_target_from_status(
        status,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        timeout_ms,
    )?;
    let timeout = cdp_timeout(timeout_ms);
    let click = click_target(
        &selected.target,
        selector.as_deref(),
        text.as_deref(),
        viewport_x,
        viewport_y,
        timeout,
    )?;
    std::thread::sleep(Duration::from_millis(wait_ms.unwrap_or(250).min(30_000)));
    // A click can navigate (links, SPA routing), so the pre-click target
    // metadata may now describe the previous page. Refresh it.
    let mut clicked_target = selected.target.clone();
    let mut warnings = selected.targets.warnings.clone();
    warnings.extend(selected.warnings.clone());
    match refresh_target_document(&selected.target, timeout) {
        Ok((title, current_url)) => {
            clicked_target.title = title;
            clicked_target.url = current_url;
        }
        Err(error) => {
            warnings.push(format!(
                "could not refresh browser target metadata after click; target.title/target.url may describe the previous page: {error}"
            ));
        }
    }
    let page = if snapshot {
        Some(read_page_snapshot(
            &selected.target,
            max_text_chars.unwrap_or(12_000).min(200_000),
            timeout,
        )?)
    } else {
        None
    };
    Ok(WorkspaceBrowserClick {
        ok: true,
        message: "workspace browser clicked through workspace-owned Chrome DevTools".to_string(),
        id: selected.targets.id.clone(),
        app_id: selected.targets.app_id.clone(),
        app_pid: selected.targets.app_pid,
        devtools_endpoint: selected.targets.devtools_endpoint.clone(),
        browser_target_id: Some(clicked_target.id.clone()),
        target: Some(clicked_target),
        click: Some(click),
        page,
        agent_mode: None,
        target_handles: Some(browser_target_handles(
            Some(selected.targets.id.clone()),
            selected.targets.app_id.clone(),
            vec![selected.target.id.clone()],
        )),
        recovery_hints: Vec::new(),
        warnings,
    })
}

fn select_browser_target_from_status(
    status: &WorkspaceStatus,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<SelectedBrowserTarget> {
    let targets = workspace_browser_targets_from_status(status, app_id, user_data_dir, timeout_ms)?;
    let (target, warnings) = select_page_target(
        &targets.targets,
        target_id.as_deref(),
        title_contains.as_deref(),
        url_contains.as_deref(),
    )?;
    Ok(SelectedBrowserTarget {
        targets,
        target,
        warnings,
    })
}

fn browser_targets_from_ipc_response(
    response: workspace::IpcResponse,
) -> Result<WorkspaceBrowserTargets> {
    if !response.ok {
        bail!(response.message);
    }
    response
        .browser_targets
        .ok_or_else(|| anyhow!("workspace daemon returned no browser targets payload"))
}

fn browser_snapshot_from_ipc_response(
    response: workspace::IpcResponse,
) -> Result<WorkspaceBrowserSnapshot> {
    if !response.ok {
        bail!(response.message);
    }
    response
        .browser_snapshot
        .ok_or_else(|| anyhow!("workspace daemon returned no browser snapshot payload"))
}

fn browser_search_results_from_ipc_response(
    response: workspace::IpcResponse,
) -> Result<WorkspaceBrowserSearchResults> {
    if !response.ok {
        bail!(response.message);
    }
    response
        .browser_search_results
        .ok_or_else(|| anyhow!("workspace daemon returned no browser search results payload"))
}

fn browser_navigate_from_ipc_response(
    response: workspace::IpcResponse,
) -> Result<WorkspaceBrowserNavigate> {
    if !response.ok {
        bail!(response.message);
    }
    response
        .browser_navigate
        .ok_or_else(|| anyhow!("workspace daemon returned no browser navigation payload"))
}

pub(crate) fn browser_snapshot_event_detail(
    response: &WorkspaceBrowserSnapshot,
) -> serde_json::Value {
    json!({
        "app_id": response.app_id.as_deref(),
        "target_id": response.target.as_ref().map(|target| target.id.as_str()),
        "title": response.page.as_ref().map(|page| page.title.as_str()),
        "url": response.page.as_ref().map(|page| page.url.as_str()),
        "text_chars": response.page.as_ref().map(|page| page.text_chars),
        "text_truncated": response.page.as_ref().map(|page| page.text_truncated),
        "raw_text_omitted": response.page.is_some(),
    })
}

pub(crate) fn browser_search_results_event_detail(
    response: &WorkspaceBrowserSearchResults,
    min_vram_gb: Option<u32>,
) -> serde_json::Value {
    json!({
        "app_id": response.app_id.as_deref(),
        "target_id": response.target.as_ref().map(|target| target.id.as_str()),
        "title": response.page.as_ref().map(|page| page.title.as_str()),
        "url": response.page.as_ref().map(|page| page.url.as_str()),
        "result_count": response.page.as_ref().map(|page| page.result_count),
        "min_vram_gb": min_vram_gb,
        "raw_result_text_omitted": response.page.is_some(),
    })
}

pub(crate) fn browser_navigate_event_detail(
    response: &WorkspaceBrowserNavigate,
) -> serde_json::Value {
    json!({
        "app_id": response.app_id.as_deref(),
        "target_id": response.target.as_ref().map(|target| target.id.as_str()),
        "url": &response.url,
        "frame_id": response.navigation.as_ref().and_then(|navigation| navigation.frame_id.as_deref()),
        "snapshot": response.page.is_some(),
        "title": response.page.as_ref().map(|page| page.title.as_str()),
        "current_url": response.page.as_ref().map(|page| page.url.as_str()),
        "raw_text_omitted": response.page.is_some(),
    })
}

pub(crate) fn browser_click_event_detail(response: &WorkspaceBrowserClick) -> serde_json::Value {
    json!({
        "app_id": response.app_id.as_deref(),
        "target_id": response.target.as_ref().map(|target| target.id.as_str()),
        "match_kind": response.click.as_ref().map(|click| match click.match_kind {
            BrowserClickMatch::Selector => "selector",
            BrowserClickMatch::Text => "text",
            BrowserClickMatch::Viewport => "viewport",
        }),
        "selector": response.click.as_ref().and_then(|click| click.selector.as_deref()),
        "text_query": response.click.as_ref().and_then(|click| click.text.as_deref()),
        "viewport_x": response.click.as_ref().and_then(|click| click.viewport_x),
        "viewport_y": response.click.as_ref().and_then(|click| click.viewport_y),
        "tag_name": response.click.as_ref().map(|click| click.tag_name.as_str()),
        "role": response.click.as_ref().and_then(|click| click.role.as_deref()),
        "href_present": response.click.as_ref().and_then(|click| click.href.as_ref()).is_some(),
        "snapshot": response.page.is_some(),
        "title": response.page.as_ref().map(|page| page.title.as_str()),
        "current_url": response.page.as_ref().map(|page| page.url.as_str()),
        "raw_text_omitted": response.page.is_some(),
    })
}

fn should_use_direct_browser_compat(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("failed to parse workspace IPC response")
}

fn browser_ipc_compat_warning(method: &str, error: &anyhow::Error) -> String {
    format!(
        "workspace daemon did not handle {method} over IPC ({error}); used the compatibility browser path for this already-running workspace"
    )
}

fn read_targets_with_optional_wait(
    selection: &BrowserAppSelection,
    active_port_path: &Path,
    timeout: Duration,
) -> Result<(String, Vec<BrowserTarget>, Vec<String>)> {
    let started = Instant::now();
    loop {
        match read_targets_once(selection, active_port_path) {
            Ok(result) => return Ok(result),
            Err(error) => {
                if started.elapsed() >= timeout {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn read_targets_once(
    selection: &BrowserAppSelection,
    active_port_path: &Path,
) -> Result<(String, Vec<BrowserTarget>, Vec<String>)> {
    let mut warnings = Vec::new();
    let active_port = read_devtools_active_port(active_port_path).ok();
    let port = if let Some(active_port) = &active_port {
        if active_port.browser_path.is_none() {
            warnings
                .push("DevToolsActivePort did not include the browser WebSocket path".to_string());
        }
        active_port.port
    } else if let Some(port) = selection.remote_debugging_port.filter(|port| *port > 0) {
        warnings.push(format!(
            "DevToolsActivePort was not readable at {}; using explicit --remote-debugging-port={port}",
            active_port_path.display()
        ));
        port
    } else {
        bail!(
            "DevToolsActivePort was not readable at {} and the browser app did not use a fixed --remote-debugging-port",
            active_port_path.display()
        );
    };
    let endpoint = format!("http://127.0.0.1:{port}");
    let targets = read_devtools_targets(port)?;
    Ok((endpoint, targets, warnings))
}

fn select_page_target(
    targets: &[BrowserTarget],
    target_id: Option<&str>,
    title_contains: Option<&str>,
    url_contains: Option<&str>,
) -> Result<(BrowserTarget, Vec<String>)> {
    let title_filter = title_contains.map(str::to_ascii_lowercase);
    let url_filter = url_contains.map(str::to_ascii_lowercase);
    let mut matches = Vec::new();
    for target in targets.iter().filter(|target| target.target_type == "page") {
        if target_id.is_some_and(|id| target.id != id) {
            continue;
        }
        if let Some(filter) = &title_filter {
            if !target.title.to_ascii_lowercase().contains(filter) {
                continue;
            }
        }
        if let Some(filter) = &url_filter {
            if !target.url.to_ascii_lowercase().contains(filter) {
                continue;
            }
        }
        if target.web_socket_debugger_url.is_none() {
            continue;
        }
        matches.push(target.clone());
    }
    if matches.is_empty() {
        if let Some(target_id) = target_id {
            bail!("no workspace browser page target matched target_id {target_id:?}");
        }
        bail!("no workspace browser page target exposed a loopback DevTools WebSocket");
    }
    let mut warnings = Vec::new();
    if matches.len() > 1
        && target_id.is_none()
        && title_contains.is_none()
        && url_contains.is_none()
    {
        warnings.push(format!(
            "multiple page targets were available; selected first target {}. Pass target_id, title_contains, or url_contains to disambiguate. Candidates: {}",
            matches[0].id,
            page_target_candidates_summary(&matches, 5)
        ));
    }
    Ok((matches.remove(0), warnings))
}

fn page_target_candidates_summary(targets: &[BrowserTarget], limit: usize) -> String {
    let mut parts: Vec<String> = targets
        .iter()
        .take(limit)
        .map(|target| {
            let title = if target.title.trim().is_empty() {
                "<untitled>"
            } else {
                target.title.trim()
            };
            let url = if target.url.trim().is_empty() {
                "<no url>"
            } else {
                target.url.trim()
            };
            format!("{} title={title:?} url={url:?}", target.id)
        })
        .collect();
    if targets.len() > limit {
        parts.push(format!("... {} more", targets.len() - limit));
    }
    parts.join("; ")
}

fn cdp_timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(5_000).clamp(100, 30_000))
}

/// Re-read `title`/`url` for a target after an action that changes the page.
///
/// `/json/list` metadata is captured when the target is *selected*, which is
/// before navigation happens. Returning that stale copy makes
/// `target.url`/`target.title` describe the previous page while `page.*`
/// describes the new one — callers that assert on `target` silently validate
/// the wrong document. Cheap `Runtime.evaluate`, independent of `snapshot`.
fn refresh_target_document(target: &BrowserTarget, timeout: Duration) -> Result<(String, String)> {
    let value = cdp_request(
        target,
        "Runtime.evaluate",
        json!({
            "expression": "JSON.stringify({ title: document.title, url: location.href })",
            "returnByValue": true,
            "awaitPromise": true,
        }),
        timeout,
    )?;
    if let Some(exception) = value.get("exceptionDetails") {
        bail!("browser target refresh Runtime.evaluate failed: {exception}");
    }
    let raw = value
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(serde_json::Value::as_str)
        .context("browser target refresh Runtime.evaluate did not return a JSON value")?;
    let parsed: serde_json::Value =
        serde_json::from_str(raw).context("browser target refresh returned invalid JSON")?;
    let title = parsed
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = parsed
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((title, url))
}

fn read_page_snapshot(
    target: &BrowserTarget,
    max_text_chars: usize,
    timeout: Duration,
) -> Result<BrowserPageSnapshot> {
    let value = cdp_request(
        target,
        "Runtime.evaluate",
        json!({
            "expression": snapshot_expression(max_text_chars),
            "returnByValue": true,
            "awaitPromise": true,
        }),
        timeout,
    )?;
    if let Some(exception) = value.get("exceptionDetails") {
        bail!("browser snapshot Runtime.evaluate failed: {exception}");
    }
    let snapshot_value = value
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .context("browser snapshot Runtime.evaluate did not return a JSON value")?;
    serde_json::from_value(snapshot_value).context("failed to parse browser page snapshot")
}

fn read_search_results(
    target: &BrowserTarget,
    max_results: usize,
    min_vram_gb: Option<u32>,
    timeout: Duration,
) -> Result<BrowserSearchResultsPage> {
    let value = cdp_request(
        target,
        "Runtime.evaluate",
        json!({
            "expression": search_results_expression(max_results, min_vram_gb),
            "returnByValue": true,
            "awaitPromise": true,
        }),
        timeout,
    )?;
    if let Some(exception) = value.get("exceptionDetails") {
        bail!("browser search results Runtime.evaluate failed: {exception}");
    }
    let results_value = value
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .context("browser search results Runtime.evaluate did not return a JSON value")?;
    serde_json::from_value(results_value).context("failed to parse browser search results")
}

fn navigate_target(
    target: &BrowserTarget,
    url: &str,
    timeout: Duration,
) -> Result<BrowserNavigationResult> {
    let _ = cdp_request(target, "Page.enable", json!({}), timeout)?;
    let result = cdp_request(target, "Page.navigate", json!({ "url": url }), timeout)?;
    if let Some(error_text) = result.get("errorText").and_then(serde_json::Value::as_str) {
        bail!("browser navigation failed: {error_text}");
    }
    Ok(BrowserNavigationResult {
        frame_id: result
            .get("frameId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        loader_id: result
            .get("loaderId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn click_target(
    target: &BrowserTarget,
    selector: Option<&str>,
    text: Option<&str>,
    viewport_x: Option<i32>,
    viewport_y: Option<i32>,
    timeout: Duration,
) -> Result<BrowserClickResult> {
    let value = cdp_request(
        target,
        "Runtime.evaluate",
        json!({
            "expression": click_expression(selector, text, viewport_x, viewport_y),
            "returnByValue": true,
            "awaitPromise": true,
        }),
        timeout,
    )?;
    if let Some(exception) = value.get("exceptionDetails") {
        bail!("browser click Runtime.evaluate failed: {exception}");
    }
    let click_value = value
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .context("browser click Runtime.evaluate did not return a JSON value")?;
    serde_json::from_value(click_value).context("failed to parse browser click result")
}

fn cdp_request(
    target: &BrowserTarget,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let url = target
        .web_socket_debugger_url
        .as_deref()
        .context("browser target did not expose webSocketDebuggerUrl")?;
    let mut connection = DevToolsWebSocket::connect(url, timeout)?;
    let response = connection.request(method, params, timeout)?;
    if let Some(error) = response.get("error") {
        bail!("Chrome DevTools {method} failed: {error}");
    }
    response
        .get("result")
        .cloned()
        .with_context(|| format!("Chrome DevTools {method} response did not include result"))
}

fn snapshot_expression(max_text_chars: usize) -> String {
    format!(
        r#"(() => {{
  const maxText = {max_text_chars};
  const clean = (value) => String(value || "").replace(/\s+/g, " ").trim();
  const rawText = document.body ? String(document.body.innerText || "") : "";
  return {{
    title: String(document.title || ""),
    url: String(location.href || ""),
    text: rawText.slice(0, maxText),
    text_chars: rawText.length,
    text_truncated: rawText.length > maxText,
    headings: Array.from(document.querySelectorAll("h1,h2,h3"))
      .map((element) => clean(element.innerText || element.textContent))
      .filter(Boolean)
      .slice(0, 40),
    links: Array.from(document.querySelectorAll("a[href]"))
      .map((anchor) => ({{
        text: clean(anchor.innerText || anchor.getAttribute("aria-label") || anchor.title || ""),
        href: String(anchor.href || "")
      }}))
      .filter((link) => link.text || link.href)
      .slice(0, 80)
  }};
}})()"#
    )
}

fn search_results_expression(max_results: usize, min_vram_gb: Option<u32>) -> String {
    let min_vram_gb = min_vram_gb
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"(() => {{
  const maxResults = {max_results};
  const minVramGb = {min_vram_gb};
  const clean = (value) => String(value || "").replace(/\s+/g, " ").trim();
  const linesOf = (element) => Array.from(new Set(
    String(element.innerText || "")
      .split(/\n+/)
      .map(clean)
      .filter(Boolean)
  ));
  const visible = (element) => {{
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0 && clean(element.innerText).length > 12;
  }};
  const firstText = (root, selectors) => {{
    for (const selector of selectors) {{
      const element = root.querySelector(selector);
      const text = clean(element && (element.innerText || element.textContent || element.getAttribute("aria-label")));
      if (text) return text;
    }}
    return "";
  }};
  const firstAttr = (root, selectors, attr) => {{
    for (const selector of selectors) {{
      const element = root.querySelector(selector);
      const value = element && element.getAttribute(attr);
      if (value) return value;
    }}
    return "";
  }};
  const absolutize = (href) => {{
    if (!href) return null;
    try {{ return new URL(href, location.href).href; }} catch (_) {{ return href; }}
  }};
  const findLine = (lines, patterns) => {{
    for (const line of lines) {{
      const lower = line.toLowerCase();
      if (patterns.some((pattern) => lower.includes(pattern))) return line;
    }}
    return null;
  }};
  const priceFrom = (root, lines) => {{
    const direct = firstText(root, [
      ".a-price .a-offscreen",
      "[data-testid*=price i]",
      "[class*=price i] .a-offscreen",
      "[class*=price i]"
    ]);
    if (direct && /\$\s*\d/.test(direct)) return direct;
    const joined = lines.join(" ");
    const match = joined.match(/\$\s*\d[\d,]*(?:\.\d{{2}})?/);
    return match ? match[0].replace(/\s+/g, "") : null;
  }};
  const ratingFrom = (root, lines) => {{
    const aria = firstAttr(root, [
      "[aria-label*='out of 5 stars']",
      ".a-icon-alt"
    ], "aria-label") || firstText(root, [".a-icon-alt"]);
    if (aria) return clean(aria);
    const line = lines.find((line) => /out of 5 stars/i.test(line));
    return line || null;
  }};
  const reviewsFrom = (root, lines) => {{
    const reviewLink = firstText(root, [
      "a[href*='#customerReviews']",
      "a[href*='customerReviews']",
      "[aria-label*='ratings']"
    ]);
    if (reviewLink && !/^\d{{1,2}}$/.test(reviewLink)) return reviewLink;
    const parenthesized = lines.find((line) =>
      /^\([\d,]+\)$/.test(line) || /\([\d,]+\s*(?:ratings?|reviews?)?\)/i.test(line)
    );
    if (parenthesized) return parenthesized;
    const labeled = lines.find((line) => /[\d,]+\s+(?:ratings?|reviews?)/i.test(line));
    return labeled || null;
  }};
  const vramFrom = (lines) => {{
    const joined = lines.join(" | ");
    const matches = Array.from(joined.matchAll(/\b(\d{{2,3}})\s*(?:gb|g)\b/ig))
      .map((match) => Number(match[1]))
      .filter((value) => Number.isFinite(value) && value > 0 && value <= 256);
    return matches.length ? Math.max(...matches) : null;
  }};
  const titleFrom = (root, lines) => {{
    const title = firstText(root, [
      "h2 a span",
      "h2",
      "h3",
      "[data-testid*=title i]",
      "[class*=title i]",
      "a[aria-label]"
    ]);
    if (title && title.length > 8) return title;
    return lines
      .filter((line) => line.length > 18 && !/\$\s*\d/.test(line) && !/delivery|stars|cart/i.test(line))
      .sort((a, b) => b.length - a.length)[0] || "";
  }};
  const hrefFrom = (root) => absolutize(firstAttr(root, [
    "h2 a[href]",
    "a[href*='/dp/']",
    "a[href*='/gp/product/']",
    "a[href]"
  ], "href"));
  const resultFrom = (root, index) => {{
    const lines = linesOf(root);
    const title = titleFrom(root, lines);
    const href = hrefFrom(root);
    const price = priceFrom(root, lines);
    const vramGb = vramFrom(lines);
    if (!title || (!href && !price)) return null;
    const availability = findLine(lines, [
      "in stock",
      "left in stock",
      "out of stock",
      "temporarily unavailable",
      "no featured offers",
      "see options"
    ]);
    const delivery = findLine(lines, [
      "free delivery",
      "fastest delivery",
      "delivery",
      "pickup",
      "ships"
    ]);
    const badge = findLine(lines, [
      "overall pick",
      "amazon's choice",
      "best seller",
      "top brand",
      "limited time deal",
      "new on amazon"
    ]);
    const sellerOrBrand = lines.find((line) =>
      line.length <= 40 &&
      line !== title &&
      /[A-Za-z]/.test(line) &&
      !/\$\s*\d|price|stars|rating|delivery|stock|ships|pickup|cart|sponsored|options/i.test(line) &&
      !/^\d(?:\.\d)?$/.test(line)
    ) || null;
    return {{
      index,
      title,
      href,
      price,
      vram_gb: vramGb,
      rating: ratingFrom(root, lines),
      review_count: reviewsFrom(root, lines),
      availability,
      delivery,
      badge,
      seller_or_brand: sellerOrBrand,
      text_excerpt: lines.slice(0, 14).join(" | ").slice(0, 1200)
    }};
  }};
  const selectors = [
    "[data-component-type='s-search-result']",
    "[data-testid*=product i]",
    "[data-qa*=product i]",
    "article",
    "li[role='listitem']",
    "div[role='listitem']"
  ];
  let cards = [];
  for (const selector of selectors) {{
    cards = Array.from(document.querySelectorAll(selector)).filter(visible);
    if (cards.length >= 2) break;
  }}
  if (cards.length === 0) {{
    cards = Array.from(document.querySelectorAll("h2,h3"))
      .map((heading) => heading.closest("div,li,article"))
      .filter(Boolean)
      .filter(visible);
  }}
  const seen = new Set();
  const results = [];
  for (const card of cards) {{
    if (seen.has(card)) continue;
    seen.add(card);
    const result = resultFrom(card, results.length + 1);
    if (result && minVramGb !== null && (result.vram_gb === null || result.vram_gb < minVramGb)) continue;
    if (result) results.push(result);
    if (results.length >= maxResults) break;
  }}
  return {{
    title: String(document.title || ""),
    url: String(location.href || ""),
    result_count: results.length,
    results
  }};
}})()"#
    )
}

fn click_expression(
    selector: Option<&str>,
    text: Option<&str>,
    viewport_x: Option<i32>,
    viewport_y: Option<i32>,
) -> String {
    let selector = json_literal(selector);
    let text = json_literal(text);
    let viewport_x = viewport_x
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    let viewport_y = viewport_y
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"(async () => {{
  const selector = {selector};
  const textQuery = {text};
  const viewportX = {viewport_x};
  const viewportY = {viewport_y};
  const clean = (value) => String(value || "").replace(/\s+/g, " ").trim();
  const visible = (element) => {{
    if (!element || !(element instanceof Element)) return false;
    const style = getComputedStyle(element);
    if (style.visibility === "hidden" || style.display === "none" || Number(style.opacity) === 0) return false;
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }};
  const clickableRoot = (element) =>
    element && element.closest("button,a,input,select,textarea,label,summary,[role='button'],[role='link'],[onclick],[tabindex]");
  const describe = (element, matchKind) => {{
    const rect = element.getBoundingClientRect();
    const href = element.href || (element.closest && element.closest("a[href]") && element.closest("a[href]").href) || null;
    return {{
      match_kind: matchKind,
      selector,
      text: textQuery,
      viewport_x: viewportX,
      viewport_y: viewportY,
      tag_name: String(element.tagName || "").toLowerCase(),
      role: element.getAttribute("role") || null,
      clicked_text: clean(element.innerText || element.textContent || element.value || element.getAttribute("aria-label") || element.title || ""),
      href,
      rect: {{ x: rect.x, y: rect.y, width: rect.width, height: rect.height }},
      viewport_width: window.innerWidth || 0,
      viewport_height: window.innerHeight || 0
    }};
  }};
  const clickElement = async (rawElement, matchKind, clientX = null, clientY = null) => {{
    let element = clickableRoot(rawElement) || rawElement;
    if (!element || !(element instanceof Element)) throw new Error("matched browser element is not clickable");
    if (element.disabled || element.getAttribute("aria-disabled") === "true") throw new Error("matched browser element is disabled");
    if (matchKind !== "viewport") {{
      element.scrollIntoView({{ block: "center", inline: "center" }});
      await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    }}
    const rect = element.getBoundingClientRect();
    const x = clientX === null ? rect.left + Math.max(1, rect.width / 2) : clientX;
    const y = clientY === null ? rect.top + Math.max(1, rect.height / 2) : clientY;
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
      const event = type.startsWith("pointer")
        ? new PointerEvent(type, {{ bubbles: true, cancelable: true, view: window, clientX: x, clientY: y, button: 0, pointerId: 1, pointerType: "mouse", isPrimary: true }})
        : new MouseEvent(type, {{ bubbles: true, cancelable: true, view: window, clientX: x, clientY: y, button: 0 }});
      element.dispatchEvent(event);
    }}
    return describe(element, matchKind);
  }};
  if (selector !== null) {{
    const element = document.querySelector(selector);
    if (!element) throw new Error(`no element matched selector ${{selector}}`);
    if (!visible(element)) throw new Error(`element matched selector ${{selector}} but is not visible`);
    return await clickElement(element, "selector");
  }}
  if (textQuery !== null) {{
    const needle = clean(textQuery).toLowerCase();
    if (!needle) throw new Error("text query cannot be empty");
    const preferred = Array.from(document.querySelectorAll("button,a,input,select,textarea,label,summary,[role='button'],[role='link'],[onclick],[tabindex]"));
    const fallback = Array.from(document.querySelectorAll("body *"));
    const candidates = [...preferred, ...fallback]
      .filter(visible)
      .map((element) => ({{ element, text: clean(element.innerText || element.textContent || element.value || element.getAttribute("aria-label") || element.title || "") }}))
      .filter((candidate) => candidate.text.toLowerCase().includes(needle))
      .sort((a, b) => a.text.length - b.text.length);
    if (!candidates.length) throw new Error(`no visible element contained text ${{textQuery}}`);
    return await clickElement(candidates[0].element, "text");
  }}
  if (viewportX !== null && viewportY !== null) {{
    if (viewportX < 0 || viewportY < 0 || viewportX >= window.innerWidth || viewportY >= window.innerHeight) {{
      throw new Error(`viewport coordinates ${{viewportX}},${{viewportY}} are outside the page viewport ${{window.innerWidth}}x${{window.innerHeight}}`);
    }}
    const element = document.elementFromPoint(viewportX, viewportY);
    if (!element) throw new Error(`no browser element at viewport coordinates ${{viewportX}},${{viewportY}}`);
    return await clickElement(element, "viewport", viewportX, viewportY);
  }}
  throw new Error("selector, text, or viewport_x + viewport_y is required");
}})()"#
    )
}

fn json_literal(value: Option<&str>) -> String {
    value
        .map(|value| serde_json::to_string(value).expect("string serialization cannot fail"))
        .unwrap_or_else(|| "null".to_string())
}

fn validate_navigation_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("browser navigation URL is required");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower == "about:blank"
    {
        return Ok(());
    }
    bail!("browser navigation URL must start with http://, https://, data:, or be about:blank")
}

fn validate_browser_click_request(
    selector: Option<&str>,
    text: Option<&str>,
    viewport_x: Option<i32>,
    viewport_y: Option<i32>,
) -> Result<()> {
    let selector = selector.map(str::trim).filter(|value| !value.is_empty());
    let text = text.map(str::trim).filter(|value| !value.is_empty());
    let has_viewport = viewport_x.is_some() || viewport_y.is_some();
    let count =
        usize::from(selector.is_some()) + usize::from(text.is_some()) + usize::from(has_viewport);
    if count == 0 {
        bail!("browser click requires selector, text, or viewport_x plus viewport_y");
    }
    if count > 1 {
        bail!(
            "browser click accepts only one target mode: selector, text, or viewport coordinates"
        );
    }
    if has_viewport && (viewport_x.is_none() || viewport_y.is_none()) {
        bail!("browser click viewport mode requires both viewport_x and viewport_y");
    }
    if let (Some(x), Some(y)) = (viewport_x, viewport_y) {
        if x < 0 || y < 0 {
            bail!("browser click viewport coordinates must be non-negative");
        }
    }
    Ok(())
}

fn record_browser_event(id: &str, kind: &str, detail: serde_json::Value) -> Option<String> {
    record_browser_event_with(id, kind, detail, |id, kind, detail| {
        workspace::record_workspace_event(id, kind, detail)
    })
}

fn record_browser_event_with(
    id: &str,
    kind: &str,
    detail: serde_json::Value,
    mut record: impl FnMut(&str, &str, serde_json::Value) -> Result<workspace::IpcResponse>,
) -> Option<String> {
    let result = record(id, kind, detail.clone());
    if should_retry_browser_event_as_snapshot(kind, &result) {
        let fallback = record(
            id,
            "browser_snapshot",
            compatibility_browser_snapshot_detail(kind, detail),
        );
        if matches!(fallback, Ok(ref response) if response.ok) {
            return None;
        }
        if let Some(warning) = browser_event_recording_warning("browser_snapshot", fallback) {
            return Some(format!(
                "workspace event {kind:?} was not recorded, and compatibility fallback failed: {warning}"
            ));
        }
    }
    browser_event_recording_warning(kind, result)
}

fn browser_event_recording_warning(
    kind: &str,
    result: Result<workspace::IpcResponse>,
) -> Option<String> {
    match result {
        Ok(response) if response.ok => None,
        Ok(response) => Some(format!(
            "workspace event {kind:?} was not recorded: {}; restart the workspace if the viewer activity footer is missing this action",
            response.message
        )),
        Err(error) if should_use_direct_browser_compat(&error) => Some(format!(
            "workspace event {kind:?} was not recorded because this already-running workspace daemon does not support external browser activity events; start a fresh workspace to show this action in the viewer activity footer"
        )),
        Err(error) => Some(format!(
            "workspace event {kind:?} was not recorded: {error}; restart the workspace if the viewer activity footer is missing this action"
        )),
    }
}

fn should_retry_browser_event_as_snapshot(
    kind: &str,
    result: &Result<workspace::IpcResponse>,
) -> bool {
    kind == "browser_search_results"
        && matches!(result, Ok(response) if !response.ok && looks_like_unsupported_event_kind(&response.message))
}

fn looks_like_unsupported_event_kind(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unsupported event kind")
        || (lower.contains("external workspace event kind") && lower.contains("not allowed"))
}

fn compatibility_browser_snapshot_detail(
    original_kind: &str,
    detail: serde_json::Value,
) -> serde_json::Value {
    match detail {
        serde_json::Value::Object(mut object) => {
            object
                .entry("browser_action".to_string())
                .or_insert_with(|| serde_json::Value::String(original_kind.to_string()));
            object
                .entry("original_event_kind".to_string())
                .or_insert_with(|| serde_json::Value::String(original_kind.to_string()));
            object.insert(
                "compatibility_event_kind".to_string(),
                serde_json::Value::String("browser_snapshot".to_string()),
            );
            serde_json::Value::Object(object)
        }
        other => json!({
            "browser_action": original_kind,
            "original_event_kind": original_kind,
            "compatibility_event_kind": "browser_snapshot",
            "detail": other,
        }),
    }
}

fn select_browser_app(
    status: &WorkspaceStatus,
    app_id: Option<&str>,
    user_data_dir: Option<&Path>,
) -> Result<BrowserAppSelection> {
    let mut candidates = Vec::new();
    for app in status.apps.iter().filter(|app| app.running) {
        if app_id.is_some_and(|id| !matches_browser_app_ref(app, id)) {
            continue;
        }
        let workspace_user_data_dir = command_user_data_dir(&app.command);
        let host_user_data_dir = user_data_dir.map(Path::to_path_buf).or_else(|| {
            workspace_user_data_dir
                .as_deref()
                .map(|path| host_path_for_user_data_dir(status, path))
        });
        let remote_debugging_port = command_remote_debugging_port(&app.command);
        let remote_debugging_address = command_remote_debugging_address(&app.command);
        if !looks_like_browser_app(app, remote_debugging_port, workspace_user_data_dir.as_ref()) {
            continue;
        }
        if let Some(requested_user_data_dir) = user_data_dir {
            let command_matches = workspace_user_data_dir
                .as_deref()
                .is_some_and(|path| path == requested_user_data_dir)
                || host_user_data_dir
                    .as_deref()
                    .is_some_and(|path| path == requested_user_data_dir);
            if !command_matches {
                continue;
            }
        }
        candidates.push(BrowserAppSelection {
            app: app.clone(),
            workspace_user_data_dir,
            host_user_data_dir,
            remote_debugging_port,
            remote_debugging_address,
        });
    }

    if candidates.is_empty() {
        if let Some(app_id) = app_id {
            bail!("no running workspace browser app matched app id/name/pid {app_id:?}");
        }
        bail!(
            "no running workspace browser app exposes --user-data-dir and Chrome DevTools; call workspace_open_browser (MCP) or `agent-workspace-linux workspace open-browser` (CLI), or launch Chrome inside the workspace with --user-data-dir=DIR --remote-debugging-address=127.0.0.1 --remote-debugging-port=0. Note: the DevTools port is ephemeral, so external WebSocket clients must omit the Origin header (e.g. websocket-client's suppress_origin=True) rather than rely on --remote-allow-origins"
        );
    }
    if candidates.len() > 1 && app_id.is_none() && user_data_dir.is_none() {
        let ids = candidates
            .iter()
            .map(|candidate| candidate.app.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("multiple running workspace browser apps expose DevTools ({ids}); pass app_id");
    }
    Ok(candidates.remove(0))
}

fn matches_browser_app_ref(app: &WorkspaceApp, requested: &str) -> bool {
    app.id == requested
        || app.pid.to_string() == requested
        || app.name.as_deref() == Some(requested)
}

fn looks_like_browser_app(
    app: &WorkspaceApp,
    remote_debugging_port: Option<u16>,
    workspace_user_data_dir: Option<&PathBuf>,
) -> bool {
    if remote_debugging_port.is_some() && workspace_user_data_dir.is_some() {
        return true;
    }
    app.command
        .first()
        .map(|program| {
            let lower = program.to_ascii_lowercase();
            lower.contains("chrome") || lower.contains("chromium")
        })
        .unwrap_or(false)
        && workspace_user_data_dir.is_some()
}

fn host_path_for_user_data_dir(status: &WorkspaceStatus, path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    if let Some(mapped) = status
        .applied_policy
        .as_ref()
        .and_then(|policy| map_mount_path(&policy.mounts, path))
    {
        return mapped;
    }
    path.to_path_buf()
}

fn map_mount_path(mounts: &[ProfileMount], workspace_path: &Path) -> Option<PathBuf> {
    mounts.iter().find_map(|mount| {
        let relative = workspace_path.strip_prefix(&mount.workspace_path).ok()?;
        Some(mount.host_path.join(relative))
    })
}

fn workspace_browser_command(browser_path: &Path, user_data_dir: &Path, url: &str) -> Vec<String> {
    vec![
        browser_path.display().to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
        "--no-sandbox".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--ozone-platform=x11".to_string(),
        "--new-window".to_string(),
        url.to_string(),
    ]
}

fn find_browser_executable() -> Option<PathBuf> {
    for candidate in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(path) = resolve_executable(candidate) {
            return Some(path);
        }
    }
    None
}

fn resolve_executable(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable_file(&path).then_some(path);
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable_file(candidate))
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn command_user_data_dir(command: &[String]) -> Option<PathBuf> {
    command_value(command, "--user-data-dir").map(PathBuf::from)
}

fn command_remote_debugging_port(command: &[String]) -> Option<u16> {
    command_value(command, "--remote-debugging-port")?
        .parse()
        .ok()
}

fn command_remote_debugging_address(command: &[String]) -> Option<String> {
    command_value(command, "--remote-debugging-address")
}

fn command_value(command: &[String], name: &str) -> Option<String> {
    for (index, arg) in command.iter().enumerate() {
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return (!value.is_empty()).then(|| value.to_string());
        }
        if arg == name {
            return command
                .get(index + 1)
                .filter(|value| !value.is_empty())
                .cloned();
        }
    }
    None
}

fn is_loopback_devtools_address(address: &str) -> bool {
    matches!(
        address.trim(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]" | ""
    )
}

fn read_devtools_active_port(path: &Path) -> Result<DevToolsActivePort> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut lines = content.lines();
    let port = lines
        .next()
        .context("DevToolsActivePort did not include a port")?
        .trim()
        .parse::<u16>()
        .context("DevToolsActivePort port was not a valid TCP port")?;
    let browser_path = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string);
    Ok(DevToolsActivePort { port, browser_path })
}

fn read_devtools_targets(port: u16) -> Result<Vec<BrowserTarget>> {
    let body = http_get_loopback(port, "/json/list")?;
    serde_json::from_str(&body).context("failed to parse Chrome DevTools /json/list response")
}

fn http_get_loopback(port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("failed to connect to Chrome DevTools at 127.0.0.1:{port}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set Chrome DevTools read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("failed to set Chrome DevTools write timeout")?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .context("failed to write Chrome DevTools HTTP request")?;
    let mut response = Vec::new();
    let mut header_end = None;
    let mut content_length = None;
    loop {
        let mut buffer = [0_u8; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if header_end.is_none() {
                    header_end = find_http_header_end(&response);
                    if let Some(end) = header_end {
                        let headers = String::from_utf8_lossy(&response[..end]);
                        content_length = parse_content_length(&headers);
                    }
                }
                if let (Some(end), Some(length)) = (header_end, content_length) {
                    if response.len().saturating_sub(end) >= length {
                        break;
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                if let (Some(end), Some(length)) = (header_end, content_length) {
                    if response.len().saturating_sub(end) >= length {
                        break;
                    }
                }
                return Err(error).context("timed out reading Chrome DevTools HTTP response");
            }
            Err(error) => {
                return Err(error).context("failed to read Chrome DevTools HTTP response");
            }
        }
    }
    let header_end = header_end.context("Chrome DevTools HTTP response did not include headers")?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status = headers.lines().next().unwrap_or("");
    if !status.contains(" 200 ") {
        bail!("Chrome DevTools HTTP request failed: {status}");
    }
    String::from_utf8(response[header_end..].to_vec())
        .context("Chrome DevTools HTTP response body was not UTF-8")
}

fn find_http_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .or_else(|| {
            bytes
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| index + 2)
        })
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

struct DevToolsWebSocket {
    stream: TcpStream,
    next_id: u64,
}

#[derive(Debug)]
struct ParsedDevToolsWebSocketUrl {
    host: String,
    port: u16,
    path: String,
}

impl DevToolsWebSocket {
    fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let parsed = parse_loopback_ws_url(url)?;
        let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
            .with_context(|| format!("failed to connect to Chrome DevTools WebSocket at {url}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .context("failed to set Chrome DevTools WebSocket read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("failed to set Chrome DevTools WebSocket write timeout")?;
        let key = websocket_key();
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            parsed.path, parsed.host, parsed.port, key
        );
        stream
            .write_all(request.as_bytes())
            .context("failed to write Chrome DevTools WebSocket handshake")?;
        let headers = read_websocket_handshake(&mut stream)?;
        let status = headers.lines().next().unwrap_or_default();
        if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
            bail!("Chrome DevTools WebSocket handshake failed: {status}");
        }
        Ok(Self { stream, next_id: 1 })
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        self.stream
            .set_read_timeout(Some(timeout))
            .context("failed to update Chrome DevTools WebSocket read timeout")?;
        let id = self.next_id;
        self.next_id += 1;
        self.send_json(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let message = self.read_text_message()?;
            let value: serde_json::Value =
                serde_json::from_str(&message).context("Chrome DevTools frame was not JSON")?;
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return Ok(value);
            }
        }
    }

    fn send_json(&mut self, value: &serde_json::Value) -> Result<()> {
        let bytes =
            serde_json::to_vec(value).context("failed to serialize Chrome DevTools JSON")?;
        self.send_frame(0x1, &bytes)
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::new();
        frame.push(0x80 | opcode);
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let mask = websocket_mask();
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % 4]);
        }
        self.stream
            .write_all(&frame)
            .context("failed to write Chrome DevTools WebSocket frame")
    }

    fn read_text_message(&mut self) -> Result<String> {
        let mut payload = Vec::new();
        let mut message_opcode = None;
        loop {
            let (fin, opcode, data) = self.read_frame()?;
            match opcode {
                0x1 => {
                    message_opcode = Some(opcode);
                    payload.extend_from_slice(&data);
                }
                0x0 => {
                    if message_opcode.is_none() {
                        bail!("Chrome DevTools sent a continuation frame without a message");
                    }
                    payload.extend_from_slice(&data);
                }
                0x8 => bail!("Chrome DevTools WebSocket closed"),
                0x9 => {
                    self.send_frame(0xA, &data)?;
                    continue;
                }
                0xA => continue,
                other => bail!("Chrome DevTools sent unsupported WebSocket opcode {other}"),
            }
            if fin {
                break;
            }
        }
        if message_opcode != Some(0x1) {
            bail!("Chrome DevTools did not send a text message");
        }
        String::from_utf8(payload).context("Chrome DevTools WebSocket text was not UTF-8")
    }

    fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>)> {
        let mut header = [0_u8; 2];
        self.stream
            .read_exact(&mut header)
            .context("failed to read Chrome DevTools WebSocket frame header")?;
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut length = (header[1] & 0x7f) as u64;
        if length == 126 {
            let mut extended = [0_u8; 2];
            self.stream
                .read_exact(&mut extended)
                .context("failed to read Chrome DevTools WebSocket frame length")?;
            length = u16::from_be_bytes(extended) as u64;
        } else if length == 127 {
            let mut extended = [0_u8; 8];
            self.stream
                .read_exact(&mut extended)
                .context("failed to read Chrome DevTools WebSocket frame length")?;
            length = u64::from_be_bytes(extended);
        }
        let mask = if masked {
            let mut mask = [0_u8; 4];
            self.stream
                .read_exact(&mut mask)
                .context("failed to read Chrome DevTools WebSocket frame mask")?;
            Some(mask)
        } else {
            None
        };
        if length > 5_000_000 {
            bail!("Chrome DevTools WebSocket frame exceeded the 5 MB safety limit");
        }
        let mut payload = vec![0_u8; length as usize];
        self.stream
            .read_exact(&mut payload)
            .context("failed to read Chrome DevTools WebSocket frame payload")?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        Ok((fin, opcode, payload))
    }
}

fn parse_loopback_ws_url(url: &str) -> Result<ParsedDevToolsWebSocketUrl> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| anyhow!("Chrome DevTools WebSocket must use ws:// loopback URL"))?;
    let (host_port, path) = rest
        .split_once('/')
        .map(|(host, path)| (host, format!("/{path}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port_text) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("Chrome DevTools WebSocket URL must include a port"))?;
    if !is_loopback_devtools_address(host) {
        bail!("Chrome DevTools WebSocket host must be loopback, got {host:?}");
    }
    let port = port_text
        .parse::<u16>()
        .context("Chrome DevTools WebSocket port was not a valid TCP port")?;
    Ok(ParsedDevToolsWebSocketUrl {
        host: host.to_string(),
        port,
        path,
    })
}

fn read_websocket_handshake(stream: &mut TcpStream) -> Result<String> {
    let mut response = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .context("failed to read Chrome DevTools WebSocket handshake")?;
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") || response.ends_with(b"\n\n") {
            break;
        }
        if response.len() > 16_384 {
            bail!("Chrome DevTools WebSocket handshake exceeded 16 KB");
        }
    }
    String::from_utf8(response).context("Chrome DevTools WebSocket handshake was not UTF-8")
}

fn websocket_key() -> String {
    base64_encode(&time_seed_bytes())
}

fn websocket_mask() -> [u8; 4] {
    let seed = time_seed_bytes();
    [seed[3], seed[7], seed[11], seed[15]]
}

fn time_seed_bytes() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    (nanos ^ ((std::process::id() as u128) << 64)).to_be_bytes()
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let second = bytes.get(index + 1).copied();
        let third = bytes.get(index + 2).copied();
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second.unwrap_or(0) >> 4)) as usize] as char);
        if let Some(second) = second {
            output
                .push(TABLE[(((second & 0x0f) << 2) | (third.unwrap_or(0) >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if let Some(third) = third {
            output.push(TABLE[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
        index += 3;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser_status_for_test() -> WorkspaceStatus {
        WorkspaceStatus {
            id: "browser-dogfood".to_string(),
            session_id: "session-browser-dogfood".to_string(),
            purpose: None,
            profile_id: None,
            applied_policy: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            user_acknowledged_unenforced_policy: false,
            ready: true,
            started_at_unix: 1,
            display: ":90".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: PathBuf::from("/tmp/browser-dogfood"),
            socket_path: PathBuf::from("/tmp/browser-dogfood/control.sock"),
            xauthority_path: PathBuf::from("/tmp/browser-dogfood/Xauthority"),
            daemon_pid: Some(1234),
            x_server_pid: 1235,
            window_manager_pid: Some(1236),
            last_event_sequence: 0,
            apps: vec![WorkspaceApp {
                id: "app-42".to_string(),
                name: Some("workspace-chrome-cdp".to_string()),
                pid: 4242,
                process_group_id: Some(4242),
                profile_id: None,
                mount_isolation: "host".to_string(),
                network_isolation: "host".to_string(),
                command: vec![
                    "/usr/bin/google-chrome-stable".to_string(),
                    "--user-data-dir=/tmp/browser-profile".to_string(),
                    "--remote-debugging-address=127.0.0.1".to_string(),
                    "--remote-debugging-port=9223".to_string(),
                ],
                cwd: None,
                env: Vec::new(),
                stdout_path: None,
                stderr_path: None,
                started_at_unix: 1,
                running: true,
                exit_status: None,
                exit_code: None,
                exit_signal: None,
                stopped_at_unix: None,
                runtime_seconds: None,
            }],
        }
    }

    #[test]
    fn command_value_reads_equals_and_split_forms() {
        let equals = vec![
            "chrome".to_string(),
            "--user-data-dir=/tmp/profile".to_string(),
            "--remote-debugging-port=0".to_string(),
        ];
        assert_eq!(
            command_user_data_dir(&equals),
            Some(PathBuf::from("/tmp/profile"))
        );
        assert_eq!(command_remote_debugging_port(&equals), Some(0));

        let split = vec![
            "chrome".to_string(),
            "--user-data-dir".to_string(),
            "/tmp/other".to_string(),
            "--remote-debugging-address".to_string(),
            "127.0.0.1".to_string(),
        ];
        assert_eq!(
            command_user_data_dir(&split),
            Some(PathBuf::from("/tmp/other"))
        );
        assert_eq!(
            command_remote_debugging_address(&split).as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn browser_app_selection_accepts_id_name_or_pid() {
        let status = browser_status_for_test();

        for requested in ["app-42", "workspace-chrome-cdp", "4242"] {
            let selection =
                select_browser_app(&status, Some(requested), None).expect("browser app selected");
            assert_eq!(selection.app.id, "app-42");
        }

        assert!(
            matches_browser_app_ref(&status.apps[0], "workspace-chrome-cdp"),
            "direct helper should match the launched app name"
        );
    }

    #[test]
    fn mount_mapping_resolves_workspace_profile_path() {
        let mounts = vec![ProfileMount {
            host_path: PathBuf::from("/home/me/profile-copy"),
            workspace_path: PathBuf::from("/workspace/browser-user-data"),
            mode: crate::policy::MountMode::ReadWrite,
        }];

        assert_eq!(
            map_mount_path(&mounts, Path::new("/workspace/browser-user-data/Default")).as_deref(),
            Some(Path::new("/home/me/profile-copy/Default"))
        );
        assert!(map_mount_path(&mounts, Path::new("/tmp/profile")).is_none());
    }

    #[test]
    fn non_loopback_devtools_address_is_rejected() {
        assert!(is_loopback_devtools_address("127.0.0.1"));
        assert!(is_loopback_devtools_address("localhost"));
        assert!(!is_loopback_devtools_address("0.0.0.0"));
        assert!(!is_loopback_devtools_address("192.168.1.20"));
    }

    #[test]
    fn page_target_selection_can_disambiguate_by_title_or_url() {
        let targets = vec![
            BrowserTarget {
                id: "one".to_string(),
                target_type: "page".to_string(),
                title: "Inbox".to_string(),
                url: "https://example.test/inbox".to_string(),
                web_socket_debugger_url: Some("ws://127.0.0.1:1/devtools/page/one".to_string()),
            },
            BrowserTarget {
                id: "two".to_string(),
                target_type: "page".to_string(),
                title: "Amazon GPU Search".to_string(),
                url: "https://www.amazon.com/s?k=rtx+pro+5000".to_string(),
                web_socket_debugger_url: Some("ws://127.0.0.1:1/devtools/page/two".to_string()),
            },
        ];

        let (target, warnings) =
            select_page_target(&targets, None, Some("gpu"), None).expect("target selected");
        assert_eq!(target.id, "two");
        assert!(warnings.is_empty());

        let (target, warnings) =
            select_page_target(&targets, None, None, None).expect("first target selected");
        assert_eq!(target.id, "one");
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("multiple page targets")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("Amazon GPU Search")));
    }

    #[test]
    fn search_results_payload_parses_optional_product_fields() {
        let value = serde_json::json!({
            "title": "Amazon GPU Search",
            "url": "https://www.amazon.com/s?k=gpu",
            "result_count": 1,
            "results": [{
                "index": 1,
                "title": "PNY Test GPU 48GB",
                "href": "https://example.test/dp/GPU48",
                "price": "$4,899.00",
                "vram_gb": 48,
                "rating": "4.6 out of 5 stars",
                "review_count": "(22)",
                "availability": "Only 2 left in stock - order soon.",
                "delivery": "FREE delivery Tomorrow",
                "text_excerpt": "PNY Test GPU 48GB | $4,899.00"
            }]
        });

        let page: BrowserSearchResultsPage =
            serde_json::from_value(value).expect("structured results parse");

        assert_eq!(page.result_count, 1);
        assert_eq!(page.results[0].title, "PNY Test GPU 48GB");
        assert_eq!(page.results[0].price.as_deref(), Some("$4,899.00"));
        assert_eq!(page.results[0].vram_gb, Some(48));
        assert_eq!(
            page.results[0].availability.as_deref(),
            Some("Only 2 left in stock - order soon.")
        );
    }

    #[test]
    fn browser_snapshot_event_detail_omits_raw_page_content() {
        let target = BrowserTarget {
            id: "page-1".to_string(),
            target_type: "page".to_string(),
            title: "Private Grocery Cart".to_string(),
            url: "https://example-grocery.test/cart".to_string(),
            web_socket_debugger_url: Some("ws://127.0.0.1:9222/devtools/page/page-1".to_string()),
        };
        let response = WorkspaceBrowserSnapshot {
            ok: true,
            message: "captured".to_string(),
            id: "ws".to_string(),
            app_id: Some("app-browser".to_string()),
            app_pid: Some(4242),
            devtools_endpoint: Some("http://127.0.0.1:9222".to_string()),
            target: Some(target),
            browser_target_id: Some("page-1".to_string()),
            page: Some(BrowserPageSnapshot {
                title: "Private Grocery Cart".to_string(),
                url: "https://example-grocery.test/cart".to_string(),
                text: "Name, address, phone, and cart details should not persist in events"
                    .to_string(),
                text_chars: 72,
                text_truncated: false,
                headings: vec!["Delivery address".to_string()],
                links: vec![BrowserPageLink {
                    text: "Checkout".to_string(),
                    href: "https://example-grocery.test/checkout".to_string(),
                }],
            }),
            agent_mode: None,
            target_handles: None,
            recovery_hints: Vec::new(),
            warnings: Vec::new(),
        };

        let detail = browser_snapshot_event_detail(&response);

        assert_eq!(detail["app_id"], "app-browser");
        assert_eq!(detail["target_id"], "page-1");
        assert_eq!(detail["text_chars"], 72);
        assert_eq!(detail["raw_text_omitted"], true);
        assert!(detail.get("text").is_none());
        assert!(detail.get("headings").is_none());
        assert!(detail.get("links").is_none());
    }

    #[test]
    fn browser_search_results_event_detail_omits_raw_cards() {
        let target = BrowserTarget {
            id: "page-2".to_string(),
            target_type: "page".to_string(),
            title: "Amazon GPU Search".to_string(),
            url: "https://www.amazon.com/s?k=gpu".to_string(),
            web_socket_debugger_url: Some("ws://127.0.0.1:9222/devtools/page/page-2".to_string()),
        };
        let response = WorkspaceBrowserSearchResults {
            ok: true,
            message: "extracted".to_string(),
            id: "ws".to_string(),
            app_id: Some("app-browser".to_string()),
            app_pid: Some(4242),
            devtools_endpoint: Some("http://127.0.0.1:9222".to_string()),
            target: Some(target),
            browser_target_id: Some("page-2".to_string()),
            page: Some(BrowserSearchResultsPage {
                title: "Amazon GPU Search".to_string(),
                url: "https://www.amazon.com/s?k=gpu".to_string(),
                result_count: 1,
                results: vec![BrowserSearchResult {
                    index: 1,
                    title: "PNY Test GPU 48GB".to_string(),
                    href: Some("https://example.test/dp/GPU48".to_string()),
                    price: Some("$4,899.00".to_string()),
                    vram_gb: Some(48),
                    rating: Some("4.6 out of 5 stars".to_string()),
                    review_count: Some("(22)".to_string()),
                    availability: Some("Only 2 left in stock".to_string()),
                    delivery: Some("FREE delivery Tomorrow".to_string()),
                    badge: None,
                    seller_or_brand: Some("PNY".to_string()),
                    text_excerpt: "PNY Test GPU 48GB | $4,899.00".to_string(),
                }],
            }),
            agent_mode: None,
            target_handles: None,
            recovery_hints: Vec::new(),
            warnings: Vec::new(),
        };

        let detail = browser_search_results_event_detail(&response, Some(36));

        assert_eq!(detail["result_count"], 1);
        assert_eq!(detail["min_vram_gb"], 36);
        assert_eq!(detail["raw_result_text_omitted"], true);
        assert!(detail.get("results").is_none());
        assert!(detail.get("text_excerpt").is_none());
    }

    #[test]
    fn workspace_browser_command_binds_devtools_to_loopback_without_widening_origins() {
        // Chrome rejects DevTools WebSocket upgrades whose `Origin` header is not
        // allow-listed, and it matches origins *including the port*. Because the
        // endpoint uses `--remote-debugging-port=0` (ephemeral), no static
        // `--remote-allow-origins` list can ever match — only `*` would, and that
        // disables the CSRF protection guarding this endpoint. Verified: clients
        // that omit `Origin` (the bundled CDP client, and any library with
        // origin suppression) connect fine with no flag at all. So we bind to
        // loopback and deliberately do NOT pass --remote-allow-origins.
        let command = workspace_browser_command(
            Path::new("/usr/bin/google-chrome"),
            Path::new("/tmp/profile"),
            "about:blank",
        );
        assert!(command
            .iter()
            .any(|arg| arg == "--remote-debugging-address=127.0.0.1"));
        assert!(command.iter().any(|arg| arg == "--remote-debugging-port=0"));
        assert!(
            !command
                .iter()
                .any(|arg| arg.starts_with("--remote-allow-origins")),
            "must not widen DevTools origins; document Origin suppression instead"
        );
    }

    #[test]
    fn browser_navigate_event_detail_omits_snapshot_text() {
        let target = BrowserTarget {
            id: "page-3".to_string(),
            target_type: "page".to_string(),
            title: "Private Grocery Search".to_string(),
            url: "https://example-grocery.test/search".to_string(),
            web_socket_debugger_url: Some("ws://127.0.0.1:9222/devtools/page/page-3".to_string()),
        };
        let response = WorkspaceBrowserNavigate {
            ok: true,
            message: "navigated".to_string(),
            id: "ws".to_string(),
            app_id: Some("app-browser".to_string()),
            app_pid: Some(4242),
            devtools_endpoint: Some("http://127.0.0.1:9222".to_string()),
            target: Some(target),
            browser_target_id: Some("page-3".to_string()),
            url: "https://example-grocery.test/search?q=milk".to_string(),
            navigation: Some(BrowserNavigationResult {
                frame_id: Some("frame".to_string()),
                loader_id: Some("loader".to_string()),
            }),
            page: Some(BrowserPageSnapshot {
                title: "Private Grocery Search".to_string(),
                url: "https://example-grocery.test/search?q=milk".to_string(),
                text: "Private search result text should not persist in events".to_string(),
                text_chars: 55,
                text_truncated: false,
                headings: vec!["Search results".to_string()],
                links: vec![BrowserPageLink {
                    text: "Milk".to_string(),
                    href: "https://example-grocery.test/item/milk".to_string(),
                }],
            }),
            agent_mode: None,
            target_handles: None,
            recovery_hints: Vec::new(),
            warnings: Vec::new(),
        };

        let detail = browser_navigate_event_detail(&response);

        assert_eq!(detail["snapshot"], true);
        assert_eq!(detail["raw_text_omitted"], true);
        assert_eq!(
            detail["current_url"],
            "https://example-grocery.test/search?q=milk"
        );
        assert!(detail.get("text").is_none());
        assert!(detail.get("headings").is_none());
        assert!(detail.get("links").is_none());
    }

    fn ipc_response(ok: bool, message: &str) -> workspace::IpcResponse {
        workspace::IpcResponse {
            ok,
            message: message.to_string(),
            status: None,
            start_preview: None,
            launch_preview: None,
            ipc: None,
            environment: None,
            apps: None,
            windows: None,
            active_window: None,
            pointer: None,
            screenshot: None,
            app_log: None,
            clipboard: None,
            events: None,
            browser_targets: None,
            browser_snapshot: None,
            browser_search_results: None,
            browser_navigate: None,
            agent_mode: None,
            target_handles: None,
            recovery_hints: Vec::new(),
        }
    }

    #[test]
    fn browser_event_recording_warning_surfaces_failed_ipc() {
        assert!(browser_event_recording_warning(
            "browser_search_results",
            Ok(ipc_response(true, "recorded"))
        )
        .is_none());

        let rejected = browser_event_recording_warning(
            "browser_search_results",
            Ok(ipc_response(false, "unsupported event kind")),
        )
        .expect("failed event response should warn");
        assert!(rejected.contains("browser_search_results"));
        assert!(rejected.contains("unsupported event kind"));

        let ipc_error = browser_event_recording_warning(
            "browser_snapshot",
            Err(anyhow!("workspace IPC socket is unavailable")),
        )
        .expect("IPC error should warn");
        assert!(ipc_error.contains("browser_snapshot"));
        assert!(ipc_error.contains("workspace IPC socket is unavailable"));
    }

    #[test]
    fn browser_search_results_event_falls_back_for_older_workspace_daemon() {
        let mut calls: Vec<(String, serde_json::Value)> = Vec::new();
        let warning = record_browser_event_with(
            "browser-dogfood",
            "browser_search_results",
            json!({
                "app_id": "workspace-chrome",
                "title": "Amazon GPU Search Results",
                "result_count": 3,
                "min_vram_gb": 37
            }),
            |_, kind, detail| {
                calls.push((kind.to_string(), detail));
                if kind == "browser_search_results" {
                    Ok(ipc_response(false, "unsupported event kind"))
                } else {
                    Ok(ipc_response(true, "recorded"))
                }
            },
        );

        assert!(warning.is_none());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "browser_search_results");
        assert_eq!(calls[1].0, "browser_snapshot");
        assert_eq!(
            calls[1].1["browser_action"],
            serde_json::Value::String("browser_search_results".to_string())
        );
        assert_eq!(calls[1].1["result_count"], serde_json::Value::from(3));
    }

    #[test]
    fn navigation_url_rejects_script_urls() {
        validate_navigation_url("https://example.test").expect("https is allowed");
        validate_navigation_url("data:text/html,ok").expect("data URL is allowed for smokes");
        validate_navigation_url("javascript:alert(1)")
            .expect_err("javascript navigation should be rejected");
    }

    #[test]
    fn loopback_websocket_url_parser_rejects_host_endpoints() {
        let parsed = parse_loopback_ws_url("ws://127.0.0.1:9222/devtools/page/1")
            .expect("loopback URL parses");
        assert_eq!(parsed.port, 9222);
        assert_eq!(parsed.path, "/devtools/page/1");
        parse_loopback_ws_url("ws://192.168.1.20:9222/devtools/page/1")
            .expect_err("non-loopback endpoint rejected");
    }

    #[test]
    fn base64_encoder_handles_padding() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
