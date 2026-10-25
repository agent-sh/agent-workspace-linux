#!/usr/bin/env python3
"""List the Wayland globals this client can see and bind the privileged ones.

Standard library only, so it runs inside any sandbox that has python3. Prints
one JSON object: every advertised global, the result of binding each
privileged one, and the result of binding each privileged global the
unfiltered main socket advertised (argv[1], a JSON {name: interface} map) by
its real name and interface, as a client that guesses hidden names would.
"""
import json
import os
import socket
import struct
import sys

# Interfaces a sandboxed app must not reach: screen/window capture, other
# toplevels, input injection or key interception, covering or locking the
# session, output reconfiguration, clipboard sniffing and similar.
PRIVILEGED = {
    # capture
    "zwlr_screencopy_manager_v1", "zwlr_export_dmabuf_manager_v1",
    "ext_image_copy_capture_manager_v1", "ext_output_image_capture_source_manager_v1",
    "ext_foreign_toplevel_image_capture_source_manager_v1",
    "hyprland_toplevel_export_manager_v1", "zkde_screencast_unstable_v1",
    "org_kde_kwin_remote_access_manager", "cosmic_screencopy_manager_v1",
    "cosmic_screencopy_manager_v2", "zcosmic_screencopy_manager_v1",
    "treeland_capture_manager_v1",
    # other toplevels and workspaces
    "zwlr_foreign_toplevel_manager_v1", "ext_foreign_toplevel_list_v1",
    "org_kde_plasma_window_management", "org_kde_plasma_virtual_desktop_management",
    "org_kde_plasma_activation_feedback", "hyprland_toplevel_mapping_manager_v1",
    "zcosmic_toplevel_info_v1", "zcosmic_toplevel_manager_v1",
    "ext_workspace_manager_v1", "zcosmic_workspace_manager_v1",
    "zcosmic_workspace_manager_v2", "hyprland_focus_grab_manager_v1",
    # input injection and interception
    "zwp_virtual_keyboard_manager_v1", "zwlr_virtual_pointer_manager_v1",
    "org_kde_kwin_fake_input", "zwp_input_method_manager_v2", "zwp_input_method_v1",
    "zwp_input_panel_v1", "zwp_input_method_context_v1", "org_kde_kwin_keystate",
    "hyprland_global_shortcuts_manager_v1", "zwp_xwayland_keyboard_grab_manager_v1",
    "xwayland_shell_v1", "zcosmic_virtual_pointer_manager_v1",
    # covering or locking the session
    "zwlr_layer_shell_v1", "ext_session_lock_manager_v1", "kde_lockscreen_overlay_v1",
    "org_kde_plasma_shell", "hyprland_lock_notifier_v1", "kde_screen_edge_manager_v1",
    "cosmic_overlap_notify_v1",
    # outputs
    "zwlr_output_manager_v1", "zwlr_output_power_manager_v1",
    "zwlr_gamma_control_manager_v1", "hyprland_ctm_control_manager_v1",
    "kde_output_management_v2", "org_kde_kwin_outputmanagement",
    "org_kde_kwin_dpms_manager", "wp_drm_lease_device_v1", "zcosmic_output_manager_v1",
    "kde_external_brightness_v1",
    # clipboard sniffing
    "zwlr_data_control_manager_v1", "ext_data_control_manager_v1",
}


class ProtocolError(Exception):
    pass


class Client:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(5)
        self.sock.connect(path)
        self.next_id = 2
        self.buffer = b""
        self.registry = self.new_id()
        self.send(1, 1, struct.pack("<I", self.registry))
        self.globals = {}

    def new_id(self):
        value = self.next_id
        self.next_id += 1
        return value

    def send(self, obj, opcode, payload):
        self.sock.sendall(struct.pack("<II", obj, (8 + len(payload)) << 16 | opcode) + payload)

    @staticmethod
    def string(value):
        data = value.encode() + b"\0"
        return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)

    def roundtrip(self):
        callback = self.new_id()
        self.send(1, 0, struct.pack("<I", callback))
        while True:
            while len(self.buffer) < 8:
                chunk = self.sock.recv(65536)
                if not chunk:
                    raise ProtocolError("compositor closed the connection")
                self.buffer += chunk
            obj, word = struct.unpack("<II", self.buffer[:8])
            size, opcode = word >> 16, word & 0xFFFF
            while len(self.buffer) < size:
                chunk = self.sock.recv(65536)
                if not chunk:
                    raise ProtocolError("compositor closed the connection")
                self.buffer += chunk
            body, self.buffer = self.buffer[8:size], self.buffer[size:]
            if obj == 1 and opcode == 0:
                _, code, length = struct.unpack("<III", body[:12])
                raise ProtocolError(f"error {code}: {body[12:12 + length - 1].decode(errors='replace')}")
            if obj == self.registry and opcode == 0:
                name, length = struct.unpack("<II", body[:8])
                interface = body[8:8 + length - 1].decode()
                offset = 8 + length + (-length % 4)
                (version,) = struct.unpack("<I", body[offset:offset + 4])
                self.globals[name] = (interface, version)
            elif obj == self.registry and opcode == 1:
                self.globals.pop(struct.unpack("<I", body[:4])[0], None)
            elif obj == callback and opcode == 0:
                return

    def bind(self, name, interface, version):
        self.send(self.registry, 0, struct.pack("<I", name) + self.string(interface)
                  + struct.pack("<II", version, self.new_id()))
        self.roundtrip()


def socket_path():
    display = os.environ.get("WAYLAND_DISPLAY", "wayland-0")
    if display.startswith("/"):
        return display
    return os.path.join(os.environ["XDG_RUNTIME_DIR"], display)


def try_bind(path, name, interface):
    # One connection per bind: a protocol error ends the connection.
    trial = Client(path)
    try:
        trial.roundtrip()
        trial.bind(name, interface, 1)
        return "bound"
    except (ProtocolError, OSError) as error:
        return f"refused: {error}"


def probe(guesses):
    path = socket_path()
    client = Client(path)
    client.roundtrip()
    visible = sorted({interface for interface, _ in client.globals.values()})
    binds = {}
    for name, (interface, version) in sorted(client.globals.items()):
        if interface in PRIVILEGED:
            binds[interface] = try_bind(path, name, interface)
    guessed = {interface: try_bind(path, int(name), interface) for name, interface in guesses.items()
               if (int(name), interface) not in {(n, i) for n, (i, _) in client.globals.items()}}
    return {"socket": path, "globals": visible,
            "names": {str(name): interface for name, (interface, _) in client.globals.items()},
            "privileged_visible": sorted(set(visible) & PRIVILEGED), "privileged_bind": binds,
            "hidden_bind": guessed}


if __name__ == "__main__":
    result = probe(json.loads(sys.argv[1]) if len(sys.argv) > 1 else {})
    print(json.dumps(result, sort_keys=True))
    sys.exit(0)
