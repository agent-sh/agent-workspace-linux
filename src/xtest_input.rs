//! Native X11 input and focus.
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
///   focus WINDOW_ID
///   focus-check WINDOW_ID
///   primary-once PID   (text on stdin; serves PRIMARY once to PID)
///   content-check WINDOW_ID [VISIBLE_WORKSPACE_WINDOW_ID ...]
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
        Some("window-pid") if args.len() == 2 => {
            use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt as _};
            let window: u32 = args[1].parse().context("invalid X11 window id")?;
            let reply = conn
                .res_query_client_ids(&[ClientIdSpec {
                    client: window,
                    mask: ClientIdMask::LOCAL_CLIENT_PID,
                }])?
                .reply()?;
            let pid = reply
                .ids
                .iter()
                .find_map(|id| id.value.first().copied())
                .context("XRes did not return a local client PID")?;
            println!("{pid}");
        }
        Some("content-check") if args.len() >= 2 => {
            let window: u32 = args[1].parse().context("invalid X11 window id")?;
            let visible = args[2..]
                .iter()
                .map(|id| id.parse::<u32>())
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let attributes = conn.get_window_attributes(window)?.reply()?;
            let mut current = window;
            let mut content = false;
            if attributes.map_state == x11rb::protocol::xproto::MapState::VIEWABLE {
                while current != 0 && current != root {
                    if visible.contains(&current) {
                        content = true;
                        break;
                    }
                    current = conn.query_tree(current)?.reply()?.parent;
                }
            }
            println!("{content}");
        }
        Some("primary-once") if args.len() == 2 => {
            let pid: u32 = args[1].parse().context("invalid PRIMARY target PID")?;
            serve_primary_once(&conn, root, pid)?;
        }
        Some("focus-check") if args.len() == 2 => {
            let window: u32 = args[1].parse().context("invalid X11 focus window id")?;
            println!("{}", focus_request_kind(&conn, window)?);
        }
        Some("focus") if args.len() == 2 => {
            let window: u32 = args[1].parse().context("invalid X11 focus window id")?;
            focus_window_on_connection(&conn, window)?;
        }
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

/// Own PRIMARY with text read from stdin and serve it to exactly one
/// UTF-8 request from a client whose XRes PID is `pid`, then exit. Exiting
/// drops ownership, so no other client can read the text afterwards.
/// Requests from other clients are refused. Prints `owned` once PRIMARY is
/// held and `served` after the target client received the text.
fn serve_primary_once(
    conn: &x11rb::rust_connection::RustConnection,
    root: u32,
    pid: u32,
) -> Result<()> {
    use std::io::{Read, Write};
    use x11rb::connection::RequestConnection as _;
    use x11rb::protocol::{
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xproto::{
            AtomEnum, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent, WindowClass,
            SELECTION_NOTIFY_EVENT,
        },
        Event,
    };
    use x11rb::wrapper::ConnectionExt as _;
    let mut text = Vec::new();
    std::io::stdin().read_to_end(&mut text)?;
    if std::str::from_utf8(&text).is_err() {
        bail!("PRIMARY text is not UTF-8");
    }
    // One property write must carry the whole text; INCR is not implemented.
    if text.len() + 64 > conn.maximum_request_bytes() {
        bail!(
            "text is {} bytes; one X selection transfer holds at most {}",
            text.len(),
            conn.maximum_request_bytes() - 64
        );
    }
    let atom = |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    let (utf8, text_atom, targets, stamp) = (
        atom(b"UTF8_STRING")?,
        atom(b"TEXT")?,
        atom(b"TARGETS")?,
        atom(b"AGENT_WORKSPACE_PRIMARY_TIME")?,
    );
    let window = conn.generate_id()?;
    conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()?;
    // ICCCM forbids CurrentTime for ownership; a property change yields a
    // real server timestamp.
    conn.change_property8(PropMode::REPLACE, window, stamp, AtomEnum::STRING, b"")?;
    conn.flush()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let time = loop {
        match conn.poll_for_event()? {
            Some(Event::PropertyNotify(event)) if event.window == window => break event.time,
            Some(_) => {}
            None if std::time::Instant::now() >= deadline => {
                bail!("X server did not report a timestamp")
            }
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    };
    conn.set_selection_owner(window, u32::from(AtomEnum::PRIMARY), time)?
        .check()?;
    if conn
        .get_selection_owner(u32::from(AtomEnum::PRIMARY))?
        .reply()?
        .owner
        != window
    {
        bail!("could not take PRIMARY ownership");
    }
    println!("owned");
    std::io::stdout().flush()?;
    let requester_pid = |requestor: u32| -> Option<u32> {
        conn.res_query_client_ids(&[ClientIdSpec {
            client: requestor,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        }])
        .ok()?
        .reply()
        .ok()?
        .ids
        .iter()
        .find_map(|id| id.value.first().copied())
    };
    let mut refused = Vec::new();
    loop {
        let Some(event) = conn.poll_for_event()? else {
            if std::time::Instant::now() >= deadline {
                bail!("target client did not request PRIMARY; refused requests: {refused:?}");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        };
        match event {
            Event::SelectionClear(event) if event.owner == window => {
                bail!("PRIMARY was replaced before the target client read it");
            }
            Event::SelectionRequest(request) if request.owner == window => {
                let property = if request.property == 0 {
                    request.target
                } else {
                    request.property
                };
                let from_target = requester_pid(request.requestor) == Some(pid);
                let served = if request.selection != u32::from(AtomEnum::PRIMARY) {
                    None
                } else if request.target == targets {
                    conn.change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[targets, utf8, text_atom],
                    )?;
                    Some(false)
                } else if (request.target == utf8 || request.target == text_atom) && from_target {
                    conn.change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        utf8,
                        &text,
                    )?;
                    Some(true)
                } else {
                    refused.push((requester_pid(request.requestor), request.target));
                    None
                };
                conn.send_event(
                    false,
                    request.requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: if served.is_some() { property } else { 0 },
                    },
                )?;
                conn.flush()?;
                if served == Some(true) {
                    conn.get_input_focus()?.reply()?;
                    println!("served");
                    std::io::stdout().flush()?;
                    return Ok(());
                }
            }
            _ => {}
        }
    }
}

// Only WM-managed top-level windows need an activation request. Popups and
// input children use direct focus; sending EWMH for them can steal child focus.
fn focus_request_kind(
    conn: &x11rb::rust_connection::RustConnection,
    target: u32,
) -> Result<&'static str> {
    if focus_is_within(conn, target)? {
        return Ok("focused");
    }
    if conn
        .get_window_attributes(target)?
        .reply()?
        .override_redirect
    {
        return Ok("direct");
    }
    let state = conn.intern_atom(true, b"WM_STATE")?.reply()?.atom;
    if state == 0
        || conn
            .get_property(
                false,
                target,
                state,
                x11rb::protocol::xproto::AtomEnum::ANY,
                0,
                0,
            )?
            .reply()?
            .type_
            == 0
    {
        return Ok("direct");
    }
    Ok("managed")
}

// Focus is queried through X11, never inferred from window-manager text or titles.
fn focus_is_within(conn: &x11rb::rust_connection::RustConnection, target: u32) -> Result<bool> {
    let mut current = conn.get_input_focus()?.reply()?.focus;
    while current > 1 {
        if current == target {
            return Ok(true);
        }
        let parent = conn.query_tree(current)?.reply()?.parent;
        if parent == 0 || parent == current {
            return Ok(false);
        }
        current = parent;
    }
    Ok(false)
}

fn focus_window_on_connection(
    conn: &x11rb::rust_connection::RustConnection,
    target: u32,
) -> Result<()> {
    if !focus_is_within(conn, target)? {
        conn.set_input_focus(
            x11rb::protocol::xproto::InputFocus::PARENT,
            target,
            x11rb::CURRENT_TIME,
        )?
        .check()?;
        conn.flush()?;
    }
    if !focus_is_within(conn, target)? {
        bail!("X11 did not assign keyboard focus to window {target}");
    }
    Ok(())
}

pub fn focus_window(target: u32) -> Result<()> {
    let (conn, _) = x11rb::connect(None).context("connect to viewer X11 display")?;
    focus_window_on_connection(&conn, target)
}
