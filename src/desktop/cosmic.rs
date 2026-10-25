//! COSMIC native desktop control. Geometry and confirmations come from the compositor.
use super::{resolve_screenshot_path, resolve_window_screenshot_path, DesktopControl};
use crate::{
    session::cosmic::{configure_process_environment, control},
    workspace::{
        validate_click_options, validate_clipboard_text, validate_scroll_options,
        validate_window_size_for_workspace, validate_workspace_coordinates,
        workspace_screenshot_result, ScrollDirection, WorkspaceClipboard, WorkspacePointer,
        WorkspaceScreenshot, WorkspaceScreenshotSource, WorkspaceScreenshotTarget, WorkspaceStatus,
        WorkspaceWindow,
    },
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub(crate) struct CosmicDesktopControl<'a> {
    status: &'a WorkspaceStatus,
}
impl<'a> CosmicDesktopControl<'a> {
    pub(crate) fn new(status: &'a WorkspaceStatus) -> Self {
        Self { status }
    }
    fn call(&self, operation: &str, args: Value) -> Result<Value> {
        control(&self.status.runtime_dir, operation, &args)
    }
    fn snapshot(&self) -> Result<Value> {
        self.call("snapshot", json!({}))
    }
    fn input(&self, actions: Vec<Value>) -> Result<()> {
        self.call("input", Value::Array(actions))?;
        Ok(())
    }
    fn motion(&self, x: i32, y: i32) -> Value {
        json!({"kind":"motion","x":x as f64/self.status.session.scale,"y":y as f64/self.status.session.scale})
    }
    fn wait_window(
        &self,
        id: &str,
        description: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<WorkspaceWindow> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = self.snapshot()?;
            let w = snapshot["windows"]
                .as_array()
                .context("invalid COSMIC windows")?
                .iter()
                .find(|w| w["id"] == id)
                .context("COSMIC window no longer exists")?;
            if predicate(w) {
                return Ok(serde_json::from_value(w.clone())?);
            }
            if Instant::now() >= deadline {
                bail!("COSMIC did not apply {description}; observed window {w}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn capture(
        &self,
        window: Option<&WorkspaceWindow>,
        path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        let path = match window {
            Some(w) => resolve_window_screenshot_path(self.status, &w.id, path),
            None => resolve_screenshot_path(self.status, path),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.call("capture", json!({"id":window.map(|w|&w.id),"path":path}))?;
        let (mut width, mut height) = image::image_dimensions(&path)?;
        if let Some(w) = window {
            if (width, height) != (w.geometry.width, w.geometry.height) {
                let image = image::open(&path)?.resize_exact(
                    w.geometry.width,
                    w.geometry.height,
                    image::imageops::FilterType::Triangle,
                );
                image.save(&path)?;
                width = w.geometry.width;
                height = w.geometry.height;
            }
        }
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
    fn command(&self, name: &str) -> Command {
        let mut command = Command::new(name);
        configure_process_environment(
            &mut command,
            &self.status.runtime_dir,
            &self.status.display,
            &self.status.xauthority_path,
        );
        command
    }
    fn gesture(&self, fx: i32, fy: i32, tx: i32, ty: i32, button: u8, logo: bool) -> Result<()> {
        validate_workspace_coordinates(self.status, fx, fy, "drag start")?;
        validate_workspace_coordinates(self.status, tx, ty, "drag end")?;
        let mut actions = vec![self.motion(fx, fy)];
        if logo {
            actions.push(json!({"kind":"key","keysym":0xffeb,"pressed":true}));
        }
        actions.push(json!({"kind":"button","button":button_code(button)?,"pressed":true}));
        actions.push(self.motion(tx, ty));
        actions.push(json!({"kind":"button","button":button_code(button)?,"pressed":false}));
        if logo {
            actions.push(json!({"kind":"key","keysym":0xffeb,"pressed":false}));
        }
        self.input(actions)
    }
}
impl DesktopControl for CosmicDesktopControl<'_> {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>> {
        let mut windows: Vec<WorkspaceWindow> =
            serde_json::from_value(self.snapshot()?["windows"].clone())?;
        if !include_hidden {
            windows.retain(|w| w.visible);
        }
        Ok(windows)
    }
    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>> {
        Ok(self
            .list_windows(!only_visible)?
            .into_iter()
            .map(|w| w.id)
            .collect())
    }
    fn active_window(&self) -> Result<Option<WorkspaceWindow>> {
        let snapshot = self.snapshot()?;
        snapshot["windows"]
            .as_array()
            .context("invalid COSMIC windows")?
            .iter()
            .find(|w| w["focused"] == true)
            .map(|w| serde_json::from_value(w.clone()).map_err(Into::into))
            .transpose()
    }
    fn window_info(&self, id: &str) -> Result<WorkspaceWindow> {
        super::sanitize_window_id(id)?;
        self.list_windows(true)?
            .into_iter()
            .find(|w| w.id == id)
            .context("COSMIC window no longer exists")
    }
    fn pointer(&self) -> Result<WorkspacePointer> {
        Ok(serde_json::from_value(self.call("pointer", json!({}))?)?)
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
        w: &WorkspaceWindow,
        path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        self.capture(Some(w), path, source)
    }
    fn focus_window(&self, id: &str) -> Result<()> {
        self.call("focus", json!({"id":id}))?;
        self.wait_window(id, "focus", |w| w["focused"] == true)?;
        Ok(())
    }
    fn close_window(&self, id: &str) -> Result<()> {
        self.call("close", json!({"id":id}))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.list_windows(true)?.iter().any(|w| w.id == id) {
            if Instant::now() >= deadline {
                bail!("COSMIC window did not close");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    fn move_window(&self, id: &str, x: i32, y: i32) -> Result<WorkspaceWindow> {
        self.focus_window(id)?;
        let g = self.window_info(id)?.geometry;
        let fx = g.x + g.width as i32 / 2;
        let fy = g.y + g.height as i32 / 2;
        self.gesture(fx, fy, fx + x - g.x, fy + y - g.y, 1, true)?;
        self.wait_window(id, "move", |w| {
            (w["geometry"]["x"].as_i64().unwrap_or(i64::MAX) - x as i64).abs() <= 1
                && (w["geometry"]["y"].as_i64().unwrap_or(i64::MAX) - y as i64).abs() <= 1
        })
    }
    fn resize_window(&self, id: &str, width: u32, height: u32) -> Result<WorkspaceWindow> {
        validate_window_size_for_workspace(self.status, width, height)?;
        self.focus_window(id)?;
        let g = self.window_info(id)?.geometry;
        let inset = self.status.session.scale.ceil() as i32 + 1;
        let fx = g.x + g.width as i32 - inset;
        let fy = g.y + g.height as i32 - inset;
        self.gesture(
            fx,
            fy,
            fx + width as i32 - g.width as i32,
            fy + height as i32 - g.height as i32,
            3,
            true,
        )?;
        self.wait_window(id, "resize", |w| {
            w["geometry"]["width"]
                .as_u64()
                .unwrap_or(0)
                .abs_diff(width as u64)
                <= 1
                && w["geometry"]["height"]
                    .as_u64()
                    .unwrap_or(0)
                    .abs_diff(height as u64)
                    <= 1
        })
    }
    fn raise_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.focus_window(id)?;
        self.window_info(id)
    }
    fn minimize_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.call("minimize", json!({"id":id}))?;
        self.wait_window(id, "minimize", |w| w["visible"] == false)
    }
    fn show_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.call("show", json!({"id":id}))?;
        self.wait_window(id, "show", |w| w["visible"] == true)
    }
    fn move_pointer(&self, x: i32, y: i32) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        self.input(vec![self.motion(x, y)])
    }
    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
        validate_click_options(button, count)?;
        validate_workspace_coordinates(self.status, x, y, "click")?;
        let mut actions = vec![self.motion(x, y)];
        for _ in 0..count {
            actions.push(json!({"kind":"button","button":button_code(button)?,"pressed":true}));
            actions.push(json!({"kind":"button","button":button_code(button)?,"pressed":false}));
        }
        self.input(actions)
    }
    fn drag(&self, fx: i32, fy: i32, tx: i32, ty: i32, button: u8) -> Result<()> {
        validate_click_options(button, 1)?;
        self.gesture(fx, fy, tx, ty, button, false)
    }
    fn scroll(&self, x: i32, y: i32, direction: ScrollDirection, amount: u8) -> Result<()> {
        validate_scroll_options(direction, amount)?;
        validate_workspace_coordinates(self.status, x, y, "scroll")?;
        let (horizontal, sign) = match direction {
            ScrollDirection::Up => (false, -1),
            ScrollDirection::Down => (false, 1),
            ScrollDirection::Left => (true, -1),
            ScrollDirection::Right => (true, 1),
        };
        let distance = sign * 120 * amount as i32;
        self.input(vec![self.motion(x,y),json!({"kind":"scroll","x":if horizontal{distance}else{0},"y":if horizontal{0}else{distance}})])
    }
    fn key(&self, key: &str) -> Result<()> {
        let symbols = super::input_keys::keysyms(key)?;
        let mut actions: Vec<_> = symbols
            .iter()
            .map(|s| json!({"kind":"key","keysym":s,"pressed":true}))
            .collect();
        actions.extend(
            symbols
                .iter()
                .rev()
                .map(|s| json!({"kind":"key","keysym":s,"pressed":false})),
        );
        self.input(actions)
    }
    fn type_text(&self, text: &str) -> Result<()> {
        let mut actions = Vec::new();
        for (line, segment) in text.split('\n').enumerate() {
            if line > 0 {
                actions.push(json!({"kind":"key","keysym":0xff0d,"pressed":true}));
                actions.push(json!({"kind":"key","keysym":0xff0d,"pressed":false}));
            }
            if !segment.is_empty() {
                actions.push(json!({"kind":"text","text":segment}));
            }
        }
        self.input(actions)
    }
    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard> {
        validate_clipboard_text(text)?;
        let mut child = self
            .command("timeout")
            .args(["5s", "wl-copy", "--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("wl-copy stdin missing")?
            .write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("COSMIC clipboard write failed");
        }
        Ok(WorkspaceClipboard {
            selection: "clipboard".into(),
            content: Some(text.into()),
            bytes: text.len() as u64,
        })
    }
    fn get_clipboard(&self) -> Result<WorkspaceClipboard> {
        let output = self
            .command("timeout")
            .args(["5s", "wl-paste", "--no-newline", "--type", "text"])
            .output()?;
        if !output.status.success() {
            bail!(
                "COSMIC clipboard read failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = String::from_utf8(output.stdout)?;
        Ok(WorkspaceClipboard {
            selection: "clipboard".into(),
            bytes: text.len() as u64,
            content: Some(text),
        })
    }
}
fn button_code(button: u8) -> Result<u32> {
    match button {
        1 => Ok(272),
        2 => Ok(274),
        3 => Ok(273),
        _ => bail!("COSMIC pointer button must be 1, 2 or 3"),
    }
}
