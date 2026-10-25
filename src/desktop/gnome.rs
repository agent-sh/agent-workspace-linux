#[path = "gnome_text.rs"]
mod text_input;
use super::{resolve_screenshot_path, resolve_window_screenshot_path, DesktopControl};
use crate::workspace::{
    validate_click_options, validate_clipboard_text, validate_scroll_options,
    validate_window_size_for_workspace, validate_workspace_coordinates,
    workspace_screenshot_result, ScrollDirection, WorkspaceClipboard, WorkspacePointer,
    WorkspaceScreenshot, WorkspaceScreenshotSource, WorkspaceScreenshotTarget, WorkspaceStatus,
    WorkspaceWindow,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) fn call(runtime: &Path, request: Value) -> Result<Value> {
    let connection = zbus::blocking::connection::Builder::address(
        crate::session::kwin::bus_address(runtime).as_str(),
    )?
    .method_timeout(Duration::from_secs(5))
    .build()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.agent_sh.Workspace.Gnome",
        "/org/agent_sh/Workspace/Gnome",
        "org.agent_sh.Workspace.Gnome",
    )?;
    let reply: String = proxy.call("Command", &(serde_json::to_string(&request)?,))?;
    let reply: Value = serde_json::from_str(&reply)?;
    if reply["ok"] != true {
        bail!("GNOME control: {}", reply["error"]);
    }
    Ok(reply["value"].clone())
}

type Properties = HashMap<String, zbus::zvariant::OwnedValue>;
type MonitorSpec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Properties);
type Monitor = (MonitorSpec, Vec<Mode>, Properties);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, Properties);
pub(crate) fn apply_scale(runtime: &Path, scale: f64) -> Result<()> {
    let connection = zbus::blocking::connection::Builder::address(
        crate::session::kwin::bus_address(runtime).as_str(),
    )?
    .method_timeout(Duration::from_secs(5))
    .build()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )?;
    let (serial, monitors, logical, _properties): (
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        Properties,
    ) = proxy.call("GetCurrentState", &())?;
    if monitors.len() != 1 {
        bail!("GNOME must expose exactly one private monitor");
    }
    if logical.len() == 1 && (logical[0].2 - scale).abs() < 0.001 {
        return Ok(());
    }
    let monitor = &monitors[0];
    let mode = monitor
        .1
        .iter()
        .find(|mode| {
            mode.6
                .get("is-current")
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false)
        })
        .context("GNOME current monitor mode missing")?;
    if !mode
        .5
        .iter()
        .any(|supported| (supported - scale).abs() < 0.001)
    {
        bail!(
            "GNOME monitor does not support scale {scale}; supported scales: {:?}",
            mode.5
        );
    }
    let properties: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    let outputs = vec![(monitor.0 .0.as_str(), mode.0.as_str(), &properties)];
    let logical = vec![(0i32, 0i32, scale, 0u32, true, outputs)];
    let _: () = proxy.call("ApplyMonitorsConfig", &(serial, 1u32, logical, &properties))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if call(runtime, json!({"op":"probe"}))?["scale"]
            .as_f64()
            .is_some_and(|v| (v - scale).abs() < 0.001)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("GNOME did not apply output scale {scale}");
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}
fn frame(runtime: &Path, id: Option<&str>) -> Result<image::RgbaImage> {
    let value = call(runtime, json!({"op":"screenshot","id":id}))?;
    let bytes: Vec<u8> = serde_json::from_value(value["png"].clone())?;
    Ok(
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .with_context(|| format!("decode GNOME screenshot PNG ({} bytes)", bytes.len()))?
            .to_rgba8(),
    )
}
pub(crate) fn probe_capture(runtime: &Path) -> Result<(u32, u32)> {
    Ok(frame(runtime, None)?.dimensions())
}

pub(crate) struct GnomeDesktopControl<'a> {
    status: &'a WorkspaceStatus,
}
impl<'a> GnomeDesktopControl<'a> {
    pub fn new(status: &'a WorkspaceStatus) -> Self {
        Self { status }
    }
    fn call(&self, request: Value) -> Result<Value> {
        call(&self.status.runtime_dir, request)
    }
    fn mutate(&self, op: &str, id: &str) -> Result<()> {
        self.call(json!({"op":op,"id":super::sanitize_window_id(id)?}))?;
        Ok(())
    }
    fn geometry(
        &self,
        id: &str,
        accept: impl Fn(&crate::workspace::WindowGeometry) -> bool,
    ) -> Result<WorkspaceWindow> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let window = self.window_info(id)?;
            if accept(&window.geometry) {
                return Ok(window);
            }
            if Instant::now() >= deadline {
                bail!(
                    "GNOME did not apply requested geometry: {:?}",
                    window.geometry
                );
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fn capture(
        &self,
        window: Option<&WorkspaceWindow>,
        output: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        let path = match window {
            Some(w) => resolve_window_screenshot_path(self.status, &w.id, output),
            None => resolve_screenshot_path(self.status, output),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut image = frame(&self.status.runtime_dir, window.map(|w| w.id.as_str()))?;
        if let Some(window) = window {
            let g = &window.geometry;
            if image.dimensions() != (g.width, g.height) {
                image = image::imageops::resize(
                    &image,
                    g.width,
                    g.height,
                    image::imageops::FilterType::Triangle,
                );
            }
        }
        let (width, height) = image.dimensions();
        image.save(&path)?;
        workspace_screenshot_result(
            path,
            width,
            height,
            source,
            if window.is_some() {
                WorkspaceScreenshotTarget::Window
            } else {
                WorkspaceScreenshotTarget::Root
            },
            window.map(|w| w.id.clone()),
        )
    }
}
impl DesktopControl for GnomeDesktopControl<'_> {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>> {
        Ok(serde_json::from_value(
            self.call(json!({"op":"windows","hidden":include_hidden}))?,
        )?)
    }
    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>> {
        Ok(self
            .list_windows(!only_visible)?
            .into_iter()
            .map(|w| w.id)
            .collect())
    }
    fn active_window(&self) -> Result<Option<WorkspaceWindow>> {
        Ok(serde_json::from_value(self.call(json!({"op":"active"}))?)?)
    }
    fn window_info(&self, id: &str) -> Result<WorkspaceWindow> {
        Ok(serde_json::from_value(self.call(
            json!({"op":"window","id":super::sanitize_window_id(id)?}),
        )?)?)
    }
    fn pointer(&self) -> Result<WorkspacePointer> {
        Ok(serde_json::from_value(self.call(json!({"op":"pointer"}))?)?)
    }
    fn capture_screenshot(
        &self,
        path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        self.capture(None, path, source)
    }
    fn capture_window_screenshot(
        &self,
        window: &WorkspaceWindow,
        path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        self.capture(Some(window), path, source)
    }
    fn focus_window(&self, id: &str) -> Result<()> {
        self.mutate("focus", id)
    }
    fn close_window(&self, id: &str) -> Result<()> {
        self.mutate("close", id)
    }
    fn move_window(&self, id: &str, x: i32, y: i32) -> Result<WorkspaceWindow> {
        self.call(json!({"op":"move","id":super::sanitize_window_id(id)?,"x":x,"y":y}))?;
        self.geometry(id, |g| (g.x - x).abs() <= 1 && (g.y - y).abs() <= 1)
    }
    fn resize_window(&self, id: &str, width: u32, height: u32) -> Result<WorkspaceWindow> {
        validate_window_size_for_workspace(self.status, width, height)?;
        self.call(json!({"op":"resize","id":super::sanitize_window_id(id)?,"width":width,"height":height}))?;
        self.geometry(id, |g| {
            g.width.abs_diff(width) <= 1 && g.height.abs_diff(height) <= 1
        })
    }
    fn raise_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate("raise", id)?;
        self.window_info(id)
    }
    fn minimize_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate("minimize", id)?;
        self.window_info(id)
    }
    fn show_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate("show", id)?;
        self.window_info(id)
    }
    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
        validate_click_options(button, count)?;
        validate_workspace_coordinates(self.status, x, y, "click")?;
        self.call(json!({"op":"click","x":x,"y":y,"button":button,"count":count}))?;
        Ok(())
    }
    fn move_pointer(&self, x: i32, y: i32) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        self.call(json!({"op":"motion","x":x,"y":y}))?;
        Ok(())
    }
    fn drag(&self, fx: i32, fy: i32, tx: i32, ty: i32, button: u8) -> Result<()> {
        validate_click_options(button, 1)?;
        validate_workspace_coordinates(self.status, fx, fy, "drag start")?;
        validate_workspace_coordinates(self.status, tx, ty, "drag end")?;
        self.call(json!({"op":"drag","fx":fx,"fy":fy,"tx":tx,"ty":ty,"button":button}))?;
        Ok(())
    }
    fn scroll(&self, x: i32, y: i32, direction: ScrollDirection, amount: u8) -> Result<()> {
        validate_scroll_options(direction, amount)?;
        validate_workspace_coordinates(self.status, x, y, "scroll")?;
        let direction = match direction {
            ScrollDirection::Up => 0,
            ScrollDirection::Down => 1,
            ScrollDirection::Left => 2,
            ScrollDirection::Right => 3,
        };
        self.call(json!({"op":"scroll","x":x,"y":y,"direction":direction,"amount":amount}))?;
        Ok(())
    }
    fn key(&self, key: &str) -> Result<()> {
        let keys = super::kwin::keysyms(key)?;
        self.call(json!({"op":"keys","keys":keys}))?;
        Ok(())
    }
    fn type_text(&self, text: &str) -> Result<()> {
        let result = self.call(json!({"op":"text","text":text}))?;
        if result["x11"] == true {
            let window: WorkspaceWindow = serde_json::from_value(result["window"].clone())?;
            if window.wm_class.as_deref().is_some_and(|name| {
                name.eq_ignore_ascii_case("xterm") || name.eq_ignore_ascii_case("uxterm")
            }) {
                // Xterm's defined Shift+Insert binding reads PRIMARY. Keep the
                // ordinary CLIPBOARD untouched; PRIMARY retains the typed text
                // so asynchronous selection reads cannot race restoration.
                self.call(json!({"op":"primary-set","text":text}))?;
                self.key("shift+Insert")?;
            } else {
                let pid = window.pid.context("legacy client PID missing")?;
                text_input::insert(self.status, &window.id, pid, text)?;
            }
        }
        Ok(())
    }
    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard> {
        validate_clipboard_text(text)?;
        self.call(json!({"op":"clipboard-set","text":text}))?;
        Ok(clipboard(text.to_string()))
    }
    fn get_clipboard(&self) -> Result<WorkspaceClipboard> {
        let text = serde_json::from_value(self.call(json!({"op":"clipboard-get"}))?)?;
        Ok(clipboard(text))
    }
}
fn clipboard(text: String) -> WorkspaceClipboard {
    WorkspaceClipboard {
        selection: "clipboard".to_string(),
        bytes: text.len() as u64,
        content: Some(text),
    }
}
