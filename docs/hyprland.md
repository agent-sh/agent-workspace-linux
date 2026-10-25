# Private Hyprland workspaces

Select `--session hyprland` on workspace start, or save
`"session": {"backend":"hyprland", "scale":1.5}` in a profile or MCP request.
The default remains X11. Workspace dimensions and pointer coordinates are
physical pixels, including fractional output scales.

The backend targets Hyprland 0.53 and KWin 6.6. It requires `Hyprland`,
`kwin_wayland`, `Xwayland`, `xauth`, `xdpyinfo`, `grim`, `wtype`, `wl-copy`,
`wl-paste`, `dbus-daemon`, and `timeout`, plus an accessible DRM render node.

Aquamarine requires a renderer-backed allocator even with headless outputs.
The workspace starts a private KWin virtual compositor to supply that allocator,
then creates a Hyprland headless output and removes the nested Wayland output.
KWin supplies no desktop shell. Neither compositor connects to a host display,
input device, system bus, or DRM card. Weston 14 cannot serve as the parent:
Aquamarine requests `wl_compositor` version 6 while Weston advertises version 5.

The control client discovers the Hyprland IPC socket under this workspace's
runtime and checks its peer PID before publishing the path. It never uses an
inherited `HYPRLAND_INSTANCE_SIGNATURE`. A directory descriptor supplies a short
private path for Hyprland IPC sockets, whose hash-based names would otherwise
exceed Linux Unix socket path limits. App environments use the actual workspace
paths, with no descriptor aliases. Restricted apps receive only the private
display, authenticated XWayland socket, private session bus and accessibility
bus. They cannot access the compositor control socket or its parent's display.
Their Wayland socket is the daemon's filter, not Hyprland's main socket. Hyprland
offers screencopy, toplevel export, virtual keyboard and pointer, input method,
layer shell, session lock, output, gamma and data-control globals to every client
of its main socket, and keeps virtual keyboard, pointer and input method even for
`wp_security_context_v1` clients. The filter relays each sandboxed client over a
security-context connection and forwards only globals an ordinary window needs.
The private configuration has no application launch bindings, config includes,
portal activation, or arbitrary D-Bus service activation.

Hyprland's XWayland launcher does not supply an authority file. A workspace-owned
launcher adds a fresh private authority file before starting XWayland, including
for its abstract socket. The resulting display and authority are used for legacy
X11 apps. Native apps use the workspace Wayland socket.

Window capture uses `hyprland-toplevel-export-v1` directly. It captures the target
surface without focusing, raising, or cropping occluding windows. Root and viewer
frames use `grim` against the explicitly named private output. Input uses the
private `wlr-virtual-pointer` protocol and `wtype`; clipboard ownership uses
`wl-clipboard`. Minimize moves a window to a private hidden special workspace.
Show restores it to workspace 1.

Run `scripts/hyprland_probe.sh` after building the binary. The disposable probe
uses only a DRM render node and verifies native 100% and 150% scale, delivered Qt
pointer events, Unicode text, clipboard, window operations, read-only window
capture, viewer frame metadata, private service restrictions, legacy XWayland
apps inside a network and mount namespace, and process cleanup. The host must
provide its normal GPU lease before invoking the probe.

Primary implementation references:

- [Hyprland compositor initialization](https://github.com/hyprwm/Hyprland/blob/v0.53.3/src/Compositor.cpp)
- [Aquamarine Wayland renderer](https://github.com/hyprwm/aquamarine/blob/v0.10.0/src/backend/Wayland.cpp)
- [Native toplevel export protocol](https://github.com/hyprwm/hyprland-protocols/blob/main/protocols/hyprland-toplevel-export-v1.xml)
- [Hyprland XWayland launcher](https://github.com/hyprwm/Hyprland/blob/v0.53.3/src/xwayland/Server.cpp)
