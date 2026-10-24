#!/usr/bin/env python3
"""Real KWin 6 acceptance tests. Run inside kwin_probe's isolated container."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import time

BIN = os.environ.get("KWIN_PROBE_BIN", "/opt/agent-workspace-linux")
ARTIFACTS = Path(os.environ.get("KWIN_PROBE_ARTIFACT_DIR", "/artifacts"))
ARTIFACTS.mkdir(parents=True, exist_ok=True)
os.environ.update(XDG_RUNTIME_DIR="/tmp/awl-runtime", XDG_CONFIG_HOME="/tmp/awl-config", HOME="/tmp/awl-home", LANG="C.UTF-8")
for name in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "HOME"):
    Path(os.environ[name]).mkdir(mode=0o700, parents=True, exist_ok=True)

sequence = 0

def command(*args, expected=True):
    global sequence
    sequence += 1
    result = subprocess.run([BIN, *map(str, args)], capture_output=True, text=True, timeout=60)
    (ARTIFACTS / f"{sequence:03d}.json").write_text(result.stdout)
    (ARTIFACTS / f"{sequence:03d}.stderr").write_text(result.stderr)
    if not expected:
        assert result.returncode != 0, (args, result.stdout)
        return
    assert result.returncode == 0, (args, result.stderr)
    value = json.loads(result.stdout)
    assert value.get("ok", True), (args, value)
    return value


def workspace(id, *args):
    return command("workspace", args[0], "--id", id, *args[1:])


def wait_for(probe, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if probe():
            return
        time.sleep(0.1)
    raise AssertionError(f"timed out: {label}")


def check_png(path):
    data = Path(path).read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    assert len(data) > 1000
    return struct.unpack(">II", data[16:24])

assert not Path("/dev/uinput").exists()
assert not Path("/run/ydotoold/socket").exists()
assert "DISPLAY" not in os.environ and "WAYLAND_DISPLAY" not in os.environ
# Rootless XWayland needs its socket directory, never a parent X server.
Path("/tmp/.X11-unix").mkdir(mode=0o1777, exist_ok=True)
report = command("doctor")
assert report["ready_for_kwin_workspace"], report
assert not report["ready_for_x11_workspace"], "KWin must not need an X11 window manager"

for scale in (1, 1.5):
    id = f"kwin-{int(scale * 100)}"
    note = Path(f"/tmp/kwin-note-{scale}.txt")
    note.write_text("")
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "agent-workspace-linux" / id
    status = None
    try:
        start = workspace(id, "start", "--session", "kwin", "--scale", scale, "--width", 960, "--height", 720,
                          "--ack-hidden-workspace", "--purpose", "Native KWin acceptance test")
        status = workspace(id, "status")
        assert status["session"]["backend"] == "kwin"
        assert status["session"].get("scale", 1) == scale
        assert status["x_server_pid"] == 0
        assert status["window_manager_pid"]
        initial_x_sockets = set(Path("/tmp/.X11-unix").iterdir())
        processes = subprocess.check_output(["ps", "-eo", "comm="], text=True).splitlines()
        assert not any(name.strip() == "Xvfb" for name in processes)
        env = workspace(id, "env")["environment"]["variables"]
        env = {entry["name"]: entry["value"] for entry in env}
        assert env["XDG_SESSION_TYPE"] == "wayland"
        assert env["DBUS_SESSION_BUS_ADDRESS"].endswith(f"/{id}/session-bus")
        assert env["WAYLAND_DISPLAY"] == "workspace-wayland"
        assert env["QT_QPA_PLATFORM"] == "wayland"
        # A real Qt Wayland client, controlled through the same public IPC as MCP.
        launch = workspace(id, "launch", "--wait-window", "--window-timeout-ms", 10000, "--", "kwrite", note)
        windows = workspace(id, "windows")["windows"]
        assert windows, launch
        window = next(w for w in windows if note.name in w["title"])
        wid = window["id"]
        assert not wid.isdecimal(), window
        workspace(id, "focus-window", wid)
        workspace(id, "key", "ctrl+a")
        text = f"Native Wayland at {scale}: שלום café 日本語\nSecond line"
        workspace(id, "type", text)
        workspace(id, "key", "ctrl+s")
        wait_for(lambda: note.read_text().rstrip("\n") == text, "native Unicode text and save")
        workspace(id, "clipboard-set", "clipboard שלום")
        clip = workspace(id, "clipboard-get")["clipboard"]
        assert clip["content"] == "clipboard שלום", clip
        workspace(id, "key", "ctrl+End")
        workspace(id, "paste", "\nPasted via private Wayland clipboard")
        workspace(id, "key", "ctrl+s")
        wait_for(lambda: "Pasted via private Wayland clipboard" in note.read_text(), "clipboard paste")
        workspace(id, "move-window", wid, 30, 45)
        workspace(id, "resize-window", wid, 600, 450)
        workspace(id, "raise-window", wid)
        workspace(id, "minimize-window", wid)
        assert not next(w for w in workspace(id, "windows", "--all")["windows"] if w["id"] == wid)["visible"]
        workspace(id, "show-window", wid)
        workspace(id, "focus-window", wid)
        workspace(id, "move-pointer", 180, 220)
        pointer = workspace(id, "pointer")["pointer"]
        assert abs(pointer["x"] - 180) <= 1 and abs(pointer["y"] - 220) <= 1, pointer
        workspace(id, "click", 180, 220)
        workspace(id, "drag", 180, 220, 260, 220)
        workspace(id, "scroll", 180, 220, "down")
        shot = workspace(id, "screenshot")["screenshot"]
        assert check_png(shot["path"]) == (960, 720), shot
        shot = workspace(id, "screenshot-window", "--window", wid)["screenshot"]
        assert shot["target"] == "window" and shot["window_id"] == wid
        check_png(shot["path"])
        # Exactly the request the GPUI viewer uses, including its frame metadata.
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(10)
            client.connect(status["socket_path"])
            client.sendall((json.dumps({"method": "screenshot", "output_path": "viewer-frame.png", "source": "viewer_stream"}) + "\n").encode())
            data = client.makefile().readline()
            viewer = json.loads(data)
            assert viewer["ok"], viewer
            assert viewer["screenshot"]["source"] == "viewer_stream"
            check_png(viewer["screenshot"]["path"])
        command("workspace", "start", "--id", id, "--session", "x11", "--ack-hidden-workspace", expected=False)
        workspace(id, "close-window", wid)
        print(f"KWin {scale}: native Qt, Unicode input, clipboard, window control, pointer, capture and viewer frames passed", flush=True)
    finally:
        for path in runtime.glob("*.log"):
            (ARTIFACTS / f"{id}-{path.name}").write_bytes(path.read_bytes())
        if status is None:
            continue_cleanup = subprocess.run([BIN, "workspace", "cleanup", "--id", id], capture_output=True, timeout=10)
        else:
            workspace(id, "stop")
        wait_for(lambda: not (runtime / "control.sock").exists(), "workspace stop")
        if status:
            wait_for(lambda: not Path(f"/proc/{status['window_manager_pid']}").exists(), "compositor process cleanup")
            assert not (runtime / "workspace-wayland").exists(), "Wayland socket survived clean stop"
            assert not any(path.exists() for path in initial_x_sockets), "XWayland socket survived clean stop"
print("KWin native acceptance passed", flush=True)
