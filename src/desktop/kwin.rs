use super::{resolve_screenshot_path, resolve_window_screenshot_path, DesktopControl};
use crate::{
    session::kwin::{bus_address, configure_process_environment},
    workspace::{
        validate_click_options, validate_clipboard_text, validate_scroll_options,
        validate_window_size_for_workspace, validate_workspace_coordinates,
        workspace_screenshot_result, ScrollDirection, WorkspaceClipboard, WorkspacePointer,
        WorkspaceScreenshot, WorkspaceScreenshotSource, WorkspaceScreenshotTarget, WorkspaceStatus,
        WorkspaceWindow,
    },
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
use zbus::{
    blocking::{connection::Builder, Connection, Proxy},
    zvariant::{Fd, OwnedValue},
};
#[path = "kwin_input.rs"]
mod input;
pub(super) use input::keysyms;

static SCRIPT_ID: AtomicU64 = AtomicU64::new(0);
const CALLBACK_PATH: &str = "/org/agent_sh/Workspace/KWin";
const CALLBACK_INTERFACE: &str = "org.agent_sh.Workspace.KWin";

struct Callback {
    sender: mpsc::SyncSender<String>,
    owner: String,
}
#[zbus::interface(name = "org.agent_sh.Workspace.KWin")]
impl Callback {
    fn result(
        &self,
        value: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        if header.sender().map(|s| s.as_str()) != Some(self.owner.as_str()) {
            return Err(zbus::fdo::Error::AccessDenied(
                "Only this workspace's KWin may return script data".into(),
            ));
        }
        self.sender
            .try_send(value.to_owned())
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

pub(crate) struct KwinDesktopControl<'a> {
    status: &'a WorkspaceStatus,
}
impl<'a> KwinDesktopControl<'a> {
    pub fn new(status: &'a WorkspaceStatus) -> Self {
        Self { status }
    }
    fn bus(&self) -> Result<Connection> {
        // Never fall back to the host's session bus.
        Ok(
            Builder::address(bus_address(&self.status.runtime_dir).as_str())?
                .method_timeout(Duration::from_secs(5))
                .build()?,
        )
    }
    fn script(&self, body: &str) -> Result<Value> {
        let connection = self.bus()?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
        let owner = dbus.get_name_owner("org.kde.KWin".try_into()?)?.to_string();
        let (sender, receiver) = mpsc::sync_channel(1);
        connection
            .object_server()
            .at(CALLBACK_PATH, Callback { sender, owner })?;
        let unique = connection
            .unique_name()
            .context("private bus has no unique name")?
            .as_str();
        let id = SCRIPT_ID.fetch_add(1, Ordering::Relaxed);
        let plugin = format!("agent-workspace-{}-{id}", std::process::id());
        let path = self.status.runtime_dir.join(format!("{plugin}.js"));
        let script = format!(
            r#"(function() {{
function windows() {{ return workspace.windowList ? workspace.windowList() : workspace.clientList(); }}
function find(id) {{ var list=windows(); for(var i=0;i<list.length;i++) if(String(list[i].internalId)===id) return list[i]; throw new Error('window no longer exists: '+id); }}
function describe(w) {{ var g=w.frameGeometry, s={scale}; return {{id:String(w.internalId),title:String(w.caption),wm_class:String(w.resourceClass),wm_instance:String(w.resourceName),pid:Number(w.pid)||null,app_id:null,visible:!w.minimized,geometry:{{x:Math.round(g.x*s),y:Math.round(g.y*s),width:Math.round(g.width*s),height:Math.round(g.height*s),screen:0}}}}; }}
try {{ var value=(function() {{ {body} }})(); callDBus({unique}, {path}, {interface}, 'Result', JSON.stringify({{ok:true,value:value}})); }}
catch(e) {{ callDBus({unique}, {path}, {interface}, 'Result', JSON.stringify({{ok:false,error:String(e)}})); }}
}})();"#,
            scale = self.status.session.scale,
            unique = serde_json::to_string(unique)?,
            path = serde_json::to_string(CALLBACK_PATH)?,
            interface = serde_json::to_string(CALLBACK_INTERFACE)?
        );
        fs::write(&path, script)?;
        let proxy = Proxy::new(
            &connection,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )?;
        let result = (|| -> Result<Value> {
            let script_id: i32 = proxy.call(
                "loadScript",
                &(path.to_string_lossy().as_ref(), plugin.as_str()),
            )?;
            if script_id < 0 {
                bail!("KWin refused the workspace control script");
            }
            let _: () = proxy.call("start", &())?;
            let value = receiver
                .recv_timeout(Duration::from_secs(5))
                .context("KWin control script did not return a result")?;
            let envelope: Value = serde_json::from_str(&value)?;
            if envelope["ok"] != true {
                bail!("KWin control: {}", envelope["error"]);
            }
            Ok(envelope["value"].clone())
        })();
        let _: zbus::Result<bool> = proxy.call("unloadScript", &(plugin.as_str(),));
        let _ = fs::remove_file(path);
        result
    }
    fn mutate(&self, id: &str, body: &str) -> Result<()> {
        let id = serde_json::to_string(&super::sanitize_window_id(id)?)?;
        self.script(&format!("var w=find({id}); {body}; return true;"))?;
        Ok(())
    }
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        configure_process_environment(
            &mut command,
            &self.status.runtime_dir,
            &self.status.display,
            &self.status.xauthority_path,
        );
        command
    }
    fn capture(
        &self,
        window: Option<&WorkspaceWindow>,
        output_path: Option<PathBuf>,
        source: WorkspaceScreenshotSource,
    ) -> Result<WorkspaceScreenshot> {
        let path = match window {
            Some(w) => resolve_window_screenshot_path(self.status, &w.id, output_path),
            None => resolve_screenshot_path(self.status, output_path),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (mut width, mut height, pixels) =
            read_frame(&self.status.runtime_dir, window.map(|w| w.id.as_str()))?;
        let mut image = image::RgbaImage::from_raw(width, height, pixels)
            .context("invalid KWin image dimensions")?;
        // CaptureWindow uses client target scale, which can differ from the
        // fractional output scale. Normalize onto the workspace pixel grid.
        if let Some(window) = window {
            let target = &window.geometry;
            if (width, height) != (target.width, target.height) {
                image = image::imageops::resize(
                    &image,
                    target.width,
                    target.height,
                    image::imageops::FilterType::Triangle,
                );
                width = target.width;
                height = target.height;
            }
        }
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
    fn wait_geometry(
        &self,
        id: &str,
        matches: impl Fn(&crate::workspace::WindowGeometry) -> bool,
        action: &str,
    ) -> Result<WorkspaceWindow> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let window = self.window_info(id)?;
            if matches(&window.geometry) {
                return Ok(window);
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "KWin did not apply {action}; actual geometry: {:?}",
                    window.geometry
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn input(&self) -> Result<input::Input> {
        input::Input::connect(&self.status.runtime_dir)
    }
}

impl DesktopControl for KwinDesktopControl<'_> {
    fn list_windows(&self, include_hidden: bool) -> Result<Vec<WorkspaceWindow>> {
        let mut windows: Vec<WorkspaceWindow> = serde_json::from_value(self.script("return windows().filter(function(w){return w.normalWindow || w.dialog;}).map(describe);")?)?;
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
        Ok(serde_json::from_value(self.script("var w=workspace.activeWindow || workspace.activeClient; return w ? describe(w) : null;")?)?)
    }
    fn window_info(&self, id: &str) -> Result<WorkspaceWindow> {
        Ok(serde_json::from_value(self.script(&format!(
            "return describe(find({}));",
            serde_json::to_string(&super::sanitize_window_id(id)?)?
        ))?)?)
    }
    fn pointer(&self) -> Result<WorkspacePointer> {
        Ok(serde_json::from_value(self.script(&format!("var p=workspace.cursorPos; return {{x:Math.round(p.x*{s}), y:Math.round(p.y*{s}),screen:0}};",s=self.status.session.scale))?)?)
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
        self.mutate(id, "w.minimized=false; workspace.activeWindow=w")
    }
    fn close_window(&self, id: &str) -> Result<()> {
        self.mutate(id, "w.closeWindow()")
    }
    fn move_window(&self, id: &str, x: i32, y: i32) -> Result<WorkspaceWindow> {
        self.mutate(
            id,
            &format!(
                "var g=w.frameGeometry; w.frameGeometry={{x:{},y:{},width:g.width,height:g.height}}",
                x as f64 / self.status.session.scale,
                y as f64 / self.status.session.scale
            ),
        )?;
        self.wait_geometry(id, |g| (g.x - x).abs() <= 1 && (g.y - y).abs() <= 1, "move")
    }
    fn resize_window(&self, id: &str, width: u32, height: u32) -> Result<WorkspaceWindow> {
        validate_window_size_for_workspace(self.status, width, height)?;
        self.mutate(
            id,
            &format!(
                "var g=w.frameGeometry;w.frameGeometry={{x:g.x,y:g.y,width:{},height:{}}}",
                width as f64 / self.status.session.scale,
                height as f64 / self.status.session.scale
            ),
        )?;
        self.wait_geometry(
            id,
            |g| g.width.abs_diff(width) <= 1 && g.height.abs_diff(height) <= 1,
            "resize",
        )
    }
    fn raise_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate(id, "workspace.raiseWindow(w)")?;
        self.window_info(id)
    }
    fn minimize_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate(id, "w.minimized=true")?;
        self.window_info(id)
    }
    fn show_window(&self, id: &str) -> Result<WorkspaceWindow> {
        self.mutate(id, "w.minimized=false")?;
        self.window_info(id)
    }
    fn click(&self, x: i32, y: i32, button: u8, count: u8) -> Result<()> {
        validate_click_options(button, count)?;
        self.move_pointer(x, y)?;
        let mut input = self.input()?;
        for _ in 0..count {
            input.button(linux_button(button)?, true)?;
            input.button(linux_button(button)?, false)?;
        }
        Ok(())
    }
    fn move_pointer(&self, x: i32, y: i32) -> Result<()> {
        validate_workspace_coordinates(self.status, x, y, "pointer")?;
        self.input()?.pointer(
            x as f64 / self.status.session.scale,
            y as f64 / self.status.session.scale,
        )
    }
    fn drag(&self, fx: i32, fy: i32, tx: i32, ty: i32, button: u8) -> Result<()> {
        validate_click_options(button, 1)?;
        validate_workspace_coordinates(self.status, fx, fy, "drag start")?;
        validate_workspace_coordinates(self.status, tx, ty, "drag end")?;
        let mut i = self.input()?;
        let s = self.status.session.scale;
        i.pointer(fx as f64 / s, fy as f64 / s)?;
        i.button(linux_button(button)?, true)?;
        for step in 1..=12 {
            i.pointer(
                (fx as f64 + (tx - fx) as f64 * step as f64 / 12.0) / s,
                (fy as f64 + (ty - fy) as f64 * step as f64 / 12.0) / s,
            )?;
        }
        i.button(linux_button(button)?, false)
    }
    fn scroll(&self, x: i32, y: i32, direction: ScrollDirection, amount: u8) -> Result<()> {
        validate_scroll_options(direction, amount)?;
        self.move_pointer(x, y)?;
        let (h, n) = match direction {
            ScrollDirection::Up => (false, -1.0),
            ScrollDirection::Down => (false, 1.0),
            ScrollDirection::Left => (true, -1.0),
            ScrollDirection::Right => (true, 1.0),
        };
        self.input()?.axis(h, n * 15.0 * amount as f64)
    }
    fn key(&self, key: &str) -> Result<()> {
        let symbols = input::keysyms(key)?;
        let mut i = self.input()?;
        for sym in &symbols {
            i.keysym(*sym, true)?;
        }
        for sym in symbols.iter().rev() {
            i.keysym(*sym, false)?;
        }
        Ok(())
    }
    fn type_text(&self, text: &str) -> Result<()> {
        let mut i = self.input()?;
        for c in text.chars() {
            let sym = match c {
                '\n' | '\r' => 0xff0d,
                '\t' => 0xff09,
                _ => xkbcommon::xkb::utf32_to_keysym(c as u32).raw(),
            };
            i.keysym(sym, true)?;
            i.keysym(sym, false)?;
        }
        Ok(())
    }
    fn set_clipboard(&self, text: &str) -> Result<WorkspaceClipboard> {
        validate_clipboard_text(text)?;
        let mut child = self
            .command("timeout")
            .args(["5s", "wl-copy", "--type"])
            .arg("text/plain;charset=utf-8")
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
            bail!("workspace wl-copy failed");
        }
        Ok(WorkspaceClipboard {
            selection: "clipboard".to_string(),
            content: Some(text.to_string()),
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
                "workspace clipboard is unavailable: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = String::from_utf8(output.stdout)?;
        Ok(WorkspaceClipboard {
            bytes: text.len() as u64,
            selection: "clipboard".to_string(),
            content: Some(text),
        })
    }
}
fn linux_button(button: u8) -> Result<u32> {
    match button {
        1 => Ok(272),
        2 => Ok(274),
        3 => Ok(273),
        _ => bail!("KWin pointer buttons must be 1, 2 or 3"),
    }
}
fn decode_image(
    reader: &mut impl Read,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Result<Vec<u8>> {
    if width == 0
        || height == 0
        || stride < width.checked_mul(4).context("image width overflow")?
        || !matches!(format, 4..=6 | 17 | 18)
    {
        bail!(
            "unsupported KWin screenshot layout {width}x{height}, stride {stride}, format {format}"
        );
    }
    let length = (stride as usize)
        .checked_mul(height as usize)
        .context("screenshot size overflow")?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(length)?;
    raw.resize(length, 0);
    reader.read_exact(&mut raw)?;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in raw.chunks_exact(stride as usize) {
        for p in row[..width as usize * 4].as_chunks::<4>().0 {
            let (r, g, b, a) = if format <= 6 {
                (p[2], p[1], p[0], if format == 4 { 255 } else { p[3] })
            } else {
                (p[0], p[1], p[2], p[3])
            };
            let unpremul = |v: u8| {
                if a > 0 && matches!(format, 6 | 18) {
                    ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
                } else {
                    v
                }
            };
            rgba.extend_from_slice(&[unpremul(r), unpremul(g), unpremul(b), a]);
        }
    }
    Ok(rgba)
}

fn read_frame(runtime: &std::path::Path, window: Option<&str>) -> Result<(u32, u32, Vec<u8>)> {
    let connection = Builder::address(bus_address(runtime).as_str())?
        .method_timeout(Duration::from_secs(5))
        .build()?;
    let proxy = Proxy::new(
        &connection,
        "org.kde.KWin.ScreenShot2",
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
    )?;
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_read_timeout(Some(Duration::from_secs(5)))?;
    let options: HashMap<&str, zbus::zvariant::Value<'_>> = [
        ("include-cursor", true.into()),
        ("include-decoration", true.into()),
        ("include-shadow", false.into()),
        ("native-resolution", true.into()),
        ("hide-caller-windows", false.into()),
    ]
    .into();
    let metadata: HashMap<String, OwnedValue> = if let Some(id) = window {
        proxy.call("CaptureWindow", &(id, &options, Fd::from(&writer)))?
    } else {
        proxy.call("CaptureActiveScreen", &(&options, Fd::from(&writer)))?
    };
    drop(writer);
    let get = |key: &str| -> Result<u32> {
        u32::try_from(
            metadata
                .get(key)
                .with_context(|| format!("screenshot missing {key}"))?,
        )
        .map_err(Into::into)
    };
    let (width, height, stride, format) = (
        get("width")?,
        get("height")?,
        get("stride")?,
        get("format")?,
    );
    let pixels = decode_image(&mut reader, width, height, stride, format)?;
    Ok((width, height, pixels))
}

/// Check mandatory native interfaces before the daemon publishes ready=true.
pub(crate) fn probe(runtime: &std::path::Path) -> Result<(u32, u32)> {
    let connection = Builder::address(bus_address(runtime).as_str())?
        .method_timeout(Duration::from_secs(5))
        .build()?;
    let dbus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    if !dbus.name_has_owner("org.kde.KWin.ScreenShot2".try_into()?)? {
        bail!("KWin's native screenshot plugin is missing; install kwin-common and restart the workspace");
    }
    input::Input::connect(runtime)?;
    let (width,height,_) = read_frame(runtime,None).context("KWin native capture probe failed; an accessible DRM render node and OpenGL compositing are required")?;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qt_screenshot_formats_preserve_channels_stride_and_alpha() {
        let raw = [25, 50, 100, 128, 9, 9, 9, 9];
        assert_eq!(
            decode_image(&mut &raw[..], 1, 1, 8, 6).unwrap(),
            [199, 100, 50, 128]
        );
        assert_eq!(
            decode_image(&mut &raw[..], 1, 1, 8, 4).unwrap(),
            [100, 50, 25, 255]
        );
        assert_eq!(
            decode_image(&mut &raw[..], 1, 1, 8, 17).unwrap(),
            [25, 50, 100, 128]
        );
        assert!(decode_image(&mut &raw[..], 2, 1, 4, 6).is_err());
        assert!(decode_image(&mut &raw[..], 1, 3, 4, 6).is_err());
        assert!(decode_image(&mut &raw[..], 1, 1, 4, 99).is_err());
    }
}
