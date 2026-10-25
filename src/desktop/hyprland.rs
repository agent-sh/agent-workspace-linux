use super::{resolve_screenshot_path, resolve_window_screenshot_path, DesktopControl};
use crate::{
    session::hyprland::{configure_process_environment, request},
    workspace::{
        validate_click_options, validate_clipboard_text, validate_scroll_options,
        validate_window_size_for_workspace, validate_workspace_coordinates,
        workspace_screenshot_result, ScrollDirection, WindowGeometry, WorkspaceClipboard,
        WorkspacePointer, WorkspaceScreenshot, WorkspaceScreenshotSource,
        WorkspaceScreenshotTarget, WorkspaceStatus, WorkspaceWindow,
    },
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[path = "hyprland_capture.rs"]
mod capture;
#[path = "wayland_pointer.rs"]
mod input;

pub(crate) fn prepare_input(runtime: &std::path::Path) -> Result<()> {
    input::Input::persistent(&runtime.join(crate::session::hyprland::WAYLAND_SOCKET))?;
    Ok(())
}

pub(crate) struct HyprlandDesktopControl<'a> {
    status: &'a WorkspaceStatus,
}
impl<'a> HyprlandDesktopControl<'a> {
    pub fn new(status: &'a WorkspaceStatus) -> Self {
        Self { status }
    }
    fn query(&self, command: &str) -> Result<Value> {
        Ok(serde_json::from_str(&request(
            &self.status.runtime_dir,
            &format!("j/{command}"),
        )?)?)
    }
    fn dispatch(&self, command: &str) -> Result<()> {
        let response = request(&self.status.runtime_dir, &format!("dispatch {command}"))?;
        if response.trim() != "ok" {
            bail!("Hyprland rejected operation: {response}");
        }
        Ok(())
    }
    fn address(&self, id: &str) -> Result<String> {
        let id = id
            .strip_prefix("hyprland:")
            .context("not a Hyprland window id")?;
        if !id.strip_prefix("0x").is_some_and(|v| {
            !v.is_empty() && v.len() <= 16 && v.bytes().all(|c| c.is_ascii_hexdigit())
        }) {
            bail!("invalid Hyprland window address");
        }
        Ok(format!("address:{id}"))
    }
    fn window(&self, value: &Value) -> Result<WorkspaceWindow> {
        let scale = self.status.session.scale;
        let n = |key: &str, index: usize| {
            value[key][index]
                .as_f64()
                .with_context(|| format!("missing Hyprland {key}"))
        };
        let address = value["address"]
            .as_str()
            .context("missing Hyprland window address")?;
        let id = format!("hyprland:{address}");
        self.address(&id)?;
        Ok(WorkspaceWindow {
            id,
            title: value["title"].as_str().unwrap_or_default().to_owned(),
            wm_class: value["class"].as_str().map(str::to_owned),
            wm_instance: value["initialClass"].as_str().map(str::to_owned),
            app_id: None,
            // Relayed sandboxed windows report the relay's holder process.
            pid: crate::session::wayland_filter::window_pid(
                value["pid"].as_u64().and_then(|v| u32::try_from(v).ok()),
            ),
            visible: value["mapped"] == true
                && value["hidden"] != true
                && value["workspace"]["name"].as_str() != Some("special:workspace-hidden"),
            geometry: WindowGeometry {
                x: (n("at", 0)? * scale).round() as i32,
                y: (n("at", 1)? * scale).round() as i32,
                width: (n("size", 0)? * scale).round() as u32,
                height: (n("size", 1)? * scale).round() as u32,
                screen: Some(0),
            },
        })
    }
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new("timeout");
        configure_process_environment(
            &mut command,
            &self.status.runtime_dir,
            "",
            &self.status.xauthority_path,
        );
        command.args(["5s", program]);
        command
    }
    fn input(&self) -> Result<input::PersistentInput> {
        input::Input::persistent(
            &self
                .status
                .runtime_dir
                .join(crate::session::hyprland::WAYLAND_SOCKET),
        )
    }
    fn floating(&self, id: &str) -> Result<()> {
        let address = self.address(id)?;
        self.dispatch(&format!("setfloating {address}"))
    }
    fn wait_geometry(
        &self,
        id: &str,
        matches: impl Fn(&WindowGeometry) -> bool,
    ) -> Result<WorkspaceWindow> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let window = self.window_info(id)?;
            if matches(&window.geometry) {
                return Ok(window);
            }
            if Instant::now() >= deadline {
                bail!("Hyprland did not apply geometry: {:?}", window.geometry)
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
        let image = if let Some(w) = window {
            self.address(&w.id)?;
            let address = u64::from_str_radix(w.id.trim_start_matches("hyprland:0x"), 16)?;
            let image = capture::capture(&self.status.runtime_dir, address)?;
            if image.dimensions() != (w.geometry.width, w.geometry.height) {
                image::imageops::resize(
                    &image,
                    w.geometry.width,
                    w.geometry.height,
                    image::imageops::FilterType::Triangle,
                )
            } else {
                image
            }
        } else {
            let output = self
                .command("grim")
                .arg("-c")
                .arg("-s")
                .arg(self.status.session.scale.to_string())
                .args(["-o", "workspace", "-"])
                .output()?;
            if !output.status.success() {
                bail!(
                    "Hyprland capture: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            }
            image::load_from_memory(&output.stdout)?.to_rgba8()
        };
        image.save(&path)?;
        workspace_screenshot_result(
            path,
            image.width(),
            image.height(),
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
impl DesktopControl for HyprlandDesktopControl<'_> {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>> {
        self.query("clients")?
            .as_array()
            .context("Hyprland clients missing")?
            .iter()
            .map(|v| self.window(v))
            .filter(|v| include_hidden || v.as_ref().is_ok_and(|w| w.visible) || v.is_err())
            .collect()
    }
    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>> {
        Ok(self
            .list_windows(!only_visible)?
            .into_iter()
            .map(|w| w.id)
            .collect())
    }
    fn active_window(&self) -> Result<Option<WorkspaceWindow>> {
        let value = self.query("activewindow")?;
        if value["address"].as_str().is_none_or(str::is_empty) {
            Ok(None)
        } else {
            self.window(&value).map(Some)
        }
    }
    fn window_info(&self, id: &str) -> Result<WorkspaceWindow> {
        self.address(id)?;
        self.list_windows(true)?
            .into_iter()
            .find(|w| w.id == id)
            .context("Hyprland window no longer exists")
    }
    fn pointer(&self) -> Result<WorkspacePointer> {
        let p = self.query("cursorpos")?;
        Ok(WorkspacePointer {
            x: (p["x"].as_f64().context("cursor x")? * self.status.session.scale).round() as i32,
            y: (p["y"].as_f64().context("cursor y")? * self.status.session.scale).round() as i32,
            screen: Some(0),
            window_id: None,
        })
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
        self.show_window(id)?;
        self.dispatch(&format!("focuswindow {}", self.address(id)?))
    }
    fn close_window(&self, id: &str) -> Result<()> {
        self.dispatch(&format!("closewindow {}", self.address(id)?))
    }
    fn move_window(&self, id: &str, x: i32, y: i32) -> Result<WorkspaceWindow> {
        self.floating(id)?;
        let s = self.status.session.scale;
        self.dispatch(&format!(
            "movewindowpixel exact {} {},{}",
            (x as f64 / s).round(),
            (y as f64 / s).round(),
            self.address(id)?
        ))?;
        self.wait_geometry(id, |g| (g.x - x).abs() <= 1 && (g.y - y).abs() <= 1)
    }
    fn resize_window(&self, id: &str, width: u32, height: u32) -> Result<WorkspaceWindow> {
        validate_window_size_for_workspace(self.status, width, height)?;
        self.floating(id)?;
        let s = self.status.session.scale;
        self.dispatch(&format!(
            "resizewindowpixel exact {} {},{}",
            (width as f64 / s).round(),
            (height as f64 / s).round(),
            self.address(id)?
        ))?;
        self.wait_geometry(id, |g| {
            g.width.abs_diff(width) <= 1 && g.height.abs_diff(height) <= 1
        })
    }
    fn raise_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.focus_window(id)?;
        self.window_info(id)
    }
    fn minimize_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.dispatch(&format!(
            "movetoworkspacesilent special:workspace-hidden,{}",
            self.address(id)?
        ))?;
        self.window_info(id)
    }
    fn show_window(&self, id: &str) -> Result<WorkspaceWindow> {
        if !self.window_info(id)?.visible {
            self.dispatch(&format!("movetoworkspacesilent 1,{}", self.address(id)?))?;
        }
        self.window_info(id)
    }
    fn move_pointer(&self, x: i32, y: i32) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        self.input()?
            .pointer(x as f64, y as f64, self.status.width, self.status.height)
    }
    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
        validate_click_options(button, count)?;
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        let mut i = self.input()?;
        i.pointer(x as f64, y as f64, self.status.width, self.status.height)?;
        for _ in 0..count {
            i.button(linux_button(button)?, true)?;
            i.button(linux_button(button)?, false)?;
        }
        Ok(())
    }
    fn drag(&self, fx: i32, fy: i32, tx: i32, ty: i32, button: u8) -> Result<()> {
        validate_click_options(button, 1)?;
        validate_workspace_coordinates(self.status, fx, fy, "drag start")?;
        validate_workspace_coordinates(self.status, tx, ty, "drag end")?;
        let mut i = self.input()?;
        i.pointer(fx as f64, fy as f64, self.status.width, self.status.height)?;
        i.button(linux_button(button)?, true)?;
        for step in 1..=12 {
            i.pointer(
                fx as f64 + (tx - fx) as f64 * step as f64 / 12.0,
                fy as f64 + (ty - fy) as f64 * step as f64 / 12.0,
                self.status.width,
                self.status.height,
            )?;
        }
        i.button(linux_button(button)?, false)
    }
    fn scroll(&self, x: i32, y: i32, d: ScrollDirection, amount: u8) -> Result<()> {
        validate_scroll_options(d, amount)?;
        self.move_pointer(x, y)?;
        let (h, n) = match d {
            ScrollDirection::Up => (false, -1.0),
            ScrollDirection::Down => (false, 1.0),
            ScrollDirection::Left => (true, -1.0),
            ScrollDirection::Right => (true, 1.0),
        };
        self.input()?.axis(h, n * 15.0 * amount as f64)
    }
    fn key(&self, key: &str) -> Result<()> {
        let mut command = self.command("wtype");
        let parts = key.split('+').collect::<Vec<_>>();
        for modifier in &parts[..parts.len() - 1] {
            command.args(["-M", modifier_name(modifier)?]);
        }
        command.args(["-k", parts.last().context("empty key")?]);
        for modifier in parts[..parts.len() - 1].iter().rev() {
            command.args(["-m", modifier_name(modifier)?]);
        }
        command.args(["-s", "50"]);
        let output = command.output()?;
        if !output.status.success() {
            bail!(
                "Hyprland key input: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        }
        Ok(())
    }
    fn type_text(&self, text: &str) -> Result<()> {
        let mut child = self
            .command("wtype")
            .args(["-", "-s", "50"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("wtype stdin")?
            .write_all(text.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "Hyprland text input: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        }
        Ok(())
    }
    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard> {
        validate_clipboard_text(text)?;
        let mut child = self
            .command("wl-copy")
            .args(["--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("wl-copy stdin")?
            .write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("Hyprland clipboard write failed")
        };
        Ok(WorkspaceClipboard {
            selection: "clipboard".into(),
            content: Some(text.into()),
            bytes: text.len() as u64,
        })
    }
    fn get_clipboard(&self) -> Result<WorkspaceClipboard> {
        let output = self
            .command("wl-paste")
            .args(["--no-newline", "--type", "text"])
            .output()?;
        if !output.status.success() {
            bail!("Hyprland clipboard read failed")
        };
        let text = String::from_utf8(output.stdout)?;
        Ok(WorkspaceClipboard {
            selection: "clipboard".into(),
            bytes: text.len() as u64,
            content: Some(text),
        })
    }
}
fn linux_button(button: u8) -> Result<u32> {
    match button {
        1 => Ok(272),
        2 => Ok(274),
        3 => Ok(273),
        _ => bail!("pointer buttons must be 1, 2 or 3"),
    }
}
fn modifier_name(name: &str) -> Result<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok("ctrl"),
        "alt" => Ok("alt"),
        "shift" => Ok("shift"),
        "super" | "meta" | "win" => Ok("logo"),
        _ => bail!("unsupported key modifier {name}"),
    }
}
