//! Shared key names for native desktop input.
use anyhow::{bail, Result};

pub(crate) fn keysyms(key: &str) -> Result<Vec<u32>> {
    use xkbcommon::xkb;
    let names = key
        .split('+')
        .map(|name| match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Control_L".to_owned(),
            "shift" => "Shift_L".to_owned(),
            "alt" => "Alt_L".to_owned(),
            "super" | "meta" | "win" => "Super_L".to_owned(),
            "enter" | "return" => "Return".to_owned(),
            "escape" | "esc" => "Escape".to_owned(),
            "backspace" => "BackSpace".to_owned(),
            "space" => "space".to_owned(),
            _ => name.to_owned(),
        });
    names
        .map(|name| {
            let sym = xkb::keysym_from_name(&name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
            if sym == 0 {
                bail!("unknown key name {name:?}");
            }
            Ok(sym)
        })
        .collect()
}
