# Private native COSMIC sessions

The COSMIC backend runs `cosmic-comp` as a native Wayland compositor. Its
parent is a separate Weston headless compositor using Mesa's software GL
renderer and kiosk shell. The parent has no X11 display, host Wayland socket, input device,
DRM device, session bus, or system bus. Applications connect to COSMIC, not to
Weston. X11 applications use COSMIC's authenticated child XWayland.

This chain is required by current COSMIC. Upstream commit
`8cb1802caada438bb5cc7670dfabdaca5fad0502` provides KMS, X11 and winit backends,
but no direct headless backend. Its winit backend can use a Wayland parent.
Weston's kiosk shell supplies the requested physical output dimensions. Output
scale is applied and read back through COSMIC's output-management protocol.

```
agent-workspace-linux workspace start --ack-hidden-workspace \
  --session cosmic --width 1200 --height 900 --scale 1.5
```

Install COSMIC 1.0 with the EI text input protocol, Weston with its headless
backend and kiosk shell, XWayland, xrdb, xauth, D-Bus, wl-clipboard, bubblewrap and coreutils.
`doctor` reports missing COSMIC dependencies separately. A workspace is not
ready until COSMIC publishes its client environment and the native geometry,
output scale and EI input checks succeed.

## Controls and coordinates

The backend uses COSMIC toplevel information and management, ext image-copy
capture, wlr output management, and COSMIC's private D-Bus EI input socket.
Capture uses shared memory and does not require a GPU. Window identifiers are
COSMIC's stable foreign-toplevel identifiers. PID is unavailable from this
protocol and remains unset.

All public coordinates and screenshot dimensions are physical pixels.
EI pointer regions use logical coordinates. The adapter reads the compositor's
scale and converts input once. COSMIC's winit output uses a flipped capture
transform; screenshots and cursor observations are transformed back to the
same pixel grid. A client's buffer scale can be an integer even when COSMIC's
output scale is fractional. The client buffer scale is not used as the output
scale.

COSMIC has no direct window-position or window-size request. Move and resize
use the compositor's native Super plus left/right pointer gestures, then wait
for the requested geometry to be observed. Refused geometry returns an error.
Focus, minimize, show and close also wait for the resulting native state.
Raising a window uses COSMIC activation and also gives it keyboard focus.

## Launch isolation

Only `cosmic-comp` runs, not `cosmic-session`, panels, a desktop portal or a
settings daemon. COSMIC is started in its supported kiosk mode with an owned
workspace guard child. Kiosk mode disables configured shortcuts and their
configuration watcher. It does not turn every workspace application into a
fullscreen window.

The compositor's private mount namespace has a read-only PATH containing only an XWayland launcher and `xrdb`. Bubblewrap copies the workspace executable into a private read-only mount; changing the host file or its permissions cannot change that copy. This is an executable, not a shell script, so a noexec runtime mount cannot make COSMIC fall back to an unauthenticated server.
Smithay clears the XWayland environment except `PATH` and `XDG_RUNTIME_DIR`, so the launcher takes its configuration from a read-only descriptor reserved before Bubblewrap starts, not from the environment. The configuration must name the COSMIC backend. The launcher rejects `-ac` and `-auth` arguments, refuses an Xauthority file that is missing, empty, not owned by the workspace user or readable by others, and execs the resolved XWayland binary with `-auth` set to the workspace's private Xauthority file. Any failure stops XWayland; workspace start then fails because COSMIC publishes no display. There is no unauthenticated fallback. The PATH directory is not mounted into restricted app
sandboxes. Network-only clients retain host filesystem access, but the compositor
uses the separate immutable copy. Both parent and child compositors receive a
private `/dev` with no input or DRM devices. There is no host PATH fallback. The private session bus has no
application activation services; the accessibility bus has only its registry.
The system bus is private and contains no activation services.

Sandboxed apps connect to a separate `wayland-apps` socket. At startup the
daemon registers it with COSMIC through `wp_security_context_v1`, with the
sandbox engine `agent-workspace-linux`, and keeps its close descriptor open for
the session. COSMIC withholds its privileged globals from every client of that
socket: toplevel info and management, workspaces, image capture, output
configuration and power, virtual keyboard, input method, layer shell, session
lock, keyboard layout, data control, accessibility and nested security
contexts. Startup fails if the socket advertises any of them. Only this socket
is bound into sandboxes. The compositor socket stays with the daemon's
helpers, attached controllers and unsandboxed launches, matching the control
bus: those run as the same user with full file access, and desktop services
such as portals need capture.

The current upstream spawn inventory is:

| Source | Spawn | Restriction |
| --- | --- | --- |
| `src/lib.rs` | Kiosk child | Exact workspace executable and internal guard entrypoint |
| `src/xwayland.rs` | XWayland and xrdb | Exact private PATH entries; Xauthority enforced by wrapper |
| `src/systemd.rs` | systemctl | Absent from private PATH; no host bus |
| `src/input/actions.rs` | `/bin/sh -c` for configured actions | Kiosk mode removes actions and disables their watcher |
| `src/shell/zoom.rs` | `cosmic-settings accessibility-magnifier` | Hardcoded name absent from private PATH |

## Renderer errors during screen capture

`cosmic.log` records EGL errors for each screen capture. They come from the
upstream winit backend and recover within the same frame. Acceptance checks
their exact signature and count, and fails on any other compositor error.

For an output capture, COSMIC binds the window surface, renders, then copies
the frame into the capture buffer from inside `render_output`. That copy binds
an offscreen texture, which makes the EGL context current without a surface.
The following swap then fails with `EGL_BAD_SURFACE`, because Mesa requires
the surface to be the current draw surface (`eglapi.c`). Smithay swaps with
`eglSwapBuffersWithDamageKHR` when the frame has damage and with
`eglSwapBuffers` otherwise, so the log names either call. Smithay
answers `EGL_BAD_SURFACE` by creating a new window surface before destroying
the old one. Mesa refuses a second surface on the same `wl_egl_window` with
`EGL_BAD_ALLOC` (`platform_wayland.c`), so the old surface stays in place.
COSMIC logs `Failed to render frame` and immediately renders again. The retry's
buffer-age query fails with `EGL_BAD_SURFACE` for the same reason (`eglsurface.c`),
which only forces a full repaint. Its bind restores the surface and the swap succeeds.
Window captures render offscreen between frames, so they cause only that
buffer-age error on the next frame. The `Erroneous EGL call didn't set EGLError`
warning at startup and at each capture is the probe for a DRM device, which
software rendering does not have.

Evidence from Mesa 26.0.8, Weston 14.0.2 and cosmic-comp 8cb1802:

- Errors appear only during captures. Idle time, typing, pointer input, moves
  and resizes log none. Each output capture logs one failed swap, and each window capture logs one buffer-age error.
- With Wayland client tracing, the parent received the retried frame 4 to 20 ms
  after each failed swap. Frame intervals stayed between 22.9 and 28.4 ms around
  failures, against a 25.4 ms median, so no frame was skipped.
- Captured pixels come from the rendered frame before the swap. Acceptance
  locates the native widgets from those captures and verifies their effects.

The upstream fix is to make the window surface current again before `submit`
in `src/backend/winit.rs`, and to query buffer age after `bind`.

Upstream sources:
[backend selection](https://github.com/pop-os/cosmic-comp/blob/8cb1802caada438bb5cc7670dfabdaca5fad0502/src/backend/mod.rs),
[winit output](https://github.com/pop-os/cosmic-comp/blob/8cb1802caada438bb5cc7670dfabdaca5fad0502/src/backend/winit.rs),
[kiosk shortcut handling](https://github.com/pop-os/cosmic-comp/blob/8cb1802caada438bb5cc7670dfabdaca5fad0502/src/config/mod.rs),
[EI API](https://github.com/pop-os/cosmic-comp/blob/8cb1802caada438bb5cc7670dfabdaca5fad0502/src/dbus/ei.rs).

## Container testing

`scripts/cosmic_probe.sh` runs acceptance in a container as an unprivileged user
with no capabilities, devices, network or host display. Docker masks parts of
`/proc`, which stops Bubblewrap from mounting the fresh procfs that restricted
apps get. The probe sets `--security-opt systempaths=unconfined` for that reason.
It does not grant the container any capability.
