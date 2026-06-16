use crate::{
    control::{self, McpControlMode, McpControlState},
    permissions::McpPermissionState,
    policy::{AppliedWorkspacePolicy, NetworkMode},
    profile, workspace,
};
use anyhow::{bail, Context as AnyhowContext, Result};
use gpui::{
    div, img, layer_shell::Anchor, layer_shell::KeyboardInteractivity, layer_shell::Layer,
    layer_shell::LayerShellOptions, point, prelude::*, px, rgb, rgba, size, AnyElement, App,
    Bounds, ClickEvent, Context, CursorStyle, DevicePixels, Div, DivFrameState, Element, ElementId,
    FocusHandle, GlobalElementId, Hitbox, InspectorElementId, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement, Pixels, Point, Render, RenderImage, ResizeEdge, ScrollDelta, ScrollWheelEvent,
    SharedString, Size, Stateful, Styled, Task, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
};
use gpui_platform::application;
use image::{Frame, ImageBuffer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux,
        ConnectionExt as XprotoConnectionExt, EventMask, KeyButMask, PropMode, StackMode,
        Window as X11Window,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as X11WrapperConnectionExt,
};

// Polished opaque silver-graphite palette — premium brushed metal, not flat.
// GNOME (the X11/Xwayland popup path) cannot blur, so the panel is an opaque
// graphite solid and the "metal" comes from strong silvery edges plus a bright
// brushed top highlight. Text is soft silver-white; status accents are reserved
// for the footer status dot only — the chrome stays silver/neutral.

/// Opaque root panel fill (`0xRRGGBBAA`, alpha ff) — a deep, lifted cool
/// graphite that reads as a solid premium surface (no translucency, since the
/// GNOME path cannot blur). Rendered via `rgba(..)`.
const BG_GLASS: u32 = 0x171c23ff;
const SURFACE: u32 = 0x20262e;
const SURFACE_2: u32 = 0x2c333d;
const BORDER: u32 = 0x49525e;
/// Bright silvery metallic edge for the panel's primary borders (its "chrome").
const EDGE_SILVER: u32 = 0xc4ccd6;
/// Near-white brushed-metal highlight for the bright top/inner edge.
const EDGE_HIGHLIGHT: u32 = 0xe4eaf1;
const TEXT: u32 = 0xe9edf2;
const MUTED: u32 = 0x9aa3ae;
const BUTTON_BG: u32 = 0x252c35;
const BUTTON_BG_HOVER: u32 = 0x2f3742;
const BUTTON_EDGE: u32 = 0xc4ccd6;
const BUTTON_EDGE_SOFT: u32 = 0x7b848d;
const BUTTON_SELECTED_BG: u32 = 0x37454f;
const BUTTON_SELECTED_BG_HOVER: u32 = 0x3f4e5a;
const BUTTON_SELECTED_EDGE: u32 = 0xe4eaf1;
const BUTTON_DISABLED_BG: u32 = 0x191e24;
const BUTTON_DISABLED_EDGE: u32 = 0x4a525c;
const BUTTON_DISABLED_TEXT: u32 = 0x6c7681;
/// Single unified pill radius (shared by buttons and the tooltip surface).
const BUTTON_RADIUS: f32 = 8.0;
/// Single unified pill height for every button state.
const BUTTON_HEIGHT: f32 = 24.0;
/// Single unified horizontal padding for every button state.
const BUTTON_PAD_X: f32 = 11.0;
/// Single unified button label size.
const BUTTON_TEXT: f32 = 12.0;
const TOOLTIP_BG: u32 = 0x232932;
const TOOLTIP_EDGE: u32 = 0x70797f;
// Danger state stays silver-chrome with a faint warm tint so it reads as a
// distinct, restrained accent rather than a loud red button.
const DANGER_BG: u32 = 0x2a2228;
const DANGER_EDGE: u32 = 0x9a8e92;
const DANGER_EDGE_HOVER: u32 = 0xc2b3b7;
const DANGER_HOVER: u32 = 0x342a30;
const DANGER_TEXT: u32 = 0xe6cbcf;
// Status-dot accents (footer running light only).
const GREEN: u32 = 0x36d07a;
const AMBER: u32 = 0xf0b84a;
const RED: u32 = 0xef5a5a;

const VIEWER_APP_ID: &str = "agent-workspace-linux-viewer";
const VIEWER_BACKEND_ENV: &str = "AGENT_WORKSPACE_VIEWER_BACKEND";
const VIEWER_PERMISSIONS_ENV: &str = "AGENT_WORKSPACE_VIEWER_PERMISSIONS";
const VIEWER_BACKEND_X11: &str = "x11";
const VIEWER_BACKEND_WAYLAND: &str = "wayland";
const UI_FONT: &str = ".ZedSans";
const OVERLAY_WIDTH: f32 = 420.0;
const OVERLAY_HEIGHT: f32 = 420.0;
const OVERLAY_MIN_WIDTH: f32 = 380.0;
const OVERLAY_MIN_HEIGHT: f32 = 280.0;
const OVERLAY_MARGIN: f32 = 18.0;
const CLEAN_CONFIRM_SECONDS: u64 = 6;
const REVOKE_CONFIRM_SECONDS: u64 = 6;
const INPUT_FORWARD_CONFIRM_SECONDS: u64 = 6;
const INPUT_FORWARD_DRAG_THRESHOLD_PX: f32 = 3.0;
// Bound manual input backlog so a slow workspace daemon cannot accumulate
// unbounded stale key/scroll/click events behind the current user gesture.
const MAX_INPUT_FORWARDING_QUEUE_LEN: usize = 128;
const INPUT_FORWARD_REFRESH_BURST_DELAYS_MS: [u64; 3] = [90, 220, 500];
// GPUI pixel scroll events are converted to X11 wheel ticks for workspace IPC.
// 80px is a conservative one-notch touchpad/wheel unit; a high clamp prevents
// large touchpad flings from turning into unbounded synthetic wheel bursts.
const SCROLL_PIXELS_PER_WORKSPACE_TICK: f32 = 80.0;
const MAX_WORKSPACE_SCROLL_TICKS: f32 = 12.0;
const VIEWER_PREFERENCES_FILE: &str = "viewer.json";
const VIEWER_FRAME_FILE: &str = "viewer-frame.png";
const VIEWER_REGISTRY_DIR: &str = "viewers";
const VIEWER_REGISTRY_SCHEMA: &str = "agent-workspace-linux.viewer-instance.v1";
const SIGTERM: i32 = 15;
const ESRCH: i32 = 3;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ViewerBackend {
    WaylandLayerShell,
    X11Popup,
}

impl ViewerBackend {
    fn launch_label(self, always_on_top: bool) -> &'static str {
        match (self, always_on_top) {
            (ViewerBackend::WaylandLayerShell, true) => "wayland-layer-shell",
            (ViewerBackend::WaylandLayerShell, false) => "wayland-window",
            (ViewerBackend::X11Popup, true) => "x11-popup-topmost",
            (ViewerBackend::X11Popup, false) => "x11-popup",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ViewerOptions {
    pub id: String,
    pub permissions: McpPermissionState,
    pub always_on_top: bool,
    /// Allow the viewer UI to explicitly arm manual input forwarding. Runtime
    /// forwarding still starts off and requires an in-viewer acknowledgement
    /// before click/drag/scroll/keyboard events are sent to workspace IPC.
    pub input_forwarding: bool,
    pub exit_when_workspace_gone: bool,
    /// Start with the live screen view off ("always bg"). The screen is shown by
    /// default; this launch flag opts into a background (screen-off) start. The
    /// in-viewer toggle ("optional bg") can turn it back on at runtime.
    pub background: bool,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        Self {
            id: workspace::default_workspace_id(),
            permissions: viewer_permissions_from_env().unwrap_or_default(),
            always_on_top: false,
            input_forwarding: false,
            exit_when_workspace_gone: false,
            background: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerLaunch {
    pub id: String,
    pub viewer_id: String,
    pub pid: u32,
    pub backend: String,
    pub always_on_top: bool,
    pub input_forwarding: bool,
    pub exit_when_workspace_gone: bool,
    pub executable: PathBuf,
    pub command: Vec<String>,
    pub reused: bool,
    pub registry_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewerRegistryEntry {
    schema: String,
    id: String,
    pid: u32,
    backend: String,
    always_on_top: bool,
    #[serde(default)]
    input_forwarding: bool,
    #[serde(default)]
    exit_when_workspace_gone: bool,
    executable: PathBuf,
    command: Vec<String>,
    opened_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerList {
    pub registry_dir: PathBuf,
    pub viewers: Vec<ViewerListEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerListEntry {
    pub id: String,
    pub viewer_id: String,
    pub pid: u32,
    pub backend: String,
    pub always_on_top: bool,
    pub input_forwarding: bool,
    pub exit_when_workspace_gone: bool,
    pub executable: PathBuf,
    pub command: Vec<String>,
    pub opened_at_unix: u64,
    pub registry_path: PathBuf,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerClose {
    pub registry_dir: PathBuf,
    pub dry_run: bool,
    pub target_id: Option<String>,
    pub all: bool,
    pub candidates: Vec<ViewerCloseEntry>,
    pub closed: Vec<ViewerCloseEntry>,
    pub skipped: Vec<ViewerCloseEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ViewerCloseEntry {
    pub id: String,
    pub viewer_id: String,
    pub pid: u32,
    pub backend: String,
    pub always_on_top: bool,
    pub input_forwarding: bool,
    pub exit_when_workspace_gone: bool,
    pub registry_path: PathBuf,
    pub reason: String,
}

struct ViewerInstanceGuard {
    path: PathBuf,
    pid: u32,
}

#[derive(Debug, Clone, Default)]
struct ViewerSnapshot {
    workspaces: Vec<workspace::WorkspaceListEntry>,
    profiles: Vec<profile::WorkspaceProfile>,
    doctor_ready: bool,
    doctor_blockers: Vec<String>,
}

#[derive(Clone)]
struct ViewerFrame {
    image: Arc<RenderImage>,
    width: u32,
    height: u32,
}

struct BoundsTrackedDiv {
    div: Div,
    bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
}

fn track_bounds(div: Div, bounds: Arc<Mutex<Option<Bounds<Pixels>>>>) -> BoundsTrackedDiv {
    BoundsTrackedDiv { div, bounds }
}

fn lock_screen_bounds(
    tracked_bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>,
) -> std::sync::MutexGuard<'_, Option<Bounds<Pixels>>> {
    tracked_bounds
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn store_screen_bounds(
    tracked_bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>,
    next_bounds: Option<Bounds<Pixels>>,
) {
    *lock_screen_bounds(tracked_bounds) = next_bounds;
}

fn screen_bounds_snapshot(
    tracked_bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>,
) -> Option<Bounds<Pixels>> {
    *lock_screen_bounds(tracked_bounds)
}

impl Element for BoundsTrackedDiv {
    type RequestLayoutState = DivFrameState;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Element::id(&self.div)
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        Element::source_location(&self.div)
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.div.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        store_screen_bounds(&self.bounds, Some(bounds));
        self.div
            .prepaint(id, inspector_id, bounds, request_layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.div.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        )
    }
}

impl IntoElement for BoundsTrackedDiv {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct ViewerTooltip {
    text: SharedString,
}

impl ViewerTooltip {
    fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

impl Render for ViewerTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(260.0))
            .rounded(px(BUTTON_RADIUS))
            .border_1()
            .border_color(rgb(TOOLTIP_EDGE))
            .bg(rgb(TOOLTIP_BG))
            .px(px(8.0))
            .py(px(5.0))
            .shadow_md()
            .text_size(px(10.0))
            .text_color(rgb(TEXT))
            .children(
                self.text
                    .split('\n')
                    .map(|line| div().child(SharedString::from(line.to_string()))),
            )
    }
}

struct ViewerRefreshResult {
    target_id: String,
    bound_target_missing: bool,
    snapshot: ViewerSnapshot,
    active_window: Option<workspace::WorkspaceWindow>,
    latest_activity: Option<ViewerActivity>,
    control_state: McpControlState,
    frame_update: ViewerFrameUpdate,
    last_refresh_unix: u64,
    message: String,
    error: Option<String>,
}

enum ViewerFrameUpdate {
    KeepExisting,
    Replace(Option<ViewerFrame>),
}

type QueuedInputForwardingRequest = (u64, String, InputForwardingRequest);

struct AgentWorkspaceViewer {
    target_id: String,
    bound_target_id: Option<String>,
    snapshot: ViewerSnapshot,
    selected_profile_id: Option<String>,
    permissions: McpPermissionState,
    focus_handle: FocusHandle,
    preferences: ViewerPreferences,
    active_window: Option<workspace::WorkspaceWindow>,
    latest_activity: Option<ViewerActivity>,
    control_state: McpControlState,
    frame: Option<ViewerFrame>,
    screen_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    last_refresh_unix: u64,
    message: String,
    error: Option<String>,
    screen_stream: bool,
    input_forwarding_allowed: bool,
    input_forwarding_enabled: bool,
    input_forwarding_arm_expires_at_unix: Option<u64>,
    input_forwarding_drag: Option<InputForwardingDrag>,
    input_forwarding_queue: VecDeque<QueuedInputForwardingRequest>,
    input_forwarding_in_flight: bool,
    input_forwarding_epoch: u64,
    input_forwarding_burst_generation: u64,
    input_refresh_burst_active: bool,
    exit_when_workspace_gone: bool,
    footer_mode: FooterMode,
    refresh_in_flight: bool,
    action_in_flight: Option<ViewerAction>,
    /// Whether the secondary "More" cluster (refresh, live, capture, revoke,
    /// clean, profile, workspace, artifacts, footer mode) is expanded. Transient
    /// UI state only — not persisted.
    show_more: bool,
    pending_cleanup: Option<PendingCleanup>,
    pending_revoke: Option<PendingRevoke>,
    interaction_drag: Option<InteractionDrag>,
    _poll_task: Option<Task<()>>,
    _refresh_task: Option<Task<()>>,
    _action_task: Option<Task<()>>,
    _input_forwarding_task: Option<Task<()>>,
    _interaction_task: Option<Task<()>>,
    _input_refresh_burst_task: Option<Task<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ViewerPreferences {
    width: f32,
    height: f32,
    #[serde(default = "default_screen_stream")]
    screen_stream: bool,
    #[serde(default)]
    footer_mode: FooterMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    y: Option<f32>,
}

/// The live screen view is shown by default. Users opt into a background
/// (screen-off) mode via the in-viewer toggle ("optional bg") or the
/// `--background` launch flag ("always bg"). Kept as a free function so it can
/// back both the struct default and the serde default for older preference
/// files that predate the field.
fn default_screen_stream() -> bool {
    true
}

/// Whether the live screen view starts on. The screen is shown by default; the
/// `--background` launch flag ("always bg") forces a screen-off start regardless
/// of the saved preference. The in-viewer toggle ("optional bg") flips the saved
/// preference at runtime.
fn initial_screen_stream(background: bool, preference: bool) -> bool {
    !background && preference
}

fn screen_position_to_workspace_point(
    source_width: u32,
    source_height: u32,
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
) -> Option<WorkspacePoint> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    let view_width = bounds.size.width.as_f32();
    let view_height = bounds.size.height.as_f32();
    if view_width <= 0.0 || view_height <= 0.0 {
        return None;
    }

    let local_x = (position.x - bounds.origin.x).as_f32();
    let local_y = (position.y - bounds.origin.y).as_f32();
    if local_x < 0.0 || local_y < 0.0 || local_x > view_width || local_y > view_height {
        return None;
    }

    let source_width_f = source_width as f32;
    let source_height_f = source_height as f32;
    let image_bounds = ObjectFit::Cover.get_bounds(
        bounds,
        size(
            DevicePixels::from(source_width),
            DevicePixels::from(source_height),
        ),
    );
    let rendered_width = image_bounds.size.width.as_f32();
    let rendered_height = image_bounds.size.height.as_f32();
    if rendered_width <= 0.0 || rendered_height <= 0.0 {
        return None;
    }

    let source_x = (position.x - image_bounds.origin.x).as_f32() * source_width_f / rendered_width;
    let source_y =
        (position.y - image_bounds.origin.y).as_f32() * source_height_f / rendered_height;
    if source_x < 0.0 || source_y < 0.0 || source_x > source_width_f || source_y > source_height_f {
        return None;
    }
    Some(WorkspacePoint {
        x: source_x.round().clamp(0.0, (source_width - 1) as f32) as i32,
        y: source_y.round().clamp(0.0, (source_height - 1) as f32) as i32,
    })
}

fn x11_button_for_mouse_button(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        MouseButton::Navigate(_) => None,
    }
}

fn scroll_wheel_to_workspace_scroll(
    delta: ScrollDelta,
) -> Option<(workspace::ScrollDirection, u8)> {
    let (x, y, unit) = match delta {
        ScrollDelta::Pixels(point) => (
            point.x.as_f32(),
            point.y.as_f32(),
            SCROLL_PIXELS_PER_WORKSPACE_TICK,
        ),
        ScrollDelta::Lines(point) => (point.x, point.y, 1.0),
    };
    let (direction, magnitude) = if x.abs() > y.abs() {
        if x > 0.0 {
            (workspace::ScrollDirection::Left, x.abs())
        } else {
            (workspace::ScrollDirection::Right, x.abs())
        }
    } else if y > 0.0 {
        (workspace::ScrollDirection::Up, y.abs())
    } else if y < 0.0 {
        (workspace::ScrollDirection::Down, y.abs())
    } else {
        return None;
    };
    let amount = (magnitude / unit)
        .ceil()
        .clamp(1.0, MAX_WORKSPACE_SCROLL_TICKS) as u8;
    Some((direction, amount))
}

fn is_paste_keystroke(keystroke: &gpui::Keystroke) -> bool {
    let key = keystroke.key.trim();
    (keystroke.modifiers.control || keystroke.modifiers.platform)
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.shift
        && key.eq_ignore_ascii_case("v")
}

fn printable_keystroke_text(keystroke: &gpui::Keystroke) -> Option<String> {
    if keystroke.modifiers.control
        || keystroke.modifiers.alt
        || keystroke.modifiers.platform
        || keystroke.modifiers.function
    {
        return None;
    }
    keystroke
        .key_char
        .as_ref()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .cloned()
}

fn xdotool_key_for_keystroke(keystroke: &gpui::Keystroke) -> Option<String> {
    let key = normalize_xdotool_key(&keystroke.key)?;
    let mut parts = Vec::new();
    if keystroke.modifiers.control {
        parts.push("ctrl".to_string());
    }
    if keystroke.modifiers.alt {
        parts.push("alt".to_string());
    }
    if keystroke.modifiers.shift {
        parts.push("shift".to_string());
    }
    if keystroke.modifiers.platform {
        parts.push("super".to_string());
    }
    parts.push(key);
    Some(parts.join("+"))
}

fn normalize_xdotool_key(key: &str) -> Option<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = match trimmed.to_ascii_lowercase().as_str() {
        "enter" | "return" => "Return".to_string(),
        "escape" | "esc" => "Escape".to_string(),
        "backspace" | "back_space" => "BackSpace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "tab" => "Tab".to_string(),
        "space" | " " => "space".to_string(),
        "up" | "arrowup" => "Up".to_string(),
        "down" | "arrowdown" => "Down".to_string(),
        "left" | "arrowleft" => "Left".to_string(),
        "right" | "arrowright" => "Right".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" | "page_up" => "Page_Up".to_string(),
        "pagedown" | "page_down" => "Page_Down".to_string(),
        other if other.len() == 1 => other.to_string(),
        other
            if other.starts_with('f')
                && other.len() <= 3
                && other[1..].chars().all(|ch| ch.is_ascii_digit()) =>
        {
            other.to_ascii_uppercase()
        }
        _ => return None,
    };
    Some(normalized)
}

impl Default for ViewerPreferences {
    fn default() -> Self {
        Self {
            width: OVERLAY_WIDTH,
            height: OVERLAY_HEIGHT,
            screen_stream: default_screen_stream(),
            footer_mode: FooterMode::default(),
            x: None,
            y: None,
        }
    }
}

struct InteractionDrag {
    kind: DragKind,
    start_position: Point<Pixels>,
    start_size: Size<Pixels>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WorkspacePoint {
    x: i32,
    y: i32,
}

#[derive(Copy, Clone, Debug, PartialEq)]
struct InputForwardingDrag {
    start: WorkspacePoint,
    start_position: Point<Pixels>,
    button: u8,
}

#[derive(Debug)]
enum InputForwardingRequest {
    MovePointer {
        x: i32,
        y: i32,
    },
    Click {
        x: i32,
        y: i32,
        button: u8,
    },
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        button: u8,
    },
    Scroll {
        x: i32,
        y: i32,
        direction: workspace::ScrollDirection,
        amount: u8,
    },
    PasteText {
        text: String,
    },
    TypeText {
        text: String,
    },
    Key {
        key: String,
    },
}

impl InputForwardingRequest {
    fn dispatch(self, target_id: &str) -> Result<workspace::IpcResponse> {
        match self {
            Self::MovePointer { x, y } => workspace::move_pointer(target_id, x, y),
            Self::Click { x, y, button } => {
                workspace::click(target_id, x, y, Some(button), Some(1))
            }
            Self::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
            } => workspace::drag(target_id, from_x, from_y, to_x, to_y, Some(button)),
            Self::Scroll {
                x,
                y,
                direction,
                amount,
            } => workspace::scroll(target_id, x, y, direction, Some(amount)),
            Self::PasteText { text } => workspace::paste_text(target_id, text, None),
            Self::TypeText { text } => workspace::type_text(target_id, text),
            Self::Key { key } => workspace::key(target_id, key),
        }
    }
}

fn push_bounded_input_forwarding_request(
    queue: &mut VecDeque<QueuedInputForwardingRequest>,
    request: QueuedInputForwardingRequest,
) -> bool {
    let dropped = queue.len() >= MAX_INPUT_FORWARDING_QUEUE_LEN;
    if dropped {
        queue.pop_front();
    }
    queue.push_back(request);
    dropped
}

fn host_clipboard_too_large_message(byte_len: usize) -> String {
    format!(
        "Host clipboard text is {byte_len} bytes; maximum workspace paste is {} bytes",
        workspace::MAX_CLIPBOARD_TEXT_BYTES
    )
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DragKind {
    Move,
    Resize(ResizeEdge),
}

impl DragKind {
    fn is_resize(self) -> bool {
        matches!(self, DragKind::Resize(_))
    }
}

/// Which window axes a resize edge affects, and whether the origin moves when
/// that edge is dragged (left/top edges grow the window away from a fixed far
/// corner, so the origin shifts; right/bottom keep the origin fixed).
struct EdgeAxes {
    moves_left: bool,
    moves_top: bool,
    affects_width: bool,
    affects_height: bool,
}

fn resize_edge_axes(edge: ResizeEdge) -> EdgeAxes {
    match edge {
        ResizeEdge::Right => EdgeAxes {
            moves_left: false,
            moves_top: false,
            affects_width: true,
            affects_height: false,
        },
        ResizeEdge::Bottom => EdgeAxes {
            moves_left: false,
            moves_top: false,
            affects_width: false,
            affects_height: true,
        },
        ResizeEdge::BottomRight => EdgeAxes {
            moves_left: false,
            moves_top: false,
            affects_width: true,
            affects_height: true,
        },
        ResizeEdge::Left => EdgeAxes {
            moves_left: true,
            moves_top: false,
            affects_width: true,
            affects_height: false,
        },
        ResizeEdge::Top => EdgeAxes {
            moves_left: false,
            moves_top: true,
            affects_width: false,
            affects_height: true,
        },
        ResizeEdge::TopLeft => EdgeAxes {
            moves_left: true,
            moves_top: true,
            affects_width: true,
            affects_height: true,
        },
        ResizeEdge::TopRight => EdgeAxes {
            moves_left: false,
            moves_top: true,
            affects_width: true,
            affects_height: true,
        },
        ResizeEdge::BottomLeft => EdgeAxes {
            moves_left: true,
            moves_top: false,
            affects_width: true,
            affects_height: true,
        },
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FooterMode {
    #[default]
    Activity,
    Task,
    Isolation,
    Apps,
}

impl FooterMode {
    fn next(self) -> Self {
        match self {
            Self::Activity => Self::Task,
            Self::Task => Self::Isolation,
            Self::Isolation => Self::Apps,
            Self::Apps => Self::Activity,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Activity => "Act",
            Self::Task => "Task",
            Self::Isolation => "Iso",
            Self::Apps => "Apps",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Task => "Task",
            Self::Isolation => "Isolation",
            Self::Apps => "Apps",
        }
    }
}

#[derive(Clone)]
struct ViewerActivity {
    label: String,
    timestamp_unix: u64,
}

struct PendingCleanup {
    id: String,
    expires_at_unix: u64,
}

struct PendingRevoke {
    id: String,
    expires_at_unix: u64,
}

#[derive(Clone)]
struct AppLogTarget {
    app_id: String,
    stream: String,
}

struct X11DragSession {
    connection: RustConnection,
    root: X11Window,
    window: X11Window,
    start_pointer: X11Point,
    start_bounds: X11Bounds,
}

#[derive(Copy, Clone)]
struct X11Point {
    x: i32,
    y: i32,
}

#[derive(Copy, Clone)]
struct X11Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ViewerAction {
    StartDefault {
        id: String,
    },
    StartProfile {
        id: String,
        profile_id: String,
    },
    CaptureWindow {
        id: String,
    },
    OpenArtifacts {
        id: String,
    },
    OpenEvents {
        id: String,
    },
    OpenAppLog {
        id: String,
        app_id: String,
        stream: String,
    },
    CleanStopped {
        id: String,
    },
    RevokeRunning {
        id: String,
    },
    Stop {
        id: String,
    },
}

impl ViewerAction {
    fn label(&self) -> String {
        match self {
            ViewerAction::StartDefault { .. } => "Starting workspace".to_string(),
            ViewerAction::StartProfile { profile_id, .. } => format!("Opening {profile_id}"),
            ViewerAction::CaptureWindow { .. } => "Capturing active window".to_string(),
            ViewerAction::OpenArtifacts { .. } => "Opening artifacts".to_string(),
            ViewerAction::OpenEvents { .. } => "Opening event log".to_string(),
            ViewerAction::OpenAppLog { .. } => "Opening app log".to_string(),
            ViewerAction::CleanStopped { .. } => "Cleaning stopped workspace".to_string(),
            ViewerAction::RevokeRunning { .. } => "Revoking workspace".to_string(),
            ViewerAction::Stop { .. } => "Stopping workspace".to_string(),
        }
    }

    fn button_label(&self) -> &'static str {
        match self {
            ViewerAction::StartDefault { .. } | ViewerAction::StartProfile { .. } => "Starting",
            ViewerAction::CaptureWindow { .. } => "Saving",
            ViewerAction::OpenArtifacts { .. }
            | ViewerAction::OpenEvents { .. }
            | ViewerAction::OpenAppLog { .. } => "Opening",
            ViewerAction::CleanStopped { .. } => "Cleaning",
            ViewerAction::RevokeRunning { .. } => "Revoking",
            ViewerAction::Stop { .. } => "Stopping",
        }
    }
}

struct ViewerActionResult {
    message: String,
    activity_label: Option<String>,
}

impl ViewerActionResult {
    fn new(message: impl Into<String>, activity_label: Option<String>) -> Self {
        Self {
            message: message.into(),
            activity_label,
        }
    }
}

impl AgentWorkspaceViewer {
    fn new(options: ViewerOptions, cx: &mut Context<Self>) -> Self {
        let preferences = load_viewer_preferences();
        let footer_mode = preferences.footer_mode;
        let screen_stream = initial_screen_stream(options.background, preferences.screen_stream);
        let bound_target_id = options.exit_when_workspace_gone.then(|| options.id.clone());
        let mut viewer = Self {
            target_id: options.id,
            bound_target_id,
            snapshot: ViewerSnapshot::default(),
            selected_profile_id: None,
            permissions: options.permissions,
            focus_handle: cx.focus_handle(),
            preferences,
            active_window: None,
            latest_activity: None,
            control_state: control::control_status()
                .map(|status| status.state)
                .unwrap_or_default(),
            frame: None,
            screen_bounds: Arc::new(Mutex::new(None)),
            last_refresh_unix: wall_clock_seconds(),
            message: "Loading workspace state".to_string(),
            error: None,
            screen_stream,
            input_forwarding_allowed: options.input_forwarding,
            input_forwarding_enabled: false,
            input_forwarding_arm_expires_at_unix: None,
            input_forwarding_drag: None,
            input_forwarding_queue: VecDeque::new(),
            input_forwarding_in_flight: false,
            input_forwarding_epoch: 0,
            input_forwarding_burst_generation: 0,
            input_refresh_burst_active: false,
            exit_when_workspace_gone: options.exit_when_workspace_gone,
            footer_mode,
            refresh_in_flight: false,
            action_in_flight: None,
            show_more: false,
            pending_cleanup: None,
            pending_revoke: None,
            interaction_drag: None,
            _poll_task: None,
            _refresh_task: None,
            _action_task: None,
            _input_forwarding_task: None,
            _interaction_task: None,
            _input_refresh_burst_task: None,
        };
        viewer.request_refresh(cx, None);
        viewer._poll_task = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            if this
                .update(cx, |viewer: &mut AgentWorkspaceViewer, cx| {
                    if viewer.action_in_flight.is_none() && !viewer.interaction_active() {
                        viewer.request_refresh(cx, None);
                    }
                })
                .is_err()
            {
                break;
            }
        }));
        viewer
    }

    fn request_refresh(&mut self, cx: &mut Context<Self>, message: Option<String>) {
        if self.refresh_in_flight || self.interaction_active() {
            return;
        }

        self.refresh_in_flight = true;
        if let Some(message) = message {
            self.message = message;
        }

        let target_id = self.target_id.clone();
        let bound_target_id = self.bound_target_id.clone();
        let has_frame = self.frame.is_some();
        let capture_frame = self.screen_stream;
        let executor = cx.background_executor().clone();
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move {
                    compute_viewer_refresh(target_id, bound_target_id, has_frame, capture_frame)
                })
                .await;
            let _ = this.update(cx, |viewer, cx| {
                viewer.refresh_in_flight = false;
                match result {
                    Ok(refresh) => viewer.apply_refresh(refresh, cx),
                    Err(error) => {
                        viewer.last_refresh_unix = wall_clock_seconds();
                        viewer.message = "Refresh failed".to_string();
                        viewer.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply_refresh(&mut self, refresh: ViewerRefreshResult, cx: &mut Context<Self>) {
        if self.exit_when_workspace_gone && refresh.bound_target_missing {
            self.message = format!("Workspace {} is gone; closing viewer", self.target_id);
            self.error = None;
            cx.quit();
            return;
        }

        let action_running = self.action_in_flight.is_some();
        let interaction_running = self.interaction_active();
        let target_changed = self.target_id != refresh.target_id;
        self.target_id = refresh.target_id;
        self.snapshot = refresh.snapshot;
        self.normalize_selected_profile();
        self.active_window = refresh.active_window;
        self.control_state = refresh.control_state;
        if target_changed {
            self.latest_activity = None;
            self.pending_cleanup = None;
            self.pending_revoke = None;
            self.reset_input_forwarding_state(true, true);
        }
        if let Some(activity) = refresh.latest_activity {
            if self
                .latest_activity
                .as_ref()
                .is_none_or(|current| activity.timestamp_unix > current.timestamp_unix)
            {
                self.latest_activity = Some(activity);
            }
        }
        if !interaction_running {
            match refresh.frame_update {
                ViewerFrameUpdate::KeepExisting => {}
                ViewerFrameUpdate::Replace(frame) => self.frame = frame,
            }
            self.last_refresh_unix = refresh.last_refresh_unix;
        }
        if interaction_running {
            return;
        } else if action_running {
            if refresh.error.is_some() {
                self.error = refresh.error;
            }
        } else {
            self.message = refresh.message;
            self.error = refresh.error;
        }
        let now = wall_clock_seconds();
        self.clear_stale_cleanup_prompt(now);
        self.clear_stale_revoke_prompt(now);
        self.clear_expired_input_forward_prompt(now);
    }

    fn start_selected(&mut self, cx: &mut Context<Self>) {
        let action = if let Some(profile_id) = self.selected_profile_id.clone() {
            ViewerAction::StartProfile {
                id: self.target_id.clone(),
                profile_id,
            }
        } else {
            ViewerAction::StartDefault {
                id: self.target_id.clone(),
            }
        };
        self.spawn_action(action, cx);
    }

    fn stop_selected(&mut self, cx: &mut Context<Self>) {
        self.spawn_action(
            ViewerAction::Stop {
                id: self.target_id.clone(),
            },
            cx,
        );
    }

    fn capture_active_window(&mut self, cx: &mut Context<Self>) {
        self.spawn_action(
            ViewerAction::CaptureWindow {
                id: self.target_id.clone(),
            },
            cx,
        );
    }

    fn open_artifacts(&mut self, cx: &mut Context<Self>) {
        self.spawn_action(
            ViewerAction::OpenArtifacts {
                id: self.target_id.clone(),
            },
            cx,
        );
    }

    fn open_events(&mut self, cx: &mut Context<Self>) {
        self.spawn_action(
            ViewerAction::OpenEvents {
                id: self.target_id.clone(),
            },
            cx,
        );
    }

    fn open_app_log(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.current_app_log_target() else {
            self.message = "No app log found for this workspace".to_string();
            return;
        };

        self.spawn_action(
            ViewerAction::OpenAppLog {
                id: self.target_id.clone(),
                app_id: target.app_id,
                stream: target.stream,
            },
            cx,
        );
    }

    fn clean_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(current) = &self.action_in_flight {
            self.message = format!("{} is already running", current.label());
            return;
        }

        let target_running = self.selected_workspace().map(|workspace| workspace.running);
        match target_running {
            Some(true) => {
                self.pending_cleanup = None;
                self.message = "Stop the workspace before cleaning its files".to_string();
            }
            None => {
                self.pending_cleanup = None;
                self.message = "No stopped workspace files to clean".to_string();
            }
            Some(false) => {
                let now = wall_clock_seconds();
                self.clear_expired_cleanup(now);
                let id = self.target_id.clone();
                if self.cleanup_armed(&id, now) {
                    self.pending_cleanup = None;
                    self.spawn_action(ViewerAction::CleanStopped { id }, cx);
                } else {
                    self.pending_cleanup = Some(PendingCleanup {
                        id: id.clone(),
                        expires_at_unix: now + CLEAN_CONFIRM_SECONDS,
                    });
                    self.error = None;
                    self.message = format!("Click Clean again to remove stopped workspace {id}");
                }
            }
        }
    }

    fn revoke_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(current) = &self.action_in_flight {
            self.message = format!("{} is already running", current.label());
            return;
        }

        let target_running = self.selected_workspace().map(|workspace| workspace.running);
        match target_running {
            Some(false) => {
                self.pending_revoke = None;
                self.message =
                    "Workspace is already stopped; use Clean to remove files".to_string();
            }
            None => {
                self.pending_revoke = None;
                self.message = "No running workspace to revoke".to_string();
            }
            Some(true) => {
                let now = wall_clock_seconds();
                self.clear_expired_revoke(now);
                let id = self.target_id.clone();
                if self.revoke_armed(&id, now) {
                    self.pending_revoke = None;
                    self.spawn_action(ViewerAction::RevokeRunning { id }, cx);
                } else {
                    self.pending_revoke = Some(PendingRevoke {
                        id: id.clone(),
                        expires_at_unix: now + REVOKE_CONFIRM_SECONDS,
                    });
                    self.error = None;
                    self.message = format!("Click Rev again to stop and remove workspace {id}");
                }
            }
        }
    }

    fn cycle_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(current) = &self.action_in_flight {
            self.message = format!("{} is already running", current.label());
            return;
        }
        if self.refresh_in_flight {
            self.message = "Refresh is already running".to_string();
            return;
        }

        let workspace_ids = self.ordered_workspace_ids();
        if workspace_ids.len() <= 1 {
            self.message = "No other workspaces found".to_string();
            return;
        }

        let current_index = workspace_ids
            .iter()
            .position(|id| id == &self.target_id)
            .unwrap_or(workspace_ids.len().saturating_sub(1));
        let next_index = (current_index + 1) % workspace_ids.len();
        let next_id = workspace_ids[next_index].clone();
        self.target_id = next_id.clone();
        self.frame = None;
        self.active_window = None;
        self.latest_activity = None;
        self.pending_cleanup = None;
        self.pending_revoke = None;
        self.reset_input_forwarding_state(true, true);
        self.error = None;
        self.message = format!(
            "Viewing workspace {} ({}/{})",
            next_id,
            next_index + 1,
            workspace_ids.len()
        );
        self.request_refresh(cx, Some(self.message.clone()));
    }

    fn cycle_footer_mode(&mut self) {
        self.footer_mode = self.footer_mode.next();
        self.preferences.footer_mode = self.footer_mode;
        let _ = save_viewer_preferences(&self.preferences);
        self.message = format!("Footer: {}", self.footer_mode.description());
    }

    fn spawn_action(&mut self, action: ViewerAction, cx: &mut Context<Self>) {
        if let Some(current) = &self.action_in_flight {
            self.message = format!("{} is already running", current.label());
            return;
        }

        if !matches!(action, ViewerAction::CleanStopped { .. }) {
            self.pending_cleanup = None;
        }
        if !matches!(action, ViewerAction::RevokeRunning { .. }) {
            self.pending_revoke = None;
        }
        let action_label = action.label();
        self.message = format!("{action_label}...");
        self.error = None;
        self.action_in_flight = Some(action.clone());
        let executor = cx.background_executor().clone();
        let permissions = self.permissions.clone();
        self._action_task = Some(cx.spawn(async move |this, cx| {
            let result = executor
                .spawn(async move { run_viewer_action(action, permissions) })
                .await;
            let _ = this.update(cx, |viewer, cx| {
                viewer.action_in_flight = None;
                match result {
                    Ok(result) => {
                        viewer.message = result.message;
                        viewer.error = None;
                        if let Some(label) = result.activity_label {
                            viewer.latest_activity = Some(ViewerActivity {
                                label,
                                timestamp_unix: wall_clock_seconds(),
                            });
                        }
                    }
                    Err(error) => {
                        viewer.message = "Action finished with an error".to_string();
                        viewer.error = Some(error.to_string());
                    }
                }
                let message = viewer.message.clone();
                viewer.request_refresh(cx, Some(message));
                cx.notify();
            });
        }));
    }

    fn interaction_active(&self) -> bool {
        self.interaction_drag.is_some() || self.input_forwarding_drag.is_some()
    }

    fn reset_input_forwarding_state(&mut self, disable_enabled: bool, clear_bounds: bool) {
        if disable_enabled {
            self.input_forwarding_enabled = false;
        }
        self.input_forwarding_arm_expires_at_unix = None;
        self.input_forwarding_drag = None;
        self.input_forwarding_queue.clear();
        self.input_forwarding_epoch = self.input_forwarding_epoch.wrapping_add(1);
        self.input_forwarding_burst_generation =
            self.input_forwarding_burst_generation.wrapping_add(1);
        if clear_bounds {
            store_screen_bounds(&self.screen_bounds, None);
        }
    }

    fn toggle_input_forwarding(&mut self) {
        if !self.input_forwarding_allowed {
            self.message = "Input forwarding was not enabled for this viewer session".to_string();
            self.error = None;
            return;
        }

        let now = wall_clock_seconds();
        self.clear_expired_input_forward_prompt(now);
        if self.input_forwarding_enabled {
            self.reset_input_forwarding_state(true, false);
            self.message = "Manual input forwarding disabled".to_string();
            self.error = None;
        } else if self
            .input_forwarding_arm_expires_at_unix
            .is_some_and(|expires_at| expires_at > now)
        {
            self.input_forwarding_enabled = true;
            self.input_forwarding_arm_expires_at_unix = None;
            self.message =
                "Manual input forwarding enabled; clicks, scroll, keys, and host clipboard paste target only the isolated workspace"
                    .to_string();
            self.error = None;
        } else {
            self.input_forwarding_arm_expires_at_unix = Some(now + INPUT_FORWARD_CONFIRM_SECONDS);
            self.message = format!(
                "Click Input again within {INPUT_FORWARD_CONFIRM_SECONDS}s to forward clicks, scroll, keyboard, and host clipboard paste into {}",
                self.target_id
            );
            self.error = None;
        }
    }

    fn clear_expired_input_forward_prompt(&mut self, now: u64) {
        if self
            .input_forwarding_arm_expires_at_unix
            .is_some_and(|expires_at| expires_at <= now)
        {
            self.input_forwarding_arm_expires_at_unix = None;
        }
    }

    fn input_forwarding_armed_seconds_left(&self, now: u64) -> Option<u64> {
        self.input_forwarding_arm_expires_at_unix
            .filter(|expires_at| *expires_at > now)
            .map(|expires_at| expires_at.saturating_sub(now).max(1))
    }

    fn input_forwarding_ready(&self) -> bool {
        self.input_forwarding_allowed
            && self.input_forwarding_enabled
            && self.screen_stream
            && matches!(self.control_state.mode, McpControlMode::Active)
            && self
                .selected_workspace()
                .is_some_and(|workspace| workspace.running)
    }

    fn input_forwarding_workspace_point(&self, position: Point<Pixels>) -> Option<WorkspacePoint> {
        if !self.input_forwarding_ready() {
            return None;
        }
        let frame = self.frame.as_ref()?;
        let bounds = screen_bounds_snapshot(&self.screen_bounds)?;
        screen_position_to_workspace_point(frame.width, frame.height, bounds, position)
    }

    fn schedule_input_refresh_burst(&mut self, cx: &mut Context<Self>) {
        if !self.screen_stream {
            return;
        }

        self.input_forwarding_burst_generation =
            self.input_forwarding_burst_generation.wrapping_add(1);
        self.request_refresh(cx, None);
        if self.input_refresh_burst_active {
            return;
        }
        self.input_refresh_burst_active = true;

        let executor = cx.background_executor().clone();
        self._input_refresh_burst_task = Some(cx.spawn(async move |this, cx| 'burst: loop {
            let generation =
                match this.update(cx, |viewer, _cx| viewer.input_forwarding_burst_generation) {
                    Ok(generation) => generation,
                    Err(_) => break,
                };
            for delay_ms in INPUT_FORWARD_REFRESH_BURST_DELAYS_MS {
                executor.timer(Duration::from_millis(delay_ms)).await;
                let restart = match this.update(cx, move |viewer, cx| {
                    if viewer.input_forwarding_burst_generation != generation {
                        return true;
                    }
                    if viewer.action_in_flight.is_none() && !viewer.interaction_active() {
                        viewer.request_refresh(cx, None);
                    }
                    false
                }) {
                    Ok(restart) => restart,
                    Err(_) => break 'burst,
                };
                if restart {
                    continue 'burst;
                }
            }
            let restart = match this.update(cx, move |viewer, _cx| {
                if viewer.input_forwarding_burst_generation != generation {
                    true
                } else {
                    viewer.input_refresh_burst_active = false;
                    false
                }
            }) {
                Ok(restart) => restart,
                Err(_) => break,
            };
            if restart {
                continue 'burst;
            }
            break;
        }));
    }

    fn enqueue_input_forwarding(
        &mut self,
        request: InputForwardingRequest,
        cx: &mut Context<Self>,
    ) {
        let dropped = push_bounded_input_forwarding_request(
            &mut self.input_forwarding_queue,
            (self.input_forwarding_epoch, self.target_id.clone(), request),
        );
        if dropped {
            self.message = format!(
                "Input forwarding is busy; dropped oldest queued input after {MAX_INPUT_FORWARDING_QUEUE_LEN} pending requests"
            );
            self.error = None;
        }
        self.spawn_input_forwarding_worker(cx);
    }

    fn spawn_input_forwarding_worker(&mut self, cx: &mut Context<Self>) {
        if self.input_forwarding_in_flight {
            return;
        }
        self.input_forwarding_in_flight = true;
        let executor = cx.background_executor().clone();
        self._input_forwarding_task = Some(cx.spawn(async move |this, cx| loop {
            let Some((epoch, target_id, request)) = this
                .update(cx, |viewer: &mut AgentWorkspaceViewer, _cx| {
                    viewer.input_forwarding_queue.pop_front()
                })
                .ok()
                .flatten()
            else {
                let _ = this.update(cx, |viewer, _cx| {
                    viewer.input_forwarding_in_flight = false;
                });
                break;
            };

            let is_current_epoch = this
                .update(cx, move |viewer, _cx| {
                    viewer.input_forwarding_epoch == epoch
                })
                .unwrap_or(false);
            if !is_current_epoch {
                continue;
            }

            let result = executor
                .spawn(async move { request.dispatch(&target_id) })
                .await;
            if this
                .update(cx, move |viewer, cx| {
                    if viewer.input_forwarding_epoch == epoch {
                        viewer.apply_input_forwarding_result(result, cx);
                    }
                })
                .is_err()
            {
                break;
            }
        }));
    }

    fn begin_input_forward(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_forwarding_allowed {
            return;
        }
        let Some(point) = self.input_forwarding_workspace_point(event.position) else {
            return;
        };
        let Some(button) = x11_button_for_mouse_button(event.button) else {
            return;
        };
        self.focus_handle.focus(window, cx);
        self.input_forwarding_drag = Some(InputForwardingDrag {
            start: point,
            start_position: event.position,
            button,
        });
        self.enqueue_input_forwarding(
            InputForwardingRequest::MovePointer {
                x: point.x,
                y: point.y,
            },
            cx,
        );
        cx.stop_propagation();
        cx.notify();
    }

    fn end_input_forward(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.input_forwarding_drag.take() else {
            return;
        };
        let Some(button) = x11_button_for_mouse_button(event.button) else {
            return;
        };
        if drag.button != button {
            return;
        }
        let Some(point) = self.input_forwarding_workspace_point(event.position) else {
            cx.stop_propagation();
            cx.notify();
            return;
        };

        let delta_x = (event.position.x - drag.start_position.x).as_f32();
        let delta_y = (event.position.y - drag.start_position.y).as_f32();
        let request = if delta_x.hypot(delta_y) >= INPUT_FORWARD_DRAG_THRESHOLD_PX {
            InputForwardingRequest::Drag {
                from_x: drag.start.x,
                from_y: drag.start.y,
                to_x: point.x,
                to_y: point.y,
                button,
            }
        } else {
            InputForwardingRequest::Click {
                x: point.x,
                y: point.y,
                button,
            }
        };
        self.enqueue_input_forwarding(request, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn forward_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(point) = self.input_forwarding_workspace_point(event.position) else {
            return;
        };
        let Some((direction, amount)) = scroll_wheel_to_workspace_scroll(event.delta) else {
            return;
        };
        self.enqueue_input_forwarding(
            InputForwardingRequest::Scroll {
                x: point.x,
                y: point.y,
                direction,
                amount,
            },
            cx,
        );
        cx.stop_propagation();
        cx.notify();
    }

    fn forward_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_forwarding_ready() || event.is_held {
            return;
        }

        let request = if is_paste_keystroke(&event.keystroke) {
            match cx.read_from_clipboard().and_then(|item| item.text()) {
                Some(text) if !text.is_empty() => {
                    let byte_len = text.len();
                    if byte_len > workspace::MAX_CLIPBOARD_TEXT_BYTES {
                        self.message = host_clipboard_too_large_message(byte_len);
                        self.error = None;
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    InputForwardingRequest::PasteText { text }
                }
                _ => {
                    self.message = "Host clipboard has no text to paste".to_string();
                    self.error = None;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        } else if let Some(text) = printable_keystroke_text(&event.keystroke) {
            InputForwardingRequest::TypeText { text }
        } else if let Some(key) = xdotool_key_for_keystroke(&event.keystroke) {
            InputForwardingRequest::Key { key }
        } else {
            return;
        };

        self.enqueue_input_forwarding(request, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn apply_input_forwarding_result(
        &mut self,
        result: Result<workspace::IpcResponse>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(response) if response.ok => self.schedule_input_refresh_burst(cx),
            Ok(response) => self.record_input_forwarding_response(response),
            Err(error) => self.record_input_forwarding_error(error),
        }
        cx.notify();
    }

    fn record_input_forwarding_response(&mut self, response: workspace::IpcResponse) {
        self.message = "Input forwarding failed".to_string();
        self.error = Some(if response.message.is_empty() {
            "workspace daemon returned an unsuccessful response".to_string()
        } else {
            response.message
        });
    }

    fn record_input_forwarding_error(&mut self, error: anyhow::Error) {
        self.message = "Input forwarding failed".to_string();
        self.error = Some(error.to_string());
    }

    fn persist_screen_stream_preference(&mut self) {
        self.preferences.screen_stream = self.screen_stream;
        let _ = save_viewer_preferences(&self.preferences);
    }

    fn set_control_mode_from_viewer(&mut self, next_mode: McpControlMode) {
        if self.control_state.mode == next_mode {
            self.message = format!("MCP control is already {}", next_mode.label());
            return;
        }

        match control::set_control_mode(
            next_mode,
            "gpui-viewer",
            Some(format!(
                "Viewer switched MCP control to {}",
                next_mode.label()
            )),
        ) {
            Ok(status) => {
                self.control_state = status.state;
                self.error = None;
                self.message = format!("MCP control is {}", self.control_state.mode.label());
            }
            Err(error) => {
                self.message = "MCP control update failed".to_string();
                self.error = Some(error.to_string());
            }
        }
    }

    fn persist_window_bounds_preference(&mut self, bounds: Bounds<Pixels>) {
        self.persist_bounds_preference(
            bounds.origin.x.as_f32(),
            bounds.origin.y.as_f32(),
            bounds.size.width.as_f32(),
            bounds.size.height.as_f32(),
        );
    }

    fn persist_x11_bounds_preference(&mut self, bounds: X11Bounds) {
        self.persist_bounds_preference(
            bounds.x as f32,
            bounds.y as f32,
            bounds.width as f32,
            bounds.height as f32,
        );
    }

    fn persist_bounds_preference(&mut self, x: f32, y: f32, width: f32, height: f32) {
        let mut preferences = self.preferences.clone();
        preferences.width = width;
        preferences.height = height;
        preferences.x = Some(x);
        preferences.y = Some(y);
        preferences = normalize_viewer_preferences(preferences);
        if preferences.width == self.preferences.width
            && preferences.height == self.preferences.height
            && preferences.x == self.preferences.x
            && preferences.y == self.preferences.y
        {
            return;
        }
        self.preferences = preferences;
        let _ = save_viewer_preferences(&self.preferences);
    }

    fn begin_move(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let x11 = x11_drag_session();
        self.interaction_drag = Some(InteractionDrag {
            kind: DragKind::Move,
            start_position: event.position,
            start_size: window.bounds().size,
        });
        if let Some(x11) = x11 {
            self.spawn_x11_interaction(DragKind::Move, x11, cx);
        } else {
            window.start_window_move();
        }
        cx.notify();
    }

    fn begin_resize(
        &mut self,
        edge: ResizeEdge,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x11 = x11_drag_session();
        self.interaction_drag = Some(InteractionDrag {
            kind: DragKind::Resize(edge),
            start_position: event.position,
            start_size: window.bounds().size,
        });
        if let Some(x11) = x11 {
            // X11 popups: drive the resize ourselves through the configure path
            // in spawn_x11_interaction (the compositor move/resize hint is not
            // wired for popups here).
            self.spawn_x11_interaction(DragKind::Resize(edge), x11, cx);
        } else {
            // Wayland layer-shell / normal windows: ask the compositor to take
            // over the interactive resize from the matching edge.
            window.start_window_resize(edge);
        }
        cx.notify();
    }

    fn spawn_x11_interaction(
        &mut self,
        kind: DragKind,
        x11: X11DragSession,
        cx: &mut Context<Self>,
    ) {
        let executor = cx.background_executor().clone();
        self._interaction_task = Some(cx.spawn(async move |this, cx| {
            let mut release_ticks = 0;
            loop {
                match x11.apply(kind) {
                    Ok(true) => release_ticks = 0,
                    Ok(false) => {
                        release_ticks += 1;
                        if release_ticks >= 3 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                executor.timer(Duration::from_millis(16)).await;
            }
            let final_bounds = x11.current_bounds().ok();
            let _ = this.update(cx, |viewer, cx| {
                if let Some(bounds) = final_bounds {
                    viewer.persist_x11_bounds_preference(bounds);
                }
                viewer.interaction_drag = None;
                cx.notify();
            });
        }));
    }

    fn update_interaction(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            if let Some(drag) = self.interaction_drag.take() {
                if matches!(drag.kind, DragKind::Move) || drag.kind.is_resize() {
                    self.persist_window_bounds_preference(window.bounds());
                }
            }
            cx.notify();
            return;
        }

        let Some(drag) = &self.interaction_drag else {
            return;
        };

        // Manual resize fallback. Reached only when neither the X11 configure
        // path nor the compositor's interactive resize is driving the gesture
        // (gpui's Window exposes `resize` for size but no origin reposition on
        // this backend). Right/Bottom/BottomRight grow naturally from the fixed
        // top-left; Left/Top edges can only resize here, so we grow width/height
        // from the drag delta and let the compositor keep the origin. Sizes stay
        // clamped to the overlay minimums.
        if let DragKind::Resize(edge) = drag.kind {
            let axes = resize_edge_axes(edge);
            let delta_x = (event.position.x - drag.start_position.x).as_f32();
            let delta_y = (event.position.y - drag.start_position.y).as_f32();

            let start_w = drag.start_size.width.as_f32();
            let start_h = drag.start_size.height.as_f32();
            let mut width = start_w;
            let mut height = start_h;

            if axes.affects_width {
                let signed = if axes.moves_left { -delta_x } else { delta_x };
                width = (start_w + signed).max(OVERLAY_MIN_WIDTH);
            }
            if axes.affects_height {
                let signed = if axes.moves_top { -delta_y } else { delta_y };
                height = (start_h + signed).max(OVERLAY_MIN_HEIGHT);
            }

            window.resize(size(px(width), px(height)));
        }
    }

    fn end_interaction(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(drag) = self.interaction_drag.take() {
            if matches!(drag.kind, DragKind::Move) || drag.kind.is_resize() {
                self.persist_window_bounds_preference(window.bounds());
            }
            cx.notify();
        }
    }

    fn selected_workspace(&self) -> Option<&workspace::WorkspaceListEntry> {
        self.snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.target_id)
    }

    fn current_app_log_target(&self) -> Option<AppLogTarget> {
        let selected = self.selected_workspace()?;
        selected_app_log_target(workspace_entry_apps(selected), self.active_window.as_ref())
    }

    fn ordered_workspace_ids(&self) -> Vec<String> {
        let mut workspaces = self.snapshot.workspaces.iter().collect::<Vec<_>>();
        workspaces.sort_by(|left, right| {
            right
                .running
                .cmp(&left.running)
                .then_with(|| left.id.cmp(&right.id))
        });
        workspaces
            .into_iter()
            .map(|workspace| workspace.id.clone())
            .collect()
    }

    fn workspace_position(&self) -> Option<(usize, usize)> {
        let workspace_ids = self.ordered_workspace_ids();
        let total = workspace_ids.len();
        if total == 0 {
            return None;
        }
        let index = workspace_ids
            .iter()
            .position(|id| id == &self.target_id)
            .unwrap_or(0);
        Some((index + 1, total))
    }

    fn selected_profile(&self) -> Option<&profile::WorkspaceProfile> {
        let selected_id = self.selected_profile_id.as_deref()?;
        self.snapshot
            .profiles
            .iter()
            .find(|profile| profile.id == selected_id)
    }

    fn selected_profile_label(&self) -> String {
        self.selected_profile()
            .map(profile_label)
            .unwrap_or_else(|| "Default workspace".to_string())
    }

    fn cleanup_armed(&self, id: &str, now: u64) -> bool {
        self.pending_cleanup
            .as_ref()
            .is_some_and(|pending| pending.id == id && pending.expires_at_unix > now)
    }

    fn revoke_armed(&self, id: &str, now: u64) -> bool {
        self.pending_revoke
            .as_ref()
            .is_some_and(|pending| pending.id == id && pending.expires_at_unix > now)
    }

    fn clear_expired_cleanup(&mut self, now: u64) {
        if self
            .pending_cleanup
            .as_ref()
            .is_some_and(|pending| pending.expires_at_unix <= now)
        {
            self.pending_cleanup = None;
        }
    }

    fn clear_expired_revoke(&mut self, now: u64) {
        if self
            .pending_revoke
            .as_ref()
            .is_some_and(|pending| pending.expires_at_unix <= now)
        {
            self.pending_revoke = None;
        }
    }

    fn clear_stale_cleanup_prompt(&mut self, now: u64) {
        self.clear_expired_cleanup(now);
        if let Some(pending) = &self.pending_cleanup {
            let still_cleanable = self
                .snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == pending.id && !workspace.running);
            if !still_cleanable {
                self.pending_cleanup = None;
            }
        }
    }

    fn clear_stale_revoke_prompt(&mut self, now: u64) {
        self.clear_expired_revoke(now);
        if let Some(pending) = &self.pending_revoke {
            let still_running = self
                .snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == pending.id && workspace.running);
            if !still_running {
                self.pending_revoke = None;
            }
        }
    }

    fn normalize_selected_profile(&mut self) {
        if self.selected_profile_id.as_ref().is_some_and(|selected| {
            !self
                .snapshot
                .profiles
                .iter()
                .any(|profile| &profile.id == selected)
        }) {
            self.selected_profile_id = None;
        }
    }

    fn cycle_profile(&mut self) {
        self.pending_cleanup = None;
        self.pending_revoke = None;
        if self.snapshot.profiles.is_empty() {
            self.selected_profile_id = None;
            self.message = "No saved profiles found".to_string();
            return;
        }

        let current = self.selected_profile_id.as_deref();
        let next = current
            .and_then(|current| {
                self.snapshot
                    .profiles
                    .iter()
                    .position(|profile| profile.id == current)
            })
            .and_then(|index| self.snapshot.profiles.get(index + 1))
            .map(|profile| profile.id.clone());

        self.selected_profile_id = match (current, next) {
            (None, _) => self
                .snapshot
                .profiles
                .first()
                .map(|profile| profile.id.clone()),
            (Some(_), Some(next)) => Some(next),
            (Some(_), None) => None,
        };
        self.message = format!("Profile: {}", self.selected_profile_label());
    }
}

fn run_viewer_action(
    action: ViewerAction,
    permissions: McpPermissionState,
) -> Result<ViewerActionResult> {
    match action {
        ViewerAction::StartDefault { id } => {
            let options = workspace::WorkspaceStartOptions {
                id,
                purpose: Some("Opened from Agent Workspace Viewer".to_string()),
                user_acknowledged_hidden_workspace: true,
                width: 1280,
                height: 800,
                // Propagate the ceiling into the spawned daemon so a
                // viewer-started workspace enforces it at the IPC socket, not
                // only here in the front-end validate below.
                permissions_source: permissions.source.clone(),
                ..Default::default()
            };
            permissions
                .validate_start_options(&options)
                .context("viewer start exceeds MCP permission ceiling")?;
            workspace::start_workspace(options)
                .map(|response| {
                    ViewerActionResult::new(response.message, Some("Started workspace".to_string()))
                })
                .context("start failed")
        }
        ViewerAction::StartProfile { id, profile_id } => {
            let mut options = workspace::WorkspaceStartOptions {
                id,
                purpose: Some(format!("Opened from Agent Workspace Viewer ({profile_id})")),
                user_acknowledged_hidden_workspace: true,
                user_acknowledged_unenforced_policy: true,
                // Propagate the ceiling into the spawned daemon so a
                // viewer-started profile workspace enforces it at the IPC
                // socket, not only in the front-end validate below.
                permissions_source: permissions.source.clone(),
                ..Default::default()
            };
            let open_options = profile::ProfileWorkspaceOpenOptions {
                run_setup: true,
                setup: profile::ProfileSetupOptions {
                    wait: true,
                    acknowledge_unenforced_policy: true,
                    ..Default::default()
                },
                startup: profile::ProfileStartupOptions {
                    acknowledge_unenforced_policy: true,
                    wait_window: true,
                    window_timeout_ms: Some(10_000),
                    ..Default::default()
                },
            };

            profile::apply_profile_to_start_options(&profile_id, &mut options, false, false)
                .and_then(|profile| {
                    validate_viewer_profile_start_permissions(&permissions, &profile, &options)?;
                    profile::open_profile_workspace(options, &profile_id, open_options)
                })
                .map(|response| {
                    let activity = format!("Opened profile {profile_id}");
                    if response.ready {
                        ViewerActionResult::new(
                            format!("Profile {profile_id} opened"),
                            Some(activity),
                        )
                    } else {
                        ViewerActionResult::new(
                            format!("Profile {profile_id} started with follow-up needed"),
                            Some(activity),
                        )
                    }
                })
                .context("profile start failed")
        }
        ViewerAction::CaptureWindow { id } => capture_active_window(&id),
        ViewerAction::OpenArtifacts { id } => open_workspace_artifacts(&id),
        ViewerAction::OpenEvents { id } => open_workspace_event_log(&id),
        ViewerAction::OpenAppLog { id, app_id, stream } => {
            open_workspace_app_log(&id, &app_id, &stream)
        }
        ViewerAction::CleanStopped { id } => cleanup_stopped_workspace(&id),
        ViewerAction::RevokeRunning { id } => revoke_running_workspace(&id),
        ViewerAction::Stop { id } => workspace::stop_workspace(&id, Some(30_000), false)
            .map(|response| {
                ViewerActionResult::new(response.message, Some("Stopped workspace".to_string()))
            })
            .context("stop failed"),
    }
}

fn validate_viewer_profile_start_permissions(
    permissions: &McpPermissionState,
    profile: &profile::WorkspaceProfile,
    options: &workspace::WorkspaceStartOptions,
) -> Result<()> {
    permissions.validate_profile(profile).with_context(|| {
        format!(
            "viewer profile {} exceeds MCP permission ceiling",
            profile.id
        )
    })?;
    permissions
        .validate_start_options(options)
        .with_context(|| {
            format!(
                "viewer start for profile {} exceeds MCP permission ceiling",
                profile.id
            )
        })?;
    Ok(())
}

fn capture_active_window(id: &str) -> Result<ViewerActionResult> {
    let window = workspace_watch_window(id).context("no visible workspace window to capture")?;
    let response = workspace::screenshot_window(
        id,
        Some(window.id.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .context("active-window screenshot failed")?;
    let screenshot = response
        .screenshot
        .context("active-window screenshot response did not include screenshot metadata")?;
    let window_label = active_window_label(&window);
    Ok(ViewerActionResult::new(
        format!("Captured {window_label} to {}", screenshot.path.display()),
        Some(format!("Captured {window_label}")),
    ))
}

fn open_workspace_app_log(id: &str, app_id: &str, stream: &str) -> Result<ViewerActionResult> {
    let response = workspace::read_app_log(id, app_id.to_string(), stream.to_string(), Some(1))
        .context("app log lookup failed")?;
    let app_log = response
        .app_log
        .context("app log response did not include log metadata")?;
    if !app_log.path.exists() {
        anyhow::bail!("app log does not exist at {}", app_log.path.display());
    }

    open_host_path(&app_log.path)?;
    let app_label = response
        .apps
        .as_ref()
        .and_then(|apps| apps.first())
        .map(app_label)
        .unwrap_or_else(|| app_id.to_string());
    Ok(ViewerActionResult::new(
        format!("Opened {stream} log for {app_label}"),
        Some(format!("Opened {app_label} {stream} log")),
    ))
}

fn open_workspace_artifacts(id: &str) -> Result<ViewerActionResult> {
    let artifacts = workspace::artifacts(id, false);
    if !artifacts.runtime_dir.exists() {
        anyhow::bail!("workspace artifacts folder does not exist for {id}");
    }
    open_host_path(&artifacts.runtime_dir)?;
    Ok(ViewerActionResult::new(
        format!("Opened artifacts for {id}"),
        Some("Opened artifacts".to_string()),
    ))
}

fn open_workspace_event_log(id: &str) -> Result<ViewerActionResult> {
    let artifacts = workspace::artifacts(id, false);
    if !artifacts.runtime_dir.exists() {
        anyhow::bail!("workspace artifacts folder does not exist for {id}");
    }

    if let Some(event_log) = event_log_artifact(&artifacts) {
        if event_log.exists {
            open_host_path(&event_log.path)?;
            return Ok(ViewerActionResult::new(
                format!("Opened event log for {id}"),
                Some("Opened event log".to_string()),
            ));
        }
    }

    open_host_path(&artifacts.runtime_dir)?;
    Ok(ViewerActionResult::new(
        format!("Event log not written yet; opened artifacts for {id}"),
        Some("Opened artifacts".to_string()),
    ))
}

fn event_log_artifact(
    artifacts: &workspace::WorkspaceArtifacts,
) -> Option<&workspace::WorkspaceArtifact> {
    artifacts.files.iter().find(|file| file.kind == "event_log")
}

fn cleanup_stopped_workspace(id: &str) -> Result<ViewerActionResult> {
    let cleanup = workspace::cleanup_stale_workspaces(Some(id.to_string()), false)
        .context("workspace cleanup failed")?;

    if let Some(removed) = cleanup.removed.iter().find(|entry| entry.id == id) {
        let process_count = removed.process_cleanup.len();
        let activity = if process_count == 0 {
            "Removed stopped workspace".to_string()
        } else {
            format!("Removed stopped workspace and {process_count} stale process action(s)")
        };
        return Ok(ViewerActionResult::new(
            format!("Removed stopped workspace {id}"),
            Some(activity),
        ));
    }

    if let Some(skipped) = cleanup.skipped.iter().find(|entry| entry.id == id) {
        anyhow::bail!("could not clean {id}: {}", skipped.reason);
    }

    anyhow::bail!("workspace {id} was not found")
}

fn revoke_running_workspace(id: &str) -> Result<ViewerActionResult> {
    let stop = workspace::stop_workspace(id, Some(30_000), false).context("stop failed")?;
    if !stop.ok {
        anyhow::bail!("{}", stop.message);
    }

    let cleanup = workspace::cleanup_stale_workspaces(Some(id.to_string()), false)
        .context("workspace cleanup failed")?;
    if let Some(removed) = cleanup.removed.iter().find(|entry| entry.id == id) {
        let process_count = removed.process_cleanup.len();
        let activity = if process_count == 0 {
            "Revoked workspace".to_string()
        } else {
            format!("Revoked workspace and {process_count} stale process action(s)")
        };
        return Ok(ViewerActionResult::new(
            format!("Revoked workspace {id}"),
            Some(activity),
        ));
    }

    if let Some(skipped) = cleanup.skipped.iter().find(|entry| entry.id == id) {
        anyhow::bail!(
            "workspace stopped, but cleanup skipped {id}: {}",
            skipped.reason
        );
    }

    anyhow::bail!("workspace {id} was not found after stop")
}

fn open_host_path(path: &Path) -> Result<()> {
    let candidates = [
        ("xdg-open", vec![path.as_os_str().to_os_string()]),
        (
            "gio",
            vec![OsString::from("open"), path.as_os_str().to_os_string()],
        ),
    ];
    let mut failures = Vec::new();
    for (program, args) in candidates {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match spawn_reaped_child(&mut command) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                failures.push(format!("{program}: not found"));
            }
            Err(error) => {
                failures.push(format!("{program}: {error}"));
            }
        }
    }
    anyhow::bail!(
        "failed to open {} ({})",
        path.display(),
        failures.join("; ")
    )
}

fn viewer_permissions_from_env() -> Option<McpPermissionState> {
    env::var(VIEWER_PERMISSIONS_ENV)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
}

fn compute_viewer_refresh(
    target_id: String,
    bound_target_id: Option<String>,
    has_frame: bool,
    capture_frame: bool,
) -> Result<ViewerRefreshResult> {
    let last_refresh_unix = wall_clock_seconds();
    let doctor = workspace::doctor_report();

    match (workspace::list_workspaces(), profile::list_profiles()) {
        (Ok(list), Ok(profiles)) => {
            let mut target_id = target_id;
            let bound_target_missing =
                bound_target_id_missing(bound_target_id.as_deref(), &list.workspaces);
            let active_id = list
                .workspaces
                .iter()
                .find(|workspace| workspace.running)
                .map(|workspace| workspace.id.clone());
            if !list
                .workspaces
                .iter()
                .any(|workspace| workspace.id == target_id)
            {
                if let Some(active_id) = active_id {
                    target_id = active_id;
                } else if let Some(workspace) = list
                    .workspaces
                    .iter()
                    .min_by(|left, right| left.id.cmp(&right.id))
                {
                    target_id = workspace.id.clone();
                }
            }

            let target_entry = list
                .workspaces
                .iter()
                .find(|workspace| workspace.id == target_id);
            let running = target_entry.is_some_and(|entry| entry.running);
            let activity_apps = target_entry
                .and_then(workspace_entry_apps)
                .map(|apps| apps.to_vec())
                .unwrap_or_default();
            let snapshot = ViewerSnapshot {
                workspaces: list.workspaces,
                profiles: profiles.profiles,
                doctor_ready: doctor.ready_for_x11_workspace,
                doctor_blockers: doctor.blockers,
            };

            let (active_window, frame_update, error) = if running {
                let active_window = workspace_watch_window(&target_id);
                if capture_frame {
                    match capture_screenshot(&target_id) {
                        Ok(frame) => (active_window, ViewerFrameUpdate::Replace(Some(frame)), None),
                        Err(error) => (
                            active_window,
                            ViewerFrameUpdate::KeepExisting,
                            Some(if has_frame {
                                format!("Screen stream failed; showing last frame: {error}")
                            } else {
                                format!("Screen stream failed: {error}")
                            }),
                        ),
                    }
                } else if should_keep_paused_frame(running, has_frame, capture_frame) {
                    (active_window, ViewerFrameUpdate::KeepExisting, None)
                } else {
                    (active_window, ViewerFrameUpdate::Replace(None), None)
                }
            } else {
                (None, ViewerFrameUpdate::Replace(None), None)
            };
            let latest_activity = if running {
                latest_workspace_activity(&target_id, &activity_apps, active_window.as_ref())
            } else {
                None
            };
            let control_state = control::control_status()
                .map(|status| status.state)
                .unwrap_or_default();
            let workspace_count = snapshot.workspaces.len();
            let profile_count = snapshot.profiles.len();

            Ok(ViewerRefreshResult {
                target_id,
                bound_target_missing,
                snapshot,
                active_window,
                latest_activity,
                control_state,
                frame_update,
                last_refresh_unix,
                message: format!(
                    "Refreshed at {} - {} workspace(s), {} profile(s)",
                    last_refresh_unix, workspace_count, profile_count
                ),
                error,
            })
        }
        (workspace_result, profile_result) => Err(anyhow::anyhow!(
            "{}{}",
            workspace_result
                .err()
                .map(|error| format!("Workspaces: {error}. "))
                .unwrap_or_default(),
            profile_result
                .err()
                .map(|error| format!("Profiles: {error}."))
                .unwrap_or_default()
        )),
    }
}

fn should_keep_paused_frame(running: bool, has_frame: bool, capture_frame: bool) -> bool {
    running && has_frame && !capture_frame
}

fn bound_target_id_missing(
    bound_target_id: Option<&str>,
    workspaces: &[workspace::WorkspaceListEntry],
) -> bool {
    bound_target_id.is_some_and(|id| !workspaces.iter().any(|workspace| workspace.id == id))
}

impl Render for AgentWorkspaceViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let on_refresh = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            if let Some(action) = &this.action_in_flight {
                this.message = format!("{} is running", action.label());
            } else {
                this.request_refresh(cx, Some("Refreshing workspace snapshot".to_string()));
            }
            cx.notify();
        });
        let on_start = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.start_selected(cx);
            cx.notify();
        });
        let on_stop = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.stop_selected(cx);
            cx.notify();
        });
        let on_revoke = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.revoke_selected(cx);
            cx.notify();
        });
        let on_capture = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.capture_active_window(cx);
            cx.notify();
        });
        let on_artifacts = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.open_artifacts(cx);
            cx.notify();
        });
        let on_events = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.open_events(cx);
            cx.notify();
        });
        let on_app_log = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.open_app_log(cx);
            cx.notify();
        });
        let on_clean = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.clean_selected(cx);
            cx.notify();
        });
        let on_live = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.screen_stream = !this.screen_stream;
            this.persist_screen_stream_preference();
            this.message = if this.screen_stream {
                "Screen stream enabled".to_string()
            } else if this.frame.is_some() {
                "Screen stream paused; keeping last frame".to_string()
            } else {
                "Screen stream paused".to_string()
            };
            if this.screen_stream {
                this.request_refresh(cx, Some("Capturing workspace screen".to_string()));
            } else {
                this.reset_input_forwarding_state(true, false);
            }
            cx.notify();
        });
        let on_input = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.toggle_input_forwarding();
            cx.notify();
        });
        let on_control_active = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.set_control_mode_from_viewer(McpControlMode::Active);
            cx.notify();
        });
        let on_control_read_only =
            cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
                this.set_control_mode_from_viewer(McpControlMode::ReadOnly);
                cx.notify();
            });
        let on_control_pause = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.set_control_mode_from_viewer(McpControlMode::Paused);
            cx.notify();
        });
        let on_profile = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.cycle_profile();
            cx.notify();
        });
        let on_workspace = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.cycle_workspace(cx);
            cx.notify();
        });
        let on_footer_mode = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.cycle_footer_mode();
            cx.notify();
        });
        let on_toggle_more = cx.listener(|this: &mut Self, _event: &ClickEvent, _window, cx| {
            this.show_more = !this.show_more;
            cx.notify();
        });
        // Live-control segmented control `[ Run · RO · Pause ]`. The current mode
        // is the filled (non-interactive) segment; the other two are the
        // switch-to choices and carry the matching control listener.
        let control_mode = self.control_state.mode;
        let control_segments = vec![
            Segment {
                id: "viewer-mcp-active",
                label: "Run".into(),
                active: control_mode == McpControlMode::Active,
                tooltip: Some(tooltip_text(mcp_control_action_tooltip(
                    control_mode,
                    McpControlMode::Active,
                ))),
                on_click: Some(Box::new(on_control_active)),
            },
            Segment {
                id: "viewer-mcp-read-only",
                label: McpControlMode::ReadOnly.button_label().into(),
                active: control_mode == McpControlMode::ReadOnly,
                tooltip: Some(tooltip_text(mcp_control_action_tooltip(
                    control_mode,
                    McpControlMode::ReadOnly,
                ))),
                on_click: Some(Box::new(on_control_read_only)),
            },
            Segment {
                id: "viewer-mcp-pause",
                label: McpControlMode::Paused.button_label().into(),
                active: control_mode == McpControlMode::Paused,
                tooltip: Some(tooltip_text(mcp_control_action_tooltip(
                    control_mode,
                    McpControlMode::Paused,
                ))),
                on_click: Some(Box::new(on_control_pause)),
            },
        ];
        let on_window_move_start = cx.listener(Self::begin_move);
        // One listener per resize edge/corner; each starts the resize bound to
        // its own ResizeEdge.
        let resize_listener = |edge: ResizeEdge| {
            cx.listener(move |this: &mut Self, event: &MouseDownEvent, window, cx| {
                this.begin_resize(edge, event, window, cx);
            })
        };
        let on_resize_top = resize_listener(ResizeEdge::Top);
        let on_resize_bottom = resize_listener(ResizeEdge::Bottom);
        let on_resize_left = resize_listener(ResizeEdge::Left);
        let on_resize_right = resize_listener(ResizeEdge::Right);
        let on_resize_top_left = resize_listener(ResizeEdge::TopLeft);
        let on_resize_top_right = resize_listener(ResizeEdge::TopRight);
        let on_resize_bottom_left = resize_listener(ResizeEdge::BottomLeft);
        let on_resize_bottom_right = resize_listener(ResizeEdge::BottomRight);
        let now = wall_clock_seconds();
        self.clear_stale_cleanup_prompt(now);
        self.clear_stale_revoke_prompt(now);
        self.clear_expired_input_forward_prompt(now);
        let selected = self.selected_workspace();
        let status = selected.and_then(|entry| entry.status.as_ref());
        let manifest = selected.and_then(|entry| entry.manifest.as_ref());
        let has_workspace_artifacts = selected.is_some();
        let running = selected.is_some_and(|entry| entry.running);
        let profile_id = status
            .and_then(|status| status.profile_id.as_deref())
            .or_else(|| manifest.and_then(|manifest| manifest.profile_id.as_deref()))
            .unwrap_or("none");
        let display = status
            .map(|status| status.display.as_str())
            .or_else(|| manifest.map(|manifest| manifest.display.as_str()))
            .unwrap_or("-");
        let app_count = status
            .map(|status| status.apps.len())
            .or_else(|| manifest.map(|manifest| manifest.apps.len()))
            .unwrap_or(0);
        let workspace_apps = selected.and_then(workspace_entry_apps);
        let app_log_target = selected_app_log_target(workspace_apps, self.active_window.as_ref());
        let frame = self.frame.clone();
        let image = frame.as_ref().map(|frame| frame.image.clone());
        let title = format!("Workspace {}", self.target_id);
        let live_label = if self.screen_stream { "View" } else { "Still" };
        let live_tooltip = if self.screen_stream {
            "Stop streaming screen frames; status still refreshes"
        } else {
            "Stream workspace screen frames every 3s"
        };
        let busy_action = self.action_in_flight.clone();
        let refreshing = self.refresh_in_flight;
        let workspace_position = self.workspace_position();
        let workspace_cycle_label = workspace_position
            .filter(|(_, total)| *total > 1)
            .map(|(index, total)| format!("{index}/{total}"));
        let selected_profile_label = self.selected_profile_label();
        let cleanup_confirm_seconds_left = self.pending_cleanup.as_ref().and_then(|pending| {
            (pending.id == self.target_id && pending.expires_at_unix > now)
                .then_some(pending.expires_at_unix.saturating_sub(now).max(1))
        });
        let cleanup_armed = cleanup_confirm_seconds_left.is_some();
        let revoke_confirm_seconds_left = self.pending_revoke.as_ref().and_then(|pending| {
            (pending.id == self.target_id && pending.expires_at_unix > now)
                .then_some(pending.expires_at_unix.saturating_sub(now).max(1))
        });
        let revoke_armed = revoke_confirm_seconds_left.is_some();
        let input_forward_confirm_seconds_left = self.input_forwarding_armed_seconds_left(now);
        let input_forwarding_wait_reason = if self.input_forwarding_enabled && !running {
            Some("waiting for a running workspace")
        } else if self.input_forwarding_enabled && !matches!(control_mode, McpControlMode::Active) {
            Some("waiting for MCP Run mode")
        } else {
            None
        };
        let activity_label = self
            .active_window
            .as_ref()
            .map(active_window_label)
            .unwrap_or_else(|| profile_id.to_string());
        let detail = if let Some(action) = &busy_action {
            format!("{}...", action.label())
        } else if refreshing {
            "Refreshing workspace snapshot...".to_string()
        } else if let Some(seconds_left) = revoke_confirm_seconds_left {
            format!(
                "Click Rev again within {seconds_left}s to stop and remove {}",
                self.target_id
            )
        } else if let Some(seconds_left) = cleanup_confirm_seconds_left {
            format!(
                "Click Clean again within {seconds_left}s to remove {}",
                self.target_id
            )
        } else if let Some(seconds_left) = input_forward_confirm_seconds_left {
            format!("Click Input again within {seconds_left}s to enable manual workspace input")
        } else if let Some(reason) = input_forwarding_wait_reason {
            format!("Input forwarding enabled, {reason}")
        } else if let Some(error) = &self.error {
            error.clone()
        } else if running {
            format!("{display} | {app_count} app(s) | {activity_label}")
        } else if self.snapshot.doctor_ready {
            format!("Ready | Profile: {selected_profile_label}")
        } else {
            self.snapshot
                .doctor_blockers
                .first()
                .cloned()
                .unwrap_or_else(|| "Runtime is not ready".to_string())
        };
        let header_detail = match workspace_position {
            Some((index, total)) if total > 1 => format!("{index}/{total} | {detail}"),
            _ => detail,
        };
        let footer_locked = revoke_armed
            || cleanup_armed
            || busy_action.is_some()
            || refreshing
            || self.error.is_some();
        let mut footer_text = if revoke_armed {
            let seconds_left = revoke_confirm_seconds_left.unwrap_or(1);
            format!(
                "Confirm revoke: click Rev again within {seconds_left}s to stop workspace and remove files"
            )
        } else if cleanup_armed {
            let seconds_left = cleanup_confirm_seconds_left.unwrap_or(1);
            format!(
                "Confirm cleanup: click Clean again within {seconds_left}s to remove stopped workspace files"
            )
        } else if let Some(seconds_left) = input_forward_confirm_seconds_left {
            format!(
                "Manual input is opt-in: click Input again within {seconds_left}s to forward clicks, scroll, keyboard, and host clipboard paste into the isolated workspace"
            )
        } else if let Some(reason) = input_forwarding_wait_reason {
            format!("Manual input forwarding is enabled but {reason}")
        } else if footer_locked {
            footer_activity_label(
                busy_action.as_ref(),
                refreshing,
                self.error.as_deref(),
                running,
                self.snapshot.doctor_ready,
                &self.snapshot.doctor_blockers,
                &selected_profile_label,
                self.latest_activity.as_ref(),
                self.active_window.as_ref(),
                workspace_apps,
                now,
            )
        } else {
            match self.footer_mode {
                FooterMode::Activity => footer_activity_label(
                    busy_action.as_ref(),
                    refreshing,
                    self.error.as_deref(),
                    running,
                    self.snapshot.doctor_ready,
                    &self.snapshot.doctor_blockers,
                    &selected_profile_label,
                    self.latest_activity.as_ref(),
                    self.active_window.as_ref(),
                    workspace_apps,
                    now,
                ),
                FooterMode::Task => footer_task_label(
                    selected,
                    running,
                    self.snapshot.doctor_ready,
                    &self.snapshot.doctor_blockers,
                    &selected_profile_label,
                    self.active_window.as_ref(),
                    workspace_apps,
                ),
                FooterMode::Isolation => footer_isolation_label(
                    selected,
                    self.snapshot.doctor_ready,
                    &self.snapshot.doctor_blockers,
                    &selected_profile_label,
                    &self.permissions,
                ),
                FooterMode::Apps => {
                    footer_apps_label(running, workspace_apps, self.active_window.as_ref())
                }
            }
        };
        if !matches!(control_mode, McpControlMode::Active) && !footer_locked {
            footer_text = format!("MCP {} | {footer_text}", control_mode.label());
        }
        if self.input_forwarding_enabled && input_forwarding_wait_reason.is_none() {
            footer_text = format!("Input RW | {footer_text}");
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .overflow_hidden()
            .rounded(px(10.0))
            .p(px(8.0))
            .gap(px(8.0))
            // Translucent graphite fill so the window blur reads through as
            // frosted glass; the silvery outer edge plus a brighter top edge
            // fakes a brushed-metal rim.
            .bg(rgba(BG_GLASS))
            .text_size(px(12.0))
            .text_color(rgb(TEXT))
            .font_family(UI_FONT)
            .border_1()
            .border_color(rgb(EDGE_SILVER))
            .cursor(CursorStyle::Arrow)
            .on_mouse_move(cx.listener(Self::update_interaction))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::end_interaction))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_interaction))
            // Faint brushed-metal highlight row hugging the inner top edge.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(rgba((EDGE_HIGHLIGHT << 8) | 0xb0)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .h(px(34.0))
                    .cursor(CursorStyle::OpenHand)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(1.0))
                            .min_w_0()
                            .cursor(CursorStyle::OpenHand)
                            .on_mouse_down(MouseButton::Left, on_window_move_start)
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .line_height(px(16.0))
                                    .truncate()
                                    .child(SharedString::from(title)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .line_height(px(14.0))
                                    .text_color(rgb(MUTED))
                                    .truncate()
                                    .child(SharedString::from(header_detail)),
                            ),
                    )
                    // Minimal top row: live-control segmented control, the
                    // primary Start/Stop, and a single "More" toggle. Everything
                    // else lives in the secondary cluster below.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(segmented_control(control_segments))
                            .child(if let Some(action) = &busy_action {
                                disabled_button_with_tooltip(
                                    "viewer-action",
                                    action.button_label(),
                                    Some(tooltip_text(action.label())),
                                )
                            } else if running {
                                danger_button_with_tooltip(
                                    "viewer-stop",
                                    "Stop",
                                    Some(tooltip_text(
                                        "Stop the workspace; keep files for inspection",
                                    )),
                                    on_stop,
                                )
                            } else {
                                button_with_tooltip(
                                    "viewer-start",
                                    "Start",
                                    Some(tooltip_text(format!("Start {selected_profile_label}"))),
                                    on_start,
                                )
                            })
                            .child(if self.show_more {
                                selected_button_with_tooltip(
                                    "viewer-more",
                                    "More ▴",
                                    Some(tooltip_text("Hide the secondary controls")),
                                    on_toggle_more,
                                )
                            } else {
                                button_with_tooltip(
                                    "viewer-more",
                                    "More ▾",
                                    Some(tooltip_text(
                                        "Show refresh, screen, capture, revoke, clean and artifact controls",
                                    )),
                                    on_toggle_more,
                                )
                            })
                            .child(button_with_tooltip(
                                "viewer-close",
                                "✕",
                                Some(tooltip_text("Close the viewer (the workspace keeps running)")),
                                |_event, _window, cx| cx.quit(),
                            )),
                    ),
            )
            // Secondary control cluster, revealed by the "More" toggle. Holds
            // every relocated action: refresh, the screen/Live toggle, capture,
            // revoke, clean, profile/workspace switching, and the artifact and
            // footer-mode buttons. No action is removed — only relocated here.
            .when(self.show_more, |panel| {
                panel.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(4.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(SURFACE))
                        .px(px(6.0))
                        .py(px(5.0))
                        .cursor(CursorStyle::Arrow)
                        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                            cx.stop_propagation();
                        })
                        .child(if busy_action.is_some() || refreshing {
                            disabled_button_with_tooltip(
                                "viewer-refresh",
                                "Refresh",
                                Some(tooltip_text("Refresh waits for the current action")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-refresh",
                                "Refresh",
                                Some(tooltip_text("Refresh workspace state now")),
                                on_refresh,
                            )
                        })
                        .child(if self.screen_stream {
                            selected_button_with_tooltip(
                                "viewer-live",
                                live_label,
                                Some(tooltip_text(live_tooltip)),
                                on_live,
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-live",
                                live_label,
                                Some(tooltip_text(live_tooltip)),
                                on_live,
                            )
                        })
                        .child(if !self.input_forwarding_allowed {
                            div().into_any_element()
                        } else if self.input_forwarding_enabled {
                            selected_button_with_tooltip(
                                "viewer-input-on",
                                "Input",
                                Some(tooltip_text(
                                    "Manual input forwarding is ON: clicks, drag release, scroll, keyboard, and host clipboard paste target the isolated workspace. Hover motion is not forwarded.",
                                )),
                                on_input,
                            )
                        } else if input_forward_confirm_seconds_left.is_some() {
                            danger_button_with_tooltip(
                                "viewer-input-confirm",
                                "Input?",
                                Some(tooltip_text(
                                    "Confirm opt-in: forward viewer clicks, scroll, keyboard, and host clipboard text into the isolated workspace only",
                                )),
                                on_input,
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-input",
                                "Input",
                                Some(tooltip_text(
                                    "Opt in to manual read-write control through this viewer, including host clipboard paste (requires a second click)",
                                )),
                                on_input,
                            )
                        })
                        .child(if let Some(label) = workspace_cycle_label {
                            if busy_action.is_some() || refreshing {
                                disabled_button_with_tooltip(
                                    "viewer-workspace",
                                    label,
                                    Some(tooltip_text("Workspace switching waits for refresh")),
                                )
                            } else {
                                button_with_tooltip(
                                    "viewer-workspace",
                                    label,
                                    Some(tooltip_text("Switch between known workspaces")),
                                    on_workspace,
                                )
                            }
                        } else {
                            div().into_any_element()
                        })
                        .child(if running || self.snapshot.profiles.is_empty() {
                            div().into_any_element()
                        } else if busy_action.is_some() {
                            disabled_button_with_tooltip(
                                "viewer-profile",
                                "Profile",
                                Some(tooltip_text(
                                    "Profile switching waits for the current action",
                                )),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-profile",
                                "Profile",
                                Some(tooltip_text("Cycle the profile used by Start")),
                                on_profile,
                            )
                        })
                        .child(if !running {
                            div().into_any_element()
                        } else if busy_action.is_some() || refreshing {
                            disabled_button_with_tooltip(
                                "viewer-capture",
                                "Shot",
                                Some(tooltip_text("Screenshot waits for the current action")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-capture",
                                "Shot",
                                Some(tooltip_text(
                                    "Save a screenshot of the active workspace window",
                                )),
                                on_capture,
                            )
                        })
                        .child(if !running {
                            div().into_any_element()
                        } else if busy_action.is_some() || refreshing {
                            disabled_button_with_tooltip(
                                "viewer-revoke",
                                "Rev",
                                Some(tooltip_text("Revoke waits for the current action")),
                            )
                        } else if revoke_armed {
                            danger_button_with_tooltip(
                                "viewer-revoke-confirm",
                                "Sure?",
                                Some(tooltip_text(
                                    "Confirm: stop workspace and remove runtime files",
                                )),
                                on_revoke,
                            )
                        } else {
                            danger_button_with_tooltip(
                                "viewer-revoke",
                                "Rev",
                                Some(tooltip_text(
                                    "Revoke: stop workspace and remove runtime files",
                                )),
                                on_revoke,
                            )
                        })
                        .child(if !has_workspace_artifacts || running {
                            div().into_any_element()
                        } else if busy_action.is_some() || refreshing {
                            disabled_button_with_tooltip(
                                "viewer-clean",
                                "Clean",
                                Some(tooltip_text("Cleanup waits for the current action")),
                            )
                        } else if cleanup_armed {
                            danger_button_with_tooltip(
                                "viewer-clean-confirm",
                                "Sure?",
                                Some(tooltip_text(
                                    "Confirm: remove stopped workspace runtime files",
                                )),
                                on_clean,
                            )
                        } else {
                            danger_button_with_tooltip(
                                "viewer-clean",
                                "Clean",
                                Some(tooltip_text("Remove stopped workspace runtime files")),
                                on_clean,
                            )
                        })
                        .child(if !has_workspace_artifacts {
                            div().into_any_element()
                        } else if footer_locked {
                            disabled_button_with_tooltip(
                                "viewer-artifacts",
                                "Files",
                                Some(tooltip_text("Artifact folder waits for the current state")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-artifacts",
                                "Files",
                                Some(tooltip_text("Open the workspace artifact folder")),
                                on_artifacts,
                            )
                        })
                        .child(if !has_workspace_artifacts {
                            div().into_any_element()
                        } else if footer_locked {
                            disabled_button_with_tooltip(
                                "viewer-events",
                                "Evt",
                                Some(tooltip_text("Event log waits for the current state")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-events",
                                "Evt",
                                Some(tooltip_text("Open the workspace event log")),
                                on_events,
                            )
                        })
                        .child(if app_log_target.is_none() {
                            div().into_any_element()
                        } else if footer_locked {
                            disabled_button_with_tooltip(
                                "viewer-app-log",
                                "Log",
                                Some(tooltip_text("App log waits for the current state")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-app-log",
                                "Log",
                                Some(tooltip_text("Open the active app log")),
                                on_app_log,
                            )
                        })
                        .child(if footer_locked {
                            disabled_button_with_tooltip(
                                "viewer-footer-mode",
                                self.footer_mode.label(),
                                Some(tooltip_text("Footer mode waits for the current state")),
                            )
                        } else {
                            button_with_tooltip(
                                "viewer-footer-mode",
                                self.footer_mode.label(),
                                Some(tooltip_text("Cycle footer: activity, task, isolation, apps")),
                                on_footer_mode,
                            )
                        }),
                )
            })
            // Screen view: spans the full content width of the panel and fills
            // the remaining vertical space. The frame image covers the box
            // edge-to-edge (no letterbox inset) while preserving aspect.
            .child(track_bounds(
                div()
                    .flex()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .relative()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(0x0b0d10))
                    .overflow_hidden()
                    .cursor(if self.input_forwarding_ready() {
                        CursorStyle::Crosshair
                    } else {
                        CursorStyle::Arrow
                    })
                    .track_focus(&self.focus_handle)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_input_forward))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::begin_input_forward))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::begin_input_forward))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::end_input_forward))
                    .on_mouse_up(MouseButton::Right, cx.listener(Self::end_input_forward))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::end_input_forward))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_input_forward))
                    .on_mouse_up_out(MouseButton::Right, cx.listener(Self::end_input_forward))
                    .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::end_input_forward))
                    // Do not forward hover-only mouse motion. In practice that
                    // can emit dozens of workspace IPC requests per second and
                    // make human handoff feel noisy/laggy. Clicks still move the
                    // workspace pointer to the target point, and
                    // drag gestures are replayed on release.
                    .on_scroll_wheel(cx.listener(Self::forward_scroll))
                    .capture_key_down(cx.listener(Self::forward_key_down))
                    .child(match image {
                        Some(image) => img(image)
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                        None => div()
                            .px(px(14.0))
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(if running {
                                "Screen stream off"
                            } else {
                                "No running workspace yet"
                            })
                            .into_any_element(),
                    }),
                self.screen_bounds.clone(),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .px(px(6.0))
                    .py(px(4.0))
                    .cursor(CursorStyle::Arrow)
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.0))
                            .line_height(px(14.0))
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(SharedString::from(footer_text)),
                    )
                    // Footer right side: just the running status light (no text
                    // banner). The artifact/footer-mode buttons moved into the
                    // "More" cluster above.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .pl(px(8.0))
                            .child(status_light(running_status_color(
                                running,
                                control_mode,
                                self.error.is_some(),
                            ))),
                    ),
            )
            // Resize hit-zones on every edge and corner. Edges are thin strips;
            // corners are small squares layered on top so they win the overlap.
            .child(resize_edge_zone(
                "viewer-resize-top",
                ResizeEdge::Top,
                CursorStyle::ResizeUpDown,
                on_resize_top,
            ))
            .child(resize_edge_zone(
                "viewer-resize-bottom",
                ResizeEdge::Bottom,
                CursorStyle::ResizeUpDown,
                on_resize_bottom,
            ))
            .child(resize_edge_zone(
                "viewer-resize-left",
                ResizeEdge::Left,
                CursorStyle::ResizeLeftRight,
                on_resize_left,
            ))
            .child(resize_edge_zone(
                "viewer-resize-right",
                ResizeEdge::Right,
                CursorStyle::ResizeLeftRight,
                on_resize_right,
            ))
            .child(resize_edge_zone(
                "viewer-resize-top-left",
                ResizeEdge::TopLeft,
                CursorStyle::ResizeUpLeftDownRight,
                on_resize_top_left,
            ))
            .child(resize_edge_zone(
                "viewer-resize-top-right",
                ResizeEdge::TopRight,
                CursorStyle::ResizeUpRightDownLeft,
                on_resize_top_right,
            ))
            .child(resize_edge_zone(
                "viewer-resize-bottom-left",
                ResizeEdge::BottomLeft,
                CursorStyle::ResizeUpRightDownLeft,
                on_resize_bottom_left,
            ))
            .child(resize_edge_zone(
                "viewer-resize-bottom-right",
                ResizeEdge::BottomRight,
                CursorStyle::ResizeUpLeftDownRight,
                on_resize_bottom_right,
            ))
    }
}

pub fn run(options: ViewerOptions) -> Result<()> {
    let backend = prepare_viewer_backend();
    let _instance_guard = match acquire_viewer_instance(&options, backend) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            eprintln!(
                "agent workspace viewer is already running for workspace {} ({})",
                options.id,
                backend.launch_label(options.always_on_top)
            );
            return Ok(());
        }
        Err(error) => {
            eprintln!("failed to register agent workspace viewer instance: {error}");
            None
        }
    };
    application().run(move |cx: &mut App| {
        if backend == ViewerBackend::X11Popup {
            let popup_options = options.clone();
            cx.open_window(
                x11_window_options(cx),
                move |window, cx| {
                    window.set_app_id(VIEWER_APP_ID);
                    cx.new(|cx| AgentWorkspaceViewer::new(popup_options.clone(), cx))
                },
            )
            .expect("failed to open agent workspace viewer");
            spawn_x11_overlay_hint_task(options.always_on_top);
            return;
        }

        if !options.always_on_top {
            let normal_options = options.clone();
            cx.open_window(normal_window_options(cx), move |window, cx| {
                window.set_app_id(VIEWER_APP_ID);
                cx.new(|cx| AgentWorkspaceViewer::new(normal_options.clone(), cx))
            })
            .expect("failed to open agent workspace viewer");
            return;
        }

        let layer_options = options.clone();
        if let Err(error) = cx.open_window(layer_shell_window_options(options.input_forwarding), move |window, cx| {
            window.set_app_id(VIEWER_APP_ID);
            cx.new(|cx| AgentWorkspaceViewer::new(layer_options.clone(), cx))
        }) {
            match maybe_spawn_x11_replacement(&options) {
                Ok(true) => {
                    eprintln!(
                        "layer-shell overlay unavailable ({error}); relaunched viewer through X11"
                    );
                    cx.quit();
                    return;
                }
                Ok(false) => {}
                Err(relaunch_error) => {
                    eprintln!(
                        "layer-shell overlay unavailable ({error}); X11 relaunch failed: {relaunch_error}"
                    );
                }
            }
            eprintln!(
                "layer-shell overlay unavailable ({error}); falling back to undecorated popup"
            );
            let fallback_options = options.clone();
            cx.open_window(
                x11_window_options(cx),
                move |window, cx| {
                    window.set_app_id(VIEWER_APP_ID);
                    cx.new(|cx| AgentWorkspaceViewer::new(fallback_options.clone(), cx))
                },
            )
            .expect("failed to open agent workspace viewer");
            spawn_x11_overlay_hint_task(options.always_on_top);
        }
    });
    Ok(())
}

fn prepare_viewer_backend() -> ViewerBackend {
    let backend = preferred_viewer_backend();
    if backend == ViewerBackend::X11Popup {
        env::remove_var("WAYLAND_DISPLAY");
        env::set_var(VIEWER_BACKEND_ENV, VIEWER_BACKEND_X11);
    }
    backend
}

fn preferred_viewer_backend() -> ViewerBackend {
    match env::var(VIEWER_BACKEND_ENV)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        VIEWER_BACKEND_X11 => return ViewerBackend::X11Popup,
        VIEWER_BACKEND_WAYLAND => return ViewerBackend::WaylandLayerShell,
        _ => {}
    }

    if should_launch_viewer_x11() {
        ViewerBackend::X11Popup
    } else {
        ViewerBackend::WaylandLayerShell
    }
}

fn should_launch_viewer_x11() -> bool {
    let has_wayland = env::var_os("WAYLAND_DISPLAY").is_some_and(|display| !display.is_empty());
    let has_x11 = env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    if !has_x11 {
        return false;
    }
    !has_wayland || is_gnome_desktop()
}

fn is_gnome_desktop() -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "GDMSESSION",
    ]
    .iter()
    .filter_map(|name| env::var(name).ok())
    .any(|value| value.to_ascii_lowercase().contains("gnome"))
}

fn layer_shell_window_options(input_forwarding: bool) -> WindowOptions {
    let margin = px(OVERLAY_MARGIN);
    let preferences = load_viewer_preferences();
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(overlay_size_bounds(&preferences))),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::LayerShell(LayerShellOptions {
            namespace: VIEWER_APP_ID.to_string(),
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::RIGHT,
            margin: Some((margin, margin, px(0.0), px(0.0))),
            keyboard_interactivity: if input_forwarding {
                KeyboardInteractivity::OnDemand
            } else {
                KeyboardInteractivity::None
            },
            ..Default::default()
        }),
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        app_id: Some(VIEWER_APP_ID.to_string()),
        window_min_size: Some(size(px(OVERLAY_MIN_WIDTH), px(OVERLAY_MIN_HEIGHT))),
        // Frosted glass: blur the content behind the translucent panel. The
        // Wayland layer-shell path supports this on compositors with a blur
        // protocol; where unsupported it degrades to plain transparency and the
        // translucent graphite fill still reads as a premium surface.
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    }
}

fn maybe_spawn_x11_replacement(options: &ViewerOptions) -> Result<bool> {
    let already_x11 = env::var(VIEWER_BACKEND_ENV)
        .map(|backend| backend.eq_ignore_ascii_case(VIEWER_BACKEND_X11))
        .unwrap_or(false);
    let has_x11 = env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    if already_x11 || !has_x11 {
        return Ok(false);
    }

    let executable = env::current_exe().context("failed to resolve current executable")?;
    let permissions_json = serde_json::to_string(&options.permissions)
        .context("failed to serialize viewer permissions")?;
    let args = viewer_command_args(options);
    let mut command = Command::new(&executable);
    command
        .args(&args)
        .env(VIEWER_BACKEND_ENV, VIEWER_BACKEND_X11)
        .env(VIEWER_PERMISSIONS_ENV, permissions_json)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_reaped_child(&mut command)
        .with_context(|| format!("failed to relaunch {} through X11", executable.display()))?;
    Ok(true)
}

fn normal_window_options(cx: &App) -> WindowOptions {
    let preferences = load_viewer_preferences();
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(overlay_bounds(cx, &preferences))),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        is_minimizable: true,
        app_id: Some(VIEWER_APP_ID.to_string()),
        window_min_size: Some(size(px(OVERLAY_MIN_WIDTH), px(OVERLAY_MIN_HEIGHT))),
        // Frosted glass where the compositor supports blur; otherwise it falls
        // back to plain transparency over the translucent graphite panel.
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    }
}

fn x11_window_options(cx: &App) -> WindowOptions {
    let preferences = load_viewer_preferences();
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(overlay_bounds(cx, &preferences))),
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        app_id: Some(VIEWER_APP_ID.to_string()),
        window_min_size: Some(size(px(OVERLAY_MIN_WIDTH), px(OVERLAY_MIN_HEIGHT))),
        // X11 popups generally cannot blur the backdrop, so this requests blur
        // but in practice degrades to plain transparency; the translucent
        // graphite panel keeps the premium frosted-metal look on this backend.
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    }
}

fn overlay_size_bounds(preferences: &ViewerPreferences) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.0), px(0.0)),
        size: overlay_size(preferences),
    }
}

fn overlay_bounds(cx: &App, preferences: &ViewerPreferences) -> Bounds<Pixels> {
    let overlay_size = overlay_size(preferences);
    let margin = px(OVERLAY_MARGIN);

    cx.primary_display()
        .map(|display| {
            let visible = display.visible_bounds();
            if let Some(origin) = preferred_overlay_origin(cx, preferences, overlay_size) {
                return Bounds {
                    origin,
                    size: overlay_size,
                };
            }
            let max_x_offset = (visible.size.width - overlay_size.width - margin).max(margin);
            Bounds {
                origin: point(visible.origin.x + max_x_offset, visible.origin.y + margin),
                size: overlay_size,
            }
        })
        .unwrap_or_else(|| Bounds::centered(None, overlay_size, cx))
}

fn preferred_overlay_origin(
    cx: &App,
    preferences: &ViewerPreferences,
    overlay_size: Size<Pixels>,
) -> Option<Point<Pixels>> {
    let x = preferences.x?;
    let y = preferences.y?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }

    let visible = cx
        .displays()
        .into_iter()
        .map(|display| display.visible_bounds())
        .find(|visible| point_is_in_bounds(x, y, *visible))
        .or_else(|| cx.primary_display().map(|display| display.visible_bounds()))?;
    Some(clamp_overlay_origin_to_bounds(x, y, overlay_size, visible))
}

fn point_is_in_bounds(x: f32, y: f32, bounds: Bounds<Pixels>) -> bool {
    let left = bounds.origin.x.as_f32();
    let top = bounds.origin.y.as_f32();
    let right = left + bounds.size.width.as_f32();
    let bottom = top + bounds.size.height.as_f32();
    x >= left && x <= right && y >= top && y <= bottom
}

fn clamp_overlay_origin_to_bounds(
    x: f32,
    y: f32,
    overlay_size: Size<Pixels>,
    bounds: Bounds<Pixels>,
) -> Point<Pixels> {
    let left = bounds.origin.x.as_f32();
    let top = bounds.origin.y.as_f32();
    let max_x = (left + bounds.size.width.as_f32() - overlay_size.width.as_f32()).max(left);
    let max_y = (top + bounds.size.height.as_f32() - overlay_size.height.as_f32()).max(top);
    point(px(x.clamp(left, max_x)), px(y.clamp(top, max_y)))
}

fn overlay_size(preferences: &ViewerPreferences) -> Size<Pixels> {
    let preferences = normalize_viewer_preferences(preferences.clone());
    size(px(preferences.width), px(preferences.height))
}

fn normalize_viewer_preferences(mut preferences: ViewerPreferences) -> ViewerPreferences {
    if preferences.width.is_finite() {
        preferences.width = preferences.width.max(OVERLAY_MIN_WIDTH);
    } else {
        preferences.width = OVERLAY_WIDTH;
    }
    if preferences.height.is_finite() {
        preferences.height = preferences.height.max(OVERLAY_MIN_HEIGHT);
    } else {
        preferences.height = OVERLAY_HEIGHT;
    }
    preferences.x = preferences.x.filter(|x| x.is_finite());
    preferences.y = preferences.y.filter(|y| y.is_finite());
    preferences
}

fn load_viewer_preferences() -> ViewerPreferences {
    load_viewer_preferences_from_path(&viewer_preferences_path())
}

fn load_viewer_preferences_from_path(path: &Path) -> ViewerPreferences {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<ViewerPreferences>(&content).ok())
        .map(normalize_viewer_preferences)
        .unwrap_or_default()
}

fn save_viewer_preferences(preferences: &ViewerPreferences) -> Result<()> {
    save_viewer_preferences_to_path(&viewer_preferences_path(), preferences)
}

fn save_viewer_preferences_to_path(path: &Path, preferences: &ViewerPreferences) -> Result<()> {
    let preferences = normalize_viewer_preferences(preferences.clone());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(&preferences)
        .context("failed to serialize viewer preferences")?;
    let temp_path = viewer_preferences_temp_path(path);
    fs::write(&temp_path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

fn viewer_preferences_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(VIEWER_PREFERENCES_FILE);
    path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()))
}

fn viewer_preferences_path() -> PathBuf {
    viewer_config_dir().join(VIEWER_PREFERENCES_FILE)
}

fn viewer_config_dir() -> PathBuf {
    viewer_config_dir_from_env(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn viewer_config_dir_from_env(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    xdg_config_home
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agent-workspace-linux")
}

fn viewer_registry_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("agent-workspace-linux")
        .join(VIEWER_REGISTRY_DIR)
}

fn viewer_registry_path(
    id: &str,
    backend: ViewerBackend,
    always_on_top: bool,
    input_forwarding: bool,
    exit_when_workspace_gone: bool,
) -> PathBuf {
    let mode = if always_on_top { "topmost" } else { "normal" };
    let input = if input_forwarding { "rw" } else { "ro" };
    let lifecycle = if exit_when_workspace_gone {
        "bound"
    } else {
        "free"
    };
    viewer_registry_dir().join(format!(
        "{}-{}-{mode}-{input}-{lifecycle}.json",
        sanitize_viewer_registry_component(id),
        backend.launch_label(always_on_top)
    ))
}

fn sanitize_viewer_registry_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => character,
            _ => '_',
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

#[allow(clippy::too_many_arguments)]
fn viewer_registry_entry(
    id: &str,
    pid: u32,
    backend: ViewerBackend,
    always_on_top: bool,
    input_forwarding: bool,
    exit_when_workspace_gone: bool,
    executable: PathBuf,
    command: Vec<String>,
) -> ViewerRegistryEntry {
    ViewerRegistryEntry {
        schema: VIEWER_REGISTRY_SCHEMA.to_string(),
        id: id.to_string(),
        pid,
        backend: backend.launch_label(always_on_top).to_string(),
        always_on_top,
        input_forwarding,
        exit_when_workspace_gone,
        executable,
        command,
        opened_at_unix: wall_clock_seconds(),
    }
}

fn write_viewer_registry_entry(path: &Path, entry: &ViewerRegistryEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(entry).context("failed to serialize viewer registry entry")?;
    let temp_path = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("viewer-instance.json"),
        std::process::id()
    ));
    fs::write(&temp_path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

fn read_viewer_registry_entry(path: &Path) -> Option<ViewerRegistryEntry> {
    let bytes = fs::read(path).ok()?;
    let entry = serde_json::from_slice::<ViewerRegistryEntry>(&bytes).ok()?;
    (entry.schema == VIEWER_REGISTRY_SCHEMA).then_some(entry)
}

fn viewer_registry_entries() -> Vec<(PathBuf, ViewerRegistryEntry)> {
    let Ok(entries) = fs::read_dir(viewer_registry_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| read_viewer_registry_entry(&path).map(|entry| (path, entry)))
        .collect()
}

pub fn list_viewers() -> Result<ViewerList> {
    let registry_dir = viewer_registry_dir();
    let mut viewers = viewer_registry_entries()
        .into_iter()
        .map(|(registry_path, entry)| ViewerListEntry {
            id: entry.id.clone(),
            viewer_id: entry.id.clone(),
            pid: entry.pid,
            backend: entry.backend.clone(),
            always_on_top: entry.always_on_top,
            input_forwarding: entry.input_forwarding,
            exit_when_workspace_gone: entry.exit_when_workspace_gone,
            executable: entry.executable.clone(),
            command: entry.command.clone(),
            opened_at_unix: entry.opened_at_unix,
            alive: viewer_registry_entry_is_alive(&entry),
            registry_path,
        })
        .collect::<Vec<_>>();
    viewers.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.backend.cmp(&right.backend))
            .then(left.pid.cmp(&right.pid))
    });
    Ok(ViewerList {
        registry_dir,
        viewers,
    })
}

pub fn close_viewers(id: Option<String>, all: bool, dry_run: bool) -> Result<ViewerClose> {
    if id.is_some() && all {
        bail!("viewer close accepts either id or all=true, not both");
    }
    if id.is_none() && !all {
        bail!("viewer close requires id or all=true");
    }

    let registry_dir = viewer_registry_dir();
    let mut close = ViewerClose {
        registry_dir,
        dry_run,
        target_id: id.clone(),
        all,
        candidates: Vec::new(),
        closed: Vec::new(),
        skipped: Vec::new(),
    };

    for (registry_path, entry) in viewer_registry_entries() {
        if id.as_ref().is_some_and(|target_id| target_id != &entry.id) {
            continue;
        }
        close_viewer_registry_entry(registry_path, entry, dry_run, &mut close);
    }

    Ok(close)
}

fn close_viewer_registry_entry(
    registry_path: PathBuf,
    entry: ViewerRegistryEntry,
    dry_run: bool,
    close: &mut ViewerClose,
) {
    let close_entry = |reason: String| ViewerCloseEntry {
        id: entry.id.clone(),
        viewer_id: entry.id.clone(),
        pid: entry.pid,
        backend: entry.backend.clone(),
        always_on_top: entry.always_on_top,
        input_forwarding: entry.input_forwarding,
        exit_when_workspace_gone: entry.exit_when_workspace_gone,
        registry_path: registry_path.clone(),
        reason,
    };

    if !viewer_registry_entry_is_alive(&entry) {
        if dry_run {
            close
                .skipped
                .push(close_entry("stale viewer registry entry".to_string()));
        } else {
            let _ = fs::remove_file(&registry_path);
            close.skipped.push(close_entry(
                "removed stale viewer registry entry".to_string(),
            ));
        }
        return;
    }

    if dry_run {
        close.candidates.push(close_entry(
            "would send SIGTERM to registered viewer".to_string(),
        ));
        return;
    }

    match signal_viewer_process(entry.pid, SIGTERM) {
        Ok(()) => {
            if wait_for_viewer_exit(entry.pid, Duration::from_millis(1_500)) {
                let _ = fs::remove_file(&registry_path);
                close
                    .closed
                    .push(close_entry("sent SIGTERM and viewer exited".to_string()));
            } else {
                close.skipped.push(close_entry(
                    "sent SIGTERM but viewer process was still alive".to_string(),
                ));
            }
        }
        Err(error) => close.skipped.push(close_entry(format!(
            "failed to signal viewer process: {error}"
        ))),
    }
}

fn existing_viewer_launch(
    id: &str,
    backend: ViewerBackend,
    always_on_top: bool,
    input_forwarding: bool,
    exit_when_workspace_gone: bool,
) -> Option<ViewerLaunch> {
    let path = viewer_registry_path(
        id,
        backend,
        always_on_top,
        input_forwarding,
        exit_when_workspace_gone,
    );
    let entry = read_viewer_registry_entry(&path)?;
    if viewer_registry_entry_is_alive(&entry) {
        return Some(viewer_launch_from_registry_entry(path, entry));
    }
    let _ = fs::remove_file(path);
    None
}

fn existing_viewer_launch_for_id(id: &str) -> Option<ViewerLaunch> {
    let mut live_entries = Vec::new();
    for (path, entry) in viewer_registry_entries() {
        if entry.id != id {
            continue;
        }
        if viewer_registry_entry_is_alive(&entry) {
            live_entries.push((path, entry));
        } else {
            let _ = fs::remove_file(path);
        }
    }
    best_viewer_launch_for_id(id, live_entries)
}

fn best_viewer_launch_for_id(
    id: &str,
    mut entries: Vec<(PathBuf, ViewerRegistryEntry)>,
) -> Option<ViewerLaunch> {
    entries.retain(|(_, entry)| entry.id == id);
    entries.sort_by(|left, right| {
        right
            .1
            .exit_when_workspace_gone
            .cmp(&left.1.exit_when_workspace_gone)
            .then(left.1.always_on_top.cmp(&right.1.always_on_top))
            .then(left.1.opened_at_unix.cmp(&right.1.opened_at_unix))
            .then(left.1.pid.cmp(&right.1.pid))
    });
    entries
        .into_iter()
        .next()
        .map(|(path, entry)| viewer_launch_from_registry_entry(path, entry))
}

fn viewer_launch_from_registry_entry(path: PathBuf, entry: ViewerRegistryEntry) -> ViewerLaunch {
    ViewerLaunch {
        viewer_id: entry.id.clone(),
        id: entry.id,
        pid: entry.pid,
        backend: entry.backend,
        always_on_top: entry.always_on_top,
        input_forwarding: entry.input_forwarding,
        exit_when_workspace_gone: entry.exit_when_workspace_gone,
        executable: entry.executable,
        command: entry.command,
        reused: true,
        registry_path: Some(path),
    }
}

fn acquire_viewer_instance(
    options: &ViewerOptions,
    backend: ViewerBackend,
) -> Result<Option<ViewerInstanceGuard>> {
    if let Some(existing) = existing_viewer_launch(
        &options.id,
        backend,
        options.always_on_top,
        options.input_forwarding,
        options.exit_when_workspace_gone,
    ) {
        if existing.pid != std::process::id() {
            return Ok(None);
        }
    }

    let path = viewer_registry_path(
        &options.id,
        backend,
        options.always_on_top,
        options.input_forwarding,
        options.exit_when_workspace_gone,
    );
    let executable = env::current_exe().context("failed to resolve current executable")?;
    let command = env::args().collect::<Vec<_>>();
    let pid = std::process::id();
    let entry = viewer_registry_entry(
        &options.id,
        pid,
        backend,
        options.always_on_top,
        options.input_forwarding,
        options.exit_when_workspace_gone,
        executable,
        command,
    );
    write_viewer_registry_entry(&path, &entry)?;
    Ok(Some(ViewerInstanceGuard { path, pid }))
}

fn viewer_registry_entry_is_alive(entry: &ViewerRegistryEntry) -> bool {
    viewer_process_matches_entry(entry)
}

fn viewer_process_matches_entry(entry: &ViewerRegistryEntry) -> bool {
    let cmdline = match viewer_process_cmdline(entry.pid) {
        Some(cmdline) => cmdline,
        None => return false,
    };
    if !viewer_cmdline_matches(
        &entry.id,
        entry.always_on_top,
        entry.input_forwarding,
        entry.exit_when_workspace_gone,
        &cmdline,
    ) {
        return false;
    }
    cmdline
        .first()
        .is_some_and(|program| viewer_executable_matches(program, &entry.executable))
}

fn viewer_executable_matches(program: &str, executable: &Path) -> bool {
    let program = Path::new(program);
    if program == executable {
        return true;
    }
    program
        .file_name()
        .zip(executable.file_name())
        .is_some_and(|(program_name, executable_name)| program_name == executable_name)
}

fn viewer_process_cmdline(pid: u32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .filter_map(|arg| String::from_utf8(arg.to_vec()).ok())
        .collect::<Vec<_>>();
    (!args.is_empty()).then_some(args)
}

fn signal_viewer_process(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

fn viewer_pid_exists(pid: u32) -> bool {
    match signal_viewer_process(pid, 0) {
        Ok(()) => true,
        Err(error) => error.raw_os_error() != Some(ESRCH),
    }
}

fn wait_for_viewer_exit(pid: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !viewer_pid_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !viewer_pid_exists(pid)
}

fn viewer_cmdline_matches(
    id: &str,
    always_on_top: bool,
    input_forwarding: bool,
    exit_when_workspace_gone: bool,
    args: &[String],
) -> bool {
    if !args.iter().any(|arg| arg == "viewer") {
        return false;
    }
    if args.iter().any(|arg| arg == "--always-on-top") != always_on_top {
        return false;
    }
    if args.iter().any(|arg| arg == "--input-forwarding") != input_forwarding {
        return false;
    }
    if args.iter().any(|arg| arg == "--exit-when-workspace-gone") != exit_when_workspace_gone {
        return false;
    }
    let mut explicit_id = None;
    for pair in args.windows(2) {
        if pair[0] == "--id" {
            explicit_id = Some(pair[1].as_str());
            break;
        }
    }
    match explicit_id {
        Some(arg_id) => arg_id == id,
        None => id == workspace::default_workspace_id(),
    }
}

impl Drop for ViewerInstanceGuard {
    fn drop(&mut self) {
        let Some(entry) = read_viewer_registry_entry(&self.path) else {
            return;
        };
        if entry.pid == self.pid {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn open_viewer(
    id: Option<String>,
    permissions: &McpPermissionState,
    always_on_top: bool,
    input_forwarding: bool,
) -> Result<ViewerLaunch> {
    let id = id.unwrap_or_else(workspace::default_workspace_id);
    let options = ViewerOptions {
        id: id.clone(),
        permissions: permissions.clone(),
        always_on_top,
        input_forwarding,
        exit_when_workspace_gone: true,
        background: false,
    };
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let args = viewer_command_args(&options);
    let backend = preferred_viewer_backend();
    if let Some(existing) =
        existing_viewer_launch(&id, backend, always_on_top, input_forwarding, true)
    {
        return Ok(existing);
    }
    if !input_forwarding {
        if let Some(existing) = existing_viewer_launch_for_id(&id) {
            return Ok(existing);
        }
    }
    let mut command = Command::new(&executable);
    let permissions_json =
        serde_json::to_string(permissions).context("failed to serialize viewer permissions")?;
    command
        .args(&args)
        .env(VIEWER_PERMISSIONS_ENV, permissions_json)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match backend {
        ViewerBackend::X11Popup => {
            command.env(VIEWER_BACKEND_ENV, VIEWER_BACKEND_X11);
            command.env_remove("WAYLAND_DISPLAY");
        }
        ViewerBackend::WaylandLayerShell => {
            command.env(VIEWER_BACKEND_ENV, VIEWER_BACKEND_WAYLAND);
        }
    }
    let pid = spawn_reaped_child(&mut command)
        .with_context(|| format!("failed to launch {}", executable.display()))?;
    let command = std::iter::once(executable.display().to_string())
        .chain(args)
        .collect::<Vec<_>>();
    let registry_path = viewer_registry_path(&id, backend, always_on_top, input_forwarding, true);
    let entry = viewer_registry_entry(
        &id,
        pid,
        backend,
        always_on_top,
        input_forwarding,
        true,
        executable.clone(),
        command.clone(),
    );
    if let Err(error) = write_viewer_registry_entry(&registry_path, &entry) {
        eprintln!("failed to record agent workspace viewer instance: {error}");
    }
    Ok(ViewerLaunch {
        viewer_id: id.clone(),
        id,
        pid,
        backend: backend.launch_label(always_on_top).to_string(),
        always_on_top,
        input_forwarding,
        exit_when_workspace_gone: true,
        executable,
        command,
        reused: false,
        registry_path: Some(registry_path),
    })
}

fn viewer_command_args(options: &ViewerOptions) -> Vec<String> {
    let mut args = vec!["viewer".to_string(), "--id".to_string(), options.id.clone()];
    if options.always_on_top {
        args.push("--always-on-top".to_string());
    }
    if options.exit_when_workspace_gone {
        args.push("--exit-when-workspace-gone".to_string());
    }
    if options.input_forwarding {
        args.push("--input-forwarding".to_string());
    }
    if options.background {
        args.push("--background".to_string());
    }
    args
}

fn spawn_reaped_child(command: &mut Command) -> std::io::Result<u32> {
    let child = command.spawn()?;
    let pid = child.id();
    reap_child_in_background(child);
    Ok(pid)
}

fn reap_child_in_background(mut child: Child) {
    let pid = child.id();
    let _ = std::thread::Builder::new()
        .name(format!("agent-workspace-child-{pid}-reaper"))
        .spawn(move || {
            let _ = child.wait();
        });
}

fn capture_screenshot(id: &str) -> Result<ViewerFrame> {
    let response =
        workspace::screenshot_for_viewer_stream(id, Some(PathBuf::from(VIEWER_FRAME_FILE)))?;
    let screenshot = response
        .screenshot
        .context("workspace screenshot response did not include screenshot metadata")?;
    let bytes = fs::read(&screenshot.path)
        .with_context(|| format!("failed to read {}", screenshot.path.display()))?;
    let img = image::load_from_memory(&bytes)?.to_rgba8();
    let (width, height) = img.dimensions();
    let mut bgra = img.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer: ImageBuffer<image::Rgba<u8>, _> =
        ImageBuffer::from_raw(width, height, bgra).context("invalid screenshot buffer")?;
    let frame = Frame::new(buffer);
    Ok(ViewerFrame {
        image: Arc::new(RenderImage::new(vec![frame])),
        width,
        height,
    })
}

fn spawn_x11_overlay_hint_task(always_on_top: bool) {
    if env::var(VIEWER_BACKEND_ENV)
        .map(|backend| !backend.eq_ignore_ascii_case(VIEWER_BACKEND_X11))
        .unwrap_or(false)
    {
        return;
    }

    std::thread::spawn(move || {
        let mut applied = false;
        for _ in 0..80 {
            match apply_x11_overlay_hints(always_on_top) {
                Ok(true) => applied = true,
                Ok(false) => {}
                Err(error) => {
                    eprintln!("failed to apply X11 viewer overlay hints: {error}");
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !applied {
            eprintln!("failed to find X11 viewer window for overlay hints");
        }
    });
}

fn apply_x11_overlay_hints(always_on_top: bool) -> Result<bool> {
    let (connection, screen_index) = x11rb::connect(None).context("failed to connect to X11")?;
    let root = connection
        .setup()
        .roots
        .get(screen_index)
        .context("X11 screen not found")?
        .root;
    let pid_atom = intern_x11_atom(&connection, b"_NET_WM_PID")
        .context("failed to resolve _NET_WM_PID atom")?;
    let Some(window) = find_x11_viewer_window(&connection, root, pid_atom, std::process::id(), 0)
    else {
        return Ok(false);
    };

    set_x11_overlay_hints(&connection, root, window, always_on_top)?;
    connection.flush()?;
    Ok(true)
}

fn x11_viewer_state_atom_names(always_on_top: bool) -> Vec<&'static [u8]> {
    let mut atoms = Vec::new();
    if always_on_top {
        atoms.push(b"_NET_WM_STATE_ABOVE".as_slice());
        atoms.push(b"_NET_WM_STATE_STICKY".as_slice());
    }
    atoms.push(b"_NET_WM_STATE_SKIP_TASKBAR".as_slice());
    atoms.push(b"_NET_WM_STATE_SKIP_PAGER".as_slice());
    atoms
}

fn x11_viewer_window_type_atom_names(always_on_top: bool) -> Vec<&'static [u8]> {
    let mut atoms = Vec::new();
    if always_on_top {
        atoms.push(b"_NET_WM_WINDOW_TYPE_NOTIFICATION".as_slice());
    }
    atoms.push(b"_NET_WM_WINDOW_TYPE_UTILITY".as_slice());
    atoms
}

fn x11_atom_label(atom_name: &[u8]) -> String {
    String::from_utf8_lossy(atom_name).into_owned()
}

fn set_x11_overlay_hints(
    connection: &RustConnection,
    root: X11Window,
    window: X11Window,
    always_on_top: bool,
) -> Result<()> {
    let state_atom = intern_x11_atom(connection, b"_NET_WM_STATE")
        .context("failed to resolve _NET_WM_STATE atom")?;
    let state_values = x11_viewer_state_atom_names(always_on_top)
        .iter()
        .map(|atom_name| {
            intern_x11_atom(connection, atom_name)
                .with_context(|| format!("failed to resolve {} atom", x11_atom_label(atom_name)))
        })
        .collect::<Result<Vec<_>>>()?;
    connection.change_property32(
        PropMode::REPLACE,
        window,
        state_atom,
        AtomEnum::ATOM,
        &state_values,
    )?;
    for &state in &state_values {
        request_x11_wm_state(connection, root, window, state_atom, state)?;
    }

    let window_type_atom = intern_x11_atom(connection, b"_NET_WM_WINDOW_TYPE")
        .context("failed to resolve _NET_WM_WINDOW_TYPE atom")?;
    let window_type_values = x11_viewer_window_type_atom_names(always_on_top)
        .iter()
        .map(|atom_name| {
            intern_x11_atom(connection, atom_name)
                .with_context(|| format!("failed to resolve {} atom", x11_atom_label(atom_name)))
        })
        .collect::<Result<Vec<_>>>()?;
    connection.change_property32(
        PropMode::REPLACE,
        window,
        window_type_atom,
        AtomEnum::ATOM,
        &window_type_values,
    )?;
    if always_on_top {
        connection.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
    }
    Ok(())
}

fn request_x11_wm_state(
    connection: &RustConnection,
    root: X11Window,
    window: X11Window,
    state_atom: u32,
    requested_state: u32,
) -> Result<()> {
    let data = ClientMessageData::from([1, requested_state, 0, 1, 0]);
    let event = ClientMessageEvent {
        response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window,
        type_: state_atom,
        data,
    };
    connection.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    Ok(())
}

impl X11DragSession {
    fn apply(&self, kind: DragKind) -> Result<bool> {
        let (pointer, button_down) = self.pointer()?;

        let delta_x = pointer.x - self.start_pointer.x;
        let delta_y = pointer.y - self.start_pointer.y;
        match kind {
            DragKind::Move => {
                self.move_to(self.start_bounds.x + delta_x, self.start_bounds.y + delta_y)?;
            }
            DragKind::Resize(edge) => {
                // X11 popups can be reconfigured with both an origin and a size,
                // so every edge is honored: right/bottom grow from the fixed
                // top-left, while left/top edges move the origin and resize so
                // the opposite corner stays pinned. Sizes clamp to the overlay
                // minimums; a clamped left/top edge stops moving the origin.
                let axes = resize_edge_axes(edge);
                let min_w = OVERLAY_MIN_WIDTH as i32;
                let min_h = OVERLAY_MIN_HEIGHT as i32;
                let start_w = self.start_bounds.width as i32;
                let start_h = self.start_bounds.height as i32;

                let mut x = self.start_bounds.x;
                let mut y = self.start_bounds.y;
                let mut width = start_w;
                let mut height = start_h;

                if axes.affects_width {
                    if axes.moves_left {
                        width = (start_w - delta_x).max(min_w);
                        x = self.start_bounds.x + (start_w - width);
                    } else {
                        width = (start_w + delta_x).max(min_w);
                    }
                }
                if axes.affects_height {
                    if axes.moves_top {
                        height = (start_h - delta_y).max(min_h);
                        y = self.start_bounds.y + (start_h - height);
                    } else {
                        height = (start_h + delta_y).max(min_h);
                    }
                }

                if axes.moves_left || axes.moves_top {
                    self.configure(x, y, width as u32, height as u32)?;
                } else {
                    self.resize(width as u32, height as u32)?;
                }
            }
        }
        Ok(button_down)
    }

    fn pointer(&self) -> Result<(X11Point, bool)> {
        let pointer = self.connection.query_pointer(self.root)?.reply()?;
        let mask = u16::from(pointer.mask);
        let button_down = mask & u16::from(KeyButMask::BUTTON1) != 0;
        Ok((
            X11Point {
                x: i32::from(pointer.root_x),
                y: i32::from(pointer.root_y),
            },
            button_down,
        ))
    }

    fn move_to(&self, x: i32, y: i32) -> Result<()> {
        self.connection
            .configure_window(self.window, &ConfigureWindowAux::new().x(x).y(y))?;
        self.connection.flush()?;
        Ok(())
    }

    fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.connection.configure_window(
            self.window,
            &ConfigureWindowAux::new().width(width).height(height),
        )?;
        self.connection.flush()?;
        Ok(())
    }

    /// Reposition and resize together — needed for left/top edge drags where the
    /// origin moves so the opposite corner stays fixed.
    fn configure(&self, x: i32, y: i32, width: u32, height: u32) -> Result<()> {
        self.connection.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width)
                .height(height),
        )?;
        self.connection.flush()?;
        Ok(())
    }

    fn current_bounds(&self) -> Result<X11Bounds> {
        let geometry = self.connection.get_geometry(self.window)?.reply()?;
        Ok(X11Bounds {
            x: i32::from(geometry.x),
            y: i32::from(geometry.y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        })
    }
}

fn x11_drag_session() -> Option<X11DragSession> {
    if env::var(VIEWER_BACKEND_ENV)
        .map(|backend| !backend.eq_ignore_ascii_case(VIEWER_BACKEND_X11))
        .unwrap_or(false)
    {
        return None;
    }

    let (connection, screen_index) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen_index)?.root;
    let pid_atom = intern_x11_atom(&connection, b"_NET_WM_PID")?;
    let pointer = connection.query_pointer(root).ok()?.reply().ok()?;
    let pid = std::process::id();
    let window =
        if pointer.child != 0 && x11_window_matches(&connection, pointer.child, pid_atom, pid) {
            pointer.child
        } else {
            find_x11_viewer_window(&connection, root, pid_atom, pid, 0)?
        };
    let geometry = connection.get_geometry(window).ok()?.reply().ok()?;

    Some(X11DragSession {
        connection,
        root,
        window,
        start_pointer: X11Point {
            x: i32::from(pointer.root_x),
            y: i32::from(pointer.root_y),
        },
        start_bounds: X11Bounds {
            x: i32::from(geometry.x),
            y: i32::from(geometry.y),
            width: u32::from(geometry.width),
            height: u32::from(geometry.height),
        },
    })
}

fn intern_x11_atom(connection: &RustConnection, name: &[u8]) -> Option<u32> {
    Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom)
}

fn find_x11_viewer_window(
    connection: &RustConnection,
    root: X11Window,
    pid_atom: u32,
    pid: u32,
    depth: usize,
) -> Option<X11Window> {
    if depth > 3 {
        return None;
    }

    let tree = connection.query_tree(root).ok()?.reply().ok()?;
    for &window in tree.children.iter().rev() {
        if x11_window_matches(connection, window, pid_atom, pid) {
            return Some(window);
        }
    }
    for &window in tree.children.iter().rev() {
        if let Some(found) = find_x11_viewer_window(connection, window, pid_atom, pid, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn x11_window_matches(
    connection: &RustConnection,
    window: X11Window,
    pid_atom: u32,
    pid: u32,
) -> bool {
    let pid_matches = connection
        .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| {
            reply
                .value32()
                .map(|mut values| values.any(|value| value == pid))
        })
        .unwrap_or(false);
    if !pid_matches {
        return false;
    }

    connection
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| String::from_utf8(reply.value).ok())
        .map(|class| class.split('\0').any(|value| value == VIEWER_APP_ID))
        .unwrap_or(false)
}

/// Thickness of the thin edge hit-zones (corner zones are this square).
const RESIZE_EDGE_HIT: f32 = 6.0;

/// Build one thin resize hit-zone pinned to the given edge/corner. `on_down`
/// starts the resize for that edge; the visible bottom-right corner also carries
/// a faint brushed-metal grip so the resize affordance stays discoverable.
fn resize_edge_zone<F>(
    id: &'static str,
    edge: ResizeEdge,
    cursor: CursorStyle,
    on_down: F,
) -> AnyElement
where
    F: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let thick = px(RESIZE_EDGE_HIT);
    let mut zone = div().id(id).absolute().cursor(cursor).on_mouse_down(
        MouseButton::Left,
        move |event, window, cx| {
            on_down(event, window, cx);
            cx.stop_propagation();
        },
    );
    zone = match edge {
        ResizeEdge::Top => zone.top_0().left_0().right_0().h(thick),
        ResizeEdge::Bottom => zone.bottom_0().left_0().right_0().h(thick),
        ResizeEdge::Left => zone.left_0().top_0().bottom_0().w(thick),
        ResizeEdge::Right => zone.right_0().top_0().bottom_0().w(thick),
        ResizeEdge::TopLeft => zone.top_0().left_0().w(thick).h(thick),
        ResizeEdge::TopRight => zone.top_0().right_0().w(thick).h(thick),
        ResizeEdge::BottomLeft => zone.bottom_0().left_0().w(thick).h(thick),
        ResizeEdge::BottomRight => zone.bottom_0().right_0().w(thick).h(thick),
    };
    if edge == ResizeEdge::BottomRight {
        zone = zone
            .w(px(16.0))
            .h(px(16.0))
            .flex()
            .items_end()
            .justify_end()
            .pr(px(3.0))
            .pb(px(3.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(2.0))
                    .child(div().w(px(6.0)).border_b_1().border_color(rgb(EDGE_SILVER)))
                    .child(
                        div()
                            .w(px(10.0))
                            .border_b_1()
                            .border_color(rgb(EDGE_HIGHLIGHT)),
                    ),
            );
    }
    zone.into_any_element()
}

fn tooltip_text(text: impl Into<SharedString>) -> SharedString {
    text.into()
}

fn mcp_control_tooltip(mode: McpControlMode) -> String {
    format!(
        "MCP control is {}. Read-only and paused block mutating agent actions; switching back to active is an explicit user control action.",
        mode.label()
    )
}

fn mcp_control_action_tooltip(current: McpControlMode, target: McpControlMode) -> String {
    if current == target {
        return mcp_control_tooltip(current);
    }

    match target {
        McpControlMode::Active => {
            "Run: switch MCP control back to active so mutating agent actions can continue."
                .to_string()
        }
        McpControlMode::ReadOnly => {
            "Read-only: block mutating agent actions while preserving observation and safety stop."
                .to_string()
        }
        McpControlMode::Paused => {
            "Pause: hold mutating agent actions until the user explicitly resumes.".to_string()
        }
    }
}

#[cfg(test)]
fn control_segment_labels(mode: McpControlMode) -> Vec<&'static str> {
    match mode {
        McpControlMode::Active => vec![
            McpControlMode::ReadOnly.button_label(),
            McpControlMode::Paused.button_label(),
        ],
        McpControlMode::ReadOnly => vec!["Run", McpControlMode::Paused.button_label()],
        McpControlMode::Paused => vec!["Run", McpControlMode::ReadOnly.button_label()],
    }
}

fn attach_tooltip(element: Stateful<Div>, tooltip: Option<SharedString>) -> Stateful<Div> {
    element.when_some(tooltip, |element, tooltip| {
        element.tooltip(move |_window, cx| {
            let tooltip = tooltip.clone();
            cx.new(|_| ViewerTooltip::new(tooltip)).into()
        })
    })
}

/// The visual state of the one and only button shape. Every viewer button is
/// the same pill (one radius, one height, one horizontal padding, one font);
/// state is expressed through fill + border + text color ONLY — never a
/// different shape or size.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ButtonKind {
    Normal,
    Selected,
    Danger,
    Disabled,
}

impl ButtonKind {
    /// (fill, border, text, hover_fill, hover_border) for this state.
    fn colors(self) -> (u32, u32, u32, u32, u32) {
        match self {
            ButtonKind::Normal => (
                BUTTON_BG,
                BUTTON_EDGE_SOFT,
                TEXT,
                BUTTON_BG_HOVER,
                BUTTON_EDGE,
            ),
            ButtonKind::Selected => (
                BUTTON_SELECTED_BG,
                BUTTON_SELECTED_EDGE,
                TEXT,
                BUTTON_SELECTED_BG_HOVER,
                BUTTON_EDGE,
            ),
            ButtonKind::Danger => (
                DANGER_BG,
                DANGER_EDGE,
                DANGER_TEXT,
                DANGER_HOVER,
                DANGER_EDGE_HOVER,
            ),
            ButtonKind::Disabled => (
                BUTTON_DISABLED_BG,
                BUTTON_DISABLED_EDGE,
                BUTTON_DISABLED_TEXT,
                BUTTON_DISABLED_BG,
                BUTTON_DISABLED_EDGE,
            ),
        }
    }
}

/// The single uniform button component. Builds the one pill shape and wires the
/// optional click handler. `Disabled` renders the same pill with muted fill and
/// no hover/click affordance.
fn pill_button<F>(
    id: &'static str,
    label: impl Into<SharedString>,
    tooltip: Option<SharedString>,
    kind: ButtonKind,
    on_click: Option<F>,
) -> AnyElement
where
    F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    let (bg, border, text, hover_bg, hover_border) = kind.colors();
    let interactive = !matches!(kind, ButtonKind::Disabled);
    let base = div()
        .id(id)
        .px(px(BUTTON_PAD_X))
        .h(px(BUTTON_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(BUTTON_RADIUS))
        .bg(rgb(bg))
        .text_size(px(BUTTON_TEXT))
        .font_weight(gpui::FontWeight::MEDIUM)
        .line_height(px(BUTTON_HEIGHT))
        .text_color(rgb(text))
        .border_1()
        .border_color(rgb(border))
        // Faint brushed-metal highlight on the top edge of the pill.
        .border_t_1()
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation();
        });
    let base = if interactive {
        base.cursor_pointer()
            .hover(move |style| style.bg(rgb(hover_bg)).border_color(rgb(hover_border)))
    } else {
        base
    };
    let base = base.when_some(on_click.filter(|_| interactive), |element, on_click| {
        element.on_click(on_click)
    });
    attach_tooltip(base.child(label.into()), tooltip).into_any_element()
}

/// A boxed click handler for a segmented-control segment.
type SegmentClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// One segment in the segmented control: a stable id, its label, the live-mode
/// flag (active segment is filled and non-interactive), an optional tooltip, and
/// an optional click handler (present only for the inactive, switch-to segments).
struct Segment {
    id: &'static str,
    label: SharedString,
    active: bool,
    tooltip: Option<SharedString>,
    on_click: Option<SegmentClick>,
}

/// A connected segmented control — equal-width segments sharing one rounded
/// silvery rim, with hairline dividers between them. The active segment shows
/// the selected fill; the others are quiet and clickable. Used for the live
/// control mode `[ Run · RO · Pause ]`.
fn segmented_control(segments: Vec<Segment>) -> AnyElement {
    let last = segments.len().saturating_sub(1);
    let mut row = div()
        .flex()
        .items_center()
        .h(px(BUTTON_HEIGHT))
        .rounded(px(BUTTON_RADIUS))
        .border_1()
        .border_color(rgb(EDGE_SILVER))
        .bg(rgb(SURFACE))
        .overflow_hidden();

    for (index, segment) in segments.into_iter().enumerate() {
        let (bg, text) = if segment.active {
            (BUTTON_SELECTED_BG, TEXT)
        } else {
            (SURFACE, MUTED)
        };
        let mut cell = div()
            .id(segment.id)
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .h_full()
            .px(px(10.0))
            .bg(rgb(bg))
            .text_size(px(BUTTON_TEXT))
            .font_weight(gpui::FontWeight::MEDIUM)
            .line_height(px(BUTTON_HEIGHT))
            .text_color(rgb(text))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            });
        if index < last {
            // Hairline divider between segments.
            cell = cell.border_r_1().border_color(rgb(BORDER));
        }
        if !segment.active {
            cell = cell
                .cursor_pointer()
                .hover(|style| style.bg(rgb(SURFACE_2)).text_color(rgb(TEXT)));
            if let Some(on_click) = segment.on_click {
                cell = cell.on_click(move |event, window, app| on_click(event, window, app));
            }
        }
        row = row.child(attach_tooltip(cell.child(segment.label), segment.tooltip));
    }

    row.into_any_element()
}

fn button_with_tooltip<F>(
    id: &'static str,
    label: impl Into<SharedString>,
    tooltip: Option<SharedString>,
    on_click: F,
) -> AnyElement
where
    F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    pill_button(id, label, tooltip, ButtonKind::Normal, Some(on_click))
}

fn danger_button_with_tooltip<F>(
    id: &'static str,
    label: impl Into<SharedString>,
    tooltip: Option<SharedString>,
    on_click: F,
) -> AnyElement
where
    F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    pill_button(id, label, tooltip, ButtonKind::Danger, Some(on_click))
}

fn selected_button_with_tooltip<F>(
    id: &'static str,
    label: impl Into<SharedString>,
    tooltip: Option<SharedString>,
    on_click: F,
) -> AnyElement
where
    F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    pill_button(id, label, tooltip, ButtonKind::Selected, Some(on_click))
}

fn disabled_button_with_tooltip(
    id: &'static str,
    label: impl Into<SharedString>,
    tooltip: Option<SharedString>,
) -> AnyElement {
    pill_button(
        id,
        label,
        tooltip,
        ButtonKind::Disabled,
        None::<fn(&ClickEvent, &mut Window, &mut App)>,
    )
}

/// Pick the running indicator color: green when running, amber when paused or
/// read-only (or when something needs attention), muted grey when stopped.
fn running_status_color(running: bool, mode: McpControlMode, has_error: bool) -> u32 {
    if has_error {
        RED
    } else if !matches!(mode, McpControlMode::Active) {
        AMBER
    } else if running {
        GREEN
    } else {
        MUTED
    }
}

/// A small filled status light (no text label) used as the running indicator.
/// Carries a faint matching ring so it reads as a polished dot, not a flat blob.
fn status_light(color: u32) -> AnyElement {
    div()
        .w(px(8.0))
        .h(px(8.0))
        .rounded_full()
        .bg(rgb(color))
        .border_1()
        .border_color(rgba((color << 8) | 0x40))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn footer_activity_label(
    busy_action: Option<&ViewerAction>,
    refreshing: bool,
    error: Option<&str>,
    running: bool,
    doctor_ready: bool,
    doctor_blockers: &[String],
    selected_profile_label: &str,
    latest_activity: Option<&ViewerActivity>,
    active_window: Option<&workspace::WorkspaceWindow>,
    apps: Option<&[workspace::WorkspaceApp]>,
    now: u64,
) -> String {
    if let Some(action) = busy_action {
        return format!("Agent action: {}...", action.label());
    }
    if refreshing {
        return "Agent action: Refreshing workspace state".to_string();
    }
    if let Some(error) = error.map(str::trim).filter(|error| !error.is_empty()) {
        return format!("Needs attention: {error}");
    }
    if running {
        let activity = latest_activity
            .map(|activity| {
                format!(
                    "Last action: {} {}",
                    activity.label,
                    elapsed_label(activity.timestamp_unix, now)
                )
            })
            .unwrap_or_else(|| "Agent idle".to_string());
        let target = active_target_label(active_window, apps)
            .or_else(|| running_apps_label(apps))
            .unwrap_or_else(|| "Workspace running".to_string());
        return format!("{activity} | {target}");
    }
    if doctor_ready {
        format!("Agent idle | Ready to start {selected_profile_label}")
    } else {
        let blocker = doctor_blockers
            .first()
            .map(|blocker| blocker.as_str())
            .unwrap_or("Runtime is not ready");
        format!("Setup needed: {blocker}")
    }
}

fn footer_isolation_label(
    entry: Option<&workspace::WorkspaceListEntry>,
    doctor_ready: bool,
    doctor_blockers: &[String],
    selected_profile_label: &str,
    permissions: &McpPermissionState,
) -> String {
    let permission = permission_ceiling_label(permissions);
    let Some(entry) = entry else {
        if doctor_ready {
            return format!(
                "Ready | Start profile: {selected_profile_label} | Scope: display/input | {permission}"
            );
        }
        let blocker = doctor_blockers
            .first()
            .map(|blocker| blocker.as_str())
            .unwrap_or("Runtime is not ready");
        return format!("Setup needed: {blocker} | {permission}");
    };

    let profile = workspace_entry_profile_id(entry)
        .map(|profile| format!("Profile: {profile}"))
        .unwrap_or_else(|| "Profile: default".to_string());
    let Some(policy) = workspace_entry_policy(entry) else {
        return format!(
            "{profile} | Scope: display/input | Net: host | Mounts: host | {permission}"
        );
    };

    let network = network_policy_label(policy);
    let mounts = mount_policy_label(policy);
    let acknowledgement = policy_acknowledgement_label(policy, workspace_entry_policy_ack(entry));
    format!(
        "{profile} | Scope: display/input | {network} | {mounts}{acknowledgement} | {permission}"
    )
}

fn footer_task_label(
    entry: Option<&workspace::WorkspaceListEntry>,
    running: bool,
    doctor_ready: bool,
    doctor_blockers: &[String],
    selected_profile_label: &str,
    active_window: Option<&workspace::WorkspaceWindow>,
    apps: Option<&[workspace::WorkspaceApp]>,
) -> String {
    let Some(entry) = entry else {
        if doctor_ready {
            return format!("Task: ready to start {selected_profile_label}");
        }
        let blocker = doctor_blockers
            .first()
            .map(|blocker| blocker.as_str())
            .unwrap_or("Runtime is not ready");
        return format!("Task blocked: {blocker}");
    };

    let task = viewer_task_intent_label(entry, active_window, apps);
    let target = if running {
        active_target_label(active_window, apps)
            .or_else(|| running_apps_label(apps))
            .unwrap_or_else(|| "Workspace running".to_string())
    } else {
        "Stopped workspace".to_string()
    };
    format!("Task: {task} | {target}")
}

fn viewer_task_intent_label(
    entry: &workspace::WorkspaceListEntry,
    active_window: Option<&workspace::WorkspaceWindow>,
    apps: Option<&[workspace::WorkspaceApp]>,
) -> &'static str {
    let mut markers = Vec::new();
    if let Some(profile_id) = workspace_entry_profile_id(entry) {
        markers.push(profile_id.to_string());
    }
    if let Some(purpose) = entry
        .status
        .as_ref()
        .and_then(|status| status.purpose.as_deref())
        .or_else(|| {
            entry
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.purpose.as_deref())
        })
    {
        markers.push(purpose.to_string());
    }
    if let Some(window) = active_window {
        markers.push(active_window_label(window));
    }
    if let Some(apps) = apps {
        markers.extend(apps.iter().take(5).map(app_label));
    }
    let marker = markers.join(" ").to_ascii_lowercase();
    if marker.contains("browser")
        || marker.contains("chrome")
        || marker.contains("chromium")
        || marker.contains("firefox")
        || marker.contains("shopping")
        || marker.contains("grocery")
    {
        "Browser/shopping"
    } else if marker.contains("project")
        || marker.contains("qa")
        || marker.contains("dev")
        || marker.contains("cargo")
        || marker.contains("npm")
        || marker.contains("test")
        || marker.contains("editor")
        || marker.contains("xterm")
    {
        "App QA"
    } else if entry.running {
        "Observe running workspace"
    } else {
        "Review stopped workspace"
    }
}

fn permission_ceiling_label(permissions: &McpPermissionState) -> String {
    if !permissions.configured {
        return "MCP ceiling: open".to_string();
    }
    if !permissions.restricted {
        return "MCP ceiling: configured/open".to_string();
    }

    let mut parts = Vec::new();
    if let Some(network) = &permissions.ceiling.network {
        parts.push(match network.mode {
            NetworkMode::InheritHost => "net host".to_string(),
            NetworkMode::Disabled => "net off".to_string(),
            NetworkMode::LocalOnly => {
                if network.allow_hosts.is_empty() {
                    "net local".to_string()
                } else {
                    format!("net local {}", network.allow_hosts.len())
                }
            }
        });
    }
    if !permissions.ceiling.mounts.is_empty() {
        parts.push(format!("mounts {}", permissions.ceiling.mounts.len()));
    }
    if !permissions.ceiling.apps.allow.is_empty() {
        parts.push(format!("apps {}", permissions.ceiling.apps.allow.len()));
    }

    if parts.is_empty() {
        "MCP ceiling: restricted".to_string()
    } else {
        format!("MCP ceiling: {}", parts.join(", "))
    }
}

fn footer_apps_label(
    running: bool,
    apps: Option<&[workspace::WorkspaceApp]>,
    active_window: Option<&workspace::WorkspaceWindow>,
) -> String {
    let Some(apps) = apps.filter(|apps| !apps.is_empty()) else {
        return if running {
            "Apps: no launched apps yet".to_string()
        } else {
            "Apps: no saved app history".to_string()
        };
    };

    let running_labels = apps
        .iter()
        .filter(|app| app.running)
        .map(app_label)
        .take(3)
        .collect::<Vec<_>>();
    let stopped_count = apps.iter().filter(|app| !app.running).count();
    let active = active_target_label(active_window, Some(apps));

    let mut parts = Vec::new();
    if let Some(active) = active {
        parts.push(active);
    }
    match running_labels.len() {
        0 => parts.push(format!("Saved apps: {}", apps.len())),
        1 => parts.push(format!("Running: {}", running_labels[0])),
        _ => parts.push(format!("Running: {}", running_labels.join(", "))),
    }
    if stopped_count > 0 {
        parts.push(format!("{stopped_count} stopped"));
    }
    parts.join(" | ")
}

fn workspace_entry_policy(
    entry: &workspace::WorkspaceListEntry,
) -> Option<&AppliedWorkspacePolicy> {
    entry
        .status
        .as_ref()
        .and_then(|status| status.applied_policy.as_ref())
        .or_else(|| {
            entry
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.applied_policy.as_ref())
        })
}

fn workspace_entry_profile_id(entry: &workspace::WorkspaceListEntry) -> Option<&str> {
    entry
        .status
        .as_ref()
        .and_then(|status| status.profile_id.as_deref())
        .or_else(|| {
            entry
                .manifest
                .as_ref()
                .and_then(|manifest| manifest.profile_id.as_deref())
        })
}

fn workspace_entry_policy_ack(entry: &workspace::WorkspaceListEntry) -> bool {
    entry
        .status
        .as_ref()
        .map(|status| status.user_acknowledged_unenforced_policy)
        .or_else(|| {
            entry
                .manifest
                .as_ref()
                .map(|manifest| manifest.user_acknowledged_unenforced_policy)
        })
        .unwrap_or(false)
}

fn network_policy_label(policy: &AppliedWorkspacePolicy) -> String {
    match policy.network.mode {
        NetworkMode::InheritHost => "Net: host".to_string(),
        NetworkMode::Disabled if policy.enforcement.network.enforced => "Net: off".to_string(),
        NetworkMode::Disabled => "Net: off declared".to_string(),
        NetworkMode::LocalOnly if policy.enforcement.network.enforced => "Net: local".to_string(),
        NetworkMode::LocalOnly => "Net: local declared".to_string(),
    }
}

fn mount_policy_label(policy: &AppliedWorkspacePolicy) -> String {
    match (policy.mounts.len(), policy.enforcement.mounts.enforced) {
        (0, _) => "Mounts: host".to_string(),
        (count, true) => format!("Mounts: {count} scoped"),
        (count, false) => format!("Mounts: {count} declared"),
    }
}

fn policy_acknowledgement_label(policy: &AppliedWorkspacePolicy, acknowledged: bool) -> String {
    if policy.blocks_requested_unenforced_policy() {
        " | Policy blocked".to_string()
    } else if policy.has_requested_unenforced_policy() && acknowledged {
        " | Limits acknowledged".to_string()
    } else if policy.has_requested_unenforced_policy() {
        " | Needs policy ack".to_string()
    } else {
        String::new()
    }
}

fn active_target_label(
    active_window: Option<&workspace::WorkspaceWindow>,
    apps: Option<&[workspace::WorkspaceApp]>,
) -> Option<String> {
    let window = active_window?;
    let window_label = active_window_label(window);
    let Some(app) = app_for_window(apps, window) else {
        return Some(format!("Active window: {window_label}"));
    };

    let app_label = app_label(app);
    if app_label == window_label {
        Some(format!("Active app: {app_label}"))
    } else {
        Some(format!("Active app: {app_label} | Window: {window_label}"))
    }
}

fn running_apps_label(apps: Option<&[workspace::WorkspaceApp]>) -> Option<String> {
    let labels = apps?
        .iter()
        .filter(|app| app.running)
        .map(app_label)
        .take(3)
        .collect::<Vec<_>>();
    match labels.len() {
        0 => None,
        1 => Some(format!("Running app: {}", labels[0])),
        _ => Some(format!("Running apps: {}", labels.join(", "))),
    }
}

fn app_for_window<'a>(
    apps: Option<&'a [workspace::WorkspaceApp]>,
    window: &workspace::WorkspaceWindow,
) -> Option<&'a workspace::WorkspaceApp> {
    let apps = apps?;
    if let Some(app_id) = window.app_id.as_deref() {
        if let Some(app) = apps.iter().find(|app| app.id == app_id) {
            return Some(app);
        }
    }
    window
        .pid
        .and_then(|pid| apps.iter().find(|app| app.pid == pid))
}

fn selected_app_log_target(
    apps: Option<&[workspace::WorkspaceApp]>,
    active_window: Option<&workspace::WorkspaceWindow>,
) -> Option<AppLogTarget> {
    let apps = apps?;
    let app = active_window
        .and_then(|window| app_for_window(Some(apps), window))
        .or_else(|| {
            apps.iter()
                .filter(|app| app.running)
                .max_by_key(|app| app.started_at_unix)
        })
        .or_else(|| apps.iter().max_by_key(|app| app.started_at_unix))?;

    app_log_target(app)
}

fn app_log_target(app: &workspace::WorkspaceApp) -> Option<AppLogTarget> {
    let stdout = app
        .stdout_path
        .as_ref()
        .filter(|path| path.exists())
        .map(|path| ("stdout", path));
    let stderr = app
        .stderr_path
        .as_ref()
        .filter(|path| path.exists())
        .map(|path| ("stderr", path));

    let (stream, _path) = match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => {
            if file_len(stdout.1) == Some(0) && file_len(stderr.1).unwrap_or(0) > 0 {
                stderr
            } else {
                stdout
            }
        }
        (Some(stdout), None) => stdout,
        (None, Some(stderr)) => stderr,
        (None, None) => return None,
    };

    Some(AppLogTarget {
        app_id: app.id.clone(),
        stream: stream.to_string(),
    })
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn workspace_entry_apps(
    entry: &workspace::WorkspaceListEntry,
) -> Option<&[workspace::WorkspaceApp]> {
    entry
        .status
        .as_ref()
        .map(|status| status.apps.as_slice())
        .or_else(|| {
            entry
                .manifest
                .as_ref()
                .map(|manifest| manifest.apps.as_slice())
        })
}

fn app_label(app: &workspace::WorkspaceApp) -> String {
    app.name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| app.command.first().map(|command| command_label(command)))
        .unwrap_or_else(|| app.id.clone())
}

fn command_label(command: &str) -> String {
    command
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(command)
        .to_string()
}

fn latest_workspace_activity(
    id: &str,
    apps: &[workspace::WorkspaceApp],
    active_window: Option<&workspace::WorkspaceWindow>,
) -> Option<ViewerActivity> {
    let events = workspace::read_events(id, Some(512), None).ok()?.events?;
    events
        .iter()
        .rev()
        .find_map(|event| activity_from_event(event, apps, active_window))
}

fn activity_from_event(
    event: &workspace::WorkspaceEvent,
    apps: &[workspace::WorkspaceApp],
    active_window: Option<&workspace::WorkspaceWindow>,
) -> Option<ViewerActivity> {
    let detail = &event.detail;
    let target = event_target_label(detail, apps, active_window);
    let label = match event.kind.as_str() {
        "workspace_start" => "Started workspace".to_string(),
        "workspace_stop" => "Stopped workspace".to_string(),
        "app_launch" => format!(
            "Launched {}",
            event_app_label(detail, apps).unwrap_or_else(|| "app".to_string())
        ),
        "launch_wait_window" => {
            let app = event_app_label(detail, apps).unwrap_or_else(|| "app".to_string());
            if detail_bool(detail, "found").unwrap_or(false) {
                format!("Found {app} window")
            } else {
                format!("Waiting for {app} window")
            }
        }
        "app_exit" => format!(
            "App exited: {}",
            event_app_label(detail, apps).unwrap_or_else(|| "app".to_string())
        ),
        "focus_window" => event_with_target("Focused", target, "Focused window"),
        "close_window" => event_with_target("Closed", target, "Closed window"),
        "move_window" => event_with_target("Moved", target, "Moved window"),
        "resize_window" => event_with_target("Resized", target, "Resized window"),
        "raise_window" => event_with_target("Raised", target, "Raised window"),
        "minimize_window" => event_with_target("Minimized", target, "Minimized window"),
        "show_window" => event_with_target("Showed", target, "Showed window"),
        "click" => "Clicked workspace".to_string(),
        "click_window" => event_with_target("Clicked", target, "Clicked window"),
        "move_pointer" => "Moved pointer".to_string(),
        "move_pointer_window" => {
            event_with_target("Moved pointer in", target, "Moved pointer in window")
        }
        "drag" => "Dragged in workspace".to_string(),
        "drag_window" => event_with_target("Dragged in", target, "Dragged in window"),
        "scroll" => scroll_event_label(detail, None),
        "scroll_window" => scroll_event_label(detail, target),
        "key" => key_event_label(detail, None),
        "key_window" => key_event_label(detail, target),
        "type_text" => typed_event_label(detail, None),
        "type_window" => typed_event_label(detail, target),
        "set_clipboard" => count_event_label("Set clipboard", detail, "char_count"),
        "get_clipboard" => "Read clipboard".to_string(),
        "paste_text" => pasted_event_label(detail, None),
        "paste_window" => pasted_event_label(detail, target),
        "browser_snapshot" => browser_snapshot_event_label(detail),
        "browser_search_results" => browser_search_results_event_label(detail),
        "browser_navigate" => browser_navigate_event_label(detail),
        "screenshot_window" => event_with_target(
            "Captured screenshot of",
            target,
            "Captured window screenshot",
        ),
        "wait_window" => event_with_target("Waited for", target, "Waited for window"),
        "kill_app" => event_with_target("Stopped app", target, "Stopped app"),
        _ => return None,
    };
    Some(ViewerActivity {
        label,
        timestamp_unix: event.timestamp_unix,
    })
}

fn event_with_target(action: &str, target: Option<String>, fallback: &str) -> String {
    target
        .map(|target| format!("{action} {target}"))
        .unwrap_or_else(|| fallback.to_string())
}

fn scroll_event_label(detail: &serde_json::Value, target: Option<String>) -> String {
    let direction = detail_str(detail, "direction").unwrap_or("workspace");
    match target {
        Some(target) => format!("Scrolled {direction} in {target}"),
        None => format!("Scrolled {direction}"),
    }
}

fn key_event_label(detail: &serde_json::Value, target: Option<String>) -> String {
    let key = detail_str(detail, "key").unwrap_or("key");
    match target {
        Some(target) => format!("Pressed {key} in {target}"),
        None => format!("Pressed {key}"),
    }
}

fn typed_event_label(detail: &serde_json::Value, target: Option<String>) -> String {
    let label = count_event_label("Typed", detail, "char_count");
    match target {
        Some(target) => format!("{label} into {target}"),
        None => label,
    }
}

fn pasted_event_label(detail: &serde_json::Value, target: Option<String>) -> String {
    let label = count_event_label("Pasted", detail, "char_count");
    match target {
        Some(target) => format!("{label} into {target}"),
        None => label,
    }
}

fn browser_snapshot_event_label(detail: &serde_json::Value) -> String {
    if detail_str(detail, "browser_action") == Some("browser_search_results")
        || detail_str(detail, "original_event_kind") == Some("browser_search_results")
    {
        return browser_search_results_event_label(detail);
    }
    browser_page_label(detail)
        .map(|page| format!("Read browser page: {page}"))
        .unwrap_or_else(|| "Read browser page".to_string())
}

fn browser_search_results_event_label(detail: &serde_json::Value) -> String {
    let count = detail
        .get("result_count")
        .and_then(serde_json::Value::as_u64)
        .map(|count| format!("Found {count} browser results"))
        .unwrap_or_else(|| "Read browser results".to_string());
    browser_page_label(detail)
        .map(|page| format!("{count}: {page}"))
        .unwrap_or(count)
}

fn browser_navigate_event_label(detail: &serde_json::Value) -> String {
    browser_page_label(detail)
        .map(|page| format!("Navigated browser to {page}"))
        .unwrap_or_else(|| "Navigated browser".to_string())
}

fn browser_page_label(detail: &serde_json::Value) -> Option<String> {
    detail_str(detail, "title")
        .map(compact_footer_value)
        .or_else(|| {
            detail_str(detail, "current_url")
                .or_else(|| detail_str(detail, "url"))
                .map(browser_url_footer_label)
        })
}

fn browser_url_footer_label(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("data:") {
        return "data page".to_string();
    }
    if trimmed == "about:blank" {
        return "blank page".to_string();
    }
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host_path = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    compact_footer_value(host_path)
}

fn count_event_label(action: &str, detail: &serde_json::Value, key: &str) -> String {
    detail_u64(detail, key)
        .map(|count| format!("{action} {count} chars"))
        .unwrap_or_else(|| action.to_string())
}

fn compact_footer_value(value: &str) -> String {
    let cleaned = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    const MAX_CHARS: usize = 64;
    if cleaned.chars().count() <= MAX_CHARS {
        return cleaned;
    }
    let mut truncated = cleaned.chars().take(MAX_CHARS - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn event_app_label(detail: &serde_json::Value, apps: &[workspace::WorkspaceApp]) -> Option<String> {
    detail_str(detail, "name")
        .map(str::to_string)
        .or_else(|| {
            detail_str(detail, "app_id").map(|app_id| {
                apps.iter()
                    .find(|app| app.id == app_id)
                    .map(app_label)
                    .unwrap_or_else(|| app_id.to_string())
            })
        })
        .or_else(|| {
            detail
                .get("command")
                .and_then(|command| command.as_array())
                .and_then(|command| command.first())
                .and_then(|command| command.as_str())
                .map(command_label)
        })
}

fn event_target_label(
    detail: &serde_json::Value,
    apps: &[workspace::WorkspaceApp],
    active_window: Option<&workspace::WorkspaceWindow>,
) -> Option<String> {
    detail_str(detail, "name")
        .map(str::to_string)
        .or_else(|| {
            detail_str(detail, "app_id").map(|app_id| {
                apps.iter()
                    .find(|app| app.id == app_id)
                    .map(app_label)
                    .unwrap_or_else(|| app_id.to_string())
            })
        })
        .or_else(|| detail_str(detail, "target").map(str::to_string))
        .or_else(|| detail_str(detail, "title").map(str::to_string))
        .or_else(|| detail_str(detail, "title_contains").map(|title| format!("title {title}")))
        .or_else(|| detail_str(detail, "class_contains").map(|class| format!("class {class}")))
        .or_else(|| {
            detail_str(detail, "window_id").map(|window_id| {
                active_window
                    .filter(|window| window.id == window_id)
                    .map(active_window_label)
                    .unwrap_or_else(|| format!("window {window_id}"))
            })
        })
}

fn detail_str<'a>(detail: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    detail
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn detail_u64(detail: &serde_json::Value, key: &str) -> Option<u64> {
    detail.get(key).and_then(|value| value.as_u64())
}

fn detail_bool(detail: &serde_json::Value, key: &str) -> Option<bool> {
    detail.get(key).and_then(|value| value.as_bool())
}

fn active_window_label(window: &workspace::WorkspaceWindow) -> String {
    let title = window.title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    if let Some(wm_class) = window
        .wm_class
        .as_deref()
        .filter(|wm_class| !wm_class.is_empty())
    {
        return wm_class.to_string();
    }
    window.id.clone()
}

fn profile_label(profile: &profile::WorkspaceProfile) -> String {
    match profile
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        Some(description) => format!("{} - {}", profile.id, description),
        None => profile.id.clone(),
    }
}

fn workspace_watch_window(id: &str) -> Option<workspace::WorkspaceWindow> {
    workspace::active_window(id)
        .ok()
        .and_then(|response| response.active_window)
        .or_else(|| {
            workspace::list_windows(id, false, None, None, None, None)
                .ok()
                .and_then(|response| {
                    let windows = response.windows?;
                    windows
                        .iter()
                        .find(|window| window.visible)
                        .cloned()
                        .or_else(|| windows.into_iter().next())
                })
        })
}

fn elapsed_label(timestamp: u64, now: u64) -> String {
    let seconds = now.saturating_sub(timestamp);
    match seconds {
        0 => "just now".to_string(),
        1 => "1s ago".to_string(),
        2..=59 => format!("{seconds}s ago"),
        60..=119 => "1m ago".to_string(),
        _ => format!("{}m ago", seconds / 60),
    }
}

fn wall_clock_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        MountMode, NetworkPolicy, PolicyRuntimeCapabilities, PolicyToolCheck, ProfileMount,
    };
    use std::path::PathBuf;

    fn tool(ok: bool) -> PolicyToolCheck {
        PolicyToolCheck {
            ok,
            detail: if ok { "ok" } else { "missing" }.to_string(),
        }
    }

    fn capabilities(bubblewrap: bool) -> PolicyRuntimeCapabilities {
        PolicyRuntimeCapabilities::from_tools(
            tool(bubblewrap),
            tool(false),
            tool(false),
            tool(false),
        )
    }

    fn temp_test_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "agent-workspace-viewer-{name}-{}-{}",
            std::process::id(),
            wall_clock_seconds()
        ))
    }

    #[test]
    fn viewer_preferences_round_trip_and_clamp_size() {
        let dir = temp_test_path("prefs");
        let path = dir.join("viewer.json");
        let preferences = ViewerPreferences {
            width: 120.0,
            height: 200.0,
            screen_stream: false,
            footer_mode: FooterMode::Task,
            x: Some(42.0),
            y: Some(64.0),
        };

        save_viewer_preferences_to_path(&path, &preferences).expect("save viewer preferences");
        let loaded = load_viewer_preferences_from_path(&path);

        assert_eq!(loaded.width, OVERLAY_MIN_WIDTH);
        assert_eq!(loaded.height, OVERLAY_MIN_HEIGHT);
        assert_eq!(loaded.x, Some(42.0));
        assert_eq!(loaded.y, Some(64.0));
        assert!(!loaded.screen_stream);
        assert_eq!(loaded.footer_mode, FooterMode::Task);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn viewer_preferences_normalize_invalid_position() {
        let loaded = normalize_viewer_preferences(ViewerPreferences {
            width: f32::NAN,
            height: f32::INFINITY,
            screen_stream: true,
            footer_mode: FooterMode::Isolation,
            x: Some(f32::NAN),
            y: Some(12.0),
        });

        assert_eq!(loaded.width, OVERLAY_WIDTH);
        assert_eq!(loaded.height, OVERLAY_HEIGHT);
        assert_eq!(loaded.footer_mode, FooterMode::Isolation);
        assert_eq!(loaded.x, None);
        assert_eq!(loaded.y, Some(12.0));
    }

    #[test]
    fn viewer_preferences_clamp_position_to_visible_bounds() {
        let origin = clamp_overlay_origin_to_bounds(
            900.0,
            -20.0,
            size(px(360.0), px(340.0)),
            Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(800.0), px(600.0)),
            },
        );

        assert_eq!(origin.x.as_f32(), 440.0);
        assert_eq!(origin.y.as_f32(), 0.0);
    }

    #[test]
    fn viewer_preferences_fall_back_when_missing_or_invalid() {
        let dir = temp_test_path("invalid-prefs");
        let path = dir.join("viewer.json");

        assert_eq!(
            load_viewer_preferences_from_path(&path),
            ViewerPreferences::default()
        );
        assert!(
            ViewerPreferences::default().screen_stream,
            "the live screen view is shown by default; background (screen-off) mode is opt-in"
        );

        fs::create_dir_all(&dir).expect("create preference dir");
        fs::write(&path, "{not json").expect("write invalid preferences");
        assert_eq!(
            load_viewer_preferences_from_path(&path),
            ViewerPreferences::default()
        );
        fs::write(
            &path,
            r#"{"width": 400.0, "height": 390.0, "live_refresh": true}"#,
        )
        .expect("write old preferences without footer mode");
        let loaded = load_viewer_preferences_from_path(&path);
        assert_eq!(loaded.footer_mode, FooterMode::Activity);
        assert!(
            loaded.screen_stream,
            "preferences predating the screen_stream field default to the screen shown on"
        );
        fs::write(
            &path,
            r#"{"width": 400.0, "height": 390.0, "screen_stream": false}"#,
        )
        .expect("write background (screen-off) preference");
        assert!(
            !load_viewer_preferences_from_path(&path).screen_stream,
            "an explicit screen_stream=false (optional bg) is honored across launches"
        );
        fs::write(
            &path,
            r#"{"width": 400.0, "height": 390.0, "screen_stream": true}"#,
        )
        .expect("write screen-stream preference");
        assert!(load_viewer_preferences_from_path(&path).screen_stream);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn background_flag_forces_screen_off_even_when_preference_is_on() {
        // Screen shown by default.
        assert!(initial_screen_stream(false, true));
        // "optional bg": the saved preference turns the screen off.
        assert!(!initial_screen_stream(false, false));
        // "always bg": the --background launch flag forces screen off regardless
        // of the saved preference.
        assert!(!initial_screen_stream(true, true));
        assert!(!initial_screen_stream(true, false));
    }

    #[test]
    fn paused_screen_stream_keeps_last_running_frame() {
        assert!(
            should_keep_paused_frame(true, true, false),
            "pausing screen stream should keep the last frame while the workspace is still running"
        );
        assert!(
            !should_keep_paused_frame(true, false, false),
            "there is no still frame to preserve before the first capture"
        );
        assert!(
            !should_keep_paused_frame(false, true, false),
            "stopped workspaces should clear stale frames"
        );
        assert!(
            !should_keep_paused_frame(true, true, true),
            "active streaming replaces the frame with a fresh capture"
        );
    }

    #[test]
    fn mcp_bound_viewer_args_request_exit_when_workspace_gone() {
        let args = viewer_command_args(&ViewerOptions {
            id: "qa".to_string(),
            permissions: McpPermissionState::default(),
            always_on_top: true,
            input_forwarding: false,
            exit_when_workspace_gone: true,
            background: false,
        });

        assert_eq!(
            args,
            vec![
                "viewer",
                "--id",
                "qa",
                "--always-on-top",
                "--exit-when-workspace-gone"
            ]
        );
    }

    #[test]
    fn input_forwarding_viewer_args_are_explicit() {
        let args = viewer_command_args(&ViewerOptions {
            id: "qa".to_string(),
            permissions: McpPermissionState::default(),
            always_on_top: false,
            input_forwarding: true,
            exit_when_workspace_gone: false,
            background: false,
        });

        assert_eq!(args, vec!["viewer", "--id", "qa", "--input-forwarding"]);
    }

    #[test]
    fn input_forwarding_coordinate_mapping_handles_cover_fit() {
        let matching = Bounds {
            origin: point(px(10.0), px(20.0)),
            size: size(px(200.0), px(100.0)),
        };
        assert_eq!(
            screen_position_to_workspace_point(100, 50, matching, point(px(110.0), px(70.0))),
            Some(WorkspacePoint { x: 50, y: 25 })
        );
        assert_eq!(
            screen_position_to_workspace_point(100, 50, matching, point(px(9.0), px(70.0))),
            None
        );

        let horizontal_crop = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(200.0), px(100.0)),
        };
        assert_eq!(
            screen_position_to_workspace_point(
                100,
                100,
                horizontal_crop,
                point(px(100.0), px(0.0))
            ),
            Some(WorkspacePoint { x: 50, y: 25 })
        );

        let vertical_crop = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(100.0), px(200.0)),
        };
        assert_eq!(
            screen_position_to_workspace_point(100, 100, vertical_crop, point(px(0.0), px(100.0))),
            Some(WorkspacePoint { x: 25, y: 50 })
        );
    }

    #[test]
    fn screen_bounds_tracking_recovers_from_poisoned_mutex() {
        let tracked_bounds = Arc::new(Mutex::new(None));
        let poisoned_bounds = tracked_bounds.clone();
        let old_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poison_result = std::panic::catch_unwind(move || {
            let _guard = poisoned_bounds.lock().expect("lock before poison");
            panic!("poison tracked bounds");
        });
        std::panic::set_hook(old_hook);
        assert!(poison_result.is_err());

        let next_bounds = Bounds {
            origin: point(px(1.0), px(2.0)),
            size: size(px(3.0), px(4.0)),
        };
        store_screen_bounds(&tracked_bounds, Some(next_bounds));
        assert_eq!(screen_bounds_snapshot(&tracked_bounds), Some(next_bounds));
    }

    #[test]
    fn paste_keystroke_does_not_intercept_shift_modified_terminal_paste() {
        let ctrl_v = gpui::Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            key: "v".to_string(),
            key_char: None,
        };
        assert!(is_paste_keystroke(&ctrl_v));

        let ctrl_shift_v = gpui::Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            key: "v".to_string(),
            key_char: None,
        };
        assert!(!is_paste_keystroke(&ctrl_shift_v));
    }

    #[test]
    fn xdotool_key_normalization_covers_forwarded_keyboard_edges() {
        assert_eq!(normalize_xdotool_key(" F1 "), Some("F1".to_string()));
        assert_eq!(normalize_xdotool_key("f12"), Some("F12".to_string()));
        assert_eq!(
            normalize_xdotool_key("page_down"),
            Some("Page_Down".to_string())
        );
        assert_eq!(normalize_xdotool_key("unknown-key"), None);
        assert_eq!(normalize_xdotool_key("  "), None);

        let chord = gpui::Keystroke {
            modifiers: gpui::Modifiers {
                control: true,
                alt: true,
                shift: true,
                platform: true,
                ..Default::default()
            },
            key: " F12 ".to_string(),
            key_char: None,
        };
        assert_eq!(
            xdotool_key_for_keystroke(&chord),
            Some("ctrl+alt+shift+super+F12".to_string())
        );
    }

    #[test]
    fn input_forwarding_queue_keeps_recent_requests_with_fixed_cap() {
        let mut queue = VecDeque::new();
        for x in 0..MAX_INPUT_FORWARDING_QUEUE_LEN {
            let dropped = push_bounded_input_forwarding_request(
                &mut queue,
                (
                    7,
                    "qa".to_string(),
                    InputForwardingRequest::MovePointer { x: x as i32, y: 1 },
                ),
            );
            assert!(!dropped);
        }

        let dropped = push_bounded_input_forwarding_request(
            &mut queue,
            (
                7,
                "qa".to_string(),
                InputForwardingRequest::Click {
                    x: 999,
                    y: 1,
                    button: 1,
                },
            ),
        );

        assert!(dropped);
        assert_eq!(queue.len(), MAX_INPUT_FORWARDING_QUEUE_LEN);
        assert!(matches!(
            queue.front(),
            Some((7, target, InputForwardingRequest::MovePointer { x: 1, y: 1 }))
                if target == "qa"
        ));
        assert!(matches!(
            queue.back(),
            Some((7, target, InputForwardingRequest::Click { x: 999, y: 1, button: 1 }))
                if target == "qa"
        ));
    }

    #[test]
    fn host_clipboard_size_message_names_workspace_paste_boundary() {
        let byte_len = workspace::MAX_CLIPBOARD_TEXT_BYTES + 1;
        let message = host_clipboard_too_large_message(byte_len);

        assert!(message.contains("Host clipboard text"));
        assert!(message.contains(&byte_len.to_string()));
        assert!(message.contains(&workspace::MAX_CLIPBOARD_TEXT_BYTES.to_string()));
        assert!(message.contains("workspace paste"));
    }

    #[test]
    fn scroll_wheel_to_workspace_scroll_uses_named_semantic_limits() {
        assert_eq!(SCROLL_PIXELS_PER_WORKSPACE_TICK, 80.0);
        assert_eq!(MAX_WORKSPACE_SCROLL_TICKS, 12.0);

        let small_scroll =
            scroll_wheel_to_workspace_scroll(ScrollDelta::Pixels(point(px(1.0), px(0.0))));
        assert!(matches!(
            small_scroll,
            Some((workspace::ScrollDirection::Left, 1))
        ));
        let large_scroll =
            scroll_wheel_to_workspace_scroll(ScrollDelta::Pixels(point(px(960.0), px(0.0))));
        assert!(matches!(
            large_scroll,
            Some((workspace::ScrollDirection::Left, 12))
        ));
    }

    #[test]
    fn viewer_config_dir_prefers_xdg_then_home() {
        assert_eq!(
            viewer_config_dir_from_env(
                Some(OsString::from("/xdg")),
                Some(OsString::from("/home/me"))
            ),
            PathBuf::from("/xdg/agent-workspace-linux")
        );
        assert_eq!(
            viewer_config_dir_from_env(None, Some(OsString::from("/home/me"))),
            PathBuf::from("/home/me/.config/agent-workspace-linux")
        );
    }

    fn policy(
        network: NetworkPolicy,
        mounts: Vec<ProfileMount>,
        require_full_enforcement: bool,
        bubblewrap: bool,
    ) -> AppliedWorkspacePolicy {
        AppliedWorkspacePolicy::new_with_capabilities(
            "qa".to_string(),
            mounts,
            network,
            require_full_enforcement,
            0,
            capabilities(bubblewrap),
        )
    }

    fn app(id: &str, name: &str, running: bool) -> workspace::WorkspaceApp {
        workspace::WorkspaceApp {
            id: id.to_string(),
            name: Some(name.to_string()),
            pid: 42,
            process_group_id: None,
            profile_id: Some("qa".to_string()),
            mount_isolation: "host".to_string(),
            network_isolation: "host".to_string(),
            command: vec![name.to_string()],
            cwd: None,
            env: Vec::new(),
            stdout_path: None,
            stderr_path: None,
            started_at_unix: 1,
            running,
            exit_status: None,
            exit_code: None,
            exit_signal: None,
            stopped_at_unix: None,
            runtime_seconds: None,
        }
    }

    fn status(
        id: &str,
        display: &str,
        purpose: &str,
        profile_id: &str,
        apps: Vec<workspace::WorkspaceApp>,
    ) -> workspace::WorkspaceStatus {
        workspace::WorkspaceStatus {
            id: id.to_string(),
            session_id: format!("session-{id}"),
            purpose: Some(purpose.to_string()),
            profile_id: Some(profile_id.to_string()),
            applied_policy: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            user_acknowledged_unenforced_policy: false,
            ready: true,
            started_at_unix: 1,
            display: display.to_string(),
            width: 1280,
            height: 800,
            runtime_dir: PathBuf::from(format!("/tmp/{id}")),
            socket_path: PathBuf::from(format!("/tmp/{id}/control.sock")),
            xauthority_path: PathBuf::from(format!("/tmp/{id}/Xauthority")),
            daemon_pid: None,
            x_server_pid: 0,
            window_manager_pid: None,
            last_event_sequence: 1,
            apps,
        }
    }

    fn viewer_profile(
        id: &str,
        network: NetworkPolicy,
        mounts: Vec<ProfileMount>,
        startup_command: Vec<&str>,
    ) -> profile::WorkspaceProfile {
        profile::WorkspaceProfile {
            id: id.to_string(),
            description: None,
            width: None,
            height: None,
            cwd: None,
            env: Vec::new(),
            mounts,
            network,
            require_enforced_policy: false,
            setup_commands: Vec::new(),
            startup_apps: vec![profile::ProfileStartupApp {
                name: Some("app".to_string()),
                command: startup_command
                    .into_iter()
                    .map(|part| part.to_string())
                    .collect(),
                cwd: None,
                env: Vec::new(),
            }],
        }
    }

    fn viewer_start_options_for_profile(
        profile: &profile::WorkspaceProfile,
    ) -> workspace::WorkspaceStartOptions {
        workspace::WorkspaceStartOptions {
            id: "viewer-test".to_string(),
            profile_id: Some(profile.id.clone()),
            applied_policy: Some(profile::applied_policy(profile)),
            user_acknowledged_hidden_workspace: true,
            ..Default::default()
        }
    }

    #[test]
    fn footer_mode_cycles_between_contexts() {
        assert_eq!(FooterMode::Activity.next(), FooterMode::Task);
        assert_eq!(FooterMode::Task.next(), FooterMode::Isolation);
        assert_eq!(FooterMode::Isolation.next(), FooterMode::Apps);
        assert_eq!(FooterMode::Apps.next(), FooterMode::Activity);
    }

    #[test]
    fn destructive_actions_use_distinct_busy_labels() {
        let clean = ViewerAction::CleanStopped {
            id: "qa".to_string(),
        };
        let revoke = ViewerAction::RevokeRunning {
            id: "qa".to_string(),
        };

        assert_eq!(clean.label(), "Cleaning stopped workspace");
        assert_eq!(clean.button_label(), "Cleaning");
        assert_eq!(revoke.label(), "Revoking workspace");
        assert_eq!(revoke.button_label(), "Revoking");
    }

    #[test]
    fn control_tooltip_names_reactivation_confirmation() {
        let active = mcp_control_tooltip(McpControlMode::Active);
        assert!(active.contains("MCP control is active"));
        assert!(active.contains("switching back to active is an explicit user control action"));

        let read_only_action =
            mcp_control_action_tooltip(McpControlMode::Active, McpControlMode::ReadOnly);
        assert!(read_only_action.contains("Read-only"));
        assert!(read_only_action.contains("block mutating agent actions"));

        let run_action = mcp_control_action_tooltip(McpControlMode::Paused, McpControlMode::Active);
        assert!(run_action.contains("Run"));
        assert!(run_action.contains("mutating agent actions can continue"));

        let paused = mcp_control_tooltip(McpControlMode::Paused);
        assert!(paused.contains("MCP control is paused"));
        assert!(paused.contains("switching back to active"));
    }

    #[test]
    fn control_segment_exposes_direct_user_choices() {
        assert_eq!(
            control_segment_labels(McpControlMode::Active),
            vec!["RO", "Pause"]
        );
        assert_eq!(
            control_segment_labels(McpControlMode::ReadOnly),
            vec!["Run", "Pause"]
        );
        assert_eq!(
            control_segment_labels(McpControlMode::Paused),
            vec!["Run", "RO"]
        );
    }

    #[test]
    fn viewer_clean_permissions_do_not_add_a_profile_ceiling() {
        let profile = viewer_profile(
            "clean",
            NetworkPolicy::default(),
            vec![ProfileMount {
                host_path: PathBuf::from("/home/me/project"),
                workspace_path: PathBuf::from("/workspace/project"),
                mode: MountMode::ReadWrite,
            }],
            vec!["bash", "-lc", "echo ok"],
        );
        let options = viewer_start_options_for_profile(&profile);

        validate_viewer_profile_start_permissions(
            &McpPermissionState::default(),
            &profile,
            &options,
        )
        .unwrap();
    }

    #[test]
    fn viewer_profile_start_respects_explicit_permission_ceiling() {
        let profile = viewer_profile(
            "locked",
            NetworkPolicy::default(),
            Vec::new(),
            vec!["bash", "-lc", "echo ok"],
        );
        let options = viewer_start_options_for_profile(&profile);
        let permissions = McpPermissionState::from_ceiling(
            Some(PathBuf::from("/tmp/permissions.json")),
            crate::permissions::McpPermissionCeiling {
                network: None,
                mounts: Vec::new(),
                apps: crate::permissions::AppPermissionCeiling {
                    allow: vec![PathBuf::from("xterm")],
                },
            },
        );

        let error = validate_viewer_profile_start_permissions(&permissions, &profile, &options)
            .unwrap_err();
        let details = format!("{error:#}");

        assert!(details.contains("viewer profile locked exceeds MCP permission ceiling"));
        assert!(details.contains("profile startup app program \"bash\" is not allowed"));
    }

    #[test]
    fn x11_viewer_default_does_not_request_always_on_top_state() {
        let default_atoms = x11_viewer_state_atom_names(false)
            .into_iter()
            .map(x11_atom_label)
            .collect::<Vec<_>>();
        assert_eq!(
            default_atoms,
            vec!["_NET_WM_STATE_SKIP_TASKBAR", "_NET_WM_STATE_SKIP_PAGER"]
        );

        let always_on_top_atoms = x11_viewer_state_atom_names(true)
            .into_iter()
            .map(x11_atom_label)
            .collect::<Vec<_>>();
        assert!(always_on_top_atoms.contains(&"_NET_WM_STATE_ABOVE".to_string()));
        assert!(always_on_top_atoms.contains(&"_NET_WM_STATE_STICKY".to_string()));
        assert!(always_on_top_atoms.contains(&"_NET_WM_STATE_SKIP_TASKBAR".to_string()));
        assert!(always_on_top_atoms.contains(&"_NET_WM_STATE_SKIP_PAGER".to_string()));

        let default_types = x11_viewer_window_type_atom_names(false)
            .into_iter()
            .map(x11_atom_label)
            .collect::<Vec<_>>();
        assert_eq!(default_types, vec!["_NET_WM_WINDOW_TYPE_UTILITY"]);

        let always_on_top_types = x11_viewer_window_type_atom_names(true)
            .into_iter()
            .map(x11_atom_label)
            .collect::<Vec<_>>();
        assert!(always_on_top_types.contains(&"_NET_WM_WINDOW_TYPE_NOTIFICATION".to_string()));
        assert!(always_on_top_types.contains(&"_NET_WM_WINDOW_TYPE_UTILITY".to_string()));
    }

    #[test]
    fn layer_shell_viewer_requests_keyboard_only_for_input_forwarding() {
        let read_only = layer_shell_window_options(false);
        let input_capable = layer_shell_window_options(true);

        let WindowKind::LayerShell(read_only_layer) = read_only.kind else {
            panic!("expected layer shell options");
        };
        let WindowKind::LayerShell(input_capable_layer) = input_capable.kind else {
            panic!("expected layer shell options");
        };

        assert_eq!(
            read_only_layer.keyboard_interactivity,
            KeyboardInteractivity::None
        );
        assert_eq!(
            input_capable_layer.keyboard_interactivity,
            KeyboardInteractivity::OnDemand
        );
    }

    #[test]
    fn viewer_registry_key_sanitizes_workspace_id() {
        assert_eq!(
            sanitize_viewer_registry_component("project/default:qa"),
            "project_default_qa"
        );
        assert_eq!(sanitize_viewer_registry_component(""), "default");
    }

    #[test]
    fn viewer_registry_path_separates_bound_and_free_viewers() {
        let free = viewer_registry_path("qa", ViewerBackend::X11Popup, false, false, false);
        let bound = viewer_registry_path("qa", ViewerBackend::X11Popup, false, false, true);
        let input = viewer_registry_path("qa", ViewerBackend::X11Popup, false, true, true);

        assert_ne!(free, bound);
        assert_ne!(bound, input);
        assert!(free
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-free.json")));
        assert!(bound
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-bound.json")));
        assert!(input
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("-rw-")));
    }

    #[test]
    fn best_viewer_launch_for_id_reuses_one_existing_workspace_viewer() {
        let executable = PathBuf::from("/tmp/agent-workspace-linux");
        let normal_bound = viewer_registry_entry(
            "qa",
            42,
            ViewerBackend::X11Popup,
            false,
            false,
            true,
            executable.clone(),
            vec![
                "/tmp/agent-workspace-linux".to_string(),
                "viewer".to_string(),
                "--id".to_string(),
                "qa".to_string(),
                "--exit-when-workspace-gone".to_string(),
            ],
        );
        let topmost_bound = viewer_registry_entry(
            "qa",
            43,
            ViewerBackend::X11Popup,
            true,
            false,
            true,
            executable.clone(),
            vec![
                "/tmp/agent-workspace-linux".to_string(),
                "viewer".to_string(),
                "--id".to_string(),
                "qa".to_string(),
                "--always-on-top".to_string(),
                "--exit-when-workspace-gone".to_string(),
            ],
        );
        let normal_free = viewer_registry_entry(
            "qa",
            44,
            ViewerBackend::X11Popup,
            false,
            false,
            false,
            executable,
            vec![
                "/tmp/agent-workspace-linux".to_string(),
                "viewer".to_string(),
                "--id".to_string(),
                "qa".to_string(),
            ],
        );
        let other = viewer_registry_entry(
            "other",
            45,
            ViewerBackend::X11Popup,
            false,
            false,
            true,
            PathBuf::from("/tmp/agent-workspace-linux"),
            vec![
                "/tmp/agent-workspace-linux".to_string(),
                "viewer".to_string(),
                "--id".to_string(),
                "other".to_string(),
                "--exit-when-workspace-gone".to_string(),
            ],
        );

        let launch = best_viewer_launch_for_id(
            "qa",
            vec![
                (PathBuf::from("/tmp/topmost.json"), topmost_bound),
                (PathBuf::from("/tmp/free.json"), normal_free),
                (PathBuf::from("/tmp/other.json"), other),
                (PathBuf::from("/tmp/normal.json"), normal_bound),
            ],
        )
        .expect("expected existing qa viewer");

        assert_eq!(launch.id, "qa");
        assert_eq!(launch.pid, 42);
        assert!(!launch.always_on_top);
        assert!(!launch.input_forwarding);
        assert!(launch.exit_when_workspace_gone);
        assert!(launch.reused);
        assert_eq!(
            launch.registry_path,
            Some(PathBuf::from("/tmp/normal.json"))
        );
    }

    #[test]
    fn bound_target_missing_only_when_selected_workspace_is_absent() {
        let entry = workspace::WorkspaceListEntry {
            id: "qa".to_string(),
            runtime_dir: PathBuf::from("/tmp/qa"),
            socket_path: PathBuf::from("/tmp/qa/control.sock"),
            running: true,
            manifest: None,
            manifest_error: None,
            status: None,
            error: None,
        };

        assert!(!bound_target_id_missing(
            Some("qa"),
            std::slice::from_ref(&entry)
        ));
        assert!(bound_target_id_missing(Some("other"), &[entry]));
        assert!(!bound_target_id_missing(None, &[]));
    }

    #[test]
    fn viewer_cmdline_match_requires_viewer_subcommand_and_workspace_id() {
        let args = vec![
            "/tmp/agent-workspace-linux".to_string(),
            "viewer".to_string(),
            "--id".to_string(),
            "qa".to_string(),
        ];
        assert!(viewer_cmdline_matches("qa", false, false, false, &args));
        assert!(!viewer_cmdline_matches("other", false, false, false, &args));
        assert!(!viewer_cmdline_matches("qa", true, false, false, &args));
        assert!(!viewer_cmdline_matches("qa", false, true, false, &args));
        assert!(!viewer_cmdline_matches("qa", false, false, true, &args));
        assert!(!viewer_cmdline_matches(
            "qa",
            false,
            false,
            false,
            &["/tmp/agent-workspace-linux".to_string()]
        ));

        let mut topmost_args = args.clone();
        topmost_args.push("--always-on-top".to_string());
        assert!(viewer_cmdline_matches(
            "qa",
            true,
            false,
            false,
            &topmost_args
        ));
        assert!(!viewer_cmdline_matches(
            "qa",
            false,
            false,
            false,
            &topmost_args
        ));

        let mut bound_args = args.clone();
        bound_args.push("--exit-when-workspace-gone".to_string());
        assert!(viewer_cmdline_matches(
            "qa",
            false,
            false,
            true,
            &bound_args
        ));
        assert!(!viewer_cmdline_matches(
            "qa",
            false,
            false,
            false,
            &bound_args
        ));

        let mut input_args = args.clone();
        input_args.push("--input-forwarding".to_string());
        assert!(viewer_cmdline_matches(
            "qa",
            false,
            true,
            false,
            &input_args
        ));
        assert!(!viewer_cmdline_matches(
            "qa",
            false,
            false,
            false,
            &input_args
        ));
    }

    #[test]
    fn viewer_executable_match_accepts_same_binary_name_for_symlinked_invocations() {
        assert!(viewer_executable_matches(
            "target/debug/agent-workspace-linux",
            Path::new("/tmp/build/agent-workspace-linux")
        ));
        assert!(!viewer_executable_matches(
            "target/debug/other-binary",
            Path::new("/tmp/build/agent-workspace-linux")
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_child_spawn_is_reaped() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        let pid = spawn_reaped_child(&mut command).expect("spawn short-lived child");
        let proc_path = PathBuf::from(format!("/proc/{pid}"));

        for _ in 0..100 {
            if !proc_path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        panic!("detached child {pid} was not reaped");
    }

    #[test]
    fn permission_ceiling_label_summarizes_open_and_restricted_states() {
        let open = McpPermissionState::default();
        assert_eq!(permission_ceiling_label(&open), "MCP ceiling: open");

        let restricted = McpPermissionState::from_ceiling(
            Some(PathBuf::from("/tmp/permissions.json")),
            crate::permissions::McpPermissionCeiling {
                network: Some(NetworkPolicy {
                    mode: NetworkMode::Disabled,
                    allow_hosts: Vec::new(),
                }),
                mounts: vec![ProfileMount {
                    host_path: PathBuf::from("/tmp/project"),
                    workspace_path: PathBuf::from("/workspace/project"),
                    mode: MountMode::ReadOnly,
                }],
                apps: crate::permissions::AppPermissionCeiling {
                    allow: vec![PathBuf::from("/bin/sh")],
                },
            },
        );

        assert_eq!(
            permission_ceiling_label(&restricted),
            "MCP ceiling: net off, mounts 1, apps 1"
        );
    }

    #[test]
    fn policy_labels_show_enforced_network_and_mount_state() {
        let policy = policy(
            NetworkPolicy {
                mode: NetworkMode::Disabled,
                allow_hosts: Vec::new(),
            },
            vec![ProfileMount {
                host_path: PathBuf::from("/tmp/project"),
                workspace_path: PathBuf::from("/workspace/project"),
                mode: MountMode::ReadOnly,
            }],
            false,
            true,
        );

        assert_eq!(network_policy_label(&policy), "Net: off");
        assert_eq!(mount_policy_label(&policy), "Mounts: 1 scoped");
        assert_eq!(policy_acknowledgement_label(&policy, false), "");
    }

    #[test]
    fn policy_labels_call_out_unenforced_acknowledgement_state() {
        let policy = policy(
            NetworkPolicy {
                mode: NetworkMode::LocalOnly,
                allow_hosts: Vec::new(),
            },
            Vec::new(),
            false,
            false,
        );

        assert_eq!(network_policy_label(&policy), "Net: local declared");
        assert_eq!(
            policy_acknowledgement_label(&policy, false),
            " | Needs policy ack"
        );
        assert_eq!(
            policy_acknowledgement_label(&policy, true),
            " | Limits acknowledged"
        );
    }

    #[test]
    fn apps_footer_names_active_and_running_apps() {
        let apps = vec![app("app-1", "xclock", true), app("app-2", "setup", false)];
        let active_window = workspace::WorkspaceWindow {
            id: "0x1".to_string(),
            title: "xclock".to_string(),
            wm_class: Some("XClock".to_string()),
            wm_instance: None,
            pid: Some(42),
            app_id: Some("app-1".to_string()),
            visible: true,
            geometry: workspace::WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                screen: None,
            },
        };

        let label = footer_apps_label(true, Some(&apps), Some(&active_window));
        assert!(label.contains("Active app: xclock"));
        assert!(label.contains("Running: xclock"));
        assert!(label.contains("1 stopped"));
    }

    #[test]
    fn task_footer_infers_browser_and_app_qa_context() {
        let browser_apps = vec![app("app-1", "google-chrome", true)];
        let browser_entry = workspace::WorkspaceListEntry {
            id: "browser".to_string(),
            runtime_dir: PathBuf::from("/tmp/browser"),
            socket_path: PathBuf::from("/tmp/browser/control.sock"),
            running: true,
            manifest: None,
            manifest_error: None,
            status: Some(status(
                "browser",
                ":90",
                "Grocery shopping",
                "browser-session",
                browser_apps.clone(),
            )),
            error: None,
        };
        let browser_label = footer_task_label(
            Some(&browser_entry),
            true,
            true,
            &[],
            "Default workspace",
            None,
            Some(&browser_apps),
        );
        assert!(browser_label.contains("Task: Browser/shopping"));
        assert!(browser_label.contains("Running app: google-chrome"));

        let qa_apps = vec![app("app-2", "xterm-qa", true)];
        let qa_entry = workspace::WorkspaceListEntry {
            id: "qa".to_string(),
            runtime_dir: PathBuf::from("/tmp/qa"),
            socket_path: PathBuf::from("/tmp/qa/control.sock"),
            running: true,
            manifest: None,
            manifest_error: None,
            status: Some(status(
                "qa",
                ":91",
                "Project QA",
                "project-dev",
                qa_apps.clone(),
            )),
            error: None,
        };
        let qa_label = footer_task_label(
            Some(&qa_entry),
            true,
            true,
            &[],
            "Default workspace",
            None,
            Some(&qa_apps),
        );
        assert!(qa_label.contains("Task: App QA"));
        assert!(qa_label.contains("Running app: xterm-qa"));

        assert_eq!(
            footer_task_label(None, false, true, &[], "project-dev", None, None),
            "Task: ready to start project-dev"
        );
    }

    #[test]
    fn activity_footer_names_workspace_browser_events() {
        let apps = vec![app("app-1", "google-chrome", true)];
        let snapshot = workspace::WorkspaceEvent {
            sequence: 10,
            timestamp_unix: 100,
            kind: "browser_snapshot".to_string(),
            detail: serde_json::json!({
                "app_id": "app-1",
                "title": "Amazon GPU Search Results",
                "url": "https://www.amazon.com/s?k=rtx+pro+5000"
            }),
        };
        let snapshot_activity =
            activity_from_event(&snapshot, &apps, None).expect("snapshot activity");
        assert_eq!(
            snapshot_activity.label,
            "Read browser page: Amazon GPU Search Results"
        );

        let results = workspace::WorkspaceEvent {
            sequence: 12,
            timestamp_unix: 102,
            kind: "browser_search_results".to_string(),
            detail: serde_json::json!({
                "app_id": "app-1",
                "title": "Amazon GPU Search Results",
                "url": "https://www.amazon.com/s?k=rtx+pro+5000",
                "result_count": 16
            }),
        };
        let results_activity =
            activity_from_event(&results, &apps, None).expect("results activity");
        assert_eq!(
            results_activity.label,
            "Found 16 browser results: Amazon GPU Search Results"
        );

        let compatibility_results = workspace::WorkspaceEvent {
            sequence: 13,
            timestamp_unix: 103,
            kind: "browser_snapshot".to_string(),
            detail: serde_json::json!({
                "app_id": "app-1",
                "browser_action": "browser_search_results",
                "original_event_kind": "browser_search_results",
                "title": "Amazon GPU Search Results",
                "url": "https://www.amazon.com/s?k=rtx+pro+5000",
                "result_count": 16
            }),
        };
        let compatibility_activity =
            activity_from_event(&compatibility_results, &apps, None).expect("compat activity");
        assert_eq!(
            compatibility_activity.label,
            "Found 16 browser results: Amazon GPU Search Results"
        );

        let navigate = workspace::WorkspaceEvent {
            sequence: 11,
            timestamp_unix: 101,
            kind: "browser_navigate".to_string(),
            detail: serde_json::json!({
                "app_id": "app-1",
                "url": "https://www.amazon.com/s?k=rtx+pro+6000+96gb",
                "current_url": "https://www.amazon.com/s?k=rtx+pro+6000+96gb"
            }),
        };
        let navigate_activity =
            activity_from_event(&navigate, &apps, None).expect("navigate activity");
        assert_eq!(
            navigate_activity.label,
            "Navigated browser to www.amazon.com/s"
        );
    }

    #[test]
    fn app_log_target_follows_active_window_and_prefers_useful_stream() {
        let dir = env::temp_dir().join(format!(
            "agent-workspace-viewer-log-test-{}-{}",
            std::process::id(),
            wall_clock_seconds()
        ));
        fs::create_dir_all(&dir).expect("create temp log dir");
        let alpha_out = dir.join("alpha.out.log");
        let beta_out = dir.join("beta.out.log");
        let beta_err = dir.join("beta.err.log");
        fs::write(&alpha_out, "alpha stdout\n").expect("write alpha stdout");
        fs::write(&beta_out, "").expect("write beta stdout");
        fs::write(&beta_err, "beta stderr\n").expect("write beta stderr");

        let mut alpha = app("app-alpha", "alpha", true);
        alpha.stdout_path = Some(alpha_out);
        let mut beta = app("app-beta", "beta", true);
        beta.started_at_unix = 2;
        beta.stdout_path = Some(beta_out);
        beta.stderr_path = Some(beta_err);
        let apps = vec![alpha, beta];
        let active_window = workspace::WorkspaceWindow {
            id: "0x2".to_string(),
            title: "beta".to_string(),
            wm_class: None,
            wm_instance: None,
            pid: Some(42),
            app_id: Some("app-beta".to_string()),
            visible: true,
            geometry: workspace::WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                screen: None,
            },
        };

        let target =
            selected_app_log_target(Some(&apps), Some(&active_window)).expect("app log target");
        assert_eq!(target.app_id, "app-beta");
        assert_eq!(target.stream, "stderr");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn event_log_artifact_finds_workspace_event_log() {
        let artifacts = workspace::WorkspaceArtifacts {
            ok: true,
            message: "ok".to_string(),
            id: "qa".to_string(),
            runtime_dir: PathBuf::from("/tmp/qa"),
            files: vec![
                workspace::WorkspaceArtifact {
                    kind: "manifest".to_string(),
                    label: "workspace manifest".to_string(),
                    path: PathBuf::from("/tmp/qa/manifest.json"),
                    exists: true,
                    file_type: Some("file".to_string()),
                    bytes: Some(10),
                },
                workspace::WorkspaceArtifact {
                    kind: "event_log".to_string(),
                    label: "workspace event log".to_string(),
                    path: PathBuf::from("/tmp/qa/events.jsonl"),
                    exists: true,
                    file_type: Some("file".to_string()),
                    bytes: Some(20),
                },
            ],
            manifest_error: None,
        };

        let event_log = event_log_artifact(&artifacts).expect("event log artifact");
        assert_eq!(event_log.path, PathBuf::from("/tmp/qa/events.jsonl"));
    }
}
