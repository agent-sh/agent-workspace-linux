use super::{resolve_screenshot_path, resolve_window_screenshot_path, DesktopControl};
use crate::workspace::{
    command_path_check, daemon_executable_path, validate_click_options, validate_clipboard_text,
    validate_scroll_options, validate_window_size, validate_window_size_for_workspace,
    validate_workspace_coordinates, workspace_screenshot_result, ScrollDirection, WindowGeometry,
    WorkspaceClipboard, WorkspacePointer, WorkspaceScreenshot, WorkspaceScreenshotSource,
    WorkspaceScreenshotTarget, WorkspaceStatus, WorkspaceWindow,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

pub(crate) struct X11DesktopControl<'a> {
    status: &'a WorkspaceStatus,
}

impl<'a> X11DesktopControl<'a> {
    pub(crate) fn new(status: &'a WorkspaceStatus) -> Self {
        Self { status }
    }

    fn is_i3(&self) -> bool {
        self.status.session.backend == crate::session::SessionBackend::I3
    }

    fn command(&self, program: impl AsRef<OsStr>) -> Command {
        workspace_command(self.status, program)
    }

    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>> {
        let mut command = self.command("xdotool");
        command.arg("search");
        if only_visible {
            command.arg("--onlyvisible");
        }
        let output = command
            .args(["--name", "."])
            .output()
            .context("failed to run xdotool window search")?;
        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|line| {
                let id = line.trim();
                (!id.is_empty()).then(|| id.to_string())
            })
            .collect())
    }

    fn window_pid_from_xprop(&self, id: &str) -> Option<u32> {
        let output = self
            .command("xprop")
            .args(["-id", id, "_NET_WM_PID"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        text.rsplit_once('=')
            .and_then(|(_, value)| value.trim().parse::<u32>().ok())
    }

    fn window_class_from_xprop(&self, id: &str) -> (Option<String>, Option<String>) {
        let output = self.command("xprop").args(["-id", id, "WM_CLASS"]).output();
        let Some(stdout) = output
            .ok()
            .and_then(|output| output.status.success().then_some(output.stdout))
        else {
            return (None, None);
        };
        let Some(text) = String::from_utf8(stdout).ok() else {
            return (None, None);
        };
        let Some((_, values)) = text.split_once('=') else {
            return (None, None);
        };
        let mut parts = values.split(',').map(parse_xprop_string);
        let instance = parts.next().flatten();
        let class = parts.next().flatten();
        (instance, class)
    }

    fn absolute_window_origin(&self, id: &str) -> Option<(i32, i32)> {
        let output = self.command("xwininfo").args(["-id", id]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        parse_absolute_window_origin(&text)
    }

    fn run_xtest_input(&self, args: &[String]) -> Result<()> {
        let exe = daemon_executable_path().context("resolve current executable for xtest input")?;
        let output = self
            .command(&exe)
            .arg("__xtest")
            .args(args)
            .output()
            .context("failed to run __xtest input helper")?;
        output_text(output, "xtest input")?;
        Ok(())
    }

    fn write_clipboard_command(
        &self,
        command: &str,
        args: &[&str],
        text: &str,
        label: &str,
    ) -> Result<()> {
        let mut child = self
            .command(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to run {label}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .with_context(|| format!("failed to write stdin for {label}"))?;
        } else {
            bail!("failed to open stdin for {label}");
        }
        let status = child
            .wait()
            .with_context(|| format!("failed to wait for {label}"))?;
        if !status.success() {
            bail!("{label} failed with {status}");
        }
        Ok(())
    }
}

impl DesktopControl for X11DesktopControl<'_> {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>> {
        let ids = self.search_window_ids(!include_hidden)?;
        let visible_ids: BTreeSet<String> = if include_hidden {
            self.search_window_ids(true)?.into_iter().collect()
        } else {
            ids.iter().cloned().collect()
        };

        ids.into_iter()
            .map(|id| {
                let visible = visible_ids.contains(&id);
                self.window_info_with_visibility(&id, visible)
            })
            .collect()
    }

    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>> {
        Self::search_window_ids(self, only_visible)
    }

    fn active_window(&self) -> Result<Option<WorkspaceWindow>> {
        // A destroyed popup can remain in _NET_ACTIVE_WINDOW. Actual input focus
        // remains usable when the window manager's active-window metadata is stale.
        for query in ["getactivewindow", "getwindowfocus"] {
            let output = self
                .command("xdotool")
                .arg(query)
                .output()
                .with_context(|| format!("failed to run xdotool {query}"))?;
            if !output.status.success() {
                continue;
            }
            let text = output_text(output, query)?;
            let window_id = text.trim();
            if window_id.is_empty() || window_id == "0" || window_id == "1" {
                continue;
            }
            match self.window_info(window_id) {
                Ok(window) => {
                    if query == "getwindowfocus" && !window.visible {
                        // Openbox focuses an internal support window when no application
                        // is active. An unnamed input child must belong to listed content.
                        let exe = daemon_executable_path()
                            .context("resolve executable for native visibility")?;
                        let output = self
                            .command(&exe)
                            .args(["__xtest", "content-check", window_id])
                            .args(self.search_window_ids(true)?)
                            .output()
                            .context("failed to query native workspace visibility")?;
                        let viewable = match output_text(output, "native workspace visibility") {
                            Ok(value) => value,
                            Err(error) if format!("{error:#}").contains("BadWindow") => continue,
                            Err(error) => return Err(error),
                        };
                        if viewable.trim() != "true" {
                            continue;
                        }
                    }
                    return Ok(Some(window));
                }
                Err(error) if format!("{error:#}").contains("BadWindow") => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn window_info(&self, id: &str) -> Result<WorkspaceWindow> {
        let id = sanitize_x11_id(id, "window id")?;
        let visible = self
            .search_window_ids(true)?
            .iter()
            .any(|visible_id| visible_id == &id);
        self.window_info_with_visibility(&id, visible)
    }

    fn pointer(&self) -> Result<WorkspacePointer> {
        let output = self
            .command("xdotool")
            .args(["getmouselocation", "--shell"])
            .output()
            .context("failed to run xdotool getmouselocation")?;
        let text = output_text(output, "xdotool getmouselocation")?;
        let mut x = None;
        let mut y = None;
        let mut screen = None;
        let mut window_id = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "X" => x = Some(value.parse().context("pointer X must be an integer")?),
                "Y" => y = Some(value.parse().context("pointer Y must be an integer")?),
                "SCREEN" => {
                    screen = Some(value.parse().context("pointer SCREEN must be an integer")?)
                }
                "WINDOW" if !value.trim().is_empty() && value.trim() != "0" => {
                    window_id = Some(value.trim().to_string());
                }
                _ => {}
            }
        }
        Ok(WorkspacePointer {
            x: x.context("pointer output missing X")?,
            y: y.context("pointer output missing Y")?,
            screen,
            window_id,
        })
    }

    fn capture_screenshot(
        &self,
        output_path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        let path = resolve_screenshot_path(self.status, output_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut captured = false;
        if command_path_check("ffmpeg").ok {
            let video_size = format!("{}x{}", self.status.width, self.status.height);
            let run_ffmpeg = || -> Result<()> {
                let output = self
                    .command("ffmpeg")
                    .args([
                        "-y",
                        "-loglevel",
                        "error",
                        "-f",
                        "x11grab",
                        "-draw_mouse",
                        "1",
                        "-video_size",
                        &video_size,
                        "-i",
                        &self.status.display,
                        "-frames:v",
                        "1",
                    ])
                    .arg(&path)
                    .output()
                    .context("failed to run ffmpeg for workspace screenshot")?;
                output_text(output, "ffmpeg x11grab")?;
                Ok(())
            };
            match run_ffmpeg() {
                Ok(()) => captured = true,
                Err(err) => eprintln!("ffmpeg screenshot failed, falling back: {err:#}"),
            }
        }

        if captured {
        } else if command_path_check("import").ok {
            let output = self
                .command("import")
                .args(["-window", "root"])
                .arg(&path)
                .output()
                .context("failed to run import for workspace screenshot")?;
            output_text(output, "import -window root")?;
        } else if command_path_check("scrot").ok {
            let output = self
                .command("scrot")
                .arg(&path)
                .output()
                .context("failed to run scrot for workspace screenshot")?;
            output_text(output, "scrot")?;
        } else {
            bail!("missing screenshot command: install ffmpeg (cursor capture), ImageMagick import, or scrot");
        }

        workspace_screenshot_result(
            path,
            self.status.width,
            self.status.height,
            source,
            WorkspaceScreenshotTarget::Root,
            None,
        )
    }

    fn capture_window_screenshot(
        &self,
        window: &WorkspaceWindow,
        output_path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        let window_id = sanitize_x11_id(&window.id, "window id")?;
        let path = resolve_window_screenshot_path(self.status, &window.id, output_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        if command_path_check("import").ok {
            let output = self
                .command("import")
                .args(["-window", &window_id])
                .arg(&path)
                .output()
                .context("failed to run import for workspace window screenshot")?;
            output_text(output, "import -window")?;
        } else if !self.is_i3() && command_path_check("scrot").ok {
            self.focus_window(&window_id)?;
            let output = self
                .command("scrot")
                .args(["-u"])
                .arg(&path)
                .output()
                .context("failed to run scrot for workspace window screenshot")?;
            output_text(output, "scrot -u")?;
        } else {
            bail!("missing screenshot command: install ImageMagick import or scrot");
        }

        workspace_screenshot_result(
            path,
            window.geometry.width,
            window.geometry.height,
            source,
            WorkspaceScreenshotTarget::Window,
            Some(window.id.clone()),
        )
    }

    fn focus_window(&self, window_id: &str) -> Result<()> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        self.window_info(&window_id)?;
        let exe = daemon_executable_path().context("resolve executable for native focus")?;
        let output = self
            .command(&exe)
            .args(["__xtest", "focus-check", &window_id])
            .output()
            .context("failed to query native workspace focus")?;
        match output_text(output, "native workspace input focus")?.trim() {
            "focused" => return Ok(()),
            "direct" => {}
            "managed" => {
                // Let the window manager restore or activate its own top-level window.
                // Native focus is still verified below instead of trusting EWMH alone.
                self.command("xdotool")
                    .args(["windowactivate", "--sync", &window_id])
                    .output()
                    .context("failed to run xdotool windowactivate")?;
            }
            _ => bail!("invalid native workspace focus response"),
        }
        // The child inherits this workspace's DISPLAY and XAUTHORITY, avoiding
        // process-global environment changes in the daemon.
        let output = self
            .command(exe)
            .args(["__xtest", "focus", &window_id])
            .output()
            .context("failed to run native workspace focus helper")?;
        output_text(output, "native workspace input focus")?;
        Ok(())
    }

    fn close_window(&self, window_id: &str) -> Result<()> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        let output = self
            .command("xdotool")
            .args(["windowclose", &window_id])
            .output()
            .context("failed to run xdotool windowclose")?;
        output_text(output, "xdotool windowclose")?;
        Ok(())
    }

    fn move_window(&self, window_id: &str, x: i32, y: i32) -> Result<WorkspaceWindow> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        validate_workspace_coordinates(self.status, x, y, "window move")?;
        if self.is_i3() {
            super::i3::command(
                self.status,
                &window_id,
                &format!("floating enable, move absolute position {x} px {y} px"),
            )?;
            return self.window_info(&window_id);
        }
        let output = self
            .command("xdotool")
            .args([
                "windowmove",
                "--sync",
                &window_id,
                &x.to_string(),
                &y.to_string(),
            ])
            .output()
            .context("failed to run xdotool windowmove")?;
        output_text(output, "xdotool windowmove")?;
        self.window_info(&window_id)
    }

    fn resize_window(&self, window_id: &str, width: u32, height: u32) -> Result<WorkspaceWindow> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        validate_window_size(width, height)?;
        validate_window_size_for_workspace(self.status, width, height)?;
        if self.is_i3() {
            super::i3::command(
                self.status,
                &window_id,
                &format!("floating enable, resize set {width} px {height} px"),
            )?;
            return self.window_info(&window_id);
        }
        let output = self
            .command("xdotool")
            .args([
                "windowsize",
                "--sync",
                &window_id,
                &width.to_string(),
                &height.to_string(),
            ])
            .output()
            .context("failed to run xdotool windowsize")?;
        output_text(output, "xdotool windowsize")?;
        self.window_info(&window_id)
    }

    fn raise_window(&self, window_id: &str) -> Result<WorkspaceWindow> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        let output = self
            .command("xdotool")
            .args(["windowraise", &window_id])
            .output()
            .context("failed to run xdotool windowraise")?;
        output_text(output, "xdotool windowraise")?;
        self.window_info(&window_id)
    }

    fn minimize_window(&self, window_id: &str) -> Result<WorkspaceWindow> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        if self.is_i3() {
            super::i3::command(self.status, &window_id, "move scratchpad")?;
            return self.window_info(&window_id);
        }
        let output = self
            .command("xdotool")
            .args(["windowminimize", "--sync", &window_id])
            .output()
            .context("failed to run xdotool windowminimize")?;
        output_text(output, "xdotool windowminimize")?;
        self.window_info(&window_id)
    }

    fn show_window(&self, window_id: &str) -> Result<WorkspaceWindow> {
        let window_id = sanitize_x11_id(window_id, "window id")?;
        if self.is_i3() {
            super::i3::command(
                self.status,
                &window_id,
                "move container to workspace current",
            )?;
            return self.window_info(&window_id);
        }
        let output = self
            .command("xdotool")
            .args(["windowmap", "--sync", &window_id])
            .output()
            .context("failed to run xdotool windowmap")?;
        output_text(output, "xdotool windowmap")?;
        self.window_info(&window_id)
    }

    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "click")?;
        validate_click_options(button, count)?;
        self.run_xtest_input(&[
            "click".to_string(),
            x.to_string(),
            y.to_string(),
            button.to_string(),
            count.to_string(),
        ])
    }

    fn move_pointer(&self, x: i32, y: i32) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        self.run_xtest_input(&["move".to_string(), x.to_string(), y.to_string()])
    }

    fn drag(&self, from_x: i32, from_y: i32, to_x: i32, to_y: i32, button: u8) -> Result<()> {
        validate_workspace_coordinates(self.status, from_x, from_y, "drag start")?;
        validate_workspace_coordinates(self.status, to_x, to_y, "drag end")?;
        validate_click_options(button, 1)?;
        self.run_xtest_input(&[
            "drag".to_string(),
            from_x.to_string(),
            from_y.to_string(),
            to_x.to_string(),
            to_y.to_string(),
            button.to_string(),
        ])
    }

    fn scroll(&self, x: i32, y: i32, direction: ScrollDirection, amount: u8) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "scroll")?;
        validate_scroll_options(direction, amount)?;
        self.run_xtest_input(&[
            "scroll".to_string(),
            x.to_string(),
            y.to_string(),
            direction.x11_button().to_string(),
            amount.to_string(),
        ])
    }

    fn key(&self, key: &str) -> Result<()> {
        if key.trim().is_empty() {
            bail!("key cannot be empty");
        }
        let output = self
            .command("xdotool")
            .args(["key", "--clearmodifiers", key.trim()])
            .output()
            .context("failed to run xdotool key")?;
        output_text(output, "xdotool key")?;
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            bail!("text cannot be empty");
        }
        let output = self
            .command("xdotool")
            .args(["type", "--clearmodifiers", "--delay", "1", text])
            .output()
            .context("failed to run xdotool type")?;
        output_text(output, "xdotool type")?;
        Ok(())
    }

    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard> {
        validate_clipboard_text(text)?;
        if command_path_check("xclip").ok {
            self.write_clipboard_command(
                "xclip",
                &["-selection", "clipboard"],
                text,
                "xclip clipboard input",
            )?;
        } else if command_path_check("xsel").ok {
            self.write_clipboard_command(
                "xsel",
                &["--clipboard", "--input"],
                text,
                "xsel clipboard input",
            )?;
        } else {
            bail!("missing clipboard command: install xclip or xsel");
        }

        Ok(WorkspaceClipboard {
            selection: "clipboard".to_string(),
            content: None,
            bytes: text.len() as u64,
        })
    }

    fn get_clipboard(&self) -> Result<WorkspaceClipboard> {
        let content = if command_path_check("xclip").ok {
            let output = self
                .command("xclip")
                .args(["-selection", "clipboard", "-out"])
                .output()
                .context("failed to run xclip clipboard output")?;
            output_text(output, "xclip clipboard output")?
        } else if command_path_check("xsel").ok {
            let output = self
                .command("xsel")
                .args(["--clipboard", "--output"])
                .output()
                .context("failed to run xsel clipboard output")?;
            output_text(output, "xsel clipboard output")?
        } else {
            bail!("missing clipboard command: install xclip or xsel");
        };

        Ok(WorkspaceClipboard {
            selection: "clipboard".to_string(),
            bytes: content.len() as u64,
            content: Some(content),
        })
    }
}

impl X11DesktopControl<'_> {
    fn window_info_with_visibility(&self, id: &str, visible: bool) -> Result<WorkspaceWindow> {
        let id = sanitize_x11_id(id, "window id")?;
        let title = self
            .command("xdotool")
            .args(["getwindowname", &id])
            .output()
            .with_context(|| format!("failed to read window name for {id}"))
            .and_then(|output| output_text(output, "xdotool getwindowname"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let (wm_instance, wm_class) = self.window_class_from_xprop(&id);
        // XRes returns the PID seen by our X server, unlike _NET_WM_PID which
        // an app may set to a PID from another process namespace.
        let pid = daemon_executable_path()
            .ok()
            .and_then(|exe| {
                self.command(exe)
                    .args(["__xtest", "window-pid", &id])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .and_then(|output| String::from_utf8(output.stdout).ok())
                    .and_then(|value| value.trim().parse::<u32>().ok())
            })
            .or_else(|| {
                xprop_fallback_pid(
                    self.window_pid_from_xprop(&id),
                    !self.status.apps.is_empty(),
                )
            });
        let geometry_output = self
            .command("xdotool")
            .args(["getwindowgeometry", "--shell", &id])
            .output()
            .with_context(|| format!("failed to read window geometry for {id}"))?;
        let geometry_text = output_text(geometry_output, "xdotool getwindowgeometry")?;
        let mut geometry = parse_window_geometry(&geometry_text)?;

        if let Some((abs_x, abs_y)) = self.absolute_window_origin(&id) {
            geometry.x = abs_x;
            geometry.y = abs_y;
        }

        Ok(WorkspaceWindow {
            id,
            title,
            wm_class,
            wm_instance,
            pid,
            app_id: None,
            visible,
            geometry,
        })
    }
}

pub(crate) fn workspace_command(status: &WorkspaceStatus, program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    status.session.configure_process_environment(
        &mut command,
        &status.display,
        &status.xauthority_path,
        &status.runtime_dir,
    );
    command.stdin(Stdio::null());
    command
}

fn output_text(output: std::process::Output, description: &str) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!("{description} failed: {detail}");
    }
}

fn xprop_fallback_pid(xprop_pid: Option<u32>, has_managed_apps: bool) -> Option<u32> {
    if has_managed_apps {
        None
    } else {
        xprop_pid
    }
}

fn sanitize_x11_id(id: &str, label: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("{label} cannot be empty");
    }
    if !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("{label} must be a decimal X11 id");
    }
    Ok(trimmed.to_string())
}

fn parse_xprop_string(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches('"').trim();
    (!trimmed.is_empty() && trimmed != "not found.").then(|| trimmed.to_string())
}

fn parse_window_geometry(text: &str) -> Result<WindowGeometry> {
    let mut x = None;
    let mut y = None;
    let mut width = None;
    let mut height = None;
    let mut screen = None;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "X" => x = value.parse::<i32>().ok(),
            "Y" => y = value.parse::<i32>().ok(),
            "WIDTH" => width = value.parse::<u32>().ok(),
            "HEIGHT" => height = value.parse::<u32>().ok(),
            "SCREEN" => screen = value.parse::<i32>().ok(),
            _ => {}
        }
    }

    Ok(WindowGeometry {
        x: x.context("window geometry missing X")?,
        y: y.context("window geometry missing Y")?,
        width: width.context("window geometry missing WIDTH")?,
        height: height.context("window geometry missing HEIGHT")?,
        screen,
    })
}

pub(crate) fn parse_absolute_window_origin(text: &str) -> Option<(i32, i32)> {
    let mut x = None;
    let mut y = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Absolute upper-left X:") {
            x = rest.trim().parse::<i32>().ok();
        } else if let Some(rest) = line.strip_prefix("Absolute upper-left Y:") {
            y = rest.trim().parse::<i32>().ok();
        }
    }
    match (x, y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionSpec;

    fn status() -> WorkspaceStatus {
        WorkspaceStatus {
            id: "x11-test".to_string(),
            session: SessionSpec::default(),
            session_id: "session-x11-test".to_string(),
            purpose: None,
            profile_id: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            ready: true,
            started_at_unix: 1,
            display: ":90".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: PathBuf::from("/tmp/x11-test"),
            socket_path: PathBuf::from("/tmp/x11-test/control.sock"),
            xauthority_path: PathBuf::from("/tmp/x11-test/Xauthority"),
            daemon_pid: None,
            x_server_pid: 0,
            window_manager_pid: None,
            last_event_sequence: 0,
            apps: Vec::new(),
        }
    }

    #[test]
    fn x11_ids_remain_decimal_only() {
        assert_eq!(
            sanitize_x11_id(" 4194307 ", "window id").unwrap(),
            "4194307"
        );
        assert!(sanitize_x11_id("kwin:4194307", "window id").is_err());
        assert!(sanitize_x11_id("0x400003", "window id").is_err());
    }

    #[test]
    fn xres_failure_withholds_colliding_managed_app_pid() {
        let other_app_host_pid = 4242;
        let namespace_local_pid = other_app_host_pid;

        assert_eq!(xprop_fallback_pid(Some(namespace_local_pid), true), None);
    }

    #[test]
    fn xres_failure_preserves_unmanaged_only_pid_fallback() {
        assert_eq!(xprop_fallback_pid(Some(4242), false), Some(4242));
    }

    #[test]
    fn parses_absolute_window_origin_from_xwininfo() {
        let sample = "\
xwininfo: Window id: 0x400005 \"Eigen\"

  Absolute upper-left X:  111
  Absolute upper-left Y:  142
  Relative upper-left X:  1
  Relative upper-left Y:  22
  Width: 1180
  Height: 760
";
        assert_eq!(parse_absolute_window_origin(sample), Some((111, 142)));
        assert_eq!(parse_absolute_window_origin("no geometry here"), None);
        let negative = "  Absolute upper-left X:  -5\n  Absolute upper-left Y:  -10\n";
        assert_eq!(parse_absolute_window_origin(negative), Some((-5, -10)));
    }

    #[test]
    fn workspace_command_scrubs_host_wayland_hints() {
        let command = workspace_command(&status(), "true");
        let envs = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect::<Vec<_>>();

        assert!(envs
            .iter()
            .any(|(name, value)| name == "WAYLAND_DISPLAY" && value.is_none()));
        assert_eq!(
            envs.iter()
                .find_map(|(name, value)| (name == "DISPLAY").then_some(value.as_deref()))
                .flatten(),
            Some(":90")
        );
        assert_eq!(
            envs.iter()
                .find_map(|(name, value)| (name == "XDG_SESSION_TYPE").then_some(value.as_deref()))
                .flatten(),
            Some("x11")
        );
    }
}
