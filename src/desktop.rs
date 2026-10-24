pub(crate) mod gnome;
pub(crate) mod kwin;
pub(crate) mod x11;

use crate::workspace::{
    ScrollDirection, WorkspaceClipboard, WorkspacePointer, WorkspaceScreenshot,
    WorkspaceScreenshotSource, WorkspaceStatus, WorkspaceWindow,
};
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_DESKTOP_ID_BYTES: usize = 128;

pub(crate) trait DesktopControl {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>>;
    fn search_window_ids(&self, only_visible: bool) -> Result<Vec<String>>;
    fn active_window(&self) -> Result<Option<WorkspaceWindow>>;
    fn window_info(&self, window_id: &str) -> Result<WorkspaceWindow>;
    fn pointer(&self) -> Result<WorkspacePointer>;
    fn capture_screenshot(
        &self,
        output_path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot>;
    fn capture_window_screenshot(
        &self,
        window: &WorkspaceWindow,
        output_path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot>;
    fn focus_window(&self, window_id: &str) -> Result<()>;
    fn close_window(&self, window_id: &str) -> Result<()>;
    fn move_window(&self, window_id: &str, x: i32, y: i32) -> Result<WorkspaceWindow>;
    fn resize_window(&self, window_id: &str, width: u32, height: u32) -> Result<WorkspaceWindow>;
    fn raise_window(&self, window_id: &str) -> Result<WorkspaceWindow>;
    fn minimize_window(&self, window_id: &str) -> Result<WorkspaceWindow>;
    fn show_window(&self, window_id: &str) -> Result<WorkspaceWindow>;
    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()>;
    fn move_pointer(&self, x: i32, y: i32) -> Result<()>;
    fn drag(&self, from_x: i32, from_y: i32, to_x: i32, to_y: i32, button: u8) -> Result<()>;
    fn scroll(&self, x: i32, y: i32, direction: ScrollDirection, amount: u8) -> Result<()>;
    fn key(&self, key: &str) -> Result<()>;
    fn type_text(&self, text: &str) -> Result<()>;
    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard>;
    fn get_clipboard(&self) -> Result<WorkspaceClipboard>;
}

pub(crate) fn sanitize_window_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!("window id cannot be empty");
    }
    if trimmed.len() > MAX_DESKTOP_ID_BYTES {
        bail!("window id must be at most {MAX_DESKTOP_ID_BYTES} bytes");
    }
    if matches!(trimmed, "." | "..")
        || trimmed
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\'))
    {
        bail!("window id must be a path-safe opaque desktop id");
    }
    Ok(trimmed.to_string())
}

pub(crate) fn resolve_screenshot_path(
    status: &WorkspaceStatus,
    output_path: Option<PathBuf>,
) -> PathBuf {
    match output_path {
        Some(path) if path.is_absolute() => path,
        Some(path) => status.runtime_dir.join(path),
        None => status
            .runtime_dir
            .join(format!("screenshot-{}.png", unix_now())),
    }
}

pub(crate) fn resolve_window_screenshot_path(
    status: &WorkspaceStatus,
    window_id: &str,
    output_path: Option<PathBuf>,
) -> PathBuf {
    match output_path {
        Some(path) if path.is_absolute() => path,
        Some(path) => status.runtime_dir.join(path),
        None => status.runtime_dir.join(format!(
            "screenshot-window-{}-{}.png",
            screenshot_file_id(window_id),
            unix_now()
        )),
    }
}

fn screenshot_file_id(id: &str) -> String {
    let sanitized = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() || matches!(sanitized.as_str(), "." | "..") {
        "window".to_string()
    } else {
        sanitized
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_window_ids_are_bounded_and_path_safe() {
        assert_eq!(
            sanitize_window_id(" kwin:{7f727c26-5830-4f2f-9e4e-4d62d517c5d3} ").expect("opaque id"),
            "kwin:{7f727c26-5830-4f2f-9e4e-4d62d517c5d3}"
        );
        assert!(sanitize_window_id("").is_err());
        assert!(sanitize_window_id("../outside").is_err());
        assert!(sanitize_window_id("nested/window").is_err());
        assert!(sanitize_window_id("nested\\window").is_err());
        assert!(sanitize_window_id("bad\0id").is_err());
        assert!(sanitize_window_id(&"x".repeat(MAX_DESKTOP_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn non_x11_window_ids_are_sanitized_for_screenshot_names() {
        let id = "kwin:{7f727c26/5830}";
        assert_eq!(screenshot_file_id(id), "kwin__7f727c26_5830_");
        assert_eq!(screenshot_file_id(".."), "window");
        assert_eq!(screenshot_file_id("4194307"), "4194307");
    }
}
