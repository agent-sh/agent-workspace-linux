#[path = "terminal_cleanup.rs"]
mod terminal_cleanup;

use crate::approval::{hidden_workspace_acknowledgement, ApprovalBundle};
use crate::control::{self, McpControlMode};
use crate::desktop::{sanitize_window_id, DesktopControl};
use crate::launch::resolve_executable_path;
use crate::permissions::{load_mcp_permission_state, McpPermissionState};
use crate::session::SessionSpec;
use anyhow::{anyhow, bail, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_WORKSPACE_ID: &str = "default";
const IPC_PROTOCOL_NAME: &str = "agent-workspace-linux.ipc";
/// Bumped when daemon replies change shape. A daemon starts only for a
/// spawner of the same version: an MCP server older than the binary cannot
/// read its replies.
pub(crate) const IPC_PROTOCOL_VERSION: u32 = 2;
const DEFAULT_APP_WAIT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_STOP_WAIT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_CLICK_BUTTON: u8 = 1;
const DEFAULT_CLICK_COUNT: u8 = 1;
const DEFAULT_SCROLL_AMOUNT: u8 = 1;
const MAX_SCROLL_AMOUNT: u8 = 100;
const DEFAULT_PASTE_KEY: &str = "ctrl+v";
const DEFAULT_TERMINAL_PROGRAM: &str = "xterm";
const DEFAULT_TMUX_PROGRAM: &str = "tmux";
const DEFAULT_TERMINAL_FONT: &str = "Monospace";
const DEFAULT_TERMINAL_FONT_SIZE: u8 = 16;
const DEFAULT_TERMINAL_GEOMETRY: &str = "100x30";
const DEFAULT_TERMINAL_READY_TIMEOUT_MS: u64 = 10_000;
const ACTIVE_WINDOW_RESPONSE_WAIT_MS: u64 = 250;
const ACTIVE_WINDOW_RESPONSE_POLL_MS: u64 = 20;
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DISPLAY_RANGE: std::ops::Range<u32> = 90..180;
const APP_TERMINATE_GRACE_MS: u64 = 1_000;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const ESRCH: i32 = 3;
const PRIVATE_RUNTIME_DIR_MODE: u32 = 0o700;
const PRIVATE_SOCKET_MODE: u32 = 0o600;
const DAEMON_PID_FILE: &str = "daemon.pid";
const MAX_UNIX_SOCKET_PATH_BYTES: usize = 107;
pub(crate) const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;
const EVENT_LOG_FILE: &str = "events.jsonl";
const WORKSPACE_MANIFEST_FILE: &str = "workspace.json";
// Shared runtime metadata must never be accepted as a workspace directory.
pub(crate) const VIEWER_REGISTRY_DIR: &str = "viewers";
const OBSERVE_SCREENSHOT_FILE: &str = "observe-frame.png";
const LINUX_DELETED_EXE_SUFFIX: &str = " (deleted)";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub(crate) fn x11_button(self) -> u8 {
        match self {
            Self::Up => 4,
            Self::Down => 5,
            Self::Left => 6,
            Self::Right => 7,
        }
    }
}

impl FromStr for ScrollDirection {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            _ => bail!("scroll direction must be up, down, left, or right"),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorReport {
    pub session_runtimes: serde_json::Value,
    pub runtime: RuntimeReport,
    pub viewer: ViewerRuntimeReport,
    pub ready_for_x11_workspace: bool,
    pub ready_for_i3_workspace: bool,
    pub i3_blockers: Vec<String>,
    pub ready_for_kwin_workspace: bool,
    pub ready_for_gnome_workspace: bool,
    pub gnome_blockers: Vec<String>,
    pub ready_for_hyprland_workspace: bool,
    pub hyprland_blockers: Vec<String>,
    pub kwin_blockers: Vec<String>,
    pub ready_for_cosmic_workspace: bool,
    pub cosmic_blockers: Vec<String>,
    pub ready_for_host_viewer: bool,
    pub blockers: Vec<String>,
    pub viewer_blockers: Vec<String>,
    pub recommended_next_step: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeReport {
    pub xvfb: Check,
    pub xephyr: Check,
    pub xauth: Check,
    pub xdpyinfo: Check,
    pub xprop: Check,
    pub window_manager: Check,
    pub xdotool: Check,
    pub screenshot: Check,
    pub clipboard: Check,
    /// The workspace's own session bus, which its apps use.
    pub dbus_daemon: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerRuntimeReport {
    pub host_display: Check,
    pub source_build_xkbcommon_x11: Check,
    pub host_opener: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Check {
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceStartOptions {
    pub id: String,
    pub session: SessionSpec,
    pub purpose: Option<String>,
    pub profile_id: Option<String>,
    pub profile_cwd: Option<PathBuf>,
    pub profile_env: Vec<EnvVar>,
    pub user_acknowledged_hidden_workspace: bool,
    pub width: u32,
    pub height: u32,
    /// Path to the active spawn-time MCP permission ceiling JSON, forwarded to
    /// the detached daemon so it can enforce the ceiling on every launch.
    pub permissions_source: Option<PathBuf>,
}

impl Default for WorkspaceStartOptions {
    fn default() -> Self {
        Self {
            id: DEFAULT_WORKSPACE_ID.to_string(),
            session: SessionSpec::default(),
            purpose: None,
            profile_id: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: false,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            permissions_source: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub id: String,
    pub session: SessionSpec,
    pub session_id: String,
    pub purpose: Option<String>,
    pub profile_id: Option<String>,
    pub profile_cwd: Option<PathBuf>,
    pub profile_env: Vec<EnvVar>,
    pub user_acknowledged_hidden_workspace: bool,
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub xauthority_path: PathBuf,
    /// Path to the spawn-time MCP permission ceiling JSON, if one was active
    /// when this daemon was started. The daemon loads and enforces it on every
    /// mutating IPC request so the ceiling holds regardless of IPC origin.
    pub permissions_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceStatus {
    pub id: String,
    #[serde(default)]
    pub session: SessionSpec,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile_env: Vec<EnvVar>,
    pub user_acknowledged_hidden_workspace: bool,
    pub ready: bool,
    pub started_at_unix: u64,
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub xauthority_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u32>,
    pub x_server_pid: u32,
    pub window_manager_pid: Option<u32>,
    #[serde(default)]
    pub last_event_sequence: u64,
    pub apps: Vec<WorkspaceApp>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceList {
    pub runtime_base_dir: PathBuf,
    pub workspaces: Vec<WorkspaceListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceListEntry {
    pub id: String,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub running: bool,
    pub manifest: Option<WorkspaceManifest>,
    pub manifest_error: Option<String>,
    pub status: Option<WorkspaceStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceManifestRead {
    pub ok: bool,
    pub message: String,
    pub id: String,
    pub runtime_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: Option<WorkspaceManifest>,
    pub manifest_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceArtifacts {
    pub ok: bool,
    pub message: String,
    pub id: String,
    pub runtime_dir: PathBuf,
    pub files: Vec<WorkspaceArtifact>,
    pub manifest_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceArtifact {
    pub kind: String,
    pub label: String,
    pub path: PathBuf,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceManifest {
    pub id: String,
    #[serde(default)]
    pub session: SessionSpec,
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile_env: Vec<EnvVar>,
    pub user_acknowledged_hidden_workspace: bool,
    pub ready: bool,
    pub started_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_seconds: Option<u64>,
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub xauthority_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_server_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_manager_pid: Option<u32>,
    #[serde(default)]
    pub event_log_path: PathBuf,
    #[serde(default)]
    pub daemon_stdout_path: PathBuf,
    #[serde(default)]
    pub daemon_stderr_path: PathBuf,
    #[serde(default)]
    pub last_event_sequence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<WorkspaceApp>,
    /// Kernel start times of the recorded pids. Stale cleanup signals a pid
    /// only while its start time still matches, so a recycled pid (a host
    /// compositor, another workspace's daemon) is never terminated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_ticks: Option<ProcessStartTicks>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessStartTicks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_server: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_manager: Option<u64>,
    /// Running app leaders by pid.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<u32, u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceCleanup {
    pub runtime_base_dir: PathBuf,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<WorkspaceCleanupEntry>,
    pub removed: Vec<WorkspaceCleanupEntry>,
    pub skipped: Vec<WorkspaceCleanupEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceCleanupEntry {
    pub id: String,
    pub runtime_dir: PathBuf,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_cleanup: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceStartPreview {
    pub id: String,
    #[serde(default)]
    pub session: SessionSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    pub ok_to_start: bool,
    pub would_start: bool,
    pub already_running: bool,
    #[serde(default)]
    pub session_mismatch: bool,
    pub runtime_ready: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub user_acknowledged_hidden_workspace: bool,
    pub requires_hidden_workspace_ack: bool,
    pub missing_hidden_workspace_ack: bool,
    pub message: String,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceLaunchPreview {
    pub id: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    pub wait_window: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_timeout_ms: Option<u64>,
    pub screenshot_window: bool,
    pub workspace_running: bool,
    pub ok_to_launch: bool,
    pub would_launch: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    pub message: String,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceApp {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_path: Option<PathBuf>,
    pub started_at_unix: u64,
    pub running: bool,
    pub exit_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LaunchSpec {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceWindow {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wm_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wm_instance: Option<String>,
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub visible: bool,
    pub geometry: WindowGeometry,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub screen: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspacePointer {
    pub x: i32,
    pub y: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceScreenshot {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub bytes: u64,
    pub captured_at_unix: u64,
    #[serde(default)]
    pub source: WorkspaceScreenshotSource,
    #[serde(default)]
    pub target: WorkspaceScreenshotTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceScreenshotSource {
    #[default]
    WorkspaceScreenshot,
    WorkspaceObserve,
    WorkspaceScreenshotWindow,
    WorkspaceLaunchWindow,
    ViewerStream,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceScreenshotTarget {
    #[default]
    Root,
    Window,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceAppLog {
    pub app_id: String,
    pub stream: String,
    pub path: PathBuf,
    pub content: String,
    pub bytes_read: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceIpcInfo {
    pub protocol: String,
    pub protocol_version: u32,
    pub server_version: String,
    pub workspace_id: String,
    pub session_id: String,
    pub socket_path: PathBuf,
    pub transport: String,
    pub framing: String,
    pub encoding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEnvironment {
    pub workspace_id: String,
    pub session_id: String,
    pub display: String,
    pub xauthority_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub variables: Vec<EnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceClipboard {
    pub selection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceTerminal {
    pub workspace_id: String,
    pub terminal_id: String,
    pub session_name: String,
    pub tmux_socket_path: PathBuf,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceTerminalLaunchPlan {
    pub terminal_id: String,
    pub session_name: String,
    pub tmux_socket_path: PathBuf,
    pub target: String,
    pub title: String,
    pub command: Vec<String>,
    pub spec: LaunchSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceTerminalScreen {
    pub terminal: WorkspaceTerminal,
    pub text: String,
    pub lines: Vec<String>,
    pub line_count: usize,
    pub captured_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceTerminalInput {
    pub terminal: WorkspaceTerminal,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalized_keys: Vec<String>,
    #[serde(default)]
    pub text_bytes: usize,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_grammar: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceRun {
    pub app_id: String,
    pub launch: IpcResponse,
    pub wait: IpcResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill: Option<IpcResponse>,
    pub stdout: WorkspaceAppLog,
    pub stderr: WorkspaceAppLog,
    pub completed: bool,
    pub succeeded: bool,
    pub timed_out: bool,
    pub killed_on_timeout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceRunPreview {
    pub workspace_id: String,
    pub timeout_ms: Option<u64>,
    pub effective_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_bytes: Option<u64>,
    pub kill_on_timeout: bool,
    pub would_run: bool,
    pub launch: IpcResponse,
    #[serde(default)]
    pub approval: ApprovalBundle,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceEvent {
    pub sequence: u64,
    pub timestamp_unix: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum IpcRequest {
    IpcInfo,
    Environment,
    Status,
    LaunchApp {
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<EnvVar>,
        #[serde(default)]
        wait_window: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_timeout_ms: Option<u64>,
        #[serde(default)]
        screenshot_window: bool,
    },
    ListApps {
        app_id: Option<String>,
        name_contains: Option<String>,
        command_contains: Option<String>,
        profile_id: Option<String>,
        running: Option<bool>,
    },
    ListWindows {
        #[serde(default)]
        include_hidden: bool,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
    },
    ActiveWindow,
    Pointer,
    Observe {
        screenshot: bool,
        #[serde(default)]
        include_hidden: bool,
        output_path: Option<PathBuf>,
        #[serde(default)]
        include_events: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        events_tail: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        events_since_sequence: Option<u64>,
    },
    WaitWindow {
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
    },
    Screenshot {
        output_path: Option<PathBuf>,
        #[serde(default)]
        source: WorkspaceScreenshotSource,
    },
    ScreenshotWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        output_path: Option<PathBuf>,
        timeout_ms: u64,
    },
    FocusWindow {
        window_id: String,
    },
    FocusMatchingWindow {
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
    },
    CloseWindow {
        window_id: String,
        #[serde(default)]
        dry_run: bool,
    },
    CloseMatchingWindow {
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
        #[serde(default)]
        dry_run: bool,
    },
    MoveWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        x: i32,
        y: i32,
        timeout_ms: u64,
    },
    ResizeWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        width: u32,
        height: u32,
        timeout_ms: u64,
    },
    RaiseWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
    },
    MinimizeWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
    },
    ShowWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: u64,
    },
    Click {
        x: i32,
        y: i32,
        button: u8,
        count: u8,
    },
    ClickWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        x: i32,
        y: i32,
        button: u8,
        count: u8,
        timeout_ms: u64,
    },
    MovePointer {
        x: i32,
        y: i32,
    },
    MovePointerWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        x: i32,
        y: i32,
        timeout_ms: u64,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        button: u8,
    },
    DragWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        button: u8,
        timeout_ms: u64,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDirection,
        amount: u8,
    },
    ScrollWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        x: i32,
        y: i32,
        direction: ScrollDirection,
        amount: u8,
        timeout_ms: u64,
    },
    Key {
        key: String,
    },
    KeyWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        key: String,
        timeout_ms: u64,
    },
    TypeText {
        text: String,
    },
    TypeWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        text: String,
        timeout_ms: u64,
    },
    SetClipboard {
        text: String,
    },
    GetClipboard,
    PasteText {
        text: String,
        key: String,
    },
    PasteWindow {
        window_id: Option<String>,
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        text: String,
        key: String,
        timeout_ms: u64,
    },
    ReadAppLog {
        app_id: String,
        stream: String,
        tail_bytes: Option<u64>,
    },
    WaitApp {
        app_id: String,
        timeout_ms: u64,
        #[serde(default)]
        kill_on_timeout: bool,
    },
    BrowserTargets {
        app_id: Option<String>,
        user_data_dir: Option<PathBuf>,
        timeout_ms: Option<u64>,
    },
    BrowserSnapshot {
        app_id: Option<String>,
        user_data_dir: Option<PathBuf>,
        target_id: Option<String>,
        title_contains: Option<String>,
        url_contains: Option<String>,
        max_text_chars: Option<usize>,
        timeout_ms: Option<u64>,
    },
    BrowserSearchResults {
        app_id: Option<String>,
        user_data_dir: Option<PathBuf>,
        target_id: Option<String>,
        title_contains: Option<String>,
        url_contains: Option<String>,
        max_results: Option<usize>,
        min_vram_gb: Option<u32>,
        timeout_ms: Option<u64>,
    },
    BrowserNavigate {
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
    },
    ReadEvents {
        tail: Option<usize>,
        since_sequence: Option<u64>,
    },
    RecordEvent {
        kind: String,
        #[serde(default)]
        detail: serde_json::Value,
    },
    KillApp {
        app_id: String,
        #[serde(default)]
        dry_run: bool,
    },
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IpcResponse {
    pub ok: bool,
    pub message: String,
    pub status: Option<WorkspaceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_preview: Option<WorkspaceStartPreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_preview: Option<WorkspaceLaunchPreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc: Option<WorkspaceIpcInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<WorkspaceEnvironment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<WorkspaceApp>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<WorkspaceWindow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_window: Option<WorkspaceWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<WorkspacePointer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<WorkspaceScreenshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_log: Option<WorkspaceAppLog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<WorkspaceClipboard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<WorkspaceEvent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_targets: Option<crate::browser::WorkspaceBrowserTargets>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_snapshot: Option<crate::browser::WorkspaceBrowserSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_search_results: Option<crate::browser::WorkspaceBrowserSearchResults>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_navigate: Option<crate::browser::WorkspaceBrowserNavigate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<crate::agent::AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_handles: Option<crate::agent::AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_hints: Vec<String>,
}

pub fn default_workspace_id() -> String {
    DEFAULT_WORKSPACE_ID.to_string()
}

pub fn new_session_id(workspace_id: &str) -> String {
    format!(
        "session-{}-{}-{}",
        workspace_id,
        unix_now_millis(),
        std::process::id()
    )
}

pub fn doctor_report() -> DoctorReport {
    let runtime = RuntimeReport {
        xvfb: command_path_check("Xvfb"),
        xephyr: command_path_check("Xephyr"),
        xauth: command_path_check("xauth"),
        xdpyinfo: command_path_check("xdpyinfo"),
        xprop: command_path_check("xprop"),
        window_manager: first_available_command(&["openbox", "i3", "fluxbox"]),
        xdotool: command_path_check("xdotool"),
        dbus_daemon: command_path_check("dbus-daemon"),
        // Mirror the runtime capture order in capture_workspace_screenshot:
        // ffmpeg (cursor-capable) is preferred, then import/scrot. An
        // ffmpeg-only host can take screenshots, so it must read as ready here.
        screenshot: first_available_command(&["ffmpeg", "import", "scrot"]),
        clipboard: first_available_command(&["xclip", "xsel"]),
    };
    let viewer = ViewerRuntimeReport {
        host_display: host_display_check(),
        source_build_xkbcommon_x11: pkg_config_package_check("xkbcommon-x11"),
        host_opener: first_available_command(&["xdg-open", "gio"]),
    };

    let mut blockers = Vec::new();
    if !runtime.dbus_daemon.ok {
        blockers.push("Install dbus-daemon for the workspace's session bus.".to_string());
    }
    if !runtime.xvfb.ok && !runtime.xephyr.ok {
        blockers.push("Install Xvfb or Xephyr to create the workspace's X11 display.".to_string());
    }
    if !runtime.xauth.ok {
        blockers.push(
            "Install xauth so workspace displays can use a scoped authority file.".to_string(),
        );
    }
    if !runtime.xdpyinfo.ok {
        blockers.push("Install xdpyinfo so workspace display readiness can be probed.".to_string());
    }
    if !runtime.xprop.ok {
        blockers.push(
            "Install xprop so workspace windows can be associated with app process ids."
                .to_string(),
        );
    }
    if !runtime.window_manager.ok {
        blockers.push(
            "Install a lightweight window manager such as openbox, i3, or fluxbox.".to_string(),
        );
    }
    if !runtime.xdotool.ok {
        blockers.push(
            "Install xdotool for scoped input and window control inside the workspace.".to_string(),
        );
    }
    if !runtime.screenshot.ok {
        blockers.push(
            "Install ffmpeg (cursor capture), ImageMagick import, or scrot for workspace screenshots."
                .to_string(),
        );
    }

    let mut viewer_blockers = Vec::new();
    if !viewer.host_display.ok {
        viewer_blockers.push(
            "Start the MCP from a desktop session with DISPLAY or WAYLAND_DISPLAY, or use `mcp --headless` for hosts that must never open a viewer."
                .to_string(),
        );
    }

    let ready_for_x11_workspace = blockers.is_empty();
    let ready_for_host_viewer = viewer_blockers.is_empty();
    let recommended_next_step = if ready_for_x11_workspace {
        "Run `agent-workspace-linux workspace start`, then launch apps into the workspace."
            .to_string()
    } else {
        "Install the missing X11 workspace dependencies, then rerun `agent-workspace-linux doctor`."
            .to_string()
    };

    let gnome_blockers = crate::session::gnome::blockers();
    let kwin_blockers = crate::session::kwin::blockers();
    let i3_blockers = crate::session::i3::blockers();
    let hyprland_blockers = crate::session::hyprland::blockers();
    let cosmic_blockers = crate::session::cosmic::blockers();
    DoctorReport {
        session_runtimes: crate::session::runtime::catalog(),
        ready_for_i3_workspace: i3_blockers.is_empty(),
        i3_blockers,
        ready_for_gnome_workspace: gnome_blockers.is_empty(),
        gnome_blockers,
        ready_for_hyprland_workspace: hyprland_blockers.is_empty(),
        hyprland_blockers,
        ready_for_cosmic_workspace: cosmic_blockers.is_empty(),
        cosmic_blockers,
        ready_for_kwin_workspace: kwin_blockers.is_empty(),
        kwin_blockers,
        runtime,
        viewer,
        ready_for_x11_workspace,
        ready_for_host_viewer,
        blockers,
        viewer_blockers,
        recommended_next_step,
    }
}

pub fn start_workspace(options: WorkspaceStartOptions) -> Result<IpcResponse> {
    match prepare_workspace_start(options)? {
        WorkspaceStartPlan::AlreadyRunning(status) => Ok(IpcResponse {
            ok: true,
            message: format!("workspace {:?} is already running", status.id),
            apps: Some(status.apps.clone()),
            status: Some(status),
            start_preview: None,
            launch_preview: None,
            ipc: None,
            environment: None,
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
        }),
        WorkspaceStartPlan::Start(daemon_options) => {
            spawn_detached_daemon(&daemon_options)?;
            wait_for_daemon_startup(&daemon_options)?;
            request(&daemon_options.socket_path, IpcRequest::Status)
        }
    }
}

pub fn preview_workspace_start(options: WorkspaceStartOptions) -> Result<IpcResponse> {
    let preview = workspace_start_preview(options)?;
    Ok(IpcResponse {
        ok: true,
        message: "workspace start dry run returned".to_string(),
        status: None,
        start_preview: Some(preview),
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
    })
}

pub fn start_workspace_foreground(options: WorkspaceStartOptions) -> Result<()> {
    match prepare_workspace_start(options)? {
        WorkspaceStartPlan::AlreadyRunning(status) => {
            bail!(
                "workspace {:?} is already running on {}",
                status.id,
                status.display
            )
        }
        WorkspaceStartPlan::Start(daemon_options) => run_daemon(daemon_options),
    }
}

pub fn status_workspace(id: &str) -> Result<WorkspaceStatus> {
    let id = sanitize_workspace_id(id)?;
    let response = request(&workspace_socket_path(&id), IpcRequest::Status)?;
    response
        .status
        .ok_or_else(|| anyhow!("workspace daemon returned no status"))
}

pub fn read_manifest(id: &str) -> WorkspaceManifestRead {
    let id = match sanitize_workspace_id(id) {
        Ok(id) => id,
        Err(error) => {
            return WorkspaceManifestRead {
                ok: false,
                message: error.to_string(),
                id: id.to_string(),
                runtime_dir: PathBuf::new(),
                manifest_path: PathBuf::new(),
                manifest: None,
                manifest_error: Some(error.to_string()),
            };
        }
    };
    let runtime_dir = workspace_dir(&id);
    let manifest_path = runtime_dir.join(WORKSPACE_MANIFEST_FILE);
    match read_workspace_manifest(&runtime_dir) {
        Ok(Some(manifest)) => WorkspaceManifestRead {
            ok: true,
            message: "workspace manifest returned".to_string(),
            id,
            runtime_dir,
            manifest_path,
            manifest: Some(manifest),
            manifest_error: None,
        },
        Ok(None) => WorkspaceManifestRead {
            ok: false,
            message: "workspace manifest not found".to_string(),
            id,
            runtime_dir,
            manifest_path,
            manifest: None,
            manifest_error: None,
        },
        Err(error) => WorkspaceManifestRead {
            ok: false,
            message: error.to_string(),
            id,
            runtime_dir,
            manifest_path,
            manifest: None,
            manifest_error: Some(error.to_string()),
        },
    }
}

pub fn artifacts(id: &str, existing_only: bool) -> WorkspaceArtifacts {
    let id = match sanitize_workspace_id(id) {
        Ok(id) => id,
        Err(error) => {
            return WorkspaceArtifacts {
                ok: false,
                message: error.to_string(),
                id: id.to_string(),
                runtime_dir: PathBuf::new(),
                files: Vec::new(),
                manifest_error: Some(error.to_string()),
            };
        }
    };
    let runtime_dir = workspace_dir(&id);
    let (manifest, manifest_error) = match read_workspace_manifest(&runtime_dir) {
        Ok(manifest) => (manifest, None),
        Err(error) => (None, Some(error.to_string())),
    };
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();

    push_workspace_artifact(
        &mut files,
        &mut seen,
        "manifest",
        "workspace manifest",
        runtime_dir.join(WORKSPACE_MANIFEST_FILE),
    );
    let control_socket_path = manifest
        .as_ref()
        .map(|manifest| manifest.socket_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir.join("control.sock"));
    push_workspace_artifact(
        &mut files,
        &mut seen,
        "control_socket",
        "workspace control socket",
        control_socket_path,
    );
    let xauthority_path = manifest
        .as_ref()
        .map(|manifest| manifest.xauthority_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir.join("Xauthority"));
    push_workspace_artifact(
        &mut files,
        &mut seen,
        "xauthority",
        "workspace Xauthority",
        xauthority_path,
    );
    let event_log_path = manifest
        .as_ref()
        .map(|manifest| manifest.event_log_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir.join(EVENT_LOG_FILE));
    push_workspace_artifact(
        &mut files,
        &mut seen,
        "event_log",
        "workspace event log",
        event_log_path,
    );
    let daemon_stdout_path = manifest
        .as_ref()
        .map(|manifest| manifest.daemon_stdout_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir.join("daemon.out.log"));
    push_workspace_artifact(
        &mut files,
        &mut seen,
        "daemon_log",
        "daemon stdout",
        daemon_stdout_path,
    );
    let daemon_stderr_path = manifest
        .as_ref()
        .map(|manifest| manifest.daemon_stderr_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| runtime_dir.join("daemon.err.log"));
    push_workspace_artifact(
        &mut files,
        &mut seen,
        "daemon_log",
        "daemon stderr",
        daemon_stderr_path,
    );

    if let Some(manifest) = &manifest {
        for app in &manifest.apps {
            let label = app_label(app);
            if let Some(path) = &app.stdout_path {
                push_workspace_artifact(
                    &mut files,
                    &mut seen,
                    "app_log",
                    format!("{label} stdout"),
                    path.clone(),
                );
            }
            if let Some(path) = &app.stderr_path {
                push_workspace_artifact(
                    &mut files,
                    &mut seen,
                    "app_log",
                    format!("{label} stderr"),
                    path.clone(),
                );
            }
        }
    }
    add_workspace_screenshot_artifacts(&runtime_dir, &mut files, &mut seen);
    if existing_only {
        files.retain(|file| file.exists);
    }

    let ok = runtime_dir.exists() && manifest_error.is_none();
    let message = if ok {
        "workspace artifacts returned"
    } else if !runtime_dir.exists() {
        "workspace runtime directory not found"
    } else {
        "workspace artifacts returned with manifest error"
    }
    .to_string();

    WorkspaceArtifacts {
        ok,
        message,
        id,
        runtime_dir,
        files,
        manifest_error,
    }
}

pub fn ipc_info(id: &str) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(&workspace_socket_path(&id), IpcRequest::IpcInfo)
}

pub fn environment(id: &str) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(&workspace_socket_path(&id), IpcRequest::Environment)
}

pub fn list_workspaces() -> Result<WorkspaceList> {
    let runtime_base_dir = runtime_base_dir();
    if !runtime_base_dir.exists() {
        return Ok(WorkspaceList {
            runtime_base_dir,
            workspaces: Vec::new(),
        });
    }

    let mut workspaces = Vec::new();
    for entry in fs::read_dir(&runtime_base_dir)
        .with_context(|| format!("failed to read {}", runtime_base_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if sanitize_workspace_id(&id).is_err() {
            continue;
        }
        let runtime_dir = entry.path();
        let socket_path = runtime_dir.join("control.sock");
        let (manifest, manifest_error) = match read_workspace_manifest(&runtime_dir) {
            Ok(manifest) => (manifest, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let status_result = status_workspace(&id);
        let (running, status, error) = match status_result {
            Ok(status) => (true, Some(status), None),
            Err(error) => (false, None, Some(error.to_string())),
        };
        workspaces.push(WorkspaceListEntry {
            id,
            runtime_dir,
            socket_path,
            running,
            manifest,
            manifest_error,
            status,
            error,
        });
    }

    workspaces.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(WorkspaceList {
        runtime_base_dir,
        workspaces,
    })
}

pub fn cleanup_stale_workspaces(id: Option<String>, dry_run: bool) -> Result<WorkspaceCleanup> {
    let target_id = id.map(|id| sanitize_workspace_id(&id)).transpose()?;
    let list = list_workspaces()?;
    let mut candidates = Vec::new();
    let mut removed = Vec::new();
    let mut skipped = Vec::new();

    for workspace in list.workspaces {
        if let Some(target_id) = &target_id {
            if &workspace.id != target_id {
                continue;
            }
        }

        if workspace.running {
            skipped.push(WorkspaceCleanupEntry {
                id: workspace.id,
                runtime_dir: workspace.runtime_dir,
                reason: "workspace is running".to_string(),
                process_cleanup: Vec::new(),
            });
            continue;
        }

        let mut process_cleanup = match cleanup_stale_workspace_processes(
            workspace.manifest.as_ref(),
            &workspace.id,
            &workspace.runtime_dir,
            dry_run,
        ) {
            Ok(actions) => actions,
            Err(error) => {
                skipped.push(WorkspaceCleanupEntry {
                    id: workspace.id,
                    runtime_dir: workspace.runtime_dir,
                    reason: format!("terminal cleanup could not be verified: {error:#}"),
                    process_cleanup: Vec::new(),
                });
                continue;
            }
        };
        match crate::session::runtime::cleanup_stale_userspaces(&workspace.id, dry_run) {
            Ok(actions) => process_cleanup.extend(actions),
            Err(error) => {
                skipped.push(WorkspaceCleanupEntry {
                    id: workspace.id,
                    runtime_dir: workspace.runtime_dir,
                    reason: format!("prepared userspace cleanup could not be verified: {error:#}"),
                    process_cleanup,
                });
                continue;
            }
        }
        if dry_run {
            candidates.push(WorkspaceCleanupEntry {
                id: workspace.id,
                runtime_dir: workspace.runtime_dir,
                reason: "would remove stale workspace runtime".to_string(),
                process_cleanup,
            });
            continue;
        }

        match remove_stale_runtime(&workspace.runtime_dir) {
            Ok(()) => removed.push(WorkspaceCleanupEntry {
                id: workspace.id,
                runtime_dir: workspace.runtime_dir,
                reason: "removed stale workspace runtime".to_string(),
                process_cleanup,
            }),
            Err(error) => skipped.push(WorkspaceCleanupEntry {
                id: workspace.id,
                runtime_dir: workspace.runtime_dir,
                reason: error.to_string(),
                process_cleanup,
            }),
        }
    }

    Ok(WorkspaceCleanup {
        runtime_base_dir: list.runtime_base_dir,
        dry_run,
        candidates,
        removed,
        skipped,
    })
}

fn cleanup_stale_workspace_processes(
    manifest: Option<&WorkspaceManifest>,
    workspace_id: &str,
    runtime_dir: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    let Some(manifest) = manifest else {
        return Ok(Vec::new());
    };
    if manifest.id != workspace_id
        || (manifest.runtime_dir != runtime_dir
            && fs::canonicalize(&manifest.runtime_dir)? != fs::canonicalize(runtime_dir)?)
    {
        bail!("workspace manifest does not match the selected runtime");
    }
    let terminal_preview = terminal_cleanup::cleanup(
        &manifest.runtime_dir,
        &manifest.id,
        &manifest.session_id,
        &manifest.apps,
        true,
    )?;
    let mut actions = Vec::new();

    for app in &manifest.apps {
        cleanup_stale_app_process_group(
            app,
            manifest.process_start_ticks.as_ref(),
            dry_run,
            &mut actions,
        );
    }
    actions.extend(if dry_run {
        terminal_preview
    } else {
        terminal_cleanup::cleanup(
            &manifest.runtime_dir,
            &manifest.id,
            &manifest.session_id,
            &manifest.apps,
            false,
        )?
    });
    let recorded = manifest.process_start_ticks.as_ref();
    let window_manager_pid = recorded_process(
        manifest.window_manager_pid,
        recorded,
        |ticks| ticks.window_manager,
        "window manager or compositor",
        &mut actions,
    );
    let x_server_pid = recorded_process(
        manifest.x_server_pid,
        recorded,
        |ticks| ticks.x_server,
        "X server",
        &mut actions,
    );
    manifest.session.cleanup_stale_processes(
        window_manager_pid,
        x_server_pid,
        dry_run,
        &mut actions,
    );
    let daemon_pid = recorded_process(
        manifest.daemon_pid,
        recorded,
        |ticks| ticks.daemon,
        "workspace daemon",
        &mut actions,
    );
    if let Some(pid) = daemon_pid {
        cleanup_stale_process_pid(
            pid,
            "workspace daemon",
            &["agent-workspace-linux"],
            dry_run,
            &mut actions,
        );
    }

    Ok(actions)
}

/// Processes cleanup just signalled may still add entries while they exit
/// (a log line, a socket being unlinked), so a non-empty error is retried
/// within the same grace period they get to exit.
fn remove_stale_runtime(runtime_dir: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(APP_TERMINATE_GRACE_MS);
    let mut retried = false;
    loop {
        match fs::remove_dir_all(runtime_dir) {
            Err(error)
                if error.kind() == io::ErrorKind::DirectoryNotEmpty
                    && Instant::now() < deadline =>
            {
                retried = true;
                thread::sleep(Duration::from_millis(50));
            }
            // Gone between retries: removed is what was asked for.
            Err(error) if retried && error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => return result,
        }
    }
}

fn cleanup_stale_app_process_group(
    app: &WorkspaceApp,
    recorded: Option<&ProcessStartTicks>,
    dry_run: bool,
    actions: &mut Vec<String>,
) {
    if !app.running {
        return;
    }
    let app_label = app.name.as_deref().unwrap_or(app.id.as_str());
    let Some(pgid) = app.process_group_id else {
        actions.push(format!(
            "skipped app {} process group: no process group recorded",
            app_label
        ));
        return;
    };
    if pgid == 0 {
        return;
    }
    if !process_is_alive(app.pid) {
        actions.push(format!(
            "skipped app {} process group {pgid}: leader pid {} is not running",
            app_label, app.pid
        ));
        return;
    }
    if let Some(recorded) = recorded {
        if recorded.apps.get(&app.pid).copied() != process_start_ticks(app.pid) {
            actions.push(format!(
                "skipped app {} process group {pgid}: leader pid {} is no longer the app this workspace started",
                app_label, app.pid
            ));
            return;
        }
    }
    match linux_process_group_id(app.pid) {
        Some(actual_pgid) if actual_pgid == pgid => {}
        Some(actual_pgid) => {
            actions.push(format!(
                "skipped app {} process group {pgid}: leader pid {} is in process group {actual_pgid}",
                app_label, app.pid
            ));
            return;
        }
        None => {
            actions.push(format!(
                "skipped app {} process group {pgid}: could not verify leader pid {}",
                app_label, app.pid
            ));
            return;
        }
    }

    if dry_run {
        actions.push(format!(
            "would terminate app {} process group {pgid}",
            app_label
        ));
        return;
    }

    match terminate_process_group_best_effort(pgid, Some(app.pid)) {
        Ok(()) => actions.push(format!("terminated app {} process group {pgid}", app_label)),
        Err(error) => actions.push(format!(
            "failed to terminate app {} process group {pgid}: {error}",
            app_label
        )),
    }
}

pub(crate) fn cleanup_stale_process_pid(
    pid: u32,
    label: &str,
    expected_names: &[&str],
    dry_run: bool,
    actions: &mut Vec<String>,
) {
    if pid == 0 || !process_is_alive(pid) {
        return;
    }
    if process_is_zombie(pid) {
        actions.push(format!(
            "skipped {label} pid {pid}: process is already defunct"
        ));
        return;
    }
    if !process_matches_any_name(pid, expected_names) {
        actions.push(format!(
            "skipped {label} pid {pid}: process identity did not match {}",
            expected_names.join("/")
        ));
        return;
    }

    if dry_run {
        actions.push(format!("would terminate {label} pid {pid}"));
        return;
    }

    match terminate_process_pid_best_effort(pid) {
        Ok(()) => actions.push(format!("terminated {label} pid {pid}")),
        Err(error) => actions.push(format!("failed to terminate {label} pid {pid}: {error}")),
    }
}

fn terminate_process_group_best_effort(pgid: u32, leader_pid: Option<u32>) -> Result<()> {
    signal_process_group(pgid, SIGTERM)?;
    if wait_for_process_exit(leader_pid, Duration::from_millis(APP_TERMINATE_GRACE_MS)) {
        return Ok(());
    }
    signal_process_group(pgid, SIGKILL)?;
    let _ = wait_for_process_exit(leader_pid, Duration::from_millis(APP_TERMINATE_GRACE_MS));
    Ok(())
}

fn terminate_process_pid_best_effort(pid: u32) -> Result<()> {
    signal_process_pid(pid, SIGTERM)?;
    if wait_for_process_exit(Some(pid), Duration::from_millis(APP_TERMINATE_GRACE_MS)) {
        return Ok(());
    }
    signal_process_pid(pid, SIGKILL)?;
    let _ = wait_for_process_exit(Some(pid), Duration::from_millis(APP_TERMINATE_GRACE_MS));
    Ok(())
}

fn wait_for_process_exit(pid: Option<u32>, timeout: Duration) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    let started = Instant::now();
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn signal_process_pid(pid: u32, signal: i32) -> Result<()> {
    if pid > i32::MAX as u32 {
        bail!("process id {pid} is too large to signal");
    }
    let result = unsafe { kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to signal process pid {pid}"))
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

fn process_matches_any_name(pid: u32, expected_names: &[&str]) -> bool {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == b'\0')
                .filter_map(|part| std::str::from_utf8(part).ok())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    expected_names.iter().any(|name| {
        let name = *name;
        comm.trim() == name || cmdline.contains(name)
    })
}

fn process_is_zombie(pid: u32) -> bool {
    linux_process_state(pid) == Some('Z')
}

fn linux_process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_state_from_proc_stat(&stat)
}

fn process_state_from_proc_stat(stat: &str) -> Option<char> {
    let after_comm = stat.rsplit_once(") ")?.1;
    after_comm.split_whitespace().next()?.chars().next()
}

fn linux_process_group_id(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    fields.next()?.parse().ok()
}

pub fn launch_app_with_spec(id: &str, spec: LaunchSpec) -> Result<IpcResponse> {
    launch_app_with_options(id, spec, false, None, false)
}

pub fn launch_app_with_options(
    id: &str,
    spec: LaunchSpec,
    wait_window: bool,
    window_timeout_ms: Option<u64>,
    screenshot_window: bool,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_launch_spec(&spec)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::LaunchApp {
            command: spec.command,
            name: spec.name,
            profile_id: spec.profile_id,
            cwd: spec.cwd,
            env: spec.env,
            wait_window,
            window_timeout_ms,
            screenshot_window,
        },
    )
}

pub fn preview_launch_app(
    id: &str,
    spec: LaunchSpec,
    wait_window: bool,
    window_timeout_ms: Option<u64>,
    screenshot_window: bool,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let running_status = status_workspace(&id)
        .with_context(|| format!("workspace {id:?} daemon is required for launch dry run"))?;
    let spec = launch_spec_with_workspace_defaults(&running_status, spec);
    validate_launch_spec(&spec)?;

    let workspace_running = true;
    let blockers = Vec::new();
    let ok_to_launch = workspace_running;
    let would_launch = ok_to_launch;
    let message = "workspace launch would spawn app".to_string();

    let mut preview = WorkspaceLaunchPreview {
        id,
        command: spec.command,
        name: spec.name,
        profile_id: spec.profile_id,
        cwd: spec.cwd,
        env: spec.env,
        wait_window,
        window_timeout_ms,
        screenshot_window,
        workspace_running,
        ok_to_launch,
        would_launch,
        blockers,
        message,
        approval: ApprovalBundle::default(),
    };
    preview.approval = launch_approval_bundle(&preview);

    Ok(IpcResponse {
        ok: true,
        message: "workspace launch dry run returned".to_string(),
        status: Some(running_status),
        start_preview: None,
        launch_preview: Some(preview),
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
    })
}

fn launch_approval_bundle(preview: &WorkspaceLaunchPreview) -> ApprovalBundle {
    let subject = preview
        .name
        .clone()
        .unwrap_or_else(|| preview.command.join(" "));
    let mut bundle = ApprovalBundle::new(
        "workspace_launch",
        format!("workspace {} app {}", preview.id, subject),
        preview.would_launch,
    );
    for blocker in &preview.blockers {
        bundle = bundle.add_blocker(blocker.clone());
    }

    bundle
}

fn validate_launch_spec(spec: &LaunchSpec) -> Result<()> {
    validate_command(&spec.command, "launch")?;
    validate_optional_app_name(&spec.name)?;
    if let Some(cwd) = &spec.cwd {
        if !cwd.is_dir() {
            bail!("launch cwd {} is not a directory", cwd.display());
        }
    }
    for env_var in &spec.env {
        validate_env_var(env_var)?;
    }
    Ok(())
}

pub fn validate_command(command: &[String], subject: &str) -> Result<()> {
    if command.is_empty() {
        bail!("{subject} command cannot be empty");
    }
    if command[0].trim().is_empty() {
        bail!("{subject} command program cannot be empty");
    }
    for arg in command {
        if arg.contains('\0') {
            bail!("{subject} command cannot contain NUL bytes");
        }
    }
    Ok(())
}

pub fn list_apps(
    id: &str,
    app_id: Option<String>,
    name_contains: Option<String>,
    command_contains: Option<String>,
    profile_id: Option<String>,
    running: Option<bool>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_app_list_filters(&app_id, &name_contains, &command_contains, &profile_id)?;
    match request(
        &workspace_socket_path(&id),
        IpcRequest::ListApps {
            app_id: app_id.clone(),
            name_contains: name_contains.clone(),
            command_contains: command_contains.clone(),
            profile_id: profile_id.clone(),
            running,
        },
    ) {
        Ok(response) => Ok(response),
        Err(ipc_error) => list_apps_from_workspace_manifest(
            &id,
            &app_id,
            &name_contains,
            &command_contains,
            &profile_id,
            running,
        )?
        .ok_or(ipc_error),
    }
}

pub fn list_windows(
    id: &str,
    include_hidden: bool,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_list_filters(&title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ListWindows {
            include_hidden,
            title_contains,
            class_contains,
            pid,
            app_id,
        },
    )
}

pub fn active_window(id: &str) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(&workspace_socket_path(&id), IpcRequest::ActiveWindow)
}

pub fn pointer(id: &str) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(&workspace_socket_path(&id), IpcRequest::Pointer)
}

pub fn observe(
    id: &str,
    screenshot: bool,
    include_hidden: bool,
    output_path: Option<PathBuf>,
    include_events: bool,
    events_tail: Option<usize>,
    events_since_sequence: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::Observe {
            screenshot,
            include_hidden,
            output_path,
            include_events,
            events_tail,
            events_since_sequence,
        },
    )
}

pub fn wait_window(
    id: &str,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_match_options(&title_contains, &class_contains, pid, &app_id, false)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::WaitWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn screenshot(id: &str, output_path: Option<PathBuf>) -> Result<IpcResponse> {
    screenshot_with_source(
        id,
        output_path,
        WorkspaceScreenshotSource::WorkspaceScreenshot,
    )
}

pub fn screenshot_for_viewer_stream(id: &str, output_path: Option<PathBuf>) -> Result<IpcResponse> {
    screenshot_with_source(id, output_path, WorkspaceScreenshotSource::ViewerStream)
}

fn screenshot_with_source(
    id: &str,
    output_path: Option<PathBuf>,
    source: WorkspaceScreenshotSource,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::Screenshot {
            output_path,
            source,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn screenshot_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    output_path: Option<PathBuf>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ScreenshotWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            output_path,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn focus_window(id: &str, window_id: String) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let window_id = sanitize_window_id(&window_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::FocusWindow { window_id },
    )
}

pub fn focus_matching_window(
    id: &str,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_match_options(&title_contains, &class_contains, pid, &app_id, true)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::FocusMatchingWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn close_window(id: &str, window_id: String, dry_run: bool) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let window_id = sanitize_window_id(&window_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::CloseWindow { window_id, dry_run },
    )
}

pub fn close_matching_window(
    id: &str,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
    dry_run: bool,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_match_options(&title_contains, &class_contains, pid, &app_id, true)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::CloseMatchingWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
            dry_run,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn move_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    x: i32,
    y: i32,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::MoveWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resize_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    width: u32,
    height: u32,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_window_size(width, height)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ResizeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            width,
            height,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn raise_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::RaiseWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn minimize_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::MinimizeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn show_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ShowWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn click(
    id: &str,
    x: i32,
    y: i32,
    button: Option<u8>,
    count: Option<u8>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let button = button.unwrap_or(DEFAULT_CLICK_BUTTON);
    let count = count.unwrap_or(DEFAULT_CLICK_COUNT);
    validate_click_options(button, count)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::Click {
            x,
            y,
            button,
            count,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn click_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    x: i32,
    y: i32,
    button: Option<u8>,
    count: Option<u8>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let button = button.unwrap_or(DEFAULT_CLICK_BUTTON);
    let count = count.unwrap_or(DEFAULT_CLICK_COUNT);
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_relative_click_coordinates(x, y)?;
    validate_click_options(button, count)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ClickWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            button,
            count,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn move_pointer(id: &str, x: i32, y: i32) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::MovePointer { x, y },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn move_pointer_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    x: i32,
    y: i32,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_relative_click_coordinates(x, y)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::MovePointerWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn drag(
    id: &str,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    button: Option<u8>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let button = button.unwrap_or(DEFAULT_CLICK_BUTTON);
    validate_click_options(button, DEFAULT_CLICK_COUNT)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn drag_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    button: Option<u8>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let button = button.unwrap_or(DEFAULT_CLICK_BUTTON);
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_relative_click_coordinates(from_x, from_y)?;
    validate_relative_click_coordinates(to_x, to_y)?;
    validate_click_options(button, DEFAULT_CLICK_COUNT)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::DragWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            from_x,
            from_y,
            to_x,
            to_y,
            button,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn scroll(
    id: &str,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    amount: Option<u8>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let amount = amount.unwrap_or(DEFAULT_SCROLL_AMOUNT);
    validate_scroll_options(direction, amount)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::Scroll {
            x,
            y,
            direction,
            amount,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn scroll_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    amount: Option<u8>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let amount = amount.unwrap_or(DEFAULT_SCROLL_AMOUNT);
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_relative_click_coordinates(x, y)?;
    validate_scroll_options(direction, amount)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::ScrollWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            direction,
            amount,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn key(id: &str, key: String) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    if key.trim().is_empty() {
        bail!("key cannot be empty");
    }
    request(&workspace_socket_path(&id), IpcRequest::Key { key })
}

#[allow(clippy::too_many_arguments)]
pub fn key_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    key: String,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    if key.trim().is_empty() {
        bail!("key cannot be empty");
    }
    request(
        &workspace_socket_path(&id),
        IpcRequest::KeyWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            key,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn type_text(id: &str, text: String) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    if text.is_empty() {
        bail!("text cannot be empty");
    }
    request(&workspace_socket_path(&id), IpcRequest::TypeText { text })
}

#[allow(clippy::too_many_arguments)]
pub fn type_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    text: String,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    if text.is_empty() {
        bail!("text cannot be empty");
    }
    request(
        &workspace_socket_path(&id),
        IpcRequest::TypeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            text,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn set_clipboard(id: &str, text: String) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_clipboard_text(&text)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::SetClipboard { text },
    )
}

pub fn get_clipboard(id: &str) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(&workspace_socket_path(&id), IpcRequest::GetClipboard)
}

pub fn paste_text(id: &str, text: String, key: Option<String>) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_clipboard_text(&text)?;
    let key = normalize_paste_key(key)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::PasteText { text, key },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn paste_window(
    id: &str,
    window_id: Option<String>,
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    text: String,
    key: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    validate_window_target_options(&window_id, &title_contains, &class_contains, pid, &app_id)?;
    validate_clipboard_text(&text)?;
    let key = normalize_paste_key(key)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::PasteWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            text,
            key,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        },
    )
}

pub fn read_app_log(
    id: &str,
    app_id: String,
    stream: String,
    tail_bytes: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    if app_id.trim().is_empty() {
        bail!("app id cannot be empty");
    }
    let stream = validate_log_stream(&stream)?;
    match request(
        &workspace_socket_path(&id),
        IpcRequest::ReadAppLog {
            app_id: app_id.clone(),
            stream: stream.clone(),
            tail_bytes,
        },
    ) {
        Ok(response) => Ok(response),
        Err(ipc_error) => read_app_log_from_workspace_manifest(&id, &app_id, &stream, tail_bytes)?
            .ok_or(ipc_error),
    }
}

pub fn wait_app(
    id: &str,
    app_id: String,
    timeout_ms: Option<u64>,
    kill_on_timeout: bool,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    if app_id.trim().is_empty() {
        bail!("app id cannot be empty");
    }
    request(
        &workspace_socket_path(&id),
        IpcRequest::WaitApp {
            app_id,
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
            kill_on_timeout,
        },
    )
}

pub fn browser_targets(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::BrowserTargets {
            app_id,
            user_data_dir,
            timeout_ms,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn browser_snapshot(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::BrowserSnapshot {
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_text_chars,
            timeout_ms,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn browser_search_results(
    id: &str,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_results: Option<usize>,
    min_vram_gb: Option<u32>,
    timeout_ms: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::BrowserSearchResults {
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_results,
            min_vram_gb,
            timeout_ms,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub fn browser_navigate(
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
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::BrowserNavigate {
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
        },
    )
}

pub fn run_app_with_spec(
    id: &str,
    spec: LaunchSpec,
    timeout_ms: Option<u64>,
    tail_bytes: Option<u64>,
    kill_on_timeout: bool,
) -> Result<WorkspaceRun> {
    let launch = launch_app_with_spec(id, spec)?;
    let app_id =
        response_last_app_id(&launch).context("workspace launch did not return an app id")?;
    let wait = wait_app(id, app_id.clone(), timeout_ms, false)?;
    let completed = wait.ok;
    let timed_out = !completed;
    let kill = if timed_out && kill_on_timeout {
        Some(kill_app(id, app_id.clone(), false)?)
    } else {
        None
    };
    let stdout = read_app_log(id, app_id.clone(), "stdout".to_string(), tail_bytes)?
        .app_log
        .context("workspace stdout log response did not include app_log")?;
    let stderr = read_app_log(id, app_id.clone(), "stderr".to_string(), tail_bytes)?
        .app_log
        .context("workspace stderr log response did not include app_log")?;
    let exit_source = kill.as_ref().unwrap_or(&wait);
    let exit_code = response_app(exit_source, &app_id).and_then(|app| app.exit_code);
    let exit_signal = response_app(exit_source, &app_id).and_then(|app| app.exit_signal);
    let succeeded = completed && exit_code == Some(0);
    Ok(WorkspaceRun {
        app_id,
        launch,
        wait,
        kill,
        stdout,
        stderr,
        completed,
        succeeded,
        timed_out,
        killed_on_timeout: timed_out && kill_on_timeout,
        exit_code,
        exit_signal,
    })
}

pub fn preview_run_app_with_spec(
    id: &str,
    spec: LaunchSpec,
    timeout_ms: Option<u64>,
    tail_bytes: Option<u64>,
    kill_on_timeout: bool,
) -> Result<WorkspaceRunPreview> {
    let id = sanitize_workspace_id(id)?;
    let launch = preview_launch_app(&id, spec, false, None, false)?;
    let would_run = launch
        .launch_preview
        .as_ref()
        .is_some_and(|preview| preview.would_launch);
    let approval = launch
        .launch_preview
        .as_ref()
        .map(|preview| {
            preview.approval.clone().retarget(
                "workspace_run",
                format!("workspace {id} command"),
                would_run,
            )
        })
        .unwrap_or_else(|| {
            ApprovalBundle::new("workspace_run", format!("workspace {id} command"), false)
        });
    Ok(WorkspaceRunPreview {
        workspace_id: id,
        timeout_ms,
        effective_timeout_ms: timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS),
        tail_bytes,
        kill_on_timeout,
        would_run,
        launch,
        approval,
    })
}

pub fn terminal_launch_plan(
    id: &str,
    terminal_id: Option<String>,
    title: Option<String>,
    terminal_program: Option<PathBuf>,
    command: Vec<String>,
) -> Result<WorkspaceTerminalLaunchPlan> {
    let id = sanitize_workspace_id(id)?;
    let status = status_workspace(&id)?;
    if !command.is_empty() {
        validate_command(&command, "terminal command")?;
    }
    let terminal_id = sanitize_terminal_id(terminal_id.as_deref().unwrap_or("terminal"))?;
    let session_name = terminal_id.clone();
    let target = session_name.clone();
    let tmux_socket_path = terminal_socket_path(&status.runtime_dir, &terminal_id);
    validate_unix_socket_path(&tmux_socket_path)?;
    let terminal_program = match terminal_program {
        Some(path) => path,
        None => resolve_executable_path(DEFAULT_TERMINAL_PROGRAM)
            .context("xterm not found; pass terminal_program or install xterm")?,
    };
    let tmux_program = resolve_executable_path(DEFAULT_TMUX_PROGRAM)
        .context("tmux not found; install tmux to use workspace_run_in_terminal")?;
    let title = title.unwrap_or_else(|| format!("agent-workspace:{terminal_id}"));
    let mut launch_command = vec![
        terminal_program.display().to_string(),
        "-title".to_string(),
        title.clone(),
        "-fa".to_string(),
        DEFAULT_TERMINAL_FONT.to_string(),
        "-fs".to_string(),
        DEFAULT_TERMINAL_FONT_SIZE.to_string(),
        "-geometry".to_string(),
        DEFAULT_TERMINAL_GEOMETRY.to_string(),
        "-e".to_string(),
        tmux_program.display().to_string(),
        "-S".to_string(),
        tmux_socket_path.display().to_string(),
        "new-session".to_string(),
        "-A".to_string(),
        "-s".to_string(),
        session_name.clone(),
    ];
    if !command.is_empty() {
        launch_command.push("--".to_string());
        launch_command.extend(command.clone());
    }
    Ok(WorkspaceTerminalLaunchPlan {
        terminal_id,
        session_name,
        tmux_socket_path,
        target,
        title: title.clone(),
        command,
        spec: LaunchSpec {
            command: launch_command,
            name: Some(title),
            profile_id: None,
            cwd: None,
            env: Vec::new(),
        },
    })
}

pub fn run_in_terminal(
    id: &str,
    plan: WorkspaceTerminalLaunchPlan,
    wait_window: bool,
    window_timeout_ms: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<(IpcResponse, WorkspaceTerminal)> {
    let launch = launch_app_with_options(
        id,
        plan.spec.clone(),
        wait_window,
        window_timeout_ms.or(Some(DEFAULT_APP_WAIT_TIMEOUT_MS)),
        false,
    )?;
    if !launch.ok {
        bail!(launch.message);
    }
    let app_id = launch
        .apps
        .as_ref()
        .and_then(|apps| apps.first())
        .map(|app| app.id.clone());
    let terminal = wait_for_terminal(
        id,
        &plan.terminal_id,
        app_id,
        timeout_ms.unwrap_or(DEFAULT_TERMINAL_READY_TIMEOUT_MS),
    )?;
    Ok((launch, terminal))
}

pub fn read_terminal(
    id: &str,
    terminal_id: Option<String>,
    preserve_trailing_spaces: bool,
) -> Result<WorkspaceTerminalScreen> {
    let terminal_id = sanitize_terminal_id(terminal_id.as_deref().unwrap_or("terminal"))?;
    let terminal = terminal_info(id, &terminal_id, None)?;
    let output = tmux_capture_pane(&terminal, preserve_trailing_spaces)?;
    let lines = output.lines().map(str::to_string).collect::<Vec<_>>();
    Ok(WorkspaceTerminalScreen {
        terminal,
        line_count: lines.len(),
        text: output,
        lines,
        captured_at_unix: unix_now(),
    })
}

pub fn terminal_input(
    id: &str,
    terminal_id: Option<String>,
    keys: Vec<String>,
    text: Option<String>,
    delay_ms: Option<u64>,
) -> Result<WorkspaceTerminalInput> {
    let terminal_id = sanitize_terminal_id(terminal_id.as_deref().unwrap_or("terminal"))?;
    let terminal = terminal_info(id, &terminal_id, None)?;
    let text = text.unwrap_or_default();
    if keys.is_empty() && text.is_empty() {
        bail!("terminal input requires keys or text");
    }
    let delay_ms = delay_ms.unwrap_or(0).min(5_000);
    let text_bytes = text.len();
    if !text.is_empty() {
        tmux_send_literal_text(&terminal, &text)?;
        if delay_ms > 0 && !keys.is_empty() {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }
    let normalized_keys = normalize_terminal_keys(&keys)?;
    if !normalized_keys.is_empty() {
        if delay_ms == 0 {
            tmux_send_keys(&terminal, &normalized_keys)?;
        } else {
            for key in &normalized_keys {
                tmux_send_keys(&terminal, std::slice::from_ref(key))?;
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }
    let refreshed = terminal_info(id, &terminal_id, terminal.app_id.clone()).unwrap_or(terminal);
    let input = WorkspaceTerminalInput {
        terminal: refreshed.clone(),
        keys,
        normalized_keys,
        text_bytes,
        delay_ms,
        key_grammar: terminal_key_grammar(),
    };
    let _ = record_workspace_event(
        id,
        "terminal_input",
        serde_json::json!({
            "terminal_id": refreshed.terminal_id,
            "target": refreshed.target,
            "pane_id": refreshed.pane_id,
            "key_count": input.normalized_keys.len(),
            "text_bytes": input.text_bytes,
            "delay_ms": input.delay_ms,
            "raw_text_omitted": !text.is_empty(),
        }),
    );
    Ok(input)
}

pub fn read_events(
    id: &str,
    tail: Option<usize>,
    since_sequence: Option<u64>,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    match request(
        &workspace_socket_path(&id),
        IpcRequest::ReadEvents {
            tail,
            since_sequence,
        },
    ) {
        Ok(response) => Ok(response),
        Err(ipc_error) => {
            read_events_from_workspace_log(&id, tail, since_sequence)?.ok_or(ipc_error)
        }
    }
}

pub fn record_workspace_event(
    id: &str,
    kind: impl Into<String>,
    detail: serde_json::Value,
) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    request(
        &workspace_socket_path(&id),
        IpcRequest::RecordEvent {
            kind: kind.into(),
            detail,
        },
    )
}

pub fn kill_app(id: &str, app_id: String, dry_run: bool) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    if app_id.trim().is_empty() {
        bail!("app id cannot be empty");
    }
    request(
        &workspace_socket_path(&id),
        IpcRequest::KillApp { app_id, dry_run },
    )
}

pub fn stop_workspace(id: &str, timeout_ms: Option<u64>, dry_run: bool) -> Result<IpcResponse> {
    let id = sanitize_workspace_id(id)?;
    let socket_path = workspace_socket_path(&id);
    if dry_run {
        let mut response = request(
            &socket_path,
            IpcRequest::ListApps {
                app_id: None,
                name_contains: None,
                command_contains: None,
                profile_id: None,
                running: Some(true),
            },
        )?;
        response.message = "workspace stop dry run".to_string();
        return Ok(response);
    }
    let mut response = request(&socket_path, IpcRequest::Stop)?;
    if response.ok {
        wait_for_socket_removed(
            &socket_path,
            Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_STOP_WAIT_TIMEOUT_MS)),
        )?;
        response.message = "workspace stopped".to_string();
    }
    Ok(response)
}

pub fn run_daemon(mut options: DaemonOptions) -> Result<()> {
    let id = sanitize_workspace_id(&options.id)?;
    let session_id = normalize_session_id(&options.session_id)?;
    validate_unix_socket_path(&options.socket_path)?;
    create_private_runtime_dir(&options.runtime_dir)?;
    fs::write(
        options.runtime_dir.join(DAEMON_PID_FILE),
        format!("{}\n", std::process::id()),
    )
    .context("record workspace daemon pid")?;
    if options.session.version.is_some() && !crate::session::runtime::inside(options.session) {
        if id != options.id || session_id != options.session_id {
            bail!("versioned runtime requires normalized workspace and session ids");
        }
        let result = daemon_command(&options, false).and_then(|mut command| Ok(command.status()?));
        let cleanup = crate::session::runtime::cleanup_userspace(&options);
        let status = result?;
        cleanup.with_context(|| {
            format!("cleanup selected runtime after daemon exited with {status}")
        })?;
        if !status.success() {
            bail!("versioned workspace daemon exited with {status}");
        }
        return Ok(());
    }
    if options.session.version.is_some() {
        control::claim_shared_control_directory()?;
        crate::session::runtime::record_inner_userspace(&options)?;
    }
    options.purpose = normalize_workspace_purpose(options.purpose)?;
    remove_stale_socket(&options.socket_path)?;

    // Load the spawn-time MCP permission ceiling (if any) before serving any
    // IPC. Fail closed: if a ceiling path was provided but cannot be read or
    // parsed, refuse to start rather than serve launches without the ceiling.
    let permissions = match options.permissions_source.take() {
        Some(path) => load_mcp_permission_state(path)?,
        None => McpPermissionState::default(),
    };
    control::ensure_control_state_initialized(
        "workspace_daemon",
        Some("initialize missing live control state for workspace daemon".to_string()),
    )?;

    let mut session = options.session.start(&mut options)?;

    let listener = UnixListener::bind(&options.socket_path)
        .with_context(|| format!("failed to bind {}", options.socket_path.display()))?;
    fs::set_permissions(
        &options.socket_path,
        fs::Permissions::from_mode(PRIVATE_SOCKET_MODE),
    )
    .with_context(|| {
        format!(
            "failed to set private permissions on {}",
            options.socket_path.display()
        )
    })?;
    let event_path = options.runtime_dir.join(EVENT_LOG_FILE);
    let started_at_unix = unix_now();
    let mut state = DaemonState {
        status: WorkspaceStatus {
            id,
            session: options.session,
            session_id,
            purpose: options.purpose,
            profile_id: options.profile_id,
            profile_cwd: options.profile_cwd,
            profile_env: options.profile_env,
            user_acknowledged_hidden_workspace: options.user_acknowledged_hidden_workspace,
            ready: true,
            started_at_unix,
            display: options.display,
            width: options.width,
            height: options.height,
            runtime_dir: options.runtime_dir,
            socket_path: options.socket_path,
            xauthority_path: options.xauthority_path,
            daemon_pid: Some(std::process::id()),
            x_server_pid: session.x_server_pid(),
            window_manager_pid: session.window_manager_pid(),
            last_event_sequence: 0,
            apps: Vec::new(),
        },
        apps: Vec::new(),
        window_app_ids: BTreeMap::new(),
        event_path,
        next_event_sequence: 1,
        permissions,
    };
    write_workspace_manifest(&state.status, None)?;
    let start_detail = serde_json::json!({
        "session": &state.status.session,
        "session_id": &state.status.session_id,
        "display": &state.status.display,
        "width": state.status.width,
        "height": state.status.height,
        "purpose": state.status.purpose.as_deref(),
        "profile_id": state.status.profile_id.as_deref(),
        "started_at_unix": state.status.started_at_unix,
        "user_acknowledged_hidden_workspace": state.status.user_acknowledged_hidden_workspace,
    });
    record_event(&mut state, "workspace_start", start_detail)?;

    eprintln!(
        "agent workspace daemon listening on {} for display {}",
        state.status.socket_path.display(),
        state.status.display
    );
    loop {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(error) => {
                eprintln!("workspace IPC accept failed: {error}");
                continue;
            }
        };
        let should_stop = match handle_stream(stream, &mut state) {
            Ok(should_stop) => should_stop,
            Err(error) => {
                eprintln!("workspace IPC request failed: {error:#}");
                false
            }
        };
        if should_stop {
            break;
        }
    }

    eprintln!("agent workspace daemon stopping");
    let _ = terminate_running_workspace_apps(&mut state);
    state.status.ready = false;
    write_workspace_manifest(&state.status, Some(unix_now()))?;
    session.shutdown();
    let _ = fs::remove_file(&state.status.socket_path);
    Ok(())
}

fn workspace_start_preview(options: WorkspaceStartOptions) -> Result<WorkspaceStartPreview> {
    let id = sanitize_workspace_id(&options.id)?;
    let purpose = normalize_workspace_purpose(options.purpose)?;
    let running_status = status_workspace(&id).ok();
    let already_running = running_status.is_some();
    let session_mismatch = running_status
        .as_ref()
        .is_some_and(|status| !status.session.matches(options.session));
    options.session.validate()?;
    let socket_path = workspace_socket_path(&id);
    let socket_path_error = validate_unix_socket_path(&socket_path)
        .err()
        .map(|error| error.to_string());
    let mut runtime_blockers = options.session.blockers();
    if let Some(error) = &socket_path_error {
        runtime_blockers.push(error.clone());
    }
    let runtime_ready = runtime_blockers.is_empty();
    let requires_hidden_workspace_ack = !already_running;
    let missing_hidden_workspace_ack =
        requires_hidden_workspace_ack && !options.user_acknowledged_hidden_workspace;
    let ok_to_start =
        !session_mismatch && (already_running || (runtime_ready && !missing_hidden_workspace_ack));
    let would_start = !already_running && ok_to_start;
    let message = if session_mismatch {
        session_mismatch_message(
            &id,
            &options.session,
            &running_status.as_ref().expect("running status").session,
        )
    } else if already_running {
        format!("workspace {id:?} is already running")
    } else if missing_hidden_workspace_ack {
        "workspace start would require hidden-workspace acknowledgement".to_string()
    } else if !runtime_ready {
        format!(
            "workspace runtime is not ready: {}",
            runtime_blockers.join("; ")
        )
    } else {
        "workspace start would create a new hidden workspace".to_string()
    };

    let mut preview = WorkspaceStartPreview {
        id,
        session: options.session,
        purpose,
        ok_to_start,
        would_start,
        already_running,
        session_mismatch,
        runtime_ready,
        runtime_blockers,
        profile_id: options.profile_id,
        user_acknowledged_hidden_workspace: options.user_acknowledged_hidden_workspace,
        requires_hidden_workspace_ack,
        missing_hidden_workspace_ack,
        message,
        approval: ApprovalBundle::default(),
    };
    preview.approval = start_approval_bundle(&preview);
    Ok(preview)
}

fn start_approval_bundle(preview: &WorkspaceStartPreview) -> ApprovalBundle {
    let mut bundle = ApprovalBundle::new(
        "workspace_start",
        format!("workspace {}", preview.id),
        preview.ok_to_start,
    )
    .require_acknowledgement(
        preview.requires_hidden_workspace_ack,
        hidden_workspace_acknowledgement(preview.user_acknowledged_hidden_workspace),
    );

    if preview.session_mismatch {
        bundle = bundle.add_blocker(preview.message.clone());
    }
    if !preview.runtime_ready {
        bundle = bundle.add_blockers(preview.runtime_blockers.clone());
    }

    bundle
}

fn session_mismatch_message(id: &str, requested: &SessionSpec, existing: &SessionSpec) -> String {
    format!(
        "workspace {id:?} is already running with session {existing}; requested session {requested} differs"
    )
}

fn ensure_session_matches(id: &str, requested: &SessionSpec, existing: &SessionSpec) -> Result<()> {
    if !requested.matches(*existing) {
        bail!("{}", session_mismatch_message(id, requested, existing));
    }
    Ok(())
}

enum WorkspaceStartPlan {
    AlreadyRunning(WorkspaceStatus),
    Start(DaemonOptions),
}

fn prepare_workspace_start(options: WorkspaceStartOptions) -> Result<WorkspaceStartPlan> {
    let id = sanitize_workspace_id(&options.id)?;
    let purpose = normalize_workspace_purpose(options.purpose)?;
    if let Ok(status) = status_workspace(&id) {
        ensure_session_matches(&id, &options.session, &status.session)?;
        return Ok(WorkspaceStartPlan::AlreadyRunning(status));
    }
    if !options.user_acknowledged_hidden_workspace {
        bail!(
            "starting a hidden agent workspace requires explicit acknowledgement; pass --ack-hidden-workspace or set acknowledge_hidden_workspace=true"
        );
    }
    options.session.validate()?;
    let blockers = options.session.blockers();
    if !blockers.is_empty() {
        bail!("workspace runtime is not ready: {}", blockers.join("; "));
    }

    let runtime_dir = workspace_dir(&id);
    let socket_path = runtime_dir.join("control.sock");
    validate_unix_socket_path(&socket_path)?;
    // Native compositors bind the Wayland display socket, the longest fixed
    // socket name in the runtime; reject paths they cannot bind up front.
    if !matches!(
        options.session.backend,
        crate::session::SessionBackend::X11 | crate::session::SessionBackend::I3
    ) {
        validate_unix_socket_path(&runtime_dir.join(crate::session::kwin::WAYLAND_SOCKET))?;
    }
    create_private_runtime_dir(&runtime_dir)?;
    create_client_runtime(&runtime_dir)?;
    remove_stale_socket(&socket_path)?;
    remove_stale_socket(&runtime_dir.join(DAEMON_PID_FILE))?;
    let xauthority_path = runtime_dir.join("Xauthority");
    let display = if matches!(
        options.session.backend,
        crate::session::SessionBackend::X11 | crate::session::SessionBackend::I3
    ) {
        let display = pick_display()?;
        create_xauthority(&display, &xauthority_path)?;
        display
    } else {
        String::new()
    };
    let session_id = new_session_id(&id);

    Ok(WorkspaceStartPlan::Start(DaemonOptions {
        id,
        session: options.session,
        session_id,
        purpose,
        profile_id: options.profile_id,
        profile_cwd: options.profile_cwd,
        profile_env: options.profile_env,
        user_acknowledged_hidden_workspace: options.user_acknowledged_hidden_workspace,
        display,
        width: options.width,
        height: options.height,
        runtime_dir,
        socket_path,
        xauthority_path,
        permissions_source: options.permissions_source,
    }))
}

fn spawn_detached_daemon(options: &DaemonOptions) -> Result<()> {
    let stdout_path = options.runtime_dir.join("daemon.out.log");
    let stderr_path = options.runtime_dir.join("daemon.err.log");
    let mut daemon = daemon_command(options, true)?;
    daemon
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let status = daemon
        .spawn()
        .context("failed to spawn agent workspace daemon launcher")?
        .wait()
        .context("failed to wait for agent workspace daemon launcher")?;
    if !status.success() {
        bail!("agent workspace daemon launcher exited with {status}");
    }
    Ok(())
}

fn daemon_command(options: &DaemonOptions, detached: bool) -> Result<Command> {
    let prefix = if !detached && options.session.version.is_some() {
        crate::session::runtime::daemon_prefix(options)?
    } else {
        vec![daemon_executable_path()?.into_os_string()]
    };
    let mut daemon = if detached {
        let mut command = Command::new("setsid");
        command.arg("--fork").args(&prefix);
        command
    } else {
        let mut command = Command::new(&prefix[0]);
        command.args(&prefix[1..]);
        command
    };
    daemon
        .arg("daemon")
        .arg("--protocol")
        .arg(IPC_PROTOCOL_VERSION.to_string())
        .arg("--id")
        .arg(&options.id);
    daemon.arg("--session-id").arg(&options.session_id);
    daemon
        .arg("--session")
        .arg(options.session.backend.as_str());
    if let Some(version) = options.session.version {
        daemon.arg("--session-version").arg(version.to_string());
    }
    daemon.arg("--scale").arg(options.session.scale.to_string());
    if let Some(purpose) = &options.purpose {
        daemon.arg("--purpose").arg(purpose);
    }
    if let Some(profile_id) = &options.profile_id {
        daemon.arg("--profile").arg(profile_id);
    }
    if let Some(profile_cwd) = &options.profile_cwd {
        daemon.arg("--profile-cwd").arg(profile_cwd);
    }
    for env_var in &options.profile_env {
        daemon
            .arg("--profile-env")
            .arg(format!("{}={}", env_var.name, env_var.value));
    }
    if let Some(permissions_source) = &options.permissions_source {
        daemon.arg("--permissions").arg(permissions_source);
    }
    if options.user_acknowledged_hidden_workspace {
        daemon.arg("--ack-hidden-workspace");
    }
    daemon
        .arg("--display")
        .arg(&options.display)
        .arg("--width")
        .arg(options.width.to_string())
        .arg("--height")
        .arg(options.height.to_string())
        .arg("--runtime-dir")
        .arg(&options.runtime_dir)
        .arg("--socket")
        .arg(&options.socket_path)
        .arg("--xauthority")
        .arg(&options.xauthority_path);
    if !detached && options.session.version.is_some() {
        crate::session::runtime::share_runtime_descriptors(&mut daemon, options)?;
    }
    Ok(daemon)
}

pub(crate) fn daemon_executable_path() -> Result<PathBuf> {
    let exe = env::current_exe().context("failed to resolve current executable")?;
    if exe.exists() {
        return Ok(exe);
    }
    if let Some(stable_path) = strip_linux_deleted_exe_suffix(&exe) {
        if stable_path.exists() {
            return Ok(stable_path);
        }
    }
    Ok(exe)
}

fn strip_linux_deleted_exe_suffix(path: &Path) -> Option<PathBuf> {
    let path = path.as_os_str().to_string_lossy();
    path.strip_suffix(LINUX_DELETED_EXE_SUFFIX)
        .map(PathBuf::from)
}

fn create_client_runtime(runtime: &Path) -> Result<()> {
    create_owned_private_dir(&runtime.join("client"), "workspace client runtime")
}

fn create_owned_private_dir(path: &Path, what: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new()
        .mode(PRIVATE_RUNTIME_DIR_MODE)
        .create(path)
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_owned_dir(path, what).map(drop)
        }
        Err(error) => Err(error).with_context(|| format!("create {what}")),
    }
}

/// Terminals started before the client runtime split keep their tmux server at
/// the runtime root. Attach there only while that server still accepts
/// connections; a stale socket file must not steer new launches or lookups away
/// from the per-terminal client directory.
fn terminal_socket_path(runtime: &Path, id: &str) -> PathBuf {
    let legacy = runtime.join(format!("terminal-{id}.tmux.sock"));
    if UnixStream::connect(&legacy).is_ok() {
        legacy
    } else {
        client_terminal_socket_path(runtime, id)
    }
}

fn client_terminal_socket_path(runtime: &Path, id: &str) -> PathBuf {
    runtime
        .join("client")
        .join(format!("terminal-{id}"))
        .join("tmux.sock")
}

/// `path` is `ROOT/agent-workspace-linux/ID`. ROOT may be the predictable
/// `/tmp` fallback, so never follow a symlink or reuse a directory another user
/// created or can write: either would let them swap sockets and auth files.
fn create_private_runtime_dir(path: &Path) -> Result<()> {
    let base = path
        .parent()
        .context("workspace runtime directory has no parent")?;
    let root = base
        .parent()
        .context("workspace runtime base has no parent")?;
    if !root.exists() {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_RUNTIME_DIR_MODE)
            .create(root)
            .with_context(|| format!("failed to create {}", root.display()))?;
    }
    let metadata = verify_owned_dir(root, "workspace runtime root")?;
    // XDG requires the runtime root to be private to its user. Group
    // membership cannot be proven private (members by primary gid are not
    // listed), so any group or world write is refused.
    if metadata.mode() & 0o022 != 0 {
        bail!(
            "workspace runtime root {} is writable by other users; use a private XDG_RUNTIME_DIR",
            root.display()
        );
    }
    for (dir, what) in [
        (base, "workspace runtime base"),
        (path, "workspace runtime directory"),
    ] {
        create_owned_private_dir(dir, what)?;
        // Older releases created these with the process umask (often 0775).
        fs::set_permissions(dir, fs::Permissions::from_mode(PRIVATE_RUNTIME_DIR_MODE))
            .with_context(|| format!("failed to set private permissions on {}", dir.display()))?;
    }
    Ok(())
}

fn verify_owned_dir(path: &Path, what: &str) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {what} {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "{what} {} must be a real directory owned by this user",
            path.display()
        );
    }
    Ok(metadata)
}

fn workspace_manifest(status: &WorkspaceStatus, stopped_at_unix: Option<u64>) -> WorkspaceManifest {
    WorkspaceManifest {
        id: status.id.clone(),
        session: status.session,
        session_id: status.session_id.clone(),
        purpose: status.purpose.clone(),
        profile_id: status.profile_id.clone(),
        profile_cwd: status.profile_cwd.clone(),
        profile_env: status.profile_env.clone(),
        user_acknowledged_hidden_workspace: status.user_acknowledged_hidden_workspace,
        ready: status.ready,
        started_at_unix: status.started_at_unix,
        stopped_at_unix,
        runtime_seconds: stopped_at_unix
            .and_then(|stopped| stopped.checked_sub(status.started_at_unix)),
        display: status.display.clone(),
        width: status.width,
        height: status.height,
        runtime_dir: status.runtime_dir.clone(),
        socket_path: status.socket_path.clone(),
        xauthority_path: status.xauthority_path.clone(),
        daemon_pid: status.daemon_pid,
        x_server_pid: Some(status.x_server_pid),
        window_manager_pid: status.window_manager_pid,
        event_log_path: status.runtime_dir.join(EVENT_LOG_FILE),
        daemon_stdout_path: status.runtime_dir.join("daemon.out.log"),
        daemon_stderr_path: status.runtime_dir.join("daemon.err.log"),
        last_event_sequence: status.last_event_sequence,
        apps: status.apps.clone(),
        process_start_ticks: Some(ProcessStartTicks {
            daemon: status.daemon_pid.and_then(process_start_ticks),
            x_server: Some(status.x_server_pid)
                .filter(|pid| *pid != 0)
                .and_then(process_start_ticks),
            window_manager: status.window_manager_pid.and_then(process_start_ticks),
            apps: status
                .apps
                .iter()
                .filter(|app| app.running)
                .filter_map(|app| process_start_ticks(app.pid).map(|ticks| (app.pid, ticks)))
                .collect(),
        }),
    }
}

pub(crate) fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_start_ticks_from_proc_stat(&stat)
}

/// Field 22 of /proc/PID/stat; fields after the parenthesised comm start at 3.
fn process_start_ticks_from_proc_stat(stat: &str) -> Option<u64> {
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Pass a recorded pid on to stale cleanup only while it is still the process
/// the manifest recorded. Manifests from older releases carry no start times
/// and keep the name-based identity check alone.
fn recorded_process(
    pid: Option<u32>,
    recorded: Option<&ProcessStartTicks>,
    ticks: impl Fn(&ProcessStartTicks) -> Option<u64>,
    label: &str,
    actions: &mut Vec<String>,
) -> Option<u32> {
    let pid = pid.filter(|pid| *pid != 0)?;
    let Some(recorded) = recorded else {
        return Some(pid);
    };
    if ticks(recorded).is_some_and(|expected| process_start_ticks(pid) == Some(expected)) {
        return Some(pid);
    }
    if process_is_alive(pid) {
        actions.push(format!(
            "skipped {label} pid {pid}: it is no longer the process this workspace started"
        ));
    }
    None
}

fn write_workspace_manifest(
    status: &WorkspaceStatus,
    stopped_at_unix: Option<u64>,
) -> Result<PathBuf> {
    let manifest_path = status.runtime_dir.join(WORKSPACE_MANIFEST_FILE);
    let content = serde_json::to_string_pretty(&workspace_manifest(status, stopped_at_unix))
        .context("failed to serialize workspace manifest")?;
    fs::write(&manifest_path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(manifest_path)
}

fn read_workspace_manifest(runtime_dir: &Path) -> Result<Option<WorkspaceManifest>> {
    let manifest_path = runtime_dir.join(WORKSPACE_MANIFEST_FILE);
    if !manifest_path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok(Some(manifest))
}

fn push_workspace_artifact(
    files: &mut Vec<WorkspaceArtifact>,
    seen: &mut BTreeSet<PathBuf>,
    kind: impl Into<String>,
    label: impl Into<String>,
    path: PathBuf,
) {
    if path.as_os_str().is_empty() || !seen.insert(path.clone()) {
        return;
    }
    let metadata = fs::symlink_metadata(&path).ok();
    let file_type = metadata.as_ref().map(artifact_file_type);
    let bytes = metadata
        .as_ref()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len());
    files.push(WorkspaceArtifact {
        kind: kind.into(),
        label: label.into(),
        path,
        exists: metadata.is_some(),
        file_type,
        bytes,
    });
}

fn artifact_file_type(metadata: &fs::Metadata) -> String {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        "file"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_socket() {
        "socket"
    } else if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_block_device() {
        "block_device"
    } else if file_type.is_char_device() {
        "char_device"
    } else {
        "other"
    }
    .to_string()
}

fn add_workspace_screenshot_artifacts(
    runtime_dir: &Path,
    files: &mut Vec<WorkspaceArtifact>,
    seen: &mut BTreeSet<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(runtime_dir) else {
        return;
    };
    let mut screenshots = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("screenshot") && name.ends_with(".png") {
            screenshots.push(path);
        }
    }
    screenshots.sort();
    for path in screenshots {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("screenshot")
            .to_string();
        push_workspace_artifact(files, seen, "screenshot", label, path);
    }
}

fn record_event(
    state: &mut DaemonState,
    kind: &str,
    detail: serde_json::Value,
) -> Result<WorkspaceEvent> {
    let event = WorkspaceEvent {
        sequence: state.next_event_sequence,
        timestamp_unix: unix_now(),
        kind: kind.to_string(),
        detail,
    };
    state.next_event_sequence += 1;
    state.status.last_event_sequence = event.sequence;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.event_path)
        .with_context(|| format!("failed to open {}", state.event_path.display()))?;
    serde_json::to_writer(&mut file, &event).context("failed to serialize workspace event")?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to write {}", state.event_path.display()))?;
    write_workspace_manifest(&state.status, None)?;
    Ok(event)
}

fn validate_external_event_kind(kind: &str) -> Result<()> {
    if matches!(
        kind,
        "browser_snapshot"
            | "browser_search_results"
            | "browser_navigate"
            | "browser_click"
            | "terminal_input"
    ) {
        Ok(())
    } else {
        bail!("external workspace event kind {kind:?} is not allowed")
    }
}

fn scalar_event_metadata(value: &serde_json::Value) -> bool {
    value.is_null() || value.is_boolean() || value.is_number() || value.is_string()
}

fn copy_event_metadata_field(
    input: &serde_json::Value,
    output: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
) {
    if let Some(value) = input.get(field) {
        if scalar_event_metadata(value) {
            output.insert(field.to_string(), value.clone());
        }
    }
}

fn metadata_only_browser_event_detail(
    kind: &str,
    detail: serde_json::Value,
) -> Result<serde_json::Value> {
    validate_external_event_kind(kind)?;
    let fields = match kind {
        "browser_snapshot" => &[
            "app_id",
            "target_id",
            "title",
            "url",
            "text_chars",
            "text_truncated",
        ][..],
        "browser_search_results" => &[
            "app_id",
            "target_id",
            "title",
            "url",
            "result_count",
            "min_vram_gb",
        ][..],
        "browser_navigate" => &[
            "app_id",
            "target_id",
            "url",
            "frame_id",
            "snapshot",
            "title",
            "current_url",
        ][..],
        "browser_click" => &[
            "app_id",
            "target_id",
            "match_kind",
            "selector",
            "text_query",
            "viewport_x",
            "viewport_y",
            "tag_name",
            "role",
            "href_present",
            "snapshot",
            "title",
            "current_url",
        ][..],
        "terminal_input" => &[
            "terminal_id",
            "target",
            "pane_id",
            "key_count",
            "text_bytes",
            "delay_ms",
        ][..],
        _ => unreachable!("external event kind was already validated"),
    };
    let mut sanitized = serde_json::Map::new();
    for field in fields {
        copy_event_metadata_field(&detail, &mut sanitized, field);
    }
    match kind {
        "browser_search_results" => {
            sanitized.insert(
                "raw_result_text_omitted".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        "browser_snapshot" | "browser_navigate" | "browser_click" => {
            sanitized.insert(
                "raw_text_omitted".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        "terminal_input" => {
            sanitized.insert(
                "raw_text_omitted".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        _ => {}
    }
    Ok(serde_json::Value::Object(sanitized))
}

fn read_event_log(
    path: &Path,
    tail: Option<usize>,
    since_sequence: Option<u64>,
) -> Result<Vec<WorkspaceEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut events: Vec<WorkspaceEvent> = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        events.push(
            serde_json::from_str(line)
                .with_context(|| format!("failed to parse event in {}", path.display()))?,
        );
    }
    if let Some(since_sequence) = since_sequence {
        events.retain(|event| event.sequence > since_sequence);
    }
    if let Some(tail) = tail {
        let start = events.len().saturating_sub(tail);
        Ok(events.split_off(start))
    } else {
        Ok(events)
    }
}

fn read_events_from_workspace_log(
    id: &str,
    tail: Option<usize>,
    since_sequence: Option<u64>,
) -> Result<Option<IpcResponse>> {
    let runtime_dir = workspace_dir(id);
    let event_path = runtime_dir.join(EVENT_LOG_FILE);
    if !event_path.exists() {
        return Ok(None);
    }

    let events = read_event_log(&event_path, tail, since_sequence)?;
    let apps = read_workspace_manifest(&runtime_dir)
        .ok()
        .flatten()
        .and_then(|manifest| {
            if manifest.apps.is_empty() {
                None
            } else {
                Some(manifest.apps)
            }
        });

    Ok(Some(IpcResponse {
        ok: true,
        message: "workspace events returned from saved event log".to_string(),
        status: None,
        start_preview: None,
        launch_preview: None,
        ipc: None,
        environment: None,
        apps,
        windows: None,
        active_window: None,
        pointer: None,
        screenshot: None,
        app_log: None,
        clipboard: None,
        events: Some(events),
        browser_targets: None,
        browser_snapshot: None,
        browser_search_results: None,
        browser_navigate: None,
        agent_mode: None,
        target_handles: None,
        recovery_hints: Vec::new(),
    }))
}

fn read_app_log_from_workspace_manifest(
    id: &str,
    app_id: &str,
    stream: &str,
    tail_bytes: Option<u64>,
) -> Result<Option<IpcResponse>> {
    let runtime_dir = workspace_dir(id);
    let Some(manifest) = read_workspace_manifest(&runtime_dir)? else {
        return Ok(None);
    };
    let app = resolve_workspace_app(&manifest.apps, app_id)?.clone();
    let path = match stream {
        "stdout" => app.stdout_path.as_ref(),
        "stderr" => app.stderr_path.as_ref(),
        _ => None,
    }
    .ok_or_else(|| anyhow!("workspace app {} has no {stream} log path", app.id))?;
    let (content, bytes_read, truncated) = read_log_content(path, tail_bytes)?;
    let app_log = WorkspaceAppLog {
        app_id: app.id.clone(),
        stream: stream.to_string(),
        path: path.clone(),
        content,
        bytes_read,
        truncated,
    };

    Ok(Some(IpcResponse {
        ok: true,
        message: "workspace app log read from saved manifest".to_string(),
        status: None,
        start_preview: None,
        launch_preview: None,
        ipc: None,
        environment: None,
        apps: Some(vec![app]),
        windows: None,
        active_window: None,
        pointer: None,
        screenshot: None,
        app_log: Some(app_log),
        clipboard: None,
        events: None,
        browser_targets: None,
        browser_snapshot: None,
        browser_search_results: None,
        browser_navigate: None,
        agent_mode: None,
        target_handles: None,
        recovery_hints: Vec::new(),
    }))
}

fn list_apps_from_workspace_manifest(
    id: &str,
    app_id: &Option<String>,
    name_contains: &Option<String>,
    command_contains: &Option<String>,
    profile_id: &Option<String>,
    running: Option<bool>,
) -> Result<Option<IpcResponse>> {
    let runtime_dir = workspace_dir(id);
    let Some(manifest) = read_workspace_manifest(&runtime_dir)? else {
        return Ok(None);
    };
    let apps = filter_workspace_apps(
        &manifest.apps,
        app_id,
        name_contains,
        command_contains,
        profile_id,
        running,
    );

    Ok(Some(IpcResponse {
        ok: true,
        message: "workspace apps listed from saved manifest".to_string(),
        status: None,
        start_preview: None,
        launch_preview: None,
        ipc: None,
        environment: None,
        apps: Some(apps),
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
    }))
}

fn response_with_status(
    ok: bool,
    message: impl Into<String>,
    status: &WorkspaceStatus,
) -> IpcResponse {
    IpcResponse {
        ok,
        message: message.into(),
        status: Some(compact_status(status)),
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

fn compact_status(status: &WorkspaceStatus) -> WorkspaceStatus {
    let mut status = status.clone();
    status.apps.clear();
    status
}

fn attach_active_window_best_effort(response: &mut IpcResponse, status: &WorkspaceStatus) {
    let deadline = Instant::now() + Duration::from_millis(ACTIVE_WINDOW_RESPONSE_WAIT_MS);
    loop {
        match active_workspace_window(status) {
            Ok(Some(window)) => {
                response.active_window = Some(window);
                return;
            }
            Ok(None) | Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(ACTIVE_WINDOW_RESPONSE_POLL_MS));
            }
            Ok(None) | Err(_) => return,
        }
    }
}

fn workspace_ipc_info(status: &WorkspaceStatus) -> WorkspaceIpcInfo {
    WorkspaceIpcInfo {
        protocol: IPC_PROTOCOL_NAME.to_string(),
        protocol_version: IPC_PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        workspace_id: status.id.clone(),
        session_id: status.session_id.clone(),
        socket_path: status.socket_path.clone(),
        transport: "unix_socket".to_string(),
        framing: "newline_delimited_json".to_string(),
        encoding: "utf-8".to_string(),
    }
}

fn workspace_environment(status: &WorkspaceStatus) -> WorkspaceEnvironment {
    WorkspaceEnvironment {
        workspace_id: status.id.clone(),
        session_id: status.session_id.clone(),
        display: status.display.clone(),
        xauthority_path: status.xauthority_path.clone(),
        runtime_dir: status.runtime_dir.clone(),
        socket_path: status.socket_path.clone(),
        variables: crate::launch::workspace_environment_variables(status),
    }
}

struct DaemonState {
    status: WorkspaceStatus,
    apps: Vec<AppProcess>,
    window_app_ids: BTreeMap<String, String>,
    event_path: PathBuf,
    next_event_sequence: u64,
    /// Spawn-time MCP permission ceiling loaded at daemon startup. Enforced on
    /// every launch inside `spawn_app`, independent of IPC origin.
    permissions: McpPermissionState,
}

struct AppProcess {
    info: WorkspaceApp,
    child: Child,
}

fn handle_stream(stream: UnixStream, state: &mut DaemonState) -> Result<bool> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut line)?;
    }
    let request: IpcRequest =
        serde_json::from_str(line.trim()).context("failed to parse workspace IPC request")?;
    refresh_apps(state)?;

    // Apply live control (active / read_only / paused) inside the daemon. The
    // control mode is file-backed (mcp-control.json under the shared runtime
    // dir), so this daemon observes mode changes made by the viewer or MCP
    // front-end and can honor a user's runtime pause. It is gated before the
    // request match so no mutating handler runs while paused, and read-only
    // inspection plus the safety stop stay allowed even when paused.
    //
    // This remains independent of the permission ceiling: the ceiling is
    // re-checked authoritatively in spawn_app regardless of what happens here.
    // For mutating IPC, unreadable live-control state fails closed so viewer-
    // forwarded input cannot bypass read_only/paused intent by corrupting,
    // deleting, or making the shared control file unreadable.
    let (control_gate_state, control_read_error) = match control::strict_control_status() {
        Ok(status) => (LiveControlGateState::Readable(status.state.mode), None),
        Err(error) => {
            eprintln!(
                "live control state unreadable; mutating IPC requests will fail closed: {error:#}"
            );
            (LiveControlGateState::Unreadable, Some(error.to_string()))
        }
    };
    if let Some(reason) = control_gate_block_reason(&request, control_gate_state) {
        let response = match reason {
            LiveControlBlockReason::Mode(mode) => {
                record_event(
                    state,
                    "control_blocked",
                    serde_json::json!({
                        "mode": mode.as_str(),
                        "request": ipc_request_kind(&request),
                    }),
                )?;
                response_with_status(
                    false,
                    format!(
                        "workspace is {}; this mutating action is disabled until live control returns to active",
                        mode.label()
                    ),
                    &state.status,
                )
            }
            LiveControlBlockReason::Unreadable => {
                record_event(
                    state,
                    "control_unreadable_blocked",
                    serde_json::json!({
                        "request": ipc_request_kind(&request),
                        "error": control_read_error.as_deref().unwrap_or("unknown"),
                    }),
                )?;
                response_with_status(
                    false,
                    "live control state is unreadable; mutating actions are disabled until the control state can be read",
                    &state.status,
                )
            }
        };
        return finish_ipc_response(stream, response, false);
    }

    let (response, should_stop) = match request {
        IpcRequest::IpcInfo => {
            record_event(state, "ipc_info", serde_json::json!({}))?;
            let mut response =
                response_with_status(true, "workspace IPC info returned", &state.status);
            response.ipc = Some(workspace_ipc_info(&state.status));
            (response, false)
        }
        IpcRequest::Environment => {
            record_event(state, "environment", serde_json::json!({}))?;
            let mut response =
                response_with_status(true, "workspace environment returned", &state.status);
            response.environment = Some(workspace_environment(&state.status));
            (response, false)
        }
        IpcRequest::Status => {
            let mut response = response_with_status(true, "workspace is running", &state.status);
            response.status = Some(state.status.clone());
            response.apps = Some(state.status.apps.clone());
            (response, false)
        }
        IpcRequest::LaunchApp {
            command,
            name,
            profile_id,
            cwd,
            env,
            wait_window,
            window_timeout_ms,
            screenshot_window,
        } => {
            let pre_launch_visible_window_ids = if wait_window || screenshot_window {
                Some(
                    search_workspace_window_ids(&state.status, true)?
                        .into_iter()
                        .collect::<BTreeSet<_>>(),
                )
            } else {
                None
            };
            match spawn_app(
                state,
                LaunchSpec {
                    command,
                    name,
                    profile_id,
                    cwd,
                    env,
                },
            ) {
                Ok(app) => {
                    let app_id = app.id.clone();
                    record_event(
                        state,
                        "app_launch",
                        serde_json::json!({
                            "app_id": &app.id,
                            "name": app.name.as_deref(),
                            "pid": app.pid,
                            "process_group_id": app.process_group_id,
                            "command": &app.command,
                            "profile_id": app.profile_id.as_deref(),
                            "cwd": app.cwd.as_ref().map(|path| path.display().to_string()),
                            "env_count": app.env.len(),
                            "stdout_path": app.stdout_path.as_ref().map(|path| path.display().to_string()),
                            "stderr_path": app.stderr_path.as_ref().map(|path| path.display().to_string()),
                            "started_at_unix": app.started_at_unix,
                            "wait_window": wait_window,
                            "window_timeout_ms": window_timeout_ms,
                            "screenshot_window": screenshot_window,
                        }),
                    )?;
                    if wait_window || screenshot_window {
                        let timeout_ms = window_timeout_ms.unwrap_or(DEFAULT_APP_WAIT_TIMEOUT_MS);
                        let criteria = WindowWaitCriteria {
                            title_contains: None,
                            class_contains: None,
                            pid: None,
                            app_id: Some(app_id.clone()),
                            timeout_ms,
                        };
                        match wait_launch_workspace_window(
                            state,
                            &criteria,
                            pre_launch_visible_window_ids.as_ref(),
                        ) {
                            Ok(windows) => {
                                let found = !windows.is_empty();
                                let response_app = state
                                    .status
                                    .apps
                                    .iter()
                                    .find(|candidate| candidate.id == app_id)
                                    .cloned()
                                    .unwrap_or_else(|| app.clone());
                                let screenshot_result = if screenshot_window {
                                    windows
                                        .first()
                                        .map(|window| {
                                            capture_workspace_window_screenshot(
                                                &state.status,
                                                window,
                                                None,
                                                WorkspaceScreenshotSource::WorkspaceLaunchWindow,
                                            )
                                        })
                                        .transpose()
                                } else {
                                    Ok(None)
                                };
                                match screenshot_result {
                                    Ok(screenshot) => {
                                        record_event(
                                            state,
                                            "launch_wait_window",
                                            serde_json::json!({
                                                "app_id": &app_id,
                                                "timeout_ms": timeout_ms,
                                                "found": found,
                                                "windows": windows.len(),
                                                "screenshot": screenshot.as_ref().map(screenshot_event_detail),
                                            }),
                                        )?;
                                        let message = if screenshot.is_some() {
                                            "app launched, window found, and screenshot captured"
                                        } else if found {
                                            "app launched and window found in workspace"
                                        } else {
                                            "app launched but window not found before timeout"
                                        };
                                        let mut response =
                                            response_with_status(found, message, &state.status);
                                        response.apps = Some(vec![response_app]);
                                        response.windows = Some(windows);
                                        response.screenshot = screenshot;
                                        (response, false)
                                    }
                                    Err(error) => {
                                        let mut response = response_with_status(
                                            false,
                                            error.to_string(),
                                            &state.status,
                                        );
                                        response.apps = Some(vec![response_app]);
                                        response.windows = Some(windows);
                                        (response, false)
                                    }
                                }
                            }
                            Err(error) => {
                                let mut response = response_with_status(
                                    false,
                                    format!("{error:#}"),
                                    &state.status,
                                );
                                response.apps = Some(vec![app]);
                                (response, false)
                            }
                        }
                    } else {
                        (
                            {
                                let mut response = response_with_status(
                                    true,
                                    "app launched in workspace",
                                    &state.status,
                                );
                                response.apps = Some(vec![app]);
                                response
                            },
                            false,
                        )
                    }
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::ListApps {
            app_id,
            name_contains,
            command_contains,
            profile_id,
            running,
        } => {
            match validate_app_list_filters(&app_id, &name_contains, &command_contains, &profile_id)
                .and_then(|()| refresh_apps(state))
                .map(|()| {
                    filter_workspace_apps(
                        &state.status.apps,
                        &app_id,
                        &name_contains,
                        &command_contains,
                        &profile_id,
                        running,
                    )
                }) {
                Ok(apps) => {
                    record_event(
                        state,
                        "list_apps",
                        serde_json::json!({
                            "count": apps.len(),
                            "app_id": app_id.as_deref(),
                            "name_contains": name_contains.as_deref(),
                            "command_contains": command_contains.as_deref(),
                            "profile_id": profile_id.as_deref(),
                            "running": running,
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace apps listed", &state.status);
                    response.apps = Some(apps);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::ListWindows {
            include_hidden,
            title_contains,
            class_contains,
            pid,
            app_id,
        } => {
            let criteria = WindowMatchCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
            };
            match validate_window_list_filters(
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| refresh_apps(state))
            .and_then(|()| list_matching_workspace_windows(state, include_hidden, &criteria))
            {
                Ok(windows) => {
                    record_event(
                        state,
                        "list_windows",
                        serde_json::json!({
                            "count": windows.len(),
                            "include_hidden": include_hidden,
                            "title_contains": criteria.title_contains.as_deref(),
                            "class_contains": criteria.class_contains.as_deref(),
                            "pid": criteria.pid,
                            "app_id": criteria.app_id.as_deref(),
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace windows listed", &state.status);
                    response.windows = Some(windows);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::ActiveWindow => match active_workspace_window_for_state(state) {
            Ok(Some(window)) => {
                record_event(
                    state,
                    "active_window",
                    serde_json::json!({ "window_id": &window.id }),
                )?;
                let mut response =
                    response_with_status(true, "workspace active window reported", &state.status);
                response.active_window = Some(window.clone());
                response.windows = Some(vec![window]);
                (response, false)
            }
            Ok(None) => {
                record_event(
                    state,
                    "active_window",
                    serde_json::json!({ "window_id": serde_json::Value::Null }),
                )?;
                let mut response =
                    response_with_status(false, "workspace active window not found", &state.status);
                response.windows = Some(Vec::new());
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::Pointer => match workspace_pointer(&state.status) {
            Ok(pointer) => {
                record_event(
                    state,
                    "pointer",
                    serde_json::json!({
                        "x": pointer.x,
                        "y": pointer.y,
                        "screen": pointer.screen,
                        "window_id": pointer.window_id.as_deref(),
                    }),
                )?;
                let mut response =
                    response_with_status(true, "workspace pointer reported", &state.status);
                response.pointer = Some(pointer);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::Observe {
            screenshot,
            include_hidden,
            output_path,
            include_events,
            events_tail,
            events_since_sequence,
        } => match observe_workspace(state, screenshot, include_hidden, output_path) {
            Ok(mut response) => {
                record_event(
                    state,
                    "observe",
                    serde_json::json!({
                        "windows": response.windows.as_ref().map(Vec::len).unwrap_or_default(),
                        "include_hidden": include_hidden,
                        "active_window_id": response.active_window.as_ref().map(|window| window.id.as_str()),
                        "screenshot": response.screenshot.as_ref().map(screenshot_event_detail),
                        "events": include_events,
                        "events_tail": events_tail,
                        "events_since_sequence": events_since_sequence,
                    }),
                )?;
                let event_read_error = if include_events {
                    match read_event_log(&state.event_path, events_tail, events_since_sequence) {
                        Ok(events) => {
                            response.events = Some(events);
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    }
                } else {
                    None
                };
                if let Some(message) = event_read_error {
                    (response_with_status(false, message, &state.status), false)
                } else {
                    response.status = Some(state.status.clone());
                    (response, false)
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::WaitWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match wait_workspace_window(state, &criteria) {
                Ok(windows) => {
                    let found = !windows.is_empty();
                    record_event(
                        state,
                        "wait_window",
                        serde_json::json!({
                            "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                            "pid": criteria.pid,
                            "app_id": criteria.app_id.as_deref(),
                            "timeout_ms": criteria.timeout_ms,
                            "count": windows.len(),
                        }),
                    )?;
                    let mut response = response_with_status(
                        found,
                        if found {
                            "workspace window found"
                        } else {
                            "workspace window not found before timeout"
                        },
                        &state.status,
                    );
                    response.windows = Some(windows);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::Screenshot {
            output_path,
            source,
        } => match capture_workspace_screenshot(&state.status, output_path, source) {
            Ok(screenshot) => {
                record_event(state, "screenshot", screenshot_event_detail(&screenshot))?;
                let mut response =
                    response_with_status(true, "workspace screenshot captured", &state.status);
                response.screenshot = Some(screenshot);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::ScreenshotWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            output_path,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match screenshot_workspace_window(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    output_path,
                ) {
                    Ok(Some(result)) => {
                        record_event(
                            state,
                            "screenshot_window",
                            serde_json::json!({
                                "screenshot": screenshot_event_detail(&result.screenshot),
                                "window_id": &result.window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace window screenshot captured",
                            &state.status,
                        );
                        response.screenshot = Some(result.screenshot);
                        response.windows = Some(vec![result.window]);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::FocusWindow { window_id } => {
            match focus_workspace_window(&state.status, &window_id) {
                Ok(()) => {
                    record_event(
                        state,
                        "focus_window",
                        serde_json::json!({ "window_id": &window_id }),
                    )?;
                    match window_info(&state.status, &window_id) {
                        Ok(window) => {
                            let mut response = response_with_status(
                                true,
                                "workspace window focused",
                                &state.status,
                            );
                            response.windows = Some(vec![window]);
                            attach_active_window_best_effort(&mut response, &state.status);
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::FocusMatchingWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_match_options(
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
                true,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match focus_matching_workspace_window(state, &criteria) {
                    Ok(Some(window)) => {
                        record_event(
                            state,
                            "focus_window",
                            serde_json::json!({
                                "window_id": &window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace matching window focused",
                            &state.status,
                        );
                        response.windows = Some(vec![window]);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::CloseWindow { window_id, dry_run } if dry_run => {
            match window_info(&state.status, &window_id) {
                Ok(window) => {
                    record_event(
                        state,
                        "close_window_dry_run",
                        serde_json::json!({
                            "window_id": &window_id,
                            "title": &window.title,
                            "pid": window.pid,
                            "app_id": window.app_id.as_deref(),
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace window close dry run", &state.status);
                    response.windows = Some(vec![window]);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::CloseWindow { window_id, .. } => {
            match window_info(&state.status, &window_id).and_then(|window| {
                close_workspace_window(&state.status, &window_id).map(|()| window)
            }) {
                Ok(window) => {
                    record_event(
                        state,
                        "close_window",
                        serde_json::json!({ "window_id": &window_id }),
                    )?;
                    let mut response = response_with_status(
                        true,
                        "workspace window close requested",
                        &state.status,
                    );
                    response.windows = Some(vec![window]);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::CloseMatchingWindow {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
            dry_run,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_match_options(
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
                true,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) if dry_run => match wait_workspace_window(state, &criteria)
                    .map(|windows| windows.into_iter().next())
                {
                    Ok(Some(window)) => {
                        record_event(
                            state,
                            "close_window_dry_run",
                            serde_json::json!({
                                "window_id": &window.id,
                                "title": &window.title,
                                "pid": window.pid,
                                "app_id": window.app_id.as_deref(),
                                "title_contains": criteria.title_contains.as_deref(),
                                "class_contains": criteria.class_contains.as_deref(),
                                "filter_pid": criteria.pid,
                                "filter_app_id": criteria.app_id.as_deref(),
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace matching window close dry run",
                            &state.status,
                        );
                        response.windows = Some(vec![window]);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
                Ok(()) => match close_matching_workspace_window(state, &criteria) {
                    Ok(Some(window)) => {
                        record_event(
                            state,
                            "close_window",
                            serde_json::json!({
                                "window_id": &window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace matching window close requested",
                            &state.status,
                        );
                        response.windows = Some(vec![window]);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::MoveWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_workspace_coordinates(&state.status, x, y, "window move"))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match move_workspace_window_target(state, window_id.as_deref(), &criteria, x, y)
                    {
                        Ok(Some(window)) => {
                            record_event(
                                state,
                                "move_window",
                                serde_json::json!({
                                    "window_id": &window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "x": x,
                                    "y": y,
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response =
                                response_with_status(true, "workspace window moved", &state.status);
                            response.windows = Some(vec![window]);
                            (response, false)
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::ResizeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            width,
            height,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_window_size(width, height))
            .and_then(|()| validate_window_size_for_workspace(&state.status, width, height))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match resize_workspace_window_target(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    width,
                    height,
                ) {
                    Ok(Some(window)) => {
                        record_event(
                            state,
                            "resize_window",
                            serde_json::json!({
                                "window_id": &window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "width": width,
                                "height": height,
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response =
                            response_with_status(true, "workspace window resized", &state.status);
                        response.windows = Some(vec![window]);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::RaiseWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match raise_workspace_window_target(state, window_id.as_deref(), &criteria) {
                        Ok(Some(window)) => {
                            record_event(
                                state,
                                "raise_window",
                                serde_json::json!({
                                    "window_id": &window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response = response_with_status(
                                true,
                                "workspace window raised",
                                &state.status,
                            );
                            response.windows = Some(vec![window]);
                            (response, false)
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::MinimizeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match minimize_workspace_window_target(state, window_id.as_deref(), &criteria) {
                        Ok(Some(window)) => {
                            record_event(
                                state,
                                "minimize_window",
                                serde_json::json!({
                                    "window_id": &window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response = response_with_status(
                                true,
                                "workspace window minimized",
                                &state.status,
                            );
                            response.windows = Some(vec![window]);
                            (response, false)
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::ShowWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            ) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match show_workspace_window_target(state, window_id.as_deref(), &criteria) {
                        Ok(Some(window)) => {
                            record_event(
                                state,
                                "show_window",
                                serde_json::json!({
                                    "window_id": &window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response =
                                response_with_status(true, "workspace window shown", &state.status);
                            response.windows = Some(vec![window]);
                            (response, false)
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::Click {
            x,
            y,
            button,
            count,
        } => match click_workspace(&state.status, x, y, button, count) {
            Ok(()) => {
                record_event(
                    state,
                    "click",
                    serde_json::json!({ "x": x, "y": y, "button": button, "count": count }),
                )?;
                match workspace_pointer(&state.status) {
                    Ok(pointer) => {
                        let mut response =
                            response_with_status(true, "workspace click sent", &state.status);
                        response.pointer = Some(pointer);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::ClickWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            button,
            count,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_relative_click_coordinates(x, y))
            .and_then(|()| validate_click_options(button, count))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match click_workspace_window(
                        state,
                        window_id.as_deref(),
                        &criteria,
                        x,
                        y,
                        button,
                        count,
                    ) {
                        Ok(Some(clicked)) => {
                            record_event(
                                state,
                                "click_window",
                                serde_json::json!({
                                    "window_id": &clicked.window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "relative_x": x,
                                    "relative_y": y,
                                    "x": clicked.x,
                                    "y": clicked.y,
                                    "button": button,
                                    "count": count,
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response = response_with_status(
                                true,
                                "workspace window click sent",
                                &state.status,
                            );
                            match workspace_pointer(&state.status) {
                                Ok(pointer) => {
                                    response.pointer = Some(pointer);
                                    response.windows = Some(vec![clicked.window]);
                                    attach_active_window_best_effort(&mut response, &state.status);
                                    (response, false)
                                }
                                Err(error) => (
                                    response_with_status(
                                        false,
                                        format!("{error:#}"),
                                        &state.status,
                                    ),
                                    false,
                                ),
                            }
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::MovePointer { x, y } => match move_workspace_pointer(&state.status, x, y) {
            Ok(()) => {
                record_event(state, "move_pointer", serde_json::json!({ "x": x, "y": y }))?;
                match workspace_pointer(&state.status) {
                    Ok(pointer) => {
                        let mut response =
                            response_with_status(true, "workspace pointer moved", &state.status);
                        response.pointer = Some(pointer);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::MovePointerWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_relative_click_coordinates(x, y))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match move_workspace_pointer_window(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    x,
                    y,
                ) {
                    Ok(Some(moved)) => {
                        record_event(
                            state,
                            "move_pointer_window",
                            serde_json::json!({
                                "window_id": &moved.window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "relative_x": x,
                                "relative_y": y,
                                "x": moved.x,
                                "y": moved.y,
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace window pointer moved",
                            &state.status,
                        );
                        match workspace_pointer(&state.status) {
                            Ok(pointer) => {
                                response.pointer = Some(pointer);
                                response.windows = Some(vec![moved.window]);
                                attach_active_window_best_effort(&mut response, &state.status);
                                (response, false)
                            }
                            Err(error) => (
                                response_with_status(false, format!("{error:#}"), &state.status),
                                false,
                            ),
                        }
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
            button,
        } => match drag_workspace(&state.status, from_x, from_y, to_x, to_y, button) {
            Ok(()) => {
                record_event(
                    state,
                    "drag",
                    serde_json::json!({
                        "from_x": from_x,
                        "from_y": from_y,
                        "to_x": to_x,
                        "to_y": to_y,
                        "button": button,
                    }),
                )?;
                match workspace_pointer(&state.status) {
                    Ok(pointer) => {
                        let mut response =
                            response_with_status(true, "workspace drag sent", &state.status);
                        response.pointer = Some(pointer);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::DragWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            from_x,
            from_y,
            to_x,
            to_y,
            button,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_relative_click_coordinates(from_x, from_y))
            .and_then(|()| validate_relative_click_coordinates(to_x, to_y))
            .and_then(|()| validate_click_options(button, DEFAULT_CLICK_COUNT))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match drag_workspace_window(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                    button,
                ) {
                    Ok(Some(dragged)) => {
                        record_event(
                            state,
                            "drag_window",
                            serde_json::json!({
                                "window_id": &dragged.window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "from_x": dragged.from_x,
                                "from_y": dragged.from_y,
                                "to_x": dragged.to_x,
                                "to_y": dragged.to_y,
                                "relative_from_x": from_x,
                                "relative_from_y": from_y,
                                "relative_to_x": to_x,
                                "relative_to_y": to_y,
                                "button": button,
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response =
                            response_with_status(true, "workspace window drag sent", &state.status);
                        match workspace_pointer(&state.status) {
                            Ok(pointer) => {
                                response.pointer = Some(pointer);
                                response.windows = Some(vec![dragged.window]);
                                attach_active_window_best_effort(&mut response, &state.status);
                                (response, false)
                            }
                            Err(error) => (
                                response_with_status(false, format!("{error:#}"), &state.status),
                                false,
                            ),
                        }
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::Scroll {
            x,
            y,
            direction,
            amount,
        } => match scroll_workspace(&state.status, x, y, direction, amount) {
            Ok(()) => {
                record_event(
                    state,
                    "scroll",
                    serde_json::json!({
                        "x": x,
                        "y": y,
                        "direction": direction.as_str(),
                        "amount": amount,
                    }),
                )?;
                match workspace_pointer(&state.status) {
                    Ok(pointer) => {
                        let mut response =
                            response_with_status(true, "workspace scroll sent", &state.status);
                        response.pointer = Some(pointer);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::ScrollWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            x,
            y,
            direction,
            amount,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_relative_click_coordinates(x, y))
            .and_then(|()| validate_scroll_options(direction, amount))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match scroll_workspace_window(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    x,
                    y,
                    direction,
                    amount,
                ) {
                    Ok(Some(scrolled)) => {
                        record_event(
                            state,
                            "scroll_window",
                            serde_json::json!({
                                "window_id": &scrolled.window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "relative_x": x,
                                "relative_y": y,
                                "x": scrolled.x,
                                "y": scrolled.y,
                                "direction": direction.as_str(),
                                "amount": amount,
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace window scroll sent",
                            &state.status,
                        );
                        match workspace_pointer(&state.status) {
                            Ok(pointer) => {
                                response.pointer = Some(pointer);
                                response.windows = Some(vec![scrolled.window]);
                                attach_active_window_best_effort(&mut response, &state.status);
                                (response, false)
                            }
                            Err(error) => (
                                response_with_status(false, format!("{error:#}"), &state.status),
                                false,
                            ),
                        }
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::Key { key } => {
            let logged_key = key.trim().to_string();
            match key_workspace(&state.status, key) {
                Ok(()) => {
                    record_event(state, "key", serde_json::json!({ "key": logged_key }))?;
                    let mut response =
                        response_with_status(true, "workspace key sent", &state.status);
                    attach_active_window_best_effort(&mut response, &state.status);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::KeyWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            key,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            let logged_key = key.trim().to_string();
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| {
                if logged_key.is_empty() {
                    bail!("key cannot be empty");
                }
                Ok(())
            }) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match key_workspace_window(state, window_id.as_deref(), &criteria, key) {
                    Ok(Some(window)) => {
                        record_event(
                            state,
                            "key_window",
                            serde_json::json!({
                                "window_id": &window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "key": logged_key,
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response =
                            response_with_status(true, "workspace window key sent", &state.status);
                        response.windows = Some(vec![window]);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::TypeText { text } => {
            let char_count = text.chars().count();
            match type_workspace_text(&state.status, text) {
                Ok(()) => {
                    record_event(
                        state,
                        "type_text",
                        serde_json::json!({ "char_count": char_count }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace text typed", &state.status);
                    attach_active_window_best_effort(&mut response, &state.status);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::TypeWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            text,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            let char_count = text.chars().count();
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| {
                if text.is_empty() {
                    bail!("text cannot be empty");
                }
                Ok(())
            }) {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => {
                    match type_workspace_window(state, window_id.as_deref(), &criteria, text) {
                        Ok(Some(window)) => {
                            record_event(
                                state,
                                "type_window",
                                serde_json::json!({
                                    "window_id": &window.id,
                                    "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                    "pid": criteria.pid,
                                    "app_id": criteria.app_id.as_deref(),
                                    "char_count": char_count,
                                    "timeout_ms": criteria.timeout_ms,
                                }),
                            )?;
                            let mut response = response_with_status(
                                true,
                                "workspace window text typed",
                                &state.status,
                            );
                            response.windows = Some(vec![window]);
                            attach_active_window_best_effort(&mut response, &state.status);
                            (response, false)
                        }
                        Ok(None) => {
                            let mut response = response_with_status(
                                false,
                                "workspace window not found before timeout",
                                &state.status,
                            );
                            response.windows = Some(Vec::new());
                            (response, false)
                        }
                        Err(error) => (
                            response_with_status(false, format!("{error:#}"), &state.status),
                            false,
                        ),
                    }
                }
            }
        }
        IpcRequest::SetClipboard { text } => {
            let char_count = text.chars().count();
            match validate_clipboard_text(&text)
                .and_then(|()| set_workspace_clipboard(&state.status, &text))
            {
                Ok(clipboard) => {
                    record_event(
                        state,
                        "set_clipboard",
                        serde_json::json!({
                            "selection": &clipboard.selection,
                            "char_count": char_count,
                            "bytes": clipboard.bytes,
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace clipboard set", &state.status);
                    response.clipboard = Some(clipboard);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::GetClipboard => match get_workspace_clipboard(&state.status) {
            Ok(clipboard) => {
                record_event(
                    state,
                    "get_clipboard",
                    serde_json::json!({
                        "selection": &clipboard.selection,
                        "bytes": clipboard.bytes,
                    }),
                )?;
                let mut response =
                    response_with_status(true, "workspace clipboard returned", &state.status);
                response.clipboard = Some(clipboard);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::PasteText { text, key } => {
            let char_count = text.chars().count();
            match validate_clipboard_text(&text)
                .and_then(|()| validate_key_text(&key))
                .and_then(|()| paste_workspace_text(&state.status, &text, &key))
            {
                Ok(clipboard) => {
                    record_event(
                        state,
                        "paste_text",
                        serde_json::json!({
                            "selection": &clipboard.selection,
                            "char_count": char_count,
                            "bytes": clipboard.bytes,
                            "key": key.trim(),
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace text pasted", &state.status);
                    response.clipboard = Some(clipboard);
                    attach_active_window_best_effort(&mut response, &state.status);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::PasteWindow {
            window_id,
            title_contains,
            class_contains,
            pid,
            app_id,
            text,
            key,
            timeout_ms,
        } => {
            let criteria = WindowWaitCriteria {
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            };
            let char_count = text.chars().count();
            match validate_window_target_options(
                &window_id,
                &criteria.title_contains,
                &criteria.class_contains,
                criteria.pid,
                &criteria.app_id,
            )
            .and_then(|()| validate_clipboard_text(&text))
            .and_then(|()| validate_key_text(&key))
            {
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
                Ok(()) => match paste_workspace_window(
                    state,
                    window_id.as_deref(),
                    &criteria,
                    &text,
                    &key,
                ) {
                    Ok(Some(pasted)) => {
                        record_event(
                            state,
                            "paste_window",
                            serde_json::json!({
                                "window_id": &pasted.window.id,
                                "title_contains": criteria.title_contains.as_deref(),
                                    "class_contains": criteria.class_contains.as_deref(),
                                "pid": criteria.pid,
                                "app_id": criteria.app_id.as_deref(),
                                "selection": &pasted.clipboard.selection,
                                "char_count": char_count,
                                "bytes": pasted.clipboard.bytes,
                                "key": key.trim(),
                                "timeout_ms": criteria.timeout_ms,
                            }),
                        )?;
                        let mut response = response_with_status(
                            true,
                            "workspace window text pasted",
                            &state.status,
                        );
                        response.windows = Some(vec![pasted.window]);
                        response.clipboard = Some(pasted.clipboard);
                        attach_active_window_best_effort(&mut response, &state.status);
                        (response, false)
                    }
                    Ok(None) => {
                        let mut response = response_with_status(
                            false,
                            "workspace window not found before timeout",
                            &state.status,
                        );
                        response.windows = Some(Vec::new());
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                },
            }
        }
        IpcRequest::ReadAppLog {
            app_id,
            stream,
            tail_bytes,
        } => match read_workspace_app_log(state, &app_id, &stream, tail_bytes) {
            Ok((app_log, app)) => {
                record_event(
                    state,
                    "read_app_log",
                    serde_json::json!({
                        "app_id": &app_log.app_id,
                        "stream": &app_log.stream,
                        "tail_bytes": tail_bytes,
                        "bytes_read": app_log.bytes_read,
                        "truncated": app_log.truncated,
                    }),
                )?;
                let mut response =
                    response_with_status(true, "workspace app log read", &state.status);
                response.app_log = Some(app_log);
                response.apps = Some(vec![app]);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::WaitApp {
            app_id,
            timeout_ms,
            kill_on_timeout,
        } => match wait_workspace_app(state, &app_id, timeout_ms, kill_on_timeout) {
            Ok((stopped, killed_on_timeout, app)) => {
                record_event(
                    state,
                    "wait_app",
                    serde_json::json!({
                        "target": &app_id,
                        "timeout_ms": timeout_ms,
                        "stopped": stopped,
                        "kill_on_timeout": kill_on_timeout,
                        "killed_on_timeout": killed_on_timeout,
                        "app_id": &app.id,
                        "name": app.name.as_deref(),
                        "running": app.running,
                        "exit_code": app.exit_code,
                        "exit_signal": app.exit_signal,
                        "stopped_at_unix": app.stopped_at_unix,
                        "runtime_seconds": app.runtime_seconds,
                    }),
                )?;
                let message = if killed_on_timeout {
                    "workspace app killed after timeout"
                } else if stopped {
                    "workspace app stopped"
                } else {
                    "workspace app still running after timeout"
                };
                let mut response = response_with_status(stopped, message, &state.status);
                response.apps = Some(vec![app]);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::BrowserTargets {
            app_id,
            user_data_dir,
            timeout_ms,
        } => match crate::browser::workspace_browser_targets_from_status(
            &state.status,
            app_id,
            user_data_dir,
            timeout_ms,
        ) {
            Ok(targets) => {
                let mut response =
                    response_with_status(true, "workspace browser targets returned", &state.status);
                response.browser_targets = Some(targets);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::BrowserSnapshot {
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_text_chars,
            timeout_ms,
        } => match crate::browser::workspace_browser_snapshot_from_status(
            &state.status,
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_text_chars,
            timeout_ms,
        ) {
            Ok(snapshot) => {
                let detail = crate::browser::browser_snapshot_event_detail(&snapshot);
                match record_event(state, "browser_snapshot", detail) {
                    Ok(event) => {
                        let mut response = response_with_status(
                            true,
                            "workspace browser page snapshot captured",
                            &state.status,
                        );
                        response.browser_snapshot = Some(snapshot);
                        response.events = Some(vec![event]);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::BrowserSearchResults {
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_results,
            min_vram_gb,
            timeout_ms,
        } => match crate::browser::workspace_browser_search_results_from_status(
            &state.status,
            app_id,
            user_data_dir,
            target_id,
            title_contains,
            url_contains,
            max_results,
            min_vram_gb,
            timeout_ms,
        ) {
            Ok(results) => {
                let detail =
                    crate::browser::browser_search_results_event_detail(&results, min_vram_gb);
                match record_event(state, "browser_search_results", detail) {
                    Ok(event) => {
                        let mut response = response_with_status(
                            true,
                            "workspace browser search results extracted",
                            &state.status,
                        );
                        response.browser_search_results = Some(results);
                        response.events = Some(vec![event]);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::BrowserNavigate {
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
        } => match crate::browser::workspace_browser_navigate_from_status(
            &state.status,
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
        ) {
            Ok(navigation) => {
                let detail = crate::browser::browser_navigate_event_detail(&navigation);
                match record_event(state, "browser_navigate", detail) {
                    Ok(event) => {
                        let mut response = response_with_status(
                            true,
                            "workspace browser navigated",
                            &state.status,
                        );
                        response.browser_navigate = Some(navigation);
                        response.events = Some(vec![event]);
                        (response, false)
                    }
                    Err(error) => (
                        response_with_status(false, format!("{error:#}"), &state.status),
                        false,
                    ),
                }
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::ReadEvents {
            tail,
            since_sequence,
        } => match read_event_log(&state.event_path, tail, since_sequence) {
            Ok(events) => {
                let mut response =
                    response_with_status(true, "workspace events returned", &state.status);
                response.events = Some(events);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::RecordEvent { kind, detail } => {
            match metadata_only_browser_event_detail(&kind, detail)
                .and_then(|sanitized_detail| record_event(state, &kind, sanitized_detail))
            {
                Ok(event) => {
                    let mut response =
                        response_with_status(true, "workspace event recorded", &state.status);
                    response.events = Some(vec![event]);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::KillApp { app_id, dry_run } if dry_run => {
            match refresh_apps(state)
                .and_then(|()| resolve_workspace_app(&state.status.apps, &app_id).cloned())
            {
                Ok(app) => {
                    record_event(
                        state,
                        "kill_app_dry_run",
                        serde_json::json!({
                            "target": &app_id,
                            "would_kill": app.running,
                            "app_id": &app.id,
                            "name": app.name.as_deref(),
                            "running": app.running,
                            "exit_code": app.exit_code,
                            "exit_signal": app.exit_signal,
                            "stopped_at_unix": app.stopped_at_unix,
                            "runtime_seconds": app.runtime_seconds,
                        }),
                    )?;
                    let mut response =
                        response_with_status(true, "workspace app kill dry run", &state.status);
                    response.apps = Some(vec![app]);
                    (response, false)
                }
                Err(error) => (
                    response_with_status(false, format!("{error:#}"), &state.status),
                    false,
                ),
            }
        }
        IpcRequest::KillApp { app_id, .. } => match kill_workspace_app(state, &app_id) {
            Ok((message, app, killed)) => {
                record_event(
                    state,
                    "kill_app",
                    serde_json::json!({
                        "target": &app_id,
                        "message": &message,
                        "killed": killed,
                        "app_id": &app.id,
                        "name": app.name.as_deref(),
                        "running": app.running,
                        "exit_code": app.exit_code,
                        "exit_signal": app.exit_signal,
                        "stopped_at_unix": app.stopped_at_unix,
                        "runtime_seconds": app.runtime_seconds,
                    }),
                )?;
                let mut response = response_with_status(true, message, &state.status);
                response.apps = Some(vec![app]);
                (response, false)
            }
            Err(error) => (
                response_with_status(false, format!("{error:#}"), &state.status),
                false,
            ),
        },
        IpcRequest::Stop => {
            let stopped_apps = match terminate_running_workspace_apps(state) {
                Ok(apps) => apps,
                Err(error) => {
                    return finish_ipc_response(
                        stream,
                        response_with_status(
                            false,
                            format!("workspace stop refused: {error:#}"),
                            &state.status,
                        ),
                        false,
                    )
                }
            };
            state.status.ready = false;
            record_event(
                state,
                "workspace_stop",
                serde_json::json!({
                    "stopped_app_count": stopped_apps.len(),
                    "app_ids": stopped_apps.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
                }),
            )?;
            let mut response = response_with_status(true, "workspace stopping", &state.status);
            response.apps = Some(stopped_apps);
            (response, true)
        }
    };

    finish_ipc_response(stream, response, should_stop)
}

fn finish_ipc_response(
    mut stream: UnixStream,
    response: IpcResponse,
    should_stop: bool,
) -> Result<bool> {
    let write_response = (|| -> Result<()> {
        serde_json::to_writer(&mut stream, &response)
            .context("failed to write workspace IPC response")?;
        stream
            .write_all(b"\n")
            .context("failed to finish workspace IPC response")?;
        Ok(())
    })();
    if let Err(error) = write_response {
        if should_stop {
            eprintln!("workspace IPC stop response failed after shutdown was requested: {error}");
            return Ok(true);
        }
        return Err(error);
    }
    Ok(should_stop)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveControlGateState {
    Readable(McpControlMode),
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveControlBlockReason {
    Mode(McpControlMode),
    Unreadable,
}

/// Pure live-control gate decision: given an incoming request and the current
/// control state, return `Some(reason)` when the request must be blocked, or
/// `None` when it is allowed. Kept separate from `handle_stream` so it can be
/// unit-tested without touching the file-backed control state or process-global
/// environment.
fn control_gate_block_reason(
    request: &IpcRequest,
    state: LiveControlGateState,
) -> Option<LiveControlBlockReason> {
    match state {
        LiveControlGateState::Readable(mode)
            if request_mutates(request) && !mode.allows_agent_mutation() =>
        {
            Some(LiveControlBlockReason::Mode(mode))
        }
        LiveControlGateState::Unreadable if request_mutates(request) => {
            Some(LiveControlBlockReason::Unreadable)
        }
        _ => None,
    }
}

/// Classify an IPC request as mutating (true) or read-only/inspection (false)
/// for live-control enforcement. Mutating requests are blocked when control is
/// read_only or paused. Read-only requests, dry-run previews, and the safety
/// stop are always allowed (Stop must work even when paused). New IPC variants
/// must be added here explicitly; the match is exhaustive so adding a variant
/// without classifying it is a compile error.
fn request_mutates(request: &IpcRequest) -> bool {
    match request {
        // Read-only inspection.
        IpcRequest::IpcInfo
        | IpcRequest::Environment
        | IpcRequest::Status
        | IpcRequest::ListApps { .. }
        | IpcRequest::ListWindows { .. }
        | IpcRequest::ActiveWindow
        | IpcRequest::Pointer
        | IpcRequest::Observe { .. }
        | IpcRequest::WaitWindow { .. }
        | IpcRequest::Screenshot { .. }
        | IpcRequest::ScreenshotWindow { .. }
        | IpcRequest::ReadAppLog { .. }
        | IpcRequest::WaitApp { .. }
        | IpcRequest::BrowserTargets { .. }
        | IpcRequest::BrowserSnapshot { .. }
        | IpcRequest::BrowserSearchResults { .. }
        | IpcRequest::ReadEvents { .. }
        | IpcRequest::GetClipboard => false,
        // Safety stop is always allowed, even when paused.
        IpcRequest::Stop => false,
        // Dry-run previews resolve targets without mutating.
        IpcRequest::CloseWindow { dry_run, .. } => !dry_run,
        IpcRequest::CloseMatchingWindow { dry_run, .. } => !dry_run,
        IpcRequest::KillApp { dry_run, .. } => !dry_run,
        // Everything else mutates workspace or app state.
        IpcRequest::LaunchApp { .. }
        | IpcRequest::FocusWindow { .. }
        | IpcRequest::FocusMatchingWindow { .. }
        | IpcRequest::MoveWindow { .. }
        | IpcRequest::ResizeWindow { .. }
        | IpcRequest::RaiseWindow { .. }
        | IpcRequest::MinimizeWindow { .. }
        | IpcRequest::ShowWindow { .. }
        | IpcRequest::Click { .. }
        | IpcRequest::ClickWindow { .. }
        | IpcRequest::MovePointer { .. }
        | IpcRequest::MovePointerWindow { .. }
        | IpcRequest::Drag { .. }
        | IpcRequest::DragWindow { .. }
        | IpcRequest::Scroll { .. }
        | IpcRequest::ScrollWindow { .. }
        | IpcRequest::Key { .. }
        | IpcRequest::KeyWindow { .. }
        | IpcRequest::TypeText { .. }
        | IpcRequest::TypeWindow { .. }
        | IpcRequest::SetClipboard { .. }
        | IpcRequest::PasteText { .. }
        | IpcRequest::PasteWindow { .. }
        | IpcRequest::RecordEvent { .. }
        | IpcRequest::BrowserNavigate { .. } => true,
    }
}

/// Short stable label for an IPC request, used in control-blocked event logs.
fn ipc_request_kind(request: &IpcRequest) -> &'static str {
    match request {
        IpcRequest::IpcInfo => "ipc_info",
        IpcRequest::Environment => "environment",
        IpcRequest::Status => "status",
        IpcRequest::LaunchApp { .. } => "launch_app",
        IpcRequest::ListApps { .. } => "list_apps",
        IpcRequest::ListWindows { .. } => "list_windows",
        IpcRequest::ActiveWindow => "active_window",
        IpcRequest::Pointer => "pointer",
        IpcRequest::Observe { .. } => "observe",
        IpcRequest::WaitWindow { .. } => "wait_window",
        IpcRequest::Screenshot { .. } => "screenshot",
        IpcRequest::ScreenshotWindow { .. } => "screenshot_window",
        IpcRequest::FocusWindow { .. } => "focus_window",
        IpcRequest::FocusMatchingWindow { .. } => "focus_matching_window",
        IpcRequest::CloseWindow { .. } => "close_window",
        IpcRequest::CloseMatchingWindow { .. } => "close_matching_window",
        IpcRequest::MoveWindow { .. } => "move_window",
        IpcRequest::ResizeWindow { .. } => "resize_window",
        IpcRequest::RaiseWindow { .. } => "raise_window",
        IpcRequest::MinimizeWindow { .. } => "minimize_window",
        IpcRequest::ShowWindow { .. } => "show_window",
        IpcRequest::Click { .. } => "click",
        IpcRequest::ClickWindow { .. } => "click_window",
        IpcRequest::MovePointer { .. } => "move_pointer",
        IpcRequest::MovePointerWindow { .. } => "move_pointer_window",
        IpcRequest::Drag { .. } => "drag",
        IpcRequest::DragWindow { .. } => "drag_window",
        IpcRequest::Scroll { .. } => "scroll",
        IpcRequest::ScrollWindow { .. } => "scroll_window",
        IpcRequest::Key { .. } => "key",
        IpcRequest::KeyWindow { .. } => "key_window",
        IpcRequest::TypeText { .. } => "type_text",
        IpcRequest::TypeWindow { .. } => "type_window",
        IpcRequest::SetClipboard { .. } => "set_clipboard",
        IpcRequest::GetClipboard => "get_clipboard",
        IpcRequest::PasteText { .. } => "paste_text",
        IpcRequest::PasteWindow { .. } => "paste_window",
        IpcRequest::ReadAppLog { .. } => "read_app_log",
        IpcRequest::WaitApp { .. } => "wait_app",
        IpcRequest::BrowserTargets { .. } => "browser_targets",
        IpcRequest::BrowserSnapshot { .. } => "browser_snapshot",
        IpcRequest::BrowserSearchResults { .. } => "browser_search_results",
        IpcRequest::BrowserNavigate { .. } => "browser_navigate",
        IpcRequest::ReadEvents { .. } => "read_events",
        IpcRequest::RecordEvent { .. } => "record_event",
        IpcRequest::KillApp { .. } => "kill_app",
        IpcRequest::Stop => "stop",
    }
}

fn spawn_app(state: &mut DaemonState, spec: LaunchSpec) -> Result<WorkspaceApp> {
    let spec = launch_spec_with_workspace_defaults(&state.status, spec);
    validate_launch_spec(&spec)?;
    // Enforce the spawn-time MCP permission ceiling inside the daemon, after
    // workspace defaults are applied. This holds regardless of IPC origin: a
    // same-uid process that writes to the control socket directly is still
    // capped here, not only in the MCP front-end. Covers direct launches, `workspace run`,
    // profile setup commands, and startup apps, which all funnel through here.
    state
        .permissions
        .validate_launch_spec(&spec)
        .context("workspace launch exceeds the MCP permission ceiling")?;
    if let Some(why) = crate::launch::desktop_handoff(
        spec.cwd.as_deref(),
        &spec.command,
        |name| {
            spec.env
                .iter()
                .find(|var| var.name == name)
                .map(|var| var.value.clone().into())
                .or_else(|| crate::session::host_services::user_session_var(name))
        },
        |pid| {
            state
                .status
                .apps
                .iter()
                .any(|app| app.running && app.pid == pid)
        },
    ) {
        bail!("{why}");
    }
    create_client_runtime(&state.status.runtime_dir)?;
    let terminal_dir =
        terminal_cleanup::managed_terminal_client_dir(&state.status.runtime_dir, &spec.command);
    if let Some(dir) = &terminal_dir {
        create_owned_private_dir(dir, "managed terminal client directory")?;
    }
    let log_paths = prepare_app_log_paths(&state.status.runtime_dir)?;
    let mut child_command = Command::new(&spec.command[0]);
    child_command
        .args(&spec.command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            fs::File::create(&log_paths.stdout)
                .with_context(|| format!("failed to create {}", log_paths.stdout.display()))?,
        ))
        .stderr(Stdio::from(
            fs::File::create(&log_paths.stderr)
                .with_context(|| format!("failed to create {}", log_paths.stderr.display()))?,
        ));
    if let Some(cwd) = &spec.cwd {
        child_command.current_dir(cwd);
    }
    crate::launch::configure_launch_environment(&mut child_command, &state.status, &spec.env);
    child_command.process_group(0);
    let child = child_command
        .spawn()
        .with_context(|| format!("failed to launch {}", launch_description(&spec.command)))?;
    let pid = child.id();
    let stdout_path = rename_app_log(&log_paths.stdout, pid, "stdout")?;
    let stderr_path = rename_app_log(&log_paths.stderr, pid, "stderr")?;
    let info = WorkspaceApp {
        id: format!("app-{pid}"),
        name: spec.name,
        pid,
        process_group_id: Some(pid),
        profile_id: spec.profile_id,
        command: spec.command,
        cwd: spec.cwd,
        env: spec.env,
        stdout_path: Some(stdout_path),
        stderr_path: Some(stderr_path),
        started_at_unix: unix_now(),
        running: true,
        exit_status: None,
        exit_code: None,
        exit_signal: None,
        stopped_at_unix: None,
        runtime_seconds: None,
    };
    state.status.apps.push(info.clone());
    state.apps.push(AppProcess {
        info: info.clone(),
        child,
    });
    Ok(info)
}

fn launch_spec_with_workspace_defaults(
    status: &WorkspaceStatus,
    mut spec: LaunchSpec,
) -> LaunchSpec {
    if spec.profile_id.is_some() {
        return spec;
    }

    spec.profile_id = status.profile_id.clone();
    if spec.cwd.is_none() {
        spec.cwd = status.profile_cwd.clone();
    }
    spec.env = crate::launch::merged_env(status.profile_env.clone(), spec.env);
    spec
}

fn wait_for_terminal(
    id: &str,
    terminal_id: &str,
    app_id: Option<String>,
    timeout_ms: u64,
) -> Result<WorkspaceTerminal> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.min(30_000));
    loop {
        match terminal_info(id, terminal_id, app_id.clone()) {
            Ok(terminal) => return Ok(terminal),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn terminal_info(id: &str, terminal_id: &str, app_id: Option<String>) -> Result<WorkspaceTerminal> {
    let id = sanitize_workspace_id(id)?;
    let status = status_workspace(&id)?;
    let terminal_id = sanitize_terminal_id(terminal_id)?;
    let tmux_socket_path = terminal_socket_path(&status.runtime_dir, &terminal_id);
    let session_name = terminal_id.clone();
    let target = session_name.clone();
    let output = tmux_output(
        &tmux_socket_path,
        &[
            "display-message",
            "-p",
            "-t",
            &target,
            "#{pane_id}\t#{pane_width}\t#{pane_height}\t#{pane_tty}\t#{pane_current_command}\t#{pane_current_path}",
        ],
    )?;
    let fields = output.trim_end().split('\t').collect::<Vec<_>>();
    let width = fields
        .get(1)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let height = fields
        .get(2)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(WorkspaceTerminal {
        workspace_id: id,
        terminal_id,
        session_name,
        tmux_socket_path,
        target,
        app_id,
        pane_id: non_empty_field(fields.first().copied()),
        pane_tty: non_empty_field(fields.get(3).copied()),
        current_command: non_empty_field(fields.get(4).copied()),
        current_path: non_empty_field(fields.get(5).copied()),
        width,
        height,
    })
}

fn tmux_capture_pane(
    terminal: &WorkspaceTerminal,
    preserve_trailing_spaces: bool,
) -> Result<String> {
    let mut args = vec!["capture-pane", "-p", "-t", terminal.target.as_str()];
    if preserve_trailing_spaces {
        args.insert(2, "-N");
    }
    match tmux_output(&terminal.tmux_socket_path, &args) {
        Ok(output) => Ok(output),
        Err(error) if preserve_trailing_spaces => {
            let fallback = tmux_output(
                &terminal.tmux_socket_path,
                &["capture-pane", "-p", "-t", terminal.target.as_str()],
            )?;
            Ok(format!(
                "{fallback}\n[warning: tmux capture-pane -N was not available: {error}]"
            ))
        }
        Err(error) => Err(error),
    }
}

fn tmux_send_literal_text(terminal: &WorkspaceTerminal, text: &str) -> Result<()> {
    tmux_status(
        &terminal.tmux_socket_path,
        &[
            "send-keys",
            "-t",
            terminal.target.as_str(),
            "-l",
            "--",
            text,
        ],
    )
}

fn tmux_send_keys(terminal: &WorkspaceTerminal, keys: &[String]) -> Result<()> {
    let mut args = vec!["send-keys", "-t", terminal.target.as_str()];
    args.extend(keys.iter().map(String::as_str));
    tmux_status(&terminal.tmux_socket_path, &args)
}

fn tmux_output(socket_path: &Path, args: &[&str]) -> Result<String> {
    let output = tmux_command(socket_path, args)
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("tmux output was not UTF-8")
}

fn tmux_status(socket_path: &Path, args: &[&str]) -> Result<()> {
    let output = tmux_command(socket_path, args)
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn tmux_command(socket_path: &Path, args: &[&str]) -> Command {
    let tmux = resolve_executable_path(DEFAULT_TMUX_PROGRAM)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TMUX_PROGRAM));
    let mut command = Command::new(tmux);
    // The client sends its environment to the server; send nothing private.
    command.env_clear();
    for name in ["PATH", "LANG", "LC_ALL", "LC_CTYPE"] {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    command.arg("-S").arg(socket_path).args(args);
    command
}

fn non_empty_field(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_terminal_keys(keys: &[String]) -> Result<Vec<String>> {
    keys.iter().map(|key| normalize_terminal_key(key)).collect()
}

fn normalize_terminal_key(key: &str) -> Result<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        bail!("terminal key cannot be empty");
    }
    if trimmed.contains('\0') {
        bail!("terminal key cannot contain NUL bytes");
    }
    let lower = trimmed.to_ascii_lowercase();
    let normalized = match lower.as_str() {
        "return" | "enter" => "Enter".to_string(),
        "esc" | "escape" => "Escape".to_string(),
        "space" => "Space".to_string(),
        "tab" => "Tab".to_string(),
        "backspace" | "bs" => "BSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "up" | "down" | "left" | "right" => {
            let mut chars = lower.chars();
            let first = chars.next().unwrap().to_ascii_uppercase();
            format!("{first}{}", chars.as_str())
        }
        value if value.starts_with("ctrl+") && value.len() == "ctrl+".len() + 1 => {
            let key = value.chars().last().unwrap();
            format!("C-{}", key.to_ascii_lowercase())
        }
        value if value.starts_with("c-") && value.len() == 3 => {
            format!("C-{}", value.chars().last().unwrap().to_ascii_lowercase())
        }
        _ => trimmed.to_string(),
    };
    Ok(normalized)
}

fn terminal_key_grammar() -> Vec<String> {
    vec![
        "Enter/Return".to_string(),
        "Escape/Esc".to_string(),
        "Tab".to_string(),
        "Space".to_string(),
        "Backspace/BSpace".to_string(),
        "Delete".to_string(),
        "Up/Down/Left/Right".to_string(),
        "ctrl+c or C-c".to_string(),
        "tmux send-keys names such as Home, End, PageUp, PageDown, F1".to_string(),
    ]
}

fn sanitize_terminal_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("terminal id cannot be empty");
    }
    if trimmed.len() > 48 {
        bail!("terminal id must be at most 48 characters");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("terminal id may only contain ASCII letters, numbers, '-', '_' and '.'");
    }
    Ok(trimmed.to_string())
}

fn launch_description(command: &[String]) -> String {
    if command.is_empty() {
        "<empty command>".to_string()
    } else {
        command.join(" ")
    }
}

struct AppLogPaths {
    stdout: PathBuf,
    stderr: PathBuf,
}

fn prepare_app_log_paths(runtime_dir: &Path) -> Result<AppLogPaths> {
    let log_dir = runtime_dir.join("apps");
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create {}", log_dir.display()))?;
    let prefix = format!("launch-{}", unix_now_millis());
    Ok(AppLogPaths {
        stdout: log_dir.join(format!("{prefix}.stdout.log")),
        stderr: log_dir.join(format!("{prefix}.stderr.log")),
    })
}

fn rename_app_log(path: &Path, pid: u32, stream: &str) -> Result<PathBuf> {
    let target = path
        .parent()
        .ok_or_else(|| anyhow!("app log path has no parent: {}", path.display()))?
        .join(format!("app-{pid}.{stream}.log"));
    fs::rename(path, &target).with_context(|| {
        format!(
            "failed to move app log {} to {}",
            path.display(),
            target.display()
        )
    })?;
    Ok(target)
}

fn desktop_control(status: &WorkspaceStatus) -> Box<dyn DesktopControl + '_> {
    status.session.desktop_control(status)
}

fn list_workspace_windows(
    status: &WorkspaceStatus,
    include_hidden: bool,
) -> Result<Vec<WorkspaceWindow>> {
    desktop_control(status).list_windows(include_hidden)
}

fn search_workspace_window_ids(
    status: &WorkspaceStatus,
    only_visible: bool,
) -> Result<Vec<String>> {
    desktop_control(status).search_window_ids(only_visible)
}

fn active_workspace_window(status: &WorkspaceStatus) -> Result<Option<WorkspaceWindow>> {
    desktop_control(status)
        .active_window()
        .map(|window| window.map(|window| annotate_workspace_window_for_status(status, window)))
}

fn active_workspace_window_for_state(state: &DaemonState) -> Result<Option<WorkspaceWindow>> {
    active_workspace_window(&state.status)
        .map(|window| window.map(|window| annotate_workspace_window(state, window)))
}

fn workspace_pointer(status: &WorkspaceStatus) -> Result<WorkspacePointer> {
    desktop_control(status).pointer()
}

fn observe_workspace(
    state: &DaemonState,
    screenshot: bool,
    include_hidden: bool,
    output_path: Option<PathBuf>,
) -> Result<IpcResponse> {
    let windows = list_workspace_windows_for_state(state, include_hidden)?;
    let active_window = active_workspace_window_for_state(state)?;
    let pointer = workspace_pointer(&state.status)?;
    let screenshot = if screenshot {
        let output_path = observe_screenshot_output_path(output_path);
        Some(capture_workspace_screenshot(
            &state.status,
            output_path,
            WorkspaceScreenshotSource::WorkspaceObserve,
        )?)
    } else {
        None
    };
    Ok(IpcResponse {
        ok: true,
        message: "workspace observed".to_string(),
        apps: Some(state.status.apps.clone()),
        status: Some(state.status.clone()),
        start_preview: None,
        launch_preview: None,
        ipc: None,
        environment: None,
        windows: Some(windows),
        active_window,
        pointer: Some(pointer),
        screenshot,
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
    })
}

fn observe_screenshot_output_path(output_path: Option<PathBuf>) -> Option<PathBuf> {
    output_path.or_else(|| Some(PathBuf::from(OBSERVE_SCREENSHOT_FILE)))
}

fn filter_workspace_apps(
    apps: &[WorkspaceApp],
    app_id: &Option<String>,
    name_contains: &Option<String>,
    command_contains: &Option<String>,
    profile_id: &Option<String>,
    running: Option<bool>,
) -> Vec<WorkspaceApp> {
    apps.iter()
        .filter(|app| {
            app_id
                .as_ref()
                .is_none_or(|target| matches_app_id(app, target))
        })
        .filter(|app| {
            name_contains.as_ref().is_none_or(|needle| {
                app.name
                    .as_ref()
                    .is_some_and(|name| contains_ascii_case_insensitive(name, needle))
            })
        })
        .filter(|app| {
            command_contains
                .as_ref()
                .is_none_or(|needle| command_matches(&app.command, needle))
        })
        .filter(|app| {
            profile_id
                .as_ref()
                .is_none_or(|target| app.profile_id.as_deref() == Some(target.as_str()))
        })
        .filter(|app| running.is_none_or(|running| app.running == running))
        .cloned()
        .collect()
}

struct WindowMatchCriteria {
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
}

struct WindowWaitCriteria {
    title_contains: Option<String>,
    class_contains: Option<String>,
    pid: Option<u32>,
    app_id: Option<String>,
    timeout_ms: u64,
}

fn wait_workspace_window(
    state: &mut DaemonState,
    criteria: &WindowWaitCriteria,
) -> Result<Vec<WorkspaceWindow>> {
    wait_workspace_window_with_visibility(state, criteria, false)
}

fn wait_workspace_window_with_visibility(
    state: &mut DaemonState,
    criteria: &WindowWaitCriteria,
    include_hidden: bool,
) -> Result<Vec<WorkspaceWindow>> {
    let timeout = Duration::from_millis(criteria.timeout_ms);
    let started = Instant::now();
    loop {
        refresh_apps(state)?;
        let windows = matching_workspace_windows_with_visibility(state, criteria, include_hidden)?;
        if !windows.is_empty() {
            return Ok(windows);
        }
        if started.elapsed() >= timeout {
            return Ok(Vec::new());
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn wait_launch_workspace_window(
    state: &mut DaemonState,
    criteria: &WindowWaitCriteria,
    pre_launch_visible_window_ids: Option<&BTreeSet<String>>,
) -> Result<Vec<WorkspaceWindow>> {
    let timeout = Duration::from_millis(criteria.timeout_ms);
    let started = Instant::now();
    loop {
        refresh_apps(state)?;
        let windows = matching_workspace_windows_with_visibility(state, criteria, false)?;
        if !windows.is_empty() {
            return Ok(windows);
        }
        if let Some(pre_launch_visible_window_ids) = pre_launch_visible_window_ids {
            let new_windows = list_workspace_windows_for_state(state, false)?
                .into_iter()
                .filter(|window| !pre_launch_visible_window_ids.contains(&window.id))
                .collect::<Vec<_>>();
            if !new_windows.is_empty() {
                if let Some(app_id) = criteria.app_id.as_deref() {
                    remember_workspace_window_app_ids(state, app_id, &new_windows);
                    return Ok(annotate_workspace_windows(state, new_windows));
                }
                return Ok(new_windows);
            }
        }
        if started.elapsed() >= timeout {
            return Ok(Vec::new());
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn matching_workspace_windows_with_visibility(
    state: &DaemonState,
    criteria: &WindowWaitCriteria,
    include_hidden: bool,
) -> Result<Vec<WorkspaceWindow>> {
    let match_criteria = WindowMatchCriteria {
        title_contains: criteria.title_contains.clone(),
        class_contains: criteria.class_contains.clone(),
        pid: criteria.pid,
        app_id: criteria.app_id.clone(),
    };
    list_matching_workspace_windows(state, include_hidden, &match_criteria)
}

fn list_matching_workspace_windows(
    state: &DaemonState,
    include_hidden: bool,
    criteria: &WindowMatchCriteria,
) -> Result<Vec<WorkspaceWindow>> {
    let app_filter = criteria
        .app_id
        .as_ref()
        .map(|app_id| resolve_workspace_app(&state.status.apps, app_id).cloned())
        .transpose()?;
    Ok(list_workspace_windows_for_state(state, include_hidden)?
        .into_iter()
        .filter(|window| {
            criteria
                .title_contains
                .as_ref()
                .is_none_or(|title| window.title.contains(title))
        })
        .filter(|window| {
            criteria.class_contains.as_ref().is_none_or(|class| {
                window
                    .wm_class
                    .as_ref()
                    .is_some_and(|wm_class| contains_ascii_case_insensitive(wm_class, class))
                    || window.wm_instance.as_ref().is_some_and(|wm_instance| {
                        contains_ascii_case_insensitive(wm_instance, class)
                    })
            })
        })
        .filter(|window| {
            if let Some(pid) = criteria.pid {
                return window.pid == Some(pid);
            }
            if let Some(app) = &app_filter {
                return window_belongs_to_app(window, app);
            }
            true
        })
        .collect())
}

fn list_workspace_windows_for_state(
    state: &DaemonState,
    include_hidden: bool,
) -> Result<Vec<WorkspaceWindow>> {
    let windows = list_workspace_windows(&state.status, include_hidden)?;
    Ok(annotate_workspace_windows(state, windows))
}

fn annotate_workspace_windows(
    state: &DaemonState,
    windows: Vec<WorkspaceWindow>,
) -> Vec<WorkspaceWindow> {
    windows
        .into_iter()
        .map(|window| annotate_workspace_window(state, window))
        .collect()
}

fn annotate_workspace_window(state: &DaemonState, window: WorkspaceWindow) -> WorkspaceWindow {
    let mut window = annotate_workspace_window_for_status(&state.status, window);
    if window.app_id.is_none() {
        window.app_id = state.window_app_ids.get(&window.id).cloned();
    }
    window
}

fn annotate_workspace_window_for_status(
    status: &WorkspaceStatus,
    mut window: WorkspaceWindow,
) -> WorkspaceWindow {
    if window.app_id.is_none() {
        window.app_id = window
            .pid
            .and_then(|pid| workspace_app_id_for_pid(status, pid));
    }
    window
}

fn remember_workspace_window_app_ids(
    state: &mut DaemonState,
    app_id: &str,
    windows: &[WorkspaceWindow],
) {
    for window in windows {
        if window.app_id.is_none() {
            state
                .window_app_ids
                .insert(window.id.clone(), app_id.to_string());
        }
    }
}

fn window_belongs_to_app(window: &WorkspaceWindow, app: &WorkspaceApp) -> bool {
    window.app_id.as_deref() == Some(app.id.as_str())
        || window
            .pid
            .is_some_and(|window_pid| process_belongs_to_app(window_pid, app))
}

fn process_is_descendant_or_self(pid: u32, ancestor_pid: u32) -> bool {
    let mut current = Some(pid);
    for _ in 0..64 {
        let Some(current_pid) = current else {
            return false;
        };
        if current_pid == ancestor_pid {
            return true;
        }
        current = parent_pid(current_pid);
    }
    false
}

fn process_belongs_to_app(pid: u32, app: &WorkspaceApp) -> bool {
    process_is_descendant_or_self(pid, app.pid)
        || app
            .process_group_id
            .is_some_and(|process_group_id| process_is_in_group(pid, process_group_id))
}

fn process_is_in_group(pid: u32, process_group_id: u32) -> bool {
    process_stat_ids(pid).is_some_and(|ids| ids.process_group_id == process_group_id)
}

struct ProcessStatIds {
    parent_pid: u32,
    process_group_id: u32,
}

fn parent_pid(pid: u32) -> Option<u32> {
    Some(process_stat_ids(pid)?.parent_pid)
}

fn process_stat_ids(pid: u32) -> Option<ProcessStatIds> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_command = stat.rsplit_once(") ")?.1;
    let mut fields = after_command.split_whitespace();
    fields.next()?;
    let parent_pid = fields.next()?.parse().ok()?;
    let process_group_id = fields.next()?.parse().ok()?;
    Some(ProcessStatIds {
        parent_pid,
        process_group_id,
    })
}

fn focus_matching_workspace_window(
    state: &mut DaemonState,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = wait_workspace_window(state, criteria)?.into_iter().next() else {
        return Ok(None);
    };
    focus_workspace_window(&state.status, &window.id)?;
    Ok(Some(window))
}

fn close_matching_workspace_window(
    state: &mut DaemonState,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = wait_workspace_window(state, criteria)?.into_iter().next() else {
        return Ok(None);
    };
    close_workspace_window(&state.status, &window.id)?;
    Ok(Some(window))
}

fn move_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    x: i32,
    y: i32,
) -> Result<Option<WorkspaceWindow>> {
    validate_workspace_coordinates(&state.status, x, y, "window move")?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    move_workspace_window(&state.status, &window.id, x, y)
        .map(|window| annotate_workspace_window(state, window))
        .map(Some)
        .with_context(|| format!("failed to move workspace window {}", window.id))
}

fn resize_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    width: u32,
    height: u32,
) -> Result<Option<WorkspaceWindow>> {
    validate_window_size(width, height)?;
    validate_window_size_for_workspace(&state.status, width, height)?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    resize_workspace_window(&state.status, &window.id, width, height)
        .map(|window| annotate_workspace_window(state, window))
        .map(Some)
        .with_context(|| format!("failed to resize workspace window {}", window.id))
}

fn raise_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    raise_workspace_window(&state.status, &window.id)
        .map(|window| annotate_workspace_window(state, window))
        .map(Some)
        .with_context(|| format!("failed to raise workspace window {}", window.id))
}

fn minimize_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    minimize_workspace_window(&state.status, &window.id)
        .map(|window| annotate_workspace_window(state, window))
        .map(Some)
        .with_context(|| format!("failed to minimize workspace window {}", window.id))
}

fn show_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = resolve_workspace_window_with_visibility(state, window_id, criteria, true)?
    else {
        return Ok(None);
    };
    show_workspace_window(&state.status, &window.id)
        .map(|window| annotate_workspace_window(state, window))
        .map(Some)
        .map_err(|error| anyhow!("failed to show workspace window {}: {error:#}", window.id))
}

struct WindowClickResult {
    window: WorkspaceWindow,
    x: i32,
    y: i32,
}

struct WindowPointerMoveResult {
    window: WorkspaceWindow,
    x: i32,
    y: i32,
}

struct WindowDragResult {
    window: WorkspaceWindow,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
}

struct WindowScrollResult {
    window: WorkspaceWindow,
    x: i32,
    y: i32,
}

struct WindowPasteResult {
    window: WorkspaceWindow,
    clipboard: WorkspaceClipboard,
}

fn click_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    x: i32,
    y: i32,
    button: u8,
    count: u8,
) -> Result<Option<WindowClickResult>> {
    validate_relative_click_coordinates(x, y)?;
    validate_click_options(button, count)?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    if x as u32 >= window.geometry.width || y as u32 >= window.geometry.height {
        bail!(
            "window click coordinates {x},{y} are outside window bounds {}x{}",
            window.geometry.width,
            window.geometry.height
        );
    }
    let absolute_x = window
        .geometry
        .x
        .checked_add(x)
        .context("window click X coordinate overflow")?;
    let absolute_y = window
        .geometry
        .y
        .checked_add(y)
        .context("window click Y coordinate overflow")?;
    focus_workspace_window(&state.status, &window.id)?;
    click_workspace(&state.status, absolute_x, absolute_y, button, count)?;
    Ok(Some(WindowClickResult {
        window,
        x: absolute_x,
        y: absolute_y,
    }))
}

fn move_workspace_pointer_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    x: i32,
    y: i32,
) -> Result<Option<WindowPointerMoveResult>> {
    validate_relative_click_coordinates(x, y)?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    if x as u32 >= window.geometry.width || y as u32 >= window.geometry.height {
        bail!(
            "window pointer coordinates {x},{y} are outside window bounds {}x{}",
            window.geometry.width,
            window.geometry.height
        );
    }
    let absolute_x = window
        .geometry
        .x
        .checked_add(x)
        .context("window pointer X coordinate overflow")?;
    let absolute_y = window
        .geometry
        .y
        .checked_add(y)
        .context("window pointer Y coordinate overflow")?;
    focus_workspace_window(&state.status, &window.id)?;
    move_workspace_pointer(&state.status, absolute_x, absolute_y)?;
    Ok(Some(WindowPointerMoveResult {
        window,
        x: absolute_x,
        y: absolute_y,
    }))
}

#[allow(clippy::too_many_arguments)]
fn drag_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    button: u8,
) -> Result<Option<WindowDragResult>> {
    validate_relative_click_coordinates(from_x, from_y)?;
    validate_relative_click_coordinates(to_x, to_y)?;
    validate_click_options(button, DEFAULT_CLICK_COUNT)?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    if from_x as u32 >= window.geometry.width || from_y as u32 >= window.geometry.height {
        bail!(
            "window drag start coordinates {from_x},{from_y} are outside window bounds {}x{}",
            window.geometry.width,
            window.geometry.height
        );
    }
    if to_x as u32 >= window.geometry.width || to_y as u32 >= window.geometry.height {
        bail!(
            "window drag end coordinates {to_x},{to_y} are outside window bounds {}x{}",
            window.geometry.width,
            window.geometry.height
        );
    }
    let absolute_from_x = window
        .geometry
        .x
        .checked_add(from_x)
        .context("window drag start X coordinate overflow")?;
    let absolute_from_y = window
        .geometry
        .y
        .checked_add(from_y)
        .context("window drag start Y coordinate overflow")?;
    let absolute_to_x = window
        .geometry
        .x
        .checked_add(to_x)
        .context("window drag end X coordinate overflow")?;
    let absolute_to_y = window
        .geometry
        .y
        .checked_add(to_y)
        .context("window drag end Y coordinate overflow")?;
    focus_workspace_window(&state.status, &window.id)?;
    drag_workspace(
        &state.status,
        absolute_from_x,
        absolute_from_y,
        absolute_to_x,
        absolute_to_y,
        button,
    )?;
    Ok(Some(WindowDragResult {
        window,
        from_x: absolute_from_x,
        from_y: absolute_from_y,
        to_x: absolute_to_x,
        to_y: absolute_to_y,
    }))
}

fn scroll_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    amount: u8,
) -> Result<Option<WindowScrollResult>> {
    validate_relative_click_coordinates(x, y)?;
    validate_scroll_options(direction, amount)?;
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    if x as u32 >= window.geometry.width || y as u32 >= window.geometry.height {
        bail!(
            "window scroll coordinates {x},{y} are outside window bounds {}x{}",
            window.geometry.width,
            window.geometry.height
        );
    }
    let absolute_x = window
        .geometry
        .x
        .checked_add(x)
        .context("window scroll X coordinate overflow")?;
    let absolute_y = window
        .geometry
        .y
        .checked_add(y)
        .context("window scroll Y coordinate overflow")?;
    focus_workspace_window(&state.status, &window.id)?;
    scroll_workspace(&state.status, absolute_x, absolute_y, direction, amount)?;
    Ok(Some(WindowScrollResult {
        window,
        x: absolute_x,
        y: absolute_y,
    }))
}

fn key_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    key: String,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = focus_workspace_window_target(state, window_id, criteria)? else {
        return Ok(None);
    };
    key_workspace(&state.status, key)?;
    Ok(Some(window))
}

fn type_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    text: String,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = focus_workspace_window_target(state, window_id, criteria)? else {
        return Ok(None);
    };
    type_workspace_text(&state.status, text)?;
    Ok(Some(window))
}

fn paste_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    text: &str,
    key: &str,
) -> Result<Option<WindowPasteResult>> {
    validate_clipboard_text(text)?;
    validate_key_text(key)?;
    let Some(window) = focus_workspace_window_target(state, window_id, criteria)? else {
        return Ok(None);
    };
    let clipboard = paste_workspace_text(&state.status, text, key)?;
    Ok(Some(WindowPasteResult { window, clipboard }))
}

fn focus_workspace_window_target(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    focus_workspace_window(&state.status, &window.id)?;
    Ok(Some(window))
}

fn resolve_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
) -> Result<Option<WorkspaceWindow>> {
    resolve_workspace_window_with_visibility(state, window_id, criteria, false)
}

fn resolve_workspace_window_with_visibility(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    include_hidden: bool,
) -> Result<Option<WorkspaceWindow>> {
    if let Some(window_id) = window_id {
        let window_id = sanitize_window_id(window_id)?;
        return window_info(&state.status, &window_id)
            .map(|window| annotate_workspace_window(state, window))
            .map(Some);
    }
    Ok(
        wait_workspace_window_with_visibility(state, criteria, include_hidden)?
            .into_iter()
            .next(),
    )
}

fn window_info(status: &WorkspaceStatus, id: &str) -> Result<WorkspaceWindow> {
    let id = sanitize_window_id(id)?;
    desktop_control(status)
        .window_info(&id)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn workspace_app_id_for_pid(status: &WorkspaceStatus, pid: u32) -> Option<String> {
    status
        .apps
        .iter()
        .find(|app| process_belongs_to_app(pid, app))
        .map(|app| app.id.clone())
}

fn capture_workspace_screenshot(
    status: &WorkspaceStatus,
    output_path: Option<PathBuf>,
    source: WorkspaceScreenshotSource,
) -> Result<WorkspaceScreenshot> {
    desktop_control(status).capture_screenshot(output_path, source)
}

struct WindowScreenshotResult {
    window: WorkspaceWindow,
    screenshot: WorkspaceScreenshot,
}

fn screenshot_workspace_window(
    state: &mut DaemonState,
    window_id: Option<&str>,
    criteria: &WindowWaitCriteria,
    output_path: Option<PathBuf>,
) -> Result<Option<WindowScreenshotResult>> {
    let Some(window) = resolve_workspace_window(state, window_id, criteria)? else {
        return Ok(None);
    };
    let screenshot = capture_workspace_window_screenshot(
        &state.status,
        &window,
        output_path,
        WorkspaceScreenshotSource::WorkspaceScreenshotWindow,
    )?;
    Ok(Some(WindowScreenshotResult { window, screenshot }))
}

fn capture_workspace_window_screenshot(
    status: &WorkspaceStatus,
    window: &WorkspaceWindow,
    output_path: Option<PathBuf>,
    source: WorkspaceScreenshotSource,
) -> Result<WorkspaceScreenshot> {
    desktop_control(status).capture_window_screenshot(window, output_path, source)
}

pub(crate) fn workspace_screenshot_result(
    path: PathBuf,
    width: u32,
    height: u32,
    source: WorkspaceScreenshotSource,
    target: WorkspaceScreenshotTarget,
    window_id: Option<String>,
) -> Result<WorkspaceScreenshot> {
    let bytes = fs::metadata(&path)
        .with_context(|| format!("failed to read screenshot metadata for {}", path.display()))?
        .len();
    Ok(WorkspaceScreenshot {
        path,
        width,
        height,
        format: "png".to_string(),
        bytes,
        captured_at_unix: unix_now(),
        source,
        target,
        window_id,
    })
}

fn screenshot_event_detail(screenshot: &WorkspaceScreenshot) -> serde_json::Value {
    serde_json::json!({
        "path": screenshot.path.display().to_string(),
        "source": screenshot.source,
        "target": screenshot.target,
        "window_id": screenshot.window_id.as_deref(),
    })
}

fn focus_workspace_window(status: &WorkspaceStatus, window_id: &str) -> Result<()> {
    desktop_control(status).focus_window(window_id)
}

fn close_workspace_window(status: &WorkspaceStatus, window_id: &str) -> Result<()> {
    desktop_control(status).close_window(window_id)
}

fn move_workspace_window(
    status: &WorkspaceStatus,
    window_id: &str,
    x: i32,
    y: i32,
) -> Result<WorkspaceWindow> {
    desktop_control(status)
        .move_window(window_id, x, y)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn resize_workspace_window(
    status: &WorkspaceStatus,
    window_id: &str,
    width: u32,
    height: u32,
) -> Result<WorkspaceWindow> {
    desktop_control(status)
        .resize_window(window_id, width, height)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn raise_workspace_window(status: &WorkspaceStatus, window_id: &str) -> Result<WorkspaceWindow> {
    desktop_control(status)
        .raise_window(window_id)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn minimize_workspace_window(status: &WorkspaceStatus, window_id: &str) -> Result<WorkspaceWindow> {
    desktop_control(status)
        .minimize_window(window_id)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn show_workspace_window(status: &WorkspaceStatus, window_id: &str) -> Result<WorkspaceWindow> {
    desktop_control(status)
        .show_window(window_id)
        .map(|window| annotate_workspace_window_for_status(status, window))
}

fn click_workspace(status: &WorkspaceStatus, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
    desktop_control(status).click(x, y, button, count)
}

fn move_workspace_pointer(status: &WorkspaceStatus, x: i32, y: i32) -> Result<()> {
    desktop_control(status).move_pointer(x, y)
}

fn drag_workspace(
    status: &WorkspaceStatus,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    button: u8,
) -> Result<()> {
    desktop_control(status).drag(from_x, from_y, to_x, to_y, button)
}

fn scroll_workspace(
    status: &WorkspaceStatus,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    amount: u8,
) -> Result<()> {
    desktop_control(status).scroll(x, y, direction, amount)
}

fn key_workspace(status: &WorkspaceStatus, key: String) -> Result<()> {
    desktop_control(status).key(&key)
}

fn type_workspace_text(status: &WorkspaceStatus, text: String) -> Result<()> {
    desktop_control(status).type_text(&text)
}

fn paste_workspace_text(
    status: &WorkspaceStatus,
    text: &str,
    key: &str,
) -> Result<WorkspaceClipboard> {
    validate_clipboard_text(text)?;
    validate_key_text(key)?;
    let clipboard = set_workspace_clipboard(status, text)?;
    key_workspace(status, key.trim().to_string())?;
    Ok(clipboard)
}

fn set_workspace_clipboard(status: &WorkspaceStatus, text: &str) -> Result<WorkspaceClipboard> {
    let desktop = desktop_control(status);
    let result = desktop.set_clipboard(text)?;
    // Clipboard helpers may fork before the compositor has accepted the new
    // selection. Observe it before a following paste chord can consume old data.
    // Use the same five-second budget as the clipboard helper processes.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match desktop.get_clipboard() {
            Ok(current) if current.content.as_deref() == Some(text) => return Ok(result),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).context("verify workspace clipboard selection")
            }
            _ if Instant::now() >= deadline => {
                bail!("workspace clipboard did not publish the requested text")
            }
            _ => thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn get_workspace_clipboard(status: &WorkspaceStatus) -> Result<WorkspaceClipboard> {
    desktop_control(status).get_clipboard()
}

fn read_workspace_app_log(
    state: &mut DaemonState,
    app_id: &str,
    stream: &str,
    tail_bytes: Option<u64>,
) -> Result<(WorkspaceAppLog, WorkspaceApp)> {
    refresh_apps(state)?;
    let stream = validate_log_stream(stream)?;
    let app = resolve_workspace_app(&state.status.apps, app_id)?;
    let path = match stream.as_str() {
        "stdout" => app.stdout_path.as_ref(),
        "stderr" => app.stderr_path.as_ref(),
        _ => None,
    }
    .ok_or_else(|| anyhow!("workspace app {} has no {stream} log path", app.id))?;
    let (content, bytes_read, truncated) = read_log_content(path, tail_bytes)?;

    Ok((
        WorkspaceAppLog {
            app_id: app.id.clone(),
            stream,
            path: path.clone(),
            content,
            bytes_read,
            truncated,
        },
        app.clone(),
    ))
}

fn wait_workspace_app(
    state: &mut DaemonState,
    app_id: &str,
    timeout_ms: u64,
    kill_on_timeout: bool,
) -> Result<(bool, bool, WorkspaceApp)> {
    let app_id = app_id.trim();
    if app_id.is_empty() {
        bail!("app id cannot be empty");
    }

    let timeout = Duration::from_millis(timeout_ms);
    let started = Instant::now();
    loop {
        refresh_apps(state)?;
        let app = resolve_workspace_app(&state.status.apps, app_id)?;
        if !app.running {
            return Ok((true, false, app.clone()));
        }
        if started.elapsed() >= timeout {
            if kill_on_timeout {
                let (_message, app, killed) = kill_workspace_app(state, app_id)?;
                return Ok((!app.running, killed, app));
            }
            return Ok((false, false, app.clone()));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn read_log_content(path: &Path, tail_bytes: Option<u64>) -> Result<(String, u64, bool)> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let total = bytes.len();
    let limit = tail_bytes
        .map(|value| value.min(usize::MAX as u64) as usize)
        .unwrap_or(total);
    let start = total.saturating_sub(limit);
    let content = String::from_utf8_lossy(&bytes[start..]).to_string();
    Ok((content, (total - start) as u64, start > 0))
}

fn terminate_app_process(app_id: &str, child: &mut Child) -> Result<ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .with_context(|| format!("failed to check workspace app {app_id} status"))?
    {
        return Ok(status);
    }

    let pgid = child.id();
    signal_process_group(pgid, SIGTERM)
        .with_context(|| format!("failed to terminate workspace app {app_id} process group"))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to check workspace app {app_id} status"))?
        {
            return Ok(status);
        }
        if started.elapsed() >= Duration::from_millis(APP_TERMINATE_GRACE_MS) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    signal_process_group(pgid, SIGKILL)
        .with_context(|| format!("failed to kill workspace app {app_id} process group"))?;
    let _ = child.kill();
    child
        .wait()
        .with_context(|| format!("failed to wait for workspace app {app_id}"))
}

fn signal_process_group(pgid: u32, signal: i32) -> Result<()> {
    if pgid > i32::MAX as u32 {
        bail!("process group id {pgid} is too large to signal");
    }
    let target = -(pgid as i32);
    let result = unsafe { kill(target, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to signal process group {pgid}"))
}

fn kill_workspace_app(
    state: &mut DaemonState,
    app_id: &str,
) -> Result<(String, WorkspaceApp, bool)> {
    let app_id = app_id.trim();
    if app_id.is_empty() {
        bail!("app id cannot be empty");
    }

    let (message, exit_detail, app_info, killed) = {
        let app = resolve_workspace_app_process_mut(&mut state.apps, app_id)?;

        if !app.info.running {
            (
                format!("workspace app {} is already stopped", app.info.id),
                None,
                app.info.clone(),
                false,
            )
        } else if let Some(status) = app
            .child
            .try_wait()
            .context("failed to check app process status")?
        {
            apply_app_exit_status(&mut app.info, status);
            (
                format!("workspace app {} is already stopped", app.info.id),
                Some(app_exit_event_detail(&app.info)),
                app.info.clone(),
                false,
            )
        } else {
            let status = terminate_app_process(&app.info.id, &mut app.child)?;
            apply_app_exit_status(&mut app.info, status);
            (
                format!("workspace app {} killed", app.info.id),
                Some(app_exit_event_detail(&app.info)),
                app.info.clone(),
                true,
            )
        }
    };

    state.status.apps = state.apps.iter().map(|app| app.info.clone()).collect();
    if let Some(detail) = exit_detail {
        record_event(state, "app_exit", detail)?;
    }
    Ok((message, app_info, killed))
}

fn terminate_running_workspace_apps(state: &mut DaemonState) -> Result<Vec<WorkspaceApp>> {
    terminal_cleanup::cleanup(
        &state.status.runtime_dir,
        &state.status.id,
        &state.status.session_id,
        &state.status.apps,
        true,
    )?;
    let mut stopped_apps = Vec::new();
    let mut exit_events = Vec::new();
    for app in &mut state.apps {
        if app.info.running {
            match terminate_app_process(&app.info.id, &mut app.child) {
                Ok(status) => apply_app_exit_status(&mut app.info, status),
                Err(error) => mark_app_exit_error(&mut app.info, error),
            }
            exit_events.push(app_exit_event_detail(&app.info));
            stopped_apps.push(app.info.clone());
        }
    }
    state.status.apps = state.apps.iter().map(|app| app.info.clone()).collect();
    for detail in exit_events {
        record_event(state, "app_exit", detail)?;
    }
    terminal_cleanup::cleanup(
        &state.status.runtime_dir,
        &state.status.id,
        &state.status.session_id,
        &state.status.apps,
        false,
    )?;
    Ok(stopped_apps)
}

fn resolve_workspace_app<'a>(apps: &'a [WorkspaceApp], app_id: &str) -> Result<&'a WorkspaceApp> {
    let mut matches = apps.iter().filter(|app| matches_app_id(app, app_id));
    let Some(first) = matches.next() else {
        bail!("workspace app {app_id:?} was not found");
    };
    if let Some(second) = matches.next() {
        let mut labels = vec![app_label(first), app_label(second)];
        labels.extend(matches.map(app_label));
        bail!(
            "workspace app target {app_id:?} matched multiple apps: {}",
            labels.join(", ")
        );
    }
    Ok(first)
}

fn resolve_workspace_app_process_mut<'a>(
    apps: &'a mut [AppProcess],
    app_id: &str,
) -> Result<&'a mut AppProcess> {
    let matches = apps
        .iter()
        .enumerate()
        .filter(|(_, app)| matches_app_id(&app.info, app_id))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => bail!("workspace app {app_id:?} was not found"),
        [index] => Ok(&mut apps[*index]),
        _ => {
            let labels = matches
                .iter()
                .map(|index| app_label(&apps[*index].info))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("workspace app target {app_id:?} matched multiple apps: {labels}")
        }
    }
}

fn app_label(app: &WorkspaceApp) -> String {
    match &app.name {
        Some(name) => format!("{} (name {name:?}, pid {})", app.id, app.pid),
        None => format!("{} (pid {})", app.id, app.pid),
    }
}

fn matches_app_id(app: &WorkspaceApp, app_id: &str) -> bool {
    app.id == app_id || app.pid.to_string() == app_id || app.name.as_deref() == Some(app_id)
}

fn command_matches(command: &[String], needle: &str) -> bool {
    command
        .iter()
        .any(|arg| contains_ascii_case_insensitive(arg, needle))
        || contains_ascii_case_insensitive(&command.join(" "), needle)
}

fn response_last_app_id(response: &IpcResponse) -> Option<String> {
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

fn response_app<'a>(response: &'a IpcResponse, app_id: &str) -> Option<&'a WorkspaceApp> {
    if let Some(app) = response
        .apps
        .as_ref()
        .and_then(|apps| apps.iter().find(|app| matches_app_id(app, app_id)))
    {
        return Some(app);
    }
    response
        .status
        .as_ref()?
        .apps
        .iter()
        .find(|app| matches_app_id(app, app_id))
}

fn apply_app_exit_status(app: &mut WorkspaceApp, status: ExitStatus) {
    mark_app_stopped(app);
    app.running = false;
    app.exit_status = Some(status.to_string());
    app.exit_code = status.code();
    app.exit_signal = status.signal();
}

fn mark_app_exit_error(app: &mut WorkspaceApp, error: impl ToString) {
    mark_app_stopped(app);
    app.running = false;
    app.exit_status = Some(error.to_string());
    app.exit_code = None;
    app.exit_signal = None;
}

fn mark_app_stopped(app: &mut WorkspaceApp) {
    let stopped_at_unix = unix_now();
    app.stopped_at_unix = Some(stopped_at_unix);
    app.runtime_seconds = Some(stopped_at_unix.saturating_sub(app.started_at_unix));
}

fn app_exit_event_detail(app: &WorkspaceApp) -> serde_json::Value {
    serde_json::json!({
        "app_id": &app.id,
        "name": app.name.as_deref(),
        "pid": app.pid,
        "process_group_id": app.process_group_id,
        "command": &app.command,
        "profile_id": app.profile_id.as_deref(),
        "exit_status": app.exit_status.as_deref(),
        "exit_code": app.exit_code,
        "exit_signal": app.exit_signal,
        "stopped_at_unix": app.stopped_at_unix,
        "runtime_seconds": app.runtime_seconds,
    })
}

fn refresh_apps(state: &mut DaemonState) -> Result<()> {
    let mut exit_events = Vec::new();
    for app in &mut state.apps {
        if app.info.running {
            match app.child.try_wait() {
                Ok(Some(status)) => {
                    apply_app_exit_status(&mut app.info, status);
                    exit_events.push(app_exit_event_detail(&app.info));
                }
                Ok(None) => {}
                Err(error) => {
                    mark_app_exit_error(&mut app.info, error);
                    exit_events.push(app_exit_event_detail(&app.info));
                }
            }
        }
    }
    state.status.apps = state.apps.iter().map(|app| app.info.clone()).collect();
    for detail in exit_events {
        record_event(state, "app_exit", detail)?;
    }
    Ok(())
}

fn request(socket_path: &Path, request: IpcRequest) -> Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    let response: IpcResponse =
        serde_json::from_str(line.trim()).context("failed to parse workspace IPC response")?;
    Ok(response)
}

/// Every startup step in the daemon is bounded and reports failure through
/// `daemon.err.log` or by exiting, so wait for one of those rather than a fixed
/// budget that slow native compositors can outlast. The cap only guards a hung
/// daemon (summed worst-case native startup is about 70 s), and a daemon that
/// hits it is stopped instead of left running behind a failed start.
const DAEMON_STARTUP_HANG_GUARD: Duration = Duration::from_secs(120);

fn wait_for_daemon_startup(options: &DaemonOptions) -> Result<()> {
    let pid_path = options.runtime_dir.join(DAEMON_PID_FILE);
    let deadline = Instant::now() + DAEMON_STARTUP_HANG_GUARD;
    let mut pid = None;
    while Instant::now() < deadline {
        if daemon_socket_ready(&options.socket_path)? {
            return Ok(());
        }
        if pid.is_none() {
            pid = fs::read_to_string(&pid_path)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok());
        }
        if let Some(pid) = pid {
            if !owned_daemon_running(pid, &options.session_id) {
                // The daemon may have written its error just before exiting.
                if daemon_socket_ready(&options.socket_path)? {
                    return Ok(());
                }
                bail!(
                    "workspace daemon exited before it was ready; see {}",
                    options.runtime_dir.join("daemon.err.log").display()
                );
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    if let Some(pid) = pid.filter(|pid| owned_daemon_running(*pid, &options.session_id)) {
        // setsid makes the daemon its own group leader; signal the group so
        // helpers it started without a parent-death signal go with it.
        let target = if linux_process_group_id(pid as u32) == Some(pid as u32) {
            -pid
        } else {
            pid
        };
        unsafe {
            libc::kill(target, libc::SIGTERM);
        }
        let grace = Instant::now() + Duration::from_secs(5);
        while Instant::now() < grace && owned_daemon_running(pid, &options.session_id) {
            thread::sleep(Duration::from_millis(100));
        }
        if owned_daemon_running(pid, &options.session_id) {
            unsafe {
                libc::kill(target, libc::SIGKILL);
            }
        }
    }
    let cleanup = if options.session.version.is_some() {
        match crate::session::runtime::cleanup_stale_userspaces(&options.id, false) {
            Ok(_) => String::new(),
            Err(error) => format!("; retained userspace for verified cleanup: {error:#}"),
        }
    } else {
        String::new()
    };
    bail!(
        "workspace daemon did not become ready within {} s and was stopped{cleanup}",
        DAEMON_STARTUP_HANG_GUARD.as_secs()
    )
}

/// True while `pid` is a live daemon for exactly this start, matched by its
/// unique session id so a recycled PID is never signalled.
fn owned_daemon_running(pid: i32, session_id: &str) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    if stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| matches!(state, "Z" | "X"))
    {
        return false;
    }
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args = cmdline.split(|byte| *byte == 0).collect::<Vec<_>>();
    args.windows(2)
        .any(|pair| pair[0] == b"--session-id" && pair[1] == session_id.as_bytes())
}

/// Ok(true) once the daemon serves IPC, an error once it reported a startup
/// failure, Ok(false) while it is still starting.
fn daemon_socket_ready(socket_path: &Path) -> Result<bool> {
    if socket_path.exists() {
        return Ok(true);
    }
    if let Some(parent) = socket_path.parent() {
        if let Ok(log) = fs::read_to_string(parent.join("daemon.err.log")) {
            if let Some(error) = startup_failure(&log) {
                bail!("workspace startup failed: {}", error.trim());
            }
        }
    }
    Ok(false)
}

/// The startup failure a daemon log reports, from its first `Error:` line.
/// A panic can happen before the daemon records its pid, so it counts too.
/// Lines before them are the daemon's notes, such as a service it skipped.
fn startup_failure(log: &str) -> Option<&str> {
    let mut offset = 0;
    for line in log.split_inclusive('\n') {
        if line.starts_with("Error:") || line.contains("panicked at") {
            return Some(&log[offset..]);
        }
        offset += line.len();
    }
    None
}

fn wait_for_socket_removed(socket_path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !socket_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "timed out waiting for workspace IPC socket {} to be removed",
        socket_path.display()
    );
}

pub(crate) fn pick_display() -> Result<String> {
    for number in DISPLAY_RANGE {
        let display = format!(":{number}");
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        if socket.exists() || lock.exists() {
            if display_is_reachable(&display) {
                continue;
            }
            if remove_dead_x11_display_artifacts(number, &socket, &lock)? {
                return Ok(display);
            }
            continue;
        }
        if !display_is_reachable(&display) {
            return Ok(display);
        }
    }
    bail!("no free X11 display found in range :90..:179");
}

fn display_is_reachable(display: &str) -> bool {
    Command::new("xdpyinfo")
        .arg("-display")
        .arg(display)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn remove_dead_x11_display_artifacts(number: u32, socket: &Path, lock: &Path) -> Result<bool> {
    let Some(pid) = read_x11_lock_pid(lock) else {
        return Ok(false);
    };
    if process_exists(pid) {
        return Ok(false);
    }
    // A concurrent start (or Xvfb itself) may replace the stale lock with a
    // live one; only remove artifacts while the same stale lock is in place.
    let stale = fs::symlink_metadata(lock).map(|meta| meta.ino()).ok();
    let still_stale = || {
        fs::symlink_metadata(lock).map(|meta| meta.ino()).ok() == stale
            && read_x11_lock_pid(lock) == Some(pid)
    };
    // Concurrent starts may find the same stale artifacts; already gone is
    // the result we want.
    for (path, what) in [(socket, "socket"), (lock, "lock")] {
        if !still_stale() {
            return Ok(false);
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove stale X11 {what} {}", path.display())
                })
            }
        }
    }
    eprintln!("removed stale X11 display artifacts for :{number} with dead pid {pid}");
    Ok(true)
}

fn read_x11_lock_pid(lock: &Path) -> Option<u32> {
    if !lock.exists() {
        return None;
    }
    let Ok(content) = fs::read_to_string(lock) else {
        return None;
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

pub(crate) fn create_xauthority(display: &str, path: &Path) -> Result<()> {
    let cookie = random_cookie()?;
    let _ = fs::remove_file(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let output = Command::new("xauth")
        .arg("-f")
        .arg(path)
        .arg("add")
        .arg(display)
        .arg(".")
        .arg(cookie)
        .output()
        .context("failed to run xauth")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "xauth failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}

fn random_cookie() -> Result<String> {
    let mut file = fs::File::open("/dev/urandom").context("failed to open /dev/urandom")?;
    let mut bytes = [0_u8; 16];
    file.read_exact(&mut bytes)
        .context("failed to read random X authority cookie")?;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn remove_stale_socket(socket_path: &Path) -> Result<()> {
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("failed to remove {}", socket_path.display()))?;
    }
    Ok(())
}

fn validate_unix_socket_path(socket_path: &Path) -> Result<()> {
    let byte_len = socket_path.as_os_str().as_bytes().len();
    if byte_len > MAX_UNIX_SOCKET_PATH_BYTES {
        bail!(
            "workspace socket path {} is {byte_len} bytes; Unix socket paths must be at most {MAX_UNIX_SOCKET_PATH_BYTES} bytes. Use a shorter workspace id or a shorter XDG_RUNTIME_DIR",
            socket_path.display()
        );
    }
    Ok(())
}

fn workspace_socket_path(id: &str) -> PathBuf {
    workspace_dir(id).join("control.sock")
}

fn workspace_dir(id: &str) -> PathBuf {
    runtime_base_dir().join(id)
}

fn runtime_base_dir() -> PathBuf {
    // Apps now have a session-private XDG_RUNTIME_DIR. Preserve their ability
    // to address the owning daemon (including `workspace stop` from an app)
    // through the attachment context the daemon exported at launch.
    if let (Ok(id), Some(runtime), Some(socket)) = (
        env::var("AGENT_WORKSPACE_ID"),
        env::var_os("AGENT_WORKSPACE_RUNTIME_DIR"),
        env::var_os("AGENT_WORKSPACE_SOCKET"),
    ) {
        let runtime = PathBuf::from(runtime);
        if runtime.is_absolute()
            && runtime.file_name().and_then(|name| name.to_str()) == Some(id.as_str())
            && socket == runtime.join("control.sock")
        {
            if let Some(parent) = runtime.parent() {
                return parent.to_path_buf();
            }
        }
    }
    runtime_base_dir_from_candidates(
        env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        discover_systemd_runtime_dir(),
        temp_runtime_dir(),
    )
}

fn runtime_base_dir_from_candidates(
    xdg_runtime_dir: Option<PathBuf>,
    systemd_runtime_dir: Option<PathBuf>,
    temp_runtime_dir: PathBuf,
) -> PathBuf {
    xdg_runtime_dir
        .or(systemd_runtime_dir)
        .unwrap_or(temp_runtime_dir)
        .join("agent-workspace-linux")
}

fn discover_systemd_runtime_dir() -> Option<PathBuf> {
    let uid = current_effective_uid()?;
    let path = PathBuf::from(format!("/run/user/{uid}"));
    let metadata = fs::metadata(&path).ok()?;
    if metadata.is_dir() && metadata.uid() == uid {
        Some(path)
    } else {
        None
    }
}

fn current_effective_uid() -> Option<u32> {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| parse_effective_uid_from_proc_status(&status))
        .or_else(|| env::var("EUID").ok().and_then(|uid| uid.parse().ok()))
        .or_else(|| env::var("UID").ok().and_then(|uid| uid.parse().ok()))
}

fn parse_effective_uid_from_proc_status(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != "Uid:" {
            return None;
        }
        let _real_uid = parts.next()?;
        parts.next()?.parse().ok()
    })
}

fn temp_runtime_dir() -> PathBuf {
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    PathBuf::from(format!("/tmp/agent-workspace-linux-{user}"))
}

pub(crate) fn sanitize_workspace_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("workspace id cannot be empty");
    }
    if trimmed == VIEWER_REGISTRY_DIR {
        bail!("workspace id '{trimmed}' is reserved for the viewer registry");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("workspace id may only contain ASCII letters, numbers, '-' and '_'");
    }
    Ok(trimmed.to_string())
}

fn normalize_session_id(session_id: &str) -> Result<String> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        bail!("workspace session id cannot be empty");
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("workspace session id may only contain ASCII letters, numbers, '-', '_' and '.'");
    }
    Ok(trimmed.to_string())
}

fn normalize_workspace_purpose(purpose: Option<String>) -> Result<Option<String>> {
    let Some(purpose) = purpose else {
        return Ok(None);
    };
    let trimmed = purpose.trim();
    if trimmed.is_empty() {
        bail!("workspace purpose cannot be empty");
    }
    if trimmed.contains('\0') {
        bail!("workspace purpose cannot contain NUL bytes");
    }
    if trimmed.len() > 512 {
        bail!("workspace purpose cannot be longer than 512 bytes");
    }
    Ok(Some(trimmed.to_string()))
}

fn validate_app_list_filters(
    app_id: &Option<String>,
    name_contains: &Option<String>,
    command_contains: &Option<String>,
    profile_id: &Option<String>,
) -> Result<()> {
    if app_id
        .as_ref()
        .is_some_and(|app_id| app_id.trim().is_empty())
    {
        bail!("app id cannot be empty");
    }
    if name_contains
        .as_ref()
        .is_some_and(|name| name.trim().is_empty())
    {
        bail!("app name filter cannot be empty");
    }
    if command_contains
        .as_ref()
        .is_some_and(|command| command.trim().is_empty())
    {
        bail!("app command filter cannot be empty");
    }
    if profile_id
        .as_ref()
        .is_some_and(|profile| profile.trim().is_empty())
    {
        bail!("profile id cannot be empty");
    }
    Ok(())
}

fn validate_window_match_options(
    title_contains: &Option<String>,
    class_contains: &Option<String>,
    pid: Option<u32>,
    app_id: &Option<String>,
    require_filter: bool,
) -> Result<()> {
    if title_contains
        .as_ref()
        .is_some_and(|title| title.trim().is_empty())
    {
        bail!("window title filter cannot be empty");
    }
    if class_contains
        .as_ref()
        .is_some_and(|class| class.trim().is_empty())
    {
        bail!("window class filter cannot be empty");
    }
    if app_id
        .as_ref()
        .is_some_and(|app_id| app_id.trim().is_empty())
    {
        bail!("app id cannot be empty");
    }
    if require_filter
        && title_contains.is_none()
        && class_contains.is_none()
        && pid.is_none()
        && app_id.is_none()
    {
        bail!("window match requires --title, --class, --pid, or --app");
    }
    Ok(())
}

fn validate_window_list_filters(
    title_contains: &Option<String>,
    class_contains: &Option<String>,
    pid: Option<u32>,
    app_id: &Option<String>,
) -> Result<()> {
    validate_window_match_options(title_contains, class_contains, pid, app_id, false)
}

fn validate_window_target_options(
    window_id: &Option<String>,
    title_contains: &Option<String>,
    class_contains: &Option<String>,
    pid: Option<u32>,
    app_id: &Option<String>,
) -> Result<()> {
    if let Some(window_id) = window_id {
        sanitize_window_id(window_id)?;
    }
    validate_window_match_options(
        title_contains,
        class_contains,
        pid,
        app_id,
        window_id.is_none(),
    )?;
    if window_id.is_some()
        && (title_contains.is_some()
            || class_contains.is_some()
            || pid.is_some()
            || app_id.is_some())
    {
        bail!("window target accepts either a window id or match filters, not both");
    }
    Ok(())
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn validate_relative_click_coordinates(x: i32, y: i32) -> Result<()> {
    if x < 0 || y < 0 {
        bail!("window click coordinates must be non-negative");
    }
    Ok(())
}

pub(crate) fn validate_workspace_coordinates(
    status: &WorkspaceStatus,
    x: i32,
    y: i32,
    label: &str,
) -> Result<()> {
    if x < 0 || y < 0 || x as u32 >= status.width || y as u32 >= status.height {
        bail!(
            "{label} coordinates {x},{y} are outside workspace bounds {}x{}",
            status.width,
            status.height
        );
    }
    Ok(())
}

pub(crate) fn validate_window_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("window size must be positive");
    }
    Ok(())
}

pub(crate) fn validate_window_size_for_workspace(
    status: &WorkspaceStatus,
    width: u32,
    height: u32,
) -> Result<()> {
    if width > status.width || height > status.height {
        bail!(
            "window size {}x{} is outside workspace bounds {}x{}",
            width,
            height,
            status.width,
            status.height
        );
    }
    Ok(())
}

pub(crate) fn validate_click_options(button: u8, count: u8) -> Result<()> {
    if !(1..=5).contains(&button) {
        bail!("click button must be between 1 and 5");
    }
    if count == 0 || count > 20 {
        bail!("click count must be between 1 and 20");
    }
    Ok(())
}

pub(crate) fn validate_scroll_options(_direction: ScrollDirection, amount: u8) -> Result<()> {
    if amount == 0 || amount > MAX_SCROLL_AMOUNT {
        bail!("scroll amount must be between 1 and {MAX_SCROLL_AMOUNT}");
    }
    Ok(())
}

pub(crate) fn validate_clipboard_text(text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("clipboard text cannot be empty");
    }
    let byte_len = text.len();
    if byte_len > MAX_CLIPBOARD_TEXT_BYTES {
        bail!(
            "clipboard text is {byte_len} bytes; maximum is {MAX_CLIPBOARD_TEXT_BYTES} bytes. Paste a smaller selection or transfer large content through a mounted file"
        );
    }
    if text.contains('\0') {
        bail!("clipboard text cannot contain NUL bytes");
    }
    Ok(())
}

fn normalize_paste_key(key: Option<String>) -> Result<String> {
    let key = key.unwrap_or_else(|| DEFAULT_PASTE_KEY.to_string());
    validate_key_text(&key)?;
    Ok(key.trim().to_string())
}

fn validate_key_text(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        bail!("key cannot be empty");
    }
    Ok(())
}

pub fn validate_optional_app_name(name: &Option<String>) -> Result<()> {
    let Some(name) = name else {
        return Ok(());
    };
    if name.trim().is_empty() {
        bail!("app name cannot be empty");
    }
    if name.contains('\0') {
        bail!("app name cannot contain NUL bytes");
    }
    Ok(())
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

fn validate_log_stream(stream: &str) -> Result<String> {
    match stream.trim() {
        "stdout" => Ok("stdout".to_string()),
        "stderr" => Ok("stderr".to_string()),
        _ => bail!("log stream must be 'stdout' or 'stderr'"),
    }
}

fn first_available_command(commands: &[&str]) -> Check {
    for command in commands {
        let check = command_path_check(command);
        if check.ok {
            return check;
        }
    }

    Check {
        ok: false,
        detail: format!("missing all of: {}", commands.join(", ")),
    }
}

fn host_display_check() -> Check {
    let wayland = env::var_os("WAYLAND_DISPLAY")
        .filter(|value| !value.as_os_str().is_empty())
        .map(|value| value.to_string_lossy().to_string());
    let display = env::var_os("DISPLAY")
        .filter(|value| !value.as_os_str().is_empty())
        .map(|value| value.to_string_lossy().to_string());

    match (wayland, display) {
        (Some(wayland), Some(display)) => Check {
            ok: true,
            detail: format!("WAYLAND_DISPLAY={wayland}, DISPLAY={display}"),
        },
        (Some(wayland), None) => Check {
            ok: true,
            detail: format!("WAYLAND_DISPLAY={wayland}"),
        },
        (None, Some(display)) => Check {
            ok: true,
            detail: format!("DISPLAY={display}"),
        },
        (None, None) => Check {
            ok: false,
            detail: "missing DISPLAY and WAYLAND_DISPLAY".to_string(),
        },
    }
}

fn pkg_config_package_check(package: &str) -> Check {
    match Command::new("pkg-config")
        .args(["--modversion", package])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Check {
                ok: true,
                detail: if version.is_empty() {
                    format!("{package}: available")
                } else {
                    format!("{package}: {version}")
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("{package}: missing pkg-config metadata")
            };
            Check { ok: false, detail }
        }
        Err(error) => Check {
            ok: false,
            detail: format!("pkg-config unavailable: {error}"),
        },
    }
}

pub(crate) fn first_available_command_name<'a>(commands: &'a [&str]) -> Option<&'a str> {
    commands
        .iter()
        .find(|command| command_path_check(command).ok)
        .copied()
}

pub(crate) fn command_path_check(command: &str) -> Check {
    match Command::new("sh")
        .args(["-c", &format!("command -v {command}")])
        .output()
    {
        Ok(output) if output.status.success() => {
            let detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Check {
                ok: true,
                detail: if detail.is_empty() {
                    "ok".to_string()
                } else {
                    detail
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("missing: {command}")
            };
            Check { ok: false, detail }
        }
        Err(error) => Check {
            ok: false,
            detail: error.to_string(),
        },
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn unix_now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-26: a note before the error hid GNOME's scale message.
    #[test]
    fn a_startup_failure_after_daemon_notes_is_reported() {
        let log = "agent workspace: no user session bus found; the keyring is not relayed\nError: GNOME monitor does not support scale 1.5\n";
        assert_eq!(
            startup_failure(log).map(str::trim),
            Some("Error: GNOME monitor does not support scale 1.5")
        );
        assert_eq!(startup_failure("agent workspace: note\n"), None);
        assert!(startup_failure("thread 'main' panicked at src/x.rs:1\n").is_some());
    }

    #[test]
    fn observe_screenshot_reuses_stable_default_frame() {
        assert_eq!(
            observe_screenshot_output_path(None),
            Some(PathBuf::from(OBSERVE_SCREENSHOT_FILE))
        );

        let explicit = PathBuf::from("/tmp/agent-observe.png");
        assert_eq!(
            observe_screenshot_output_path(Some(explicit.clone())),
            Some(explicit)
        );
    }

    #[test]
    fn screenshot_event_detail_includes_provenance() {
        let screenshot = WorkspaceScreenshot {
            path: PathBuf::from("/tmp/agent-window.png"),
            width: 640,
            height: 480,
            format: "png".to_string(),
            bytes: 1234,
            captured_at_unix: 42,
            source: WorkspaceScreenshotSource::WorkspaceScreenshotWindow,
            target: WorkspaceScreenshotTarget::Window,
            window_id: Some("4194307".to_string()),
        };

        let detail = screenshot_event_detail(&screenshot);

        assert_eq!(detail["path"], "/tmp/agent-window.png");
        assert_eq!(detail["source"], "workspace_screenshot_window");
        assert_eq!(detail["target"], "window");
        assert_eq!(detail["window_id"], "4194307");
    }

    #[test]
    fn clipboard_text_rejects_oversized_payloads() {
        let at_limit = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES);
        validate_clipboard_text(&at_limit).expect("limit-sized clipboard text should pass");

        let too_large = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
        let error =
            validate_clipboard_text(&too_large).expect_err("oversized clipboard text should fail");
        let message = error.to_string();
        assert!(message.contains(&(MAX_CLIPBOARD_TEXT_BYTES + 1).to_string()));
        assert!(message.contains(&MAX_CLIPBOARD_TEXT_BYTES.to_string()));
        assert!(message.contains("mounted file"));
    }

    #[test]
    fn legacy_screenshot_response_without_provenance_still_parses() {
        let json = r#"{
            "path": "/tmp/agent-window.png",
            "width": 640,
            "height": 480,
            "format": "png",
            "bytes": 1234,
            "captured_at_unix": 42
        }"#;

        let screenshot: WorkspaceScreenshot =
            serde_json::from_str(json).expect("legacy screenshot should parse");

        assert_eq!(
            screenshot.source,
            WorkspaceScreenshotSource::WorkspaceScreenshot
        );
        assert_eq!(screenshot.target, WorkspaceScreenshotTarget::Root);
    }

    fn status_with_profile_defaults() -> WorkspaceStatus {
        WorkspaceStatus {
            id: "qa-ws".to_string(),
            session: SessionSpec::default(),
            session_id: "session-qa".to_string(),
            purpose: Some("QA".to_string()),
            profile_id: Some("qa".to_string()),
            profile_cwd: Some(PathBuf::from("/workspace/project")),
            profile_env: vec![
                EnvVar {
                    name: "BASE".to_string(),
                    value: "one".to_string(),
                },
                EnvVar {
                    name: "OVERRIDE".to_string(),
                    value: "from-profile".to_string(),
                },
            ],
            user_acknowledged_hidden_workspace: true,
            ready: true,
            started_at_unix: 1,
            display: ":90".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: PathBuf::from("/tmp/qa-ws"),
            socket_path: PathBuf::from("/tmp/qa-ws/control.sock"),
            xauthority_path: PathBuf::from("/tmp/qa-ws/Xauthority"),
            daemon_pid: None,
            x_server_pid: 0,
            window_manager_pid: None,
            last_event_sequence: 0,
            apps: Vec::new(),
        }
    }

    #[test]
    fn legacy_status_and_manifest_without_session_default_to_x11() {
        let status = status_with_profile_defaults();
        let manifest = workspace_manifest(&status, None);

        let mut status_json = serde_json::to_value(&status).expect("serialize status");
        status_json
            .as_object_mut()
            .expect("status object")
            .remove("session");
        let legacy_status: WorkspaceStatus =
            serde_json::from_value(status_json).expect("legacy status should parse");

        let mut manifest_json = serde_json::to_value(&manifest).expect("serialize manifest");
        manifest_json
            .as_object_mut()
            .expect("manifest object")
            .remove("session");
        let legacy_manifest: WorkspaceManifest =
            serde_json::from_value(manifest_json).expect("legacy manifest should parse");

        assert_eq!(legacy_status.session, SessionSpec::default());
        assert_eq!(legacy_manifest.session, SessionSpec::default());
    }

    #[test]
    fn launch_environment_reserves_session_identity_and_allows_regular_env() {
        let status = status_with_profile_defaults();
        let mut command = Command::new("true");
        command.env("DBUS_SESSION_BUS_ADDRESS", "host-dbus");
        let launch_env = [
            "DISPLAY",
            "XAUTHORITY",
            "WAYLAND_DISPLAY",
            "AGENT_WORKSPACE_ID",
            "AGENT_WORKSPACE_SESSION_ID",
            "AGENT_WORKSPACE_RUNTIME_DIR",
            "AGENT_WORKSPACE_SOCKET",
            "DBUS_SESSION_BUS_ADDRESS",
        ]
        .into_iter()
        .map(|name| EnvVar {
            name: name.to_string(),
            value: "host-or-user-override".to_string(),
        })
        .chain([EnvVar {
            name: "LEGITIMATE_NON_SESSION_ENV".to_string(),
            value: "preserved".to_string(),
        }])
        .collect::<Vec<_>>();

        crate::launch::configure_launch_environment(&mut command, &status, &launch_env);
        let envs = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let value = |name| envs.get(name).and_then(|value| value.as_deref());

        assert_eq!(value("DISPLAY"), Some(":90"));
        assert_eq!(value("XAUTHORITY"), Some("/tmp/qa-ws/Xauthority"));
        assert_eq!(value("AGENT_WORKSPACE_ID"), Some("qa-ws"));
        assert_eq!(value("AGENT_WORKSPACE_SESSION_ID"), Some("session-qa"));
        assert_eq!(value("AGENT_WORKSPACE_RUNTIME_DIR"), Some("/tmp/qa-ws"));
        assert_eq!(
            value("AGENT_WORKSPACE_SOCKET"),
            Some("/tmp/qa-ws/control.sock")
        );
        assert_eq!(value("LEGITIMATE_NON_SESSION_ENV"), Some("preserved"));
        assert!(matches!(envs.get("WAYLAND_DISPLAY"), Some(None)));
        assert!(matches!(envs.get("DBUS_SESSION_BUS_ADDRESS"), Some(None)));
    }

    #[test]
    fn validates_workspace_socket_path_before_spawn() {
        let max_path = format!("/{}", "x".repeat(MAX_UNIX_SOCKET_PATH_BYTES - 1));
        validate_unix_socket_path(Path::new(&max_path)).expect("max-length socket path");

        let too_long_path = format!("/{}", "x".repeat(MAX_UNIX_SOCKET_PATH_BYTES));
        let error = validate_unix_socket_path(Path::new(&too_long_path))
            .expect_err("too-long socket path should fail before daemon spawn");

        assert!(error.to_string().contains("Unix socket paths"));
    }

    #[test]
    fn stale_cleanup_skips_pids_whose_start_time_changed() {
        let stat = "123 (a) b) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(process_start_ticks_from_proc_stat(stat), Some(424242));

        let pid = std::process::id();
        let ours = process_start_ticks(pid).expect("own start time");
        let mut actions = Vec::new();
        let recorded = |window_manager| ProcessStartTicks {
            window_manager,
            ..ProcessStartTicks::default()
        };
        let pick = |ticks: &ProcessStartTicks| ticks.window_manager;

        assert_eq!(
            recorded_process(
                Some(pid),
                Some(&recorded(Some(ours))),
                pick,
                "wm",
                &mut actions
            ),
            Some(pid)
        );
        assert_eq!(
            recorded_process(
                Some(pid),
                Some(&recorded(Some(ours + 1))),
                pick,
                "wm",
                &mut actions
            ),
            None
        );
        assert_eq!(
            recorded_process(Some(pid), Some(&recorded(None)), pick, "wm", &mut actions),
            None
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(
            recorded_process(Some(pid), None, pick, "wm", &mut actions),
            Some(pid)
        );
        assert_eq!(
            recorded_process(Some(0), None, pick, "wm", &mut actions),
            None
        );
    }

    #[test]
    fn owned_daemon_matches_only_its_live_session() {
        let mut child = Command::new("sh")
            // Block in the shell itself. An external sleep survives killing
            // the shell and keeps nextest's captured output pipes open.
            .args([
                "-c",
                "read -r unused",
                "daemon",
                "--session-id",
                "session-test-1",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn stand-in daemon");
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !owned_daemon_running(pid, "session-test-1") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(owned_daemon_running(pid, "session-test-1"));
        assert!(!owned_daemon_running(pid, "session-test-2"));
        assert!(!owned_daemon_running(pid, "session-test"));
        child.kill().expect("kill stand-in");
        let deadline = Instant::now() + Duration::from_secs(5);
        while owned_daemon_running(pid, "session-test-1") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !owned_daemon_running(pid, "session-test-1"),
            "zombie counts as gone"
        );
        child.wait().expect("reap stand-in");
    }

    #[test]
    fn private_runtime_dir_rejects_symlinked_or_shared_roots() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let scratch = std::env::temp_dir().join(format!("awl-rt-{}-{unique}", std::process::id()));
        fs::create_dir_all(&scratch).expect("scratch");
        let runtime = |root: &Path| root.join("agent-workspace-linux").join("ws");

        let owned = scratch.join("owned");
        create_private_runtime_dir(&runtime(&owned)).expect("fresh root is created private");
        create_private_runtime_dir(&runtime(&owned)).expect("existing private root is reused");
        let base = owned.join("agent-workspace-linux");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o775)).expect("chmod base");
        create_private_runtime_dir(&runtime(&owned)).expect("umask-created base is accepted");
        assert_eq!(
            fs::metadata(&base).expect("base").mode() & 0o777,
            PRIVATE_RUNTIME_DIR_MODE
        );

        let shared = scratch.join("shared");
        fs::create_dir(&shared).expect("shared root");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(create_private_runtime_dir(&runtime(&shared)).is_err());
        assert!(!shared.join("agent-workspace-linux").exists());
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o775)).expect("chmod");
        assert!(create_private_runtime_dir(&runtime(&shared)).is_err());

        let linked = scratch.join("linked");
        std::os::unix::fs::symlink(&owned, &linked).expect("symlink root");
        assert!(create_private_runtime_dir(&runtime(&linked)).is_err());

        let base_link = scratch.join("base-link");
        fs::create_dir(&base_link).expect("root");
        fs::set_permissions(&base_link, fs::Permissions::from_mode(0o700)).expect("chmod");
        std::os::unix::fs::symlink(
            owned.join("agent-workspace-linux"),
            base_link.join("agent-workspace-linux"),
        )
        .expect("symlink base");
        assert!(create_private_runtime_dir(&runtime(&base_link)).is_err());

        fs::remove_dir_all(scratch).expect("remove scratch");
    }

    #[test]
    fn terminal_socket_ignores_stale_legacy_socket_and_attaches_to_live_one() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let runtime =
            std::env::temp_dir().join(format!("awl-term-{}-{unique}", std::process::id()));
        fs::create_dir_all(&runtime).expect("create test runtime");
        let legacy = runtime.join("terminal-t.tmux.sock");
        let client = client_terminal_socket_path(&runtime, "t");

        assert_eq!(terminal_socket_path(&runtime, "t"), client);

        let stale = UnixListener::bind(&legacy).expect("bind legacy socket");
        drop(stale);
        // A parallel test that forks before exec briefly holds the listener.
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&legacy).is_ok() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(legacy
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_socket()));
        assert_eq!(terminal_socket_path(&runtime, "t"), client);

        fs::remove_file(&legacy).expect("remove stale legacy socket");
        let _live = UnixListener::bind(&legacy).expect("bind live legacy socket");
        assert_eq!(terminal_socket_path(&runtime, "t"), legacy);

        fs::remove_dir_all(runtime).expect("remove test runtime");
    }

    #[test]
    fn runtime_base_dir_prefers_xdg_runtime_dir() {
        assert_eq!(
            runtime_base_dir_from_candidates(
                Some(PathBuf::from("/custom-runtime")),
                Some(PathBuf::from("/run/user/1000")),
                PathBuf::from("/tmp/agent-workspace-linux-me"),
            ),
            PathBuf::from("/custom-runtime/agent-workspace-linux")
        );
    }

    #[test]
    fn runtime_base_dir_uses_systemd_runtime_dir_without_xdg() {
        assert_eq!(
            runtime_base_dir_from_candidates(
                None,
                Some(PathBuf::from("/run/user/1000")),
                PathBuf::from("/tmp/agent-workspace-linux-me"),
            ),
            PathBuf::from("/run/user/1000/agent-workspace-linux")
        );
    }

    #[test]
    fn runtime_base_dir_falls_back_to_temp_last() {
        assert_eq!(
            runtime_base_dir_from_candidates(
                None,
                None,
                PathBuf::from("/tmp/agent-workspace-linux-me"),
            ),
            PathBuf::from("/tmp/agent-workspace-linux-me/agent-workspace-linux")
        );
    }

    #[test]
    fn parses_effective_uid_from_proc_status() {
        let status = "Name:\tagent-workspace-linux\nUid:\t1000\t1001\t1002\t1003\n";

        assert_eq!(parse_effective_uid_from_proc_status(status), Some(1001));
    }

    #[test]
    fn parses_zombie_state_from_proc_stat_with_spaced_name() {
        let stat = "1234 (agent workspace) Z 1 1234 1234 0 -1 4194560 0 0 0 0";

        assert_eq!(process_state_from_proc_stat(stat), Some('Z'));
    }

    #[test]
    fn external_browser_snapshot_events_are_metadata_only() {
        let detail = metadata_only_browser_event_detail(
            "browser_snapshot",
            serde_json::json!({
                "app_id": "app-browser",
                "target_id": "target-1",
                "title": "Private Grocery Cart",
                "url": "https://example-grocery.test/cart",
                "text": "name, address, phone, and cart contents",
                "text_chars": 42,
                "text_truncated": false,
                "headings": ["Delivery address"],
                "links": [{"text": "Checkout", "href": "https://example-grocery.test/checkout"}],
            }),
        )
        .expect("browser snapshot event detail sanitizes");

        assert_eq!(detail["app_id"], "app-browser");
        assert_eq!(detail["target_id"], "target-1");
        assert_eq!(detail["text_chars"], 42);
        assert_eq!(detail["raw_text_omitted"], true);
        assert!(detail.get("text").is_none());
        assert!(detail.get("headings").is_none());
        assert!(detail.get("links").is_none());
    }

    #[test]
    fn external_browser_search_events_are_metadata_only() {
        let detail = metadata_only_browser_event_detail(
            "browser_search_results",
            serde_json::json!({
                "app_id": "app-browser",
                "target_id": "target-2",
                "title": "GPU Search",
                "url": "https://www.amazon.com/s?k=gpu",
                "result_count": 2,
                "min_vram_gb": 36,
                "results": [{"title": "Private card title", "href": "https://example.test"}],
                "text_excerpt": "Private card text",
            }),
        )
        .expect("browser search result event detail sanitizes");

        assert_eq!(detail["result_count"], 2);
        assert_eq!(detail["min_vram_gb"], 36);
        assert_eq!(detail["raw_result_text_omitted"], true);
        assert!(detail.get("results").is_none());
        assert!(detail.get("text_excerpt").is_none());
    }

    #[test]
    fn external_browser_navigate_events_are_metadata_only() {
        let detail = metadata_only_browser_event_detail(
            "browser_navigate",
            serde_json::json!({
                "app_id": "app-browser",
                "target_id": "target-3",
                "url": "https://example-grocery.test/search?q=milk",
                "frame_id": "frame-1",
                "snapshot": true,
                "title": "Milk Search",
                "current_url": "https://example-grocery.test/search?q=milk",
                "text": "private search result text",
                "headings": ["Milk"],
                "links": [{"text": "Milk", "href": "https://example-grocery.test/item/milk"}],
            }),
        )
        .expect("browser navigate event detail sanitizes");

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

    #[test]
    fn external_events_reject_unowned_kinds() {
        let error =
            metadata_only_browser_event_detail("type_text", serde_json::json!({"text": "secret"}))
                .expect_err("non-browser external events should reject");

        assert!(error
            .to_string()
            .contains("external workspace event kind \"type_text\" is not allowed"));
    }

    fn daemon_state_for_test(name: &str) -> DaemonState {
        daemon_state_for_test_with_permissions(name, McpPermissionState::default())
    }

    fn daemon_state_for_test_with_permissions(
        name: &str,
        permissions: McpPermissionState,
    ) -> DaemonState {
        let mut status = status_with_profile_defaults();
        status.id = name.to_string();
        let runtime_dir = env::temp_dir().join(format!("{name}-{}", std::process::id()));
        status.runtime_dir = runtime_dir.clone();
        status.socket_path = runtime_dir.join("control.sock");
        status.xauthority_path = runtime_dir.join("Xauthority");
        DaemonState {
            status,
            apps: Vec::new(),
            window_app_ids: BTreeMap::new(),
            event_path: runtime_dir.join(EVENT_LOG_FILE),
            next_event_sequence: 1,
            permissions,
        }
    }

    fn launch_app_request(command: &[&str]) -> IpcRequest {
        IpcRequest::LaunchApp {
            command: command.iter().map(|part| part.to_string()).collect(),
            name: None,
            profile_id: None,
            cwd: None,
            env: Vec::new(),
            wait_window: false,
            window_timeout_ms: None,
            screenshot_window: false,
        }
    }

    // Fix A regression: a daemon-side launch whose command exceeds a configured
    // MCP permission ceiling is rejected by spawn_app itself (the daemon path),
    // not only by the MCP front-end. This proves the McpPermissionState ceiling
    // check now runs inside the daemon. The rejection happens before any log
    // file is created or any child process is spawned.
    #[test]
    fn daemon_launch_exceeding_ceiling_is_rejected_in_spawn_app() {
        let ceiling = crate::permissions::McpPermissionCeiling {
            apps: crate::permissions::AppPermissionCeiling {
                allow: vec![PathBuf::from("xterm")],
            },
        };
        let permissions = McpPermissionState::from_ceiling(
            Some(PathBuf::from("/tmp/daemon-ceiling-test.json")),
            ceiling,
        );
        assert!(permissions.configured && permissions.restricted);
        let mut state =
            daemon_state_for_test_with_permissions("daemon-ceiling-reject", permissions);
        // Clear the inherited workspace profile so the launch spec is unprofiled
        // and the app allowlist is the deciding ceiling dimension. Also clear the
        // inherited profile_cwd (a non-existent path) so the shape-only launch
        // validation passes and the ceiling check is what rejects the launch.
        state.status.profile_id = None;
        state.status.profile_cwd = None;

        let error = spawn_app(
            &mut state,
            LaunchSpec {
                command: vec!["curl".to_string(), "https://example.com".to_string()],
                name: None,
                profile_id: None,
                cwd: None,
                env: Vec::new(),
            },
        )
        .expect_err("launch outside the app allowlist must be rejected by the daemon");
        let message = format!("{error:#}");
        assert!(
            message.contains("MCP permission ceiling"),
            "expected ceiling rejection, got: {message}"
        );
        assert!(
            message.contains("MCP app allowlist"),
            "expected app allowlist detail, got: {message}"
        );
        assert!(
            state.apps.is_empty(),
            "rejected launch must not register an app process"
        );

        // An allowed command passes the ceiling check in the same daemon path
        // (it then proceeds past validation; we only assert the ceiling does not
        // reject it).
        assert!(
            state
                .permissions
                .validate_launch_spec(&launch_spec_with_workspace_defaults(
                    &state.status,
                    LaunchSpec {
                        command: vec!["xterm".to_string()],
                        name: None,
                        profile_id: None,
                        cwd: None,
                        env: Vec::new(),
                    },
                ))
                .is_ok(),
            "allowlisted command must pass the daemon ceiling check"
        );
    }

    // Fix A regression: the daemon live-control gate blocks mutating IPC ops
    // when control is paused/read_only, while read-only inspection and the
    // safety stop stay allowed. Tested through the pure gate decision so it does
    // not depend on the process-global XDG_RUNTIME_DIR or the shared control
    // file (safe under parallel test execution).
    #[test]
    fn daemon_control_gate_blocks_mutation_but_allows_readonly_and_stop() {
        let mutating = launch_app_request(&["xterm"]);
        let type_text = IpcRequest::TypeText {
            text: "hello".to_string(),
        };
        let read_only = IpcRequest::Status;
        let safety_stop = IpcRequest::Stop;

        // Paused blocks mutating ops.
        assert_eq!(
            control_gate_block_reason(
                &mutating,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            Some(LiveControlBlockReason::Mode(McpControlMode::Paused))
        );
        assert_eq!(
            control_gate_block_reason(
                &type_text,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            Some(LiveControlBlockReason::Mode(McpControlMode::Paused))
        );
        // Read-only inspection stays allowed even when paused.
        assert_eq!(
            control_gate_block_reason(
                &read_only,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            None
        );
        // Safety stop stays allowed even when paused.
        assert_eq!(
            control_gate_block_reason(
                &safety_stop,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            None
        );
        // read_only mode also blocks mutating ops and allows inspection.
        assert_eq!(
            control_gate_block_reason(
                &mutating,
                LiveControlGateState::Readable(McpControlMode::ReadOnly)
            ),
            Some(LiveControlBlockReason::Mode(McpControlMode::ReadOnly))
        );
        assert_eq!(
            control_gate_block_reason(
                &read_only,
                LiveControlGateState::Readable(McpControlMode::ReadOnly)
            ),
            None
        );
        // Active permits everything.
        assert_eq!(
            control_gate_block_reason(
                &mutating,
                LiveControlGateState::Readable(McpControlMode::Active)
            ),
            None
        );
        assert_eq!(
            control_gate_block_reason(
                &safety_stop,
                LiveControlGateState::Readable(McpControlMode::Active)
            ),
            None
        );

        // Unreadable live-control state fails closed for mutating IPC while
        // preserving read-only inspection and the safety stop.
        let click = IpcRequest::Click {
            x: 1,
            y: 2,
            button: 1,
            count: 1,
        };
        let key = IpcRequest::Key {
            key: "Escape".to_string(),
        };
        let paste = IpcRequest::PasteText {
            text: "hello".to_string(),
            key: "ctrl+v".to_string(),
        };
        assert_eq!(
            control_gate_block_reason(&click, LiveControlGateState::Unreadable),
            Some(LiveControlBlockReason::Unreadable)
        );
        assert_eq!(
            control_gate_block_reason(&key, LiveControlGateState::Unreadable),
            Some(LiveControlBlockReason::Unreadable)
        );
        assert_eq!(
            control_gate_block_reason(&paste, LiveControlGateState::Unreadable),
            Some(LiveControlBlockReason::Unreadable)
        );
        assert_eq!(
            control_gate_block_reason(&read_only, LiveControlGateState::Unreadable),
            None
        );
        assert_eq!(
            control_gate_block_reason(&safety_stop, LiveControlGateState::Unreadable),
            None
        );

        // Dry-run previews are treated as read-only.
        let kill_dry_run = IpcRequest::KillApp {
            app_id: "app-1".to_string(),
            dry_run: true,
        };
        let kill_real = IpcRequest::KillApp {
            app_id: "app-1".to_string(),
            dry_run: false,
        };
        assert_eq!(
            control_gate_block_reason(
                &kill_dry_run,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            None
        );
        assert_eq!(
            control_gate_block_reason(
                &kill_real,
                LiveControlGateState::Readable(McpControlMode::Paused)
            ),
            Some(LiveControlBlockReason::Mode(McpControlMode::Paused))
        );
    }

    #[test]
    fn stale_app_cleanup_skips_a_leader_whose_start_time_changed() {
        let pid = std::process::id();
        let mut app = workspace_app_for_test("app-self");
        app.pid = pid;
        app.process_group_id = linux_process_group_id(pid);
        app.running = true;
        let ours = process_start_ticks(pid).expect("own start time");
        let recorded = |ticks: u64| ProcessStartTicks {
            apps: BTreeMap::from([(pid, ticks)]),
            ..ProcessStartTicks::default()
        };

        let mut actions = Vec::new();
        cleanup_stale_app_process_group(&app, Some(&recorded(ours)), true, &mut actions);
        assert!(actions[0].starts_with("would terminate"), "{actions:?}");

        let mut actions = Vec::new();
        cleanup_stale_app_process_group(&app, Some(&recorded(ours + 1)), true, &mut actions);
        assert!(actions[0].contains("no longer the app"), "{actions:?}");

        let mut actions = Vec::new();
        cleanup_stale_app_process_group(&app, None, true, &mut actions);
        assert!(actions[0].starts_with("would terminate"), "{actions:?}");
    }

    fn workspace_app_for_test(id: &str) -> WorkspaceApp {
        WorkspaceApp {
            id: id.to_string(),
            name: Some("probe".to_string()),
            pid: 4242,
            process_group_id: Some(4242),
            profile_id: Some("qa".to_string()),
            command: vec!["/bin/true".to_string()],
            cwd: None,
            env: Vec::new(),
            stdout_path: None,
            stderr_path: None,
            started_at_unix: 1,
            running: false,
            exit_status: Some("exit status: 0".to_string()),
            exit_code: Some(0),
            exit_signal: None,
            stopped_at_unix: Some(2),
            runtime_seconds: Some(1),
        }
    }

    fn workspace_window_for_test(id: &str) -> WorkspaceWindow {
        WorkspaceWindow {
            id: id.to_string(),
            title: "Calculator".to_string(),
            wm_class: Some("XCalc".to_string()),
            wm_instance: Some("xcalc".to_string()),
            pid: None,
            app_id: None,
            visible: true,
            geometry: WindowGeometry {
                x: 10,
                y: 20,
                width: 200,
                height: 160,
                screen: Some(0),
            },
        }
    }

    #[test]
    fn remembers_pidless_launch_window_for_app_filtering() {
        let mut state = daemon_state_for_test("pidless-window-association");
        let mut app = workspace_app_for_test("app-4242");
        app.running = true;
        state.status.apps = vec![app.clone()];
        let window = workspace_window_for_test("4194326");

        remember_workspace_window_app_ids(&mut state, &app.id, std::slice::from_ref(&window));
        let annotated = annotate_workspace_window(&state, window);

        assert_eq!(annotated.app_id.as_deref(), Some("app-4242"));
        assert!(window_belongs_to_app(&annotated, &app));
    }

    #[test]
    fn pidless_launch_window_does_not_match_other_apps() {
        let mut state = daemon_state_for_test("pidless-window-other-app");
        let launched_app = workspace_app_for_test("app-4242");
        let other_app = WorkspaceApp {
            id: "app-7777".to_string(),
            pid: 7777,
            process_group_id: Some(7777),
            ..workspace_app_for_test("app-7777")
        };
        let window = workspace_window_for_test("4194326");

        remember_workspace_window_app_ids(
            &mut state,
            &launched_app.id,
            std::slice::from_ref(&window),
        );
        let annotated = annotate_workspace_window(&state, window);

        assert!(window_belongs_to_app(&annotated, &launched_app));
        assert!(!window_belongs_to_app(&annotated, &other_app));
    }

    #[test]
    fn action_responses_use_compact_status_without_app_history() {
        let mut status = status_with_profile_defaults();
        status.apps = vec![
            workspace_app_for_test("app-old-1"),
            workspace_app_for_test("app-old-2"),
        ];

        let response = response_with_status(true, "ok", &status);

        assert!(response.status.expect("compact status").apps.is_empty());
        assert!(response.apps.is_none());
    }

    #[test]
    fn explicit_status_response_preserves_full_app_history() {
        let mut status = status_with_profile_defaults();
        status.apps = vec![
            workspace_app_for_test("app-old-1"),
            workspace_app_for_test("app-old-2"),
        ];

        let mut response = response_with_status(true, "workspace is running", &status);
        response.status = Some(status.clone());
        response.apps = Some(status.apps.clone());

        assert_eq!(response.apps.as_ref().expect("top-level apps").len(), 2);
        assert_eq!(response.status.expect("full status").apps.len(), 2);
    }

    #[test]
    fn strips_linux_deleted_exe_suffix() {
        let path = Path::new("/home/me/.local/bin/agent-workspace-linux (deleted)");

        assert_eq!(
            strip_linux_deleted_exe_suffix(path),
            Some(PathBuf::from("/home/me/.local/bin/agent-workspace-linux"))
        );
    }

    #[test]
    fn leaves_normal_exe_path_unchanged() {
        let path = Path::new("/home/me/.local/bin/agent-workspace-linux");

        assert_eq!(strip_linux_deleted_exe_suffix(path), None);
    }

    #[test]
    fn unprofiled_launch_inherits_started_profile_defaults() {
        let status = status_with_profile_defaults();
        let spec = launch_spec_with_workspace_defaults(
            &status,
            LaunchSpec {
                command: vec!["/bin/true".to_string()],
                name: Some("probe".to_string()),
                profile_id: None,
                cwd: None,
                env: vec![EnvVar {
                    name: "OVERRIDE".to_string(),
                    value: "from-launch".to_string(),
                }],
            },
        );

        assert_eq!(spec.profile_id.as_deref(), Some("qa"));
        assert_eq!(spec.cwd.as_deref(), Some(Path::new("/workspace/project")));
        assert_eq!(
            spec.env
                .iter()
                .find(|env| env.name == "BASE")
                .map(|env| env.value.as_str()),
            Some("one")
        );
        assert_eq!(
            spec.env
                .iter()
                .find(|env| env.name == "OVERRIDE")
                .map(|env| env.value.as_str()),
            Some("from-launch")
        );
    }

    #[test]
    fn explicit_launch_profile_skips_started_profile_defaults() {
        let status = status_with_profile_defaults();
        let spec = launch_spec_with_workspace_defaults(
            &status,
            LaunchSpec {
                command: vec!["/bin/true".to_string()],
                name: Some("probe".to_string()),
                profile_id: Some("other".to_string()),
                cwd: None,
                env: Vec::new(),
            },
        );

        assert_eq!(spec.profile_id.as_deref(), Some("other"));
        assert_eq!(spec.cwd, None);
        assert!(spec.env.is_empty());
    }

    #[test]
    fn launch_preview_requires_running_workspace_daemon() {
        let id = format!("launch-preview-stopped-{}", std::process::id());
        let error = preview_launch_app(
            &id,
            LaunchSpec {
                command: vec!["/bin/true".to_string()],
                name: Some("probe".to_string()),
                profile_id: None,
                cwd: None,
                env: vec![EnvVar {
                    name: "AGENT_WORKSPACE_TEST".to_string(),
                    value: "1".to_string(),
                }],
            },
            true,
            Some(123),
            true,
        )
        .expect_err("launch preview should require a live workspace daemon");

        assert!(error.to_string().contains("daemon is required"));
    }

    #[test]
    fn run_preview_requires_running_workspace_daemon() {
        let id = format!("run-preview-stopped-{}", std::process::id());
        let error = preview_run_app_with_spec(
            &id,
            LaunchSpec {
                command: vec!["/bin/true".to_string()],
                name: Some("run-probe".to_string()),
                profile_id: None,
                cwd: None,
                env: Vec::new(),
            },
            Some(456),
            Some(1024),
            true,
        )
        .expect_err("run preview should require a live workspace daemon");

        assert!(error.to_string().contains("daemon is required"));
    }

    #[test]
    fn stop_request_still_stops_when_client_disappears_before_response() {
        let name = "stop-broken-client";
        let mut state = daemon_state_for_test(name);
        fs::create_dir_all(&state.status.runtime_dir).expect("runtime dir");
        let (server, mut client) = UnixStream::pair().expect("unix stream pair");

        serde_json::to_writer(&mut client, &IpcRequest::Stop).expect("stop request");
        client.write_all(b"\n").expect("stop newline");
        drop(client);

        let should_stop =
            handle_stream(server, &mut state).expect("broken response should not cancel stop");

        assert!(should_stop);
        let _ = fs::remove_dir_all(&state.status.runtime_dir);
    }

    #[test]
    fn stop_response_reports_workspace_not_ready() {
        let name = "stop-ready-response";
        let mut state = daemon_state_for_test(name);
        fs::create_dir_all(&state.status.runtime_dir).expect("runtime dir");
        let runtime_dir = state.status.runtime_dir.clone();
        let (server, mut client) = UnixStream::pair().expect("unix stream pair");

        let handler = thread::spawn(move || {
            let should_stop = handle_stream(server, &mut state).expect("stop response");
            (should_stop, state.status.ready)
        });

        serde_json::to_writer(&mut client, &IpcRequest::Stop).expect("stop request");
        client.write_all(b"\n").expect("stop newline");

        let mut response_line = String::new();
        BufReader::new(client)
            .read_line(&mut response_line)
            .expect("stop response line");
        let response: IpcResponse =
            serde_json::from_str(&response_line).expect("stop response json");
        let (should_stop, state_ready) = handler.join().expect("stop handler");

        assert!(should_stop);
        assert!(!state_ready);
        assert!(!response.status.expect("response status").ready);
        let _ = fs::remove_dir_all(&runtime_dir);
    }

    #[test]
    fn generated_session_id_is_valid_and_workspace_scoped() {
        let session_id = new_session_id("qa");

        assert!(session_id.contains("qa"));
        assert!(normalize_session_id(&session_id).is_ok());
    }

    #[test]
    fn session_id_rejects_spaces() {
        let error = normalize_session_id("bad session")
            .expect_err("session ids with spaces should be rejected");

        assert!(error.to_string().contains("workspace session id"));
    }
}
