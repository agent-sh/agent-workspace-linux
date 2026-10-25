//! Focused legacy text insertion through this workspace's accessibility bus.
//! XWayland's keymap changes are asynchronous; do not claim Unicode delivery
//! from a successful XTEST request alone.
use crate::workspace::WorkspaceStatus;
use anyhow::{bail, Context, Result};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    time::{Duration, Instant},
};
use zbus::{
    blocking::{connection::Builder, Proxy},
    zvariant::OwnedObjectPath,
};

type Object = (String, OwnedObjectPath);

// AtspiRole values from at-spi2-core atspi-constants.h.
const ROLE_PASSWORD_TEXT: u32 = 40;
const ROLE_TEXT: u32 = 61;
const ROLE_ENTRY: u32 = 79;

pub(super) fn insert(
    status: &WorkspaceStatus,
    compositor: u32,
    window_id: &str,
    pid: u32,
    text: &str,
) -> Result<()> {
    let address = fs::read_to_string(status.runtime_dir.join("atspi-address"))?;
    let connection = Builder::address(address.trim())?
        .method_timeout(Duration::from_secs(5))
        .build()?;
    let dbus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    let root = Proxy::new(
        &connection,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
        "org.a11y.atspi.Accessible",
    )?;
    let apps: Vec<Object> = root.call("GetChildren", &())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    for (owner, path) in apps {
        if dbus
            .get_connection_unix_process_id(owner.as_str().try_into()?)
            .ok()
            != Some(pid)
        {
            continue;
        }
        let application_path = path.clone();
        let mut queue = VecDeque::from([path]);
        let mut visited = HashSet::new();
        while let Some(path) = queue.pop_front() {
            if Instant::now() >= deadline {
                bail!("timed out finding the focused legacy editable field");
            }
            if !visited.insert(path.clone()) {
                continue;
            }
            let accessible = Proxy::new(
                &connection,
                owner.as_str(),
                path.as_str(),
                "org.a11y.atspi.Accessible",
            )?;
            let state: Vec<u32> = accessible.call("GetState", &())?;
            let interfaces: Vec<String> = accessible.call("GetInterfaces", &())?;
            let role: u32 = accessible.call("GetRole", &())?;
            // AT-SPI StateType::Focused is bit12; interface membership is
            // checked separately so read-only text never becomes editable.
            // Role names differ per toolkit (GTK4 says "text box"), so match
            // the AT-SPI role enum: TEXT for GTK and Qt fields, ENTRY for
            // Chromium, Firefox, Electron and search fields. Readback below
            // applies to every role.
            if matches!(role, ROLE_TEXT | ROLE_ENTRY | ROLE_PASSWORD_TEXT)
                && state.first().is_some_and(|s| s & (1 << 12) != 0)
                && ["org.a11y.atspi.EditableText", "org.a11y.atspi.Text"]
                    .iter()
                    .all(|interface| interfaces.iter().any(|s| s == interface))
            {
                if role == ROLE_PASSWORD_TEXT {
                    bail!("legacy password text cannot be verified through accessibility");
                }
                let target = zbus::blocking::proxy::Builder::<Proxy>::new(&connection)
                    .destination(owner.as_str())?
                    .path(path.as_str())?
                    .interface("org.a11y.atspi.Text")?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()?;
                let editable = Proxy::new(
                    &connection,
                    owner.as_str(),
                    path.as_str(),
                    "org.a11y.atspi.EditableText",
                )?;
                let application = zbus::blocking::proxy::Builder::<Proxy>::new(&connection)
                    .destination(owner.as_str())?
                    .path(application_path.as_str())?
                    .interface("org.a11y.atspi.Application")?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()?;
                let toolkit: String = application.get_property("ToolkitName")?;
                let utf16_len = i32::try_from(text.encode_utf16().count())?;
                let insert_len = if toolkit == "Qt" {
                    utf16_len
                } else {
                    i32::try_from(text.len())?
                };
                let count: i32 = target
                    .call("GetNSelections", &())
                    .context("legacy GetNSelections")?;
                if !(0..=1).contains(&count) {
                    bail!("legacy text has unsupported multiple selections");
                }
                let (start, end) = if count == 1 {
                    target
                        .call::<_, _, (i32, i32)>("GetSelection", &(0i32,))
                        .context("legacy GetSelection")?
                } else {
                    let caret: i32 = target
                        .get_property("CaretOffset")
                        .context("legacy CaretOffset")?;
                    (caret, caret)
                };
                let (start, end) = (start.min(end), start.max(end));
                if start < 0 || end < start {
                    bail!("legacy text caret/selection is invalid");
                }
                let suffix: String = target
                    .call("GetText", &(end, i32::MAX))
                    .context("legacy suffix readback")?;
                let active = super::call(
                    &status.runtime_dir,
                    compositor,
                    serde_json::json!({"op":"active"}),
                )?;
                let state: Vec<u32> = accessible.call("GetState", &())?;
                if active["id"].as_str() != Some(window_id)
                    || !state.first().is_some_and(|s| s & (1 << 12) != 0)
                {
                    bail!("legacy text focus changed before insertion");
                }
                if end > start {
                    let deleted: bool = editable
                        .call("DeleteText", &(start, end))
                        .context("legacy DeleteText")?;
                    if !deleted {
                        bail!("legacy editable field rejected selection deletion");
                    }
                }
                let inserted: bool = editable
                    .call("InsertText", &(start, text, insert_len))
                    .context("legacy InsertText")?;
                if !inserted {
                    bail!("legacy editable field rejected text insertion");
                }
                let scalar_end = start
                    .checked_add(i32::try_from(text.chars().count())?)
                    .context("text offset overflow")?;
                let utf16_end = start
                    .checked_add(utf16_len)
                    .context("text offset overflow")?;
                let mut last_readback = None;
                loop {
                    let current: i32 = target
                        .get_property("CaretOffset")
                        .context("legacy CaretOffset readback")?;
                    for caret in [current, scalar_end, utf16_end] {
                        if caret < start {
                            continue;
                        }
                        let actual: String = target
                            .call("GetText", &(start, caret))
                            .context("legacy GetText")?;
                        if actual == text {
                            let trailing: String = target
                                .call("GetText", &(caret, i32::MAX))
                                .context("legacy trailing text readback")?;
                            if trailing == suffix {
                                if caret != current {
                                    let moved: bool = target
                                        .call("SetCaretOffset", &(caret,))
                                        .context("legacy SetCaretOffset")?;
                                    if !moved {
                                        bail!(
                                            "legacy field inserted text but rejected caret update"
                                        );
                                    }
                                }
                                return Ok(());
                            }
                        }
                        last_readback = Some((caret, actual));
                    }
                    if Instant::now() >= deadline {
                        bail!("legacy text insertion did not match readback: {last_readback:?}");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let children: Vec<Object> = accessible.call("GetChildren", &())?;
            queue.extend(
                children
                    .into_iter()
                    .filter_map(|(bus, path)| (bus == owner).then_some(path)),
            );
        }
    }
    bail!("focused legacy window has no accessible editable text field; Unicode insertion is unavailable")
}
