//! Synthetic pointer input via the XTEST extension.
//!
//! `xdotool mousemove` positions the pointer with `XWarpPointer`, a server-side
//! warp that XInput2 clients (winit/egui, GTK4, …) do not observe as a
//! `CursorMoved`/motion event. Since winit's button event carries no position,
//! egui then resolves the click against a stale pointer location and the click
//! lands on the wrong widget (or nowhere).
//!
//! `XTestFakeMotionEvent` instead injects a real device motion that XInput2
//! reports, so the target app updates its pointer position before the button.
//! This helper performs the whole motion+button sequence over XTEST.
//!
//! It is spawned as an internal `__xtest` subcommand by the workspace daemon so
//! it inherits the per-workspace `DISPLAY`/`XAUTHORITY` (no global env mutation
//! in the long-lived server process, which must keep its own host `DISPLAY`).

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;

const MOTION_NOTIFY: u8 = 6;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;

/// Entry point for `agent-workspace-linux __xtest <op> ...`.
///
/// Ops (all coordinates are root-relative on the workspace display):
///   move  X Y
///   click X Y BUTTON [COUNT]
///   scroll X Y BUTTON [COUNT]
///   drag  X1 Y1 X2 Y2 BUTTON
pub fn run(args: &[String]) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("XTEST: connect to workspace display")?;
    let root = conn.setup().roots[screen_num].root;

    let motion = |x: i16, y: i16| -> Result<()> {
        conn.xtest_fake_input(MOTION_NOTIFY, 0, x11rb::CURRENT_TIME, root, x, y, 0)?;
        conn.flush()?;
        Ok(())
    };
    let button = |b: u8, press: bool| -> Result<()> {
        let kind = if press { BUTTON_PRESS } else { BUTTON_RELEASE };
        conn.xtest_fake_input(kind, b, x11rb::CURRENT_TIME, root, 0, 0, 0)?;
        conn.flush()?;
        Ok(())
    };
    // Let the app process the motion (emit CursorMoved) before the button.
    let settle = || std::thread::sleep(std::time::Duration::from_millis(25));

    // Callers validate/serialize coordinates as i32/u32 (no upper bound on
    // --width/--height), but the XTEST wire protocol carries root_x/root_y as
    // i16. Parse the wider type first, then narrow with an explicit range check
    // so an out-of-range coordinate (display larger than 32767 px) yields a
    // clear bounds error instead of a confusing "invalid" parse failure.
    let parse = |s: &str, what: &str| -> Result<i16> {
        let v: i32 = s
            .parse()
            .with_context(|| format!("XTEST: invalid {what}: {s:?}"))?;
        i16::try_from(v).with_context(|| {
            format!(
                "XTEST: {what} {v} out of range for XTEST (must be {}..={})",
                i16::MIN,
                i16::MAX
            )
        })
    };
    // Buttons are X11 button IDs (1..=255); parse as u8 so out-of-range or
    // negative input fails loudly instead of silently wrapping (e.g. 300 -> 44).
    let parse_button = |s: &str| -> Result<u8> {
        s.parse::<u8>()
            .with_context(|| format!("XTEST: invalid button: {s:?}"))
    };

    match args.first().map(String::as_str) {
        Some("move") if args.len() >= 3 => {
            motion(parse(&args[1], "x")?, parse(&args[2], "y")?)?;
        }
        Some("click") if args.len() >= 4 => {
            let (x, y) = (parse(&args[1], "x")?, parse(&args[2], "y")?);
            let b = parse_button(&args[3])?;
            let count = args
                .get(4)
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(1)
                .max(1);
            motion(x, y)?;
            settle();
            for _ in 0..count {
                button(b, true)?;
                std::thread::sleep(std::time::Duration::from_millis(15));
                button(b, false)?;
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Some("scroll") if args.len() >= 4 => {
            let (x, y) = (parse(&args[1], "x")?, parse(&args[2], "y")?);
            let b = parse_button(&args[3])?;
            let count = args
                .get(4)
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(1)
                .max(1);
            motion(x, y)?;
            settle();
            for _ in 0..count {
                button(b, true)?;
                button(b, false)?;
            }
        }
        Some("drag") if args.len() >= 6 => {
            let (x1, y1) = (parse(&args[1], "x1")?, parse(&args[2], "y1")?);
            let (x2, y2) = (parse(&args[3], "x2")?, parse(&args[4], "y2")?);
            let b = parse_button(&args[5])?;
            motion(x1, y1)?;
            settle();
            button(b, true)?;
            settle();
            motion(x2, y2)?;
            settle();
            button(b, false)?;
        }
        other => bail!("XTEST: unknown op {:?}", other),
    }

    // Round-trip so every faked event is processed before we exit.
    conn.get_input_focus()?.reply()?;
    Ok(())
}
