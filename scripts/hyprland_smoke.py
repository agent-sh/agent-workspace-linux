#!/usr/bin/env python3
"""Real Hyprland 6 acceptance tests. Run inside kwin_probe's isolated container."""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import time
import shutil
from kwin_mcp import Mcp
from PyQt6.QtGui import QImage

BIN = os.environ.get("HYPRLAND_PROBE_BIN", "/opt/agent-workspace-linux")
ARTIFACTS = Path(os.environ.get("HYPRLAND_PROBE_ARTIFACT_DIR", "/artifacts"))
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
assert report["ready_for_hyprland_workspace"], report
assert not report["ready_for_x11_workspace"], "Hyprland must not need an X11 window manager"
for scale in (1, 1.5):
    id = f"hyprland-{int(scale * 100)}"
    note = Path(f"/tmp/hyprland-note-{scale}.txt")
    note.write_text("")
    mcp = None
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "agent-workspace-linux" / id
    # Standard service activation would execute this outside app namespaces.
    standard_services = runtime / "session-data/dbus-1/services"
    standard_services.mkdir(parents=True, exist_ok=True)
    marker = Path(f"/tmp/forbidden-service-{id}")
    (standard_services / "org.agent_sh.Forbidden.service").write_text(
        f"[D-BUS Service]\nName=org.agent_sh.Forbidden\nExec=/usr/bin/touch {marker}\n")
    # A stopped session may leave service files behind. Startup must discard them.
    stale_services = runtime / "session-services"
    stale_services.mkdir(parents=True, exist_ok=True)
    shutil.copy(standard_services / "org.agent_sh.Forbidden.service", stale_services)
    status = None
    terminal_pids = []
    owned_pids = set()
    try:
        if scale == 1.5:
            mcp = Mcp(BIN, ARTIFACTS / f"{id}-mcp.log", headless=True)
            start = mcp.tool("workspace_start", {"id": id, "session": {"backend":"hyprland", "scale":scale},
                "width":960, "height":720, "acknowledge_hidden_workspace":True,
                "purpose":"Native MCP acceptance", "open_viewer":False})
            assert start["ok"], start
        else:
            start = workspace(id, "start", "--session", "hyprland", "--scale", scale, "--width", 960, "--height", 720,
                              "--ack-hidden-workspace", "--purpose", "Native Hyprland acceptance test")
        status = workspace(id, "status")
        assert status["session"]["backend"] == "hyprland"
        assert status["session"].get("scale", 1) == scale
        output=json.loads((runtime/"output-config.json").read_text())
        assert output[0]["scale"] == scale, output
        assert status["x_server_pid"] == 0
        assert status["window_manager_pid"]
        initial_x_sockets = set(Path("/tmp/.X11-unix").iterdir())
        processes = subprocess.check_output(["ps", "-eo", "comm="], text=True).splitlines()
        assert not any(name.strip() == "Xvfb" for name in processes)
        env = workspace(id, "env")["environment"]["variables"]
        env = {entry["name"]: entry["value"] for entry in env}
        assert env["XDG_SESSION_TYPE"] == "wayland"
        assert env["DBUS_SESSION_BUS_ADDRESS"].endswith(f"/{id}/client-bus")
        assert env["WAYLAND_DISPLAY"] == "workspace-wayland"
        assert env["QT_QPA_PLATFORM"] == "wayland"
        bus_env = dict(os.environ, **env)
        empty_auth = Path(f"/tmp/{id}-empty-auth")
        empty_auth.write_bytes(b"")
        denied_x = subprocess.run(["xdpyinfo"],env=dict(bus_env,XAUTHORITY=str(empty_auth)),capture_output=True,text=True,timeout=5)
        assert denied_x.returncode != 0, "private XWayland accepted an unauthenticated client"
        a11y_address = env["AT_SPI_BUS_ADDRESS"]
        a11y_update = subprocess.run(["dbus-send", f"--bus={a11y_address}", "--print-reply", "--reply-timeout=1000",
            "--dest=org.freedesktop.DBus", "/org/freedesktop/DBus",
            "org.freedesktop.DBus.UpdateActivationEnvironment", "dict:string:string:WORKSPACE_TEST,blocked"],
            env=bus_env, capture_output=True, text=True, timeout=3)
        assert a11y_update.returncode != 0 and "AccessDenied" in a11y_update.stderr, a11y_update
        for address in (env["DBUS_SESSION_BUS_ADDRESS"], a11y_address):
            reload_config = subprocess.run(["dbus-send", f"--bus={address}", "--print-reply", "--reply-timeout=1000",
                "--dest=org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus.ReloadConfig"],
                env=bus_env, capture_output=True, text=True, timeout=3)
            assert reload_config.returncode != 0 and "AccessDenied" in reload_config.stderr, reload_config
        a11y_forbidden = subprocess.run(["dbus-send", f"--bus={a11y_address}", "--print-reply", "--reply-timeout=1000",
            "--dest=org.agent_sh.Forbidden", "/org/agent_sh/Forbidden", "org.agent_sh.Forbidden.Ping"],
            env=bus_env, capture_output=True, text=True, timeout=3)
        assert a11y_forbidden.returncode != 0 and "ServiceUnknown" in a11y_forbidden.stderr, a11y_forbidden
        forbidden = subprocess.run(["dbus-send", "--session", "--print-reply", "--reply-timeout=1000",
            "--dest=org.agent_sh.Forbidden", "/org/agent_sh/Forbidden", "org.agent_sh.Forbidden.Ping"],
            env=bus_env, capture_output=True, text=True, timeout=3)
        assert not marker.exists(), "standard desktop service escaped launch control"
        assert forbidden.returncode != 0 and "ServiceUnknown" in forbidden.stderr, forbidden
        update = subprocess.run(["dbus-send", "--session", "--print-reply", "--reply-timeout=1000",
            "--dest=org.freedesktop.DBus", "/org/freedesktop/DBus",
            "org.freedesktop.DBus.UpdateActivationEnvironment", "dict:string:string:DISPLAY,:999"],
            env=bus_env, capture_output=True, text=True, timeout=3)
        assert update.returncode != 0 and "AccessDenied" in update.stderr, update
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
        def pasted():
            # Qt receives clipboard data asynchronously after the paste chord.
            workspace(id,"key","ctrl+s")
            return "Pasted via private Wayland clipboard" in note.read_text()
        wait_for(pasted, "clipboard paste")
        moved = workspace(id, "move-window", wid, 30, 45)
        assert (moved["windows"][0]["geometry"]["x"], moved["windows"][0]["geometry"]["y"]) == (30,45), moved
        workspace(id, "resize-window", wid, 600, 450)
        def resized():
            current = next(w for w in workspace(id,"windows")["windows"] if w["id"]==wid)
            return current["geometry"]["width"]==600 and current["geometry"]["height"]==450
        wait_for(resized, "native asynchronous resize")
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
        shutil.copy2(shot["path"],ARTIFACTS/f"{id}-desktop.png")
        shot = workspace(id, "screenshot-window", "--window", wid)["screenshot"]
        assert shot["target"] == "window" and shot["window_id"] == wid
        assert check_png(shot["path"]) == (600,450), shot
        shutil.copy2(shot["path"],ARTIFACTS/f"{id}-window.png")
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
        # Native Qt exposes its actual scale and records delivered pointer events.
        record = Path(f"/tmp/{id}-pointer.json")
        fixture = workspace(id,"launch","--wait-window","--window-timeout-ms",10000,"--","python3","/opt/hyprland-probe/kwin_qt_fixture.py",record)
        fixture_window = next(w for w in workspace(id,"windows")["windows"] if w["title"] == "Workspace Qt scale fixture")
        fid = fixture_window["id"]
        workspace(id,"resize-window",fid,int(480*scale),int(360*scale))
        workspace(id,"move-window",fid,0,0)
        workspace(id,"focus-window",fid)
        wait_for(lambda: record.exists() and json.loads(record.read_text()).get("device_pixel_ratio") == scale,"actual Qt scale")
        rendered = workspace(id,"screenshot")["screenshot"]
        shutil.copy2(rendered["path"],ARTIFACTS/f"{id}-fixture.png")
        pixels = QImage(rendered["path"])
        target = [(x,y) for y in range(pixels.height()) for x in range(pixels.width())
                  if (pixels.pixel(x,y) & 0x00ffffff) == 0x00ff00ff]
        assert target, "native magenta button absent from compositor capture"
        left, right = min(p[0] for p in target), max(p[0] for p in target)
        top, bottom = min(p[1] for p in target), max(p[1] for p in target)
        assert abs((right-left+1)-60*scale)<=2, (left,right,scale)
        assert abs((bottom-top+1)-40*scale)<=2, (top,bottom,scale)
        workspace(id,"click",round((left+right)/2),round((top+bottom)/2))
        wait_for(lambda:json.loads(record.read_text())["count"] == 1,"delivered native button click")
        recorded=json.loads(record.read_text())
        recorded["rendered_target_bounds"]=[left,top,right-left+1,bottom-top+1]
        assert abs(recorded["pointer"]["x"]-30)<=1 and abs(recorded["pointer"]["y"]-20)<=1,recorded
        (ARTIFACTS/f"{id}-pointer.json").write_text(json.dumps(recorded))
        # Capture the editor while the fixture is focused and occludes it.
        before=workspace(id,"active-window")["active_window"]["id"]
        obscured = workspace(id,"screenshot-window","--window",wid)["screenshot"]
        desktop = workspace(id,"screenshot")["screenshot"]
        native_image = QImage(obscured["path"])
        root_crop = QImage(desktop["path"]).copy(30,45,600,450)
        assert native_image != root_crop, "window export returned the occluding desktop"
        shutil.copy2(obscured["path"],ARTIFACTS/f"{id}-occluded-window.png")
        after=workspace(id,"active-window")["active_window"]["id"]
        assert before == after == fid, (before,after)
        workspace(id,"close-window",fid)
        workspace(id, "close-window", wid)
        if mcp:
            terminal = mcp.tool("workspace_run_in_terminal", {"id":id, "terminal_id":"cleanup",
                "command":["sleep","600"], "window_timeout_ms":15000, "timeout_ms":10000})
            assert terminal["ok"], terminal
            terminal = terminal["terminal"]
            terminal_pids = [int(pid) for pid in subprocess.check_output(
                ["tmux", "-S", terminal["tmux_socket_path"], "display-message", "-p", "#{pid} #{pane_pid}"],
                env=bus_env, text=True, timeout=5).split()]
            killed = mcp.tool("workspace_kill_app", {"id":id, "app_id":terminal["app_id"]})
            assert killed["ok"], killed
        process_rows = [line.split(None,2) for line in subprocess.check_output(["ps","-eo","pid=,ppid=,comm="],text=True).splitlines()]
        owned_pids = {status["daemon_pid"]}
        for _ in range(len(process_rows)):
            added = {int(pid) for pid,ppid,_ in process_rows if int(ppid) in owned_pids}
            if added <= owned_pids: break
            owned_pids |= added
        (ARTIFACTS/f"{id}-owned-processes.json").write_text(json.dumps([row for row in process_rows if int(row[0]) in owned_pids]))
        print(f"Hyprland {scale}: native Qt, Unicode input, clipboard, window control, pointer, capture and viewer frames passed", flush=True)
    finally:
        fixture_record = Path(f"/tmp/{id}-pointer.json")
        if fixture_record.exists(): (ARTIFACTS/f"{id}-pointer-final.json").write_bytes(fixture_record.read_bytes())
        if mcp:
            mcp.close()
        for path in runtime.glob("hypr/*/*.log"):
            (ARTIFACTS / f"{id}-internal-{path.name}").write_bytes(path.read_bytes())
        for path in runtime.glob("*.log"):
            (ARTIFACTS / f"{id}-{path.name}").write_bytes(path.read_bytes())
        if status is None:
            continue_cleanup = subprocess.run([BIN, "workspace", "cleanup", "--id", id], capture_output=True, timeout=10)
        else:
            workspace(id, "stop")
        wait_for(lambda: not (runtime / "control.sock").exists(), "workspace stop")
        def terminal_stopped(pid):
            try:
                return Path(f"/proc/{pid}/stat").read_text().rsplit(") ",1)[1].split()[0] in ("Z","X")
            except FileNotFoundError:
                return True
        wait_for(lambda: all(terminal_stopped(pid) for pid in terminal_pids), "managed terminal shutdown")
        wait_for(lambda: all(terminal_stopped(pid) for pid in owned_pids), "all private compositor, renderer, bus and clients stopped")
        for path in runtime.glob("hypr/*/*.log"):
            (ARTIFACTS / f"{id}-internal-{path.name}").write_bytes(path.read_bytes())
        for path in runtime.glob("*.log"):
            (ARTIFACTS / f"{id}-{path.name}").write_bytes(path.read_bytes())
        hyprland_log = runtime / "hyprland.log"
        if hyprland_log.exists():
            assert "corrupted double-linked list" not in hyprland_log.read_text(), "Hyprland teardown re-entered cleanup"
        if status:
            wait_for(lambda: not Path(f"/proc/{status['window_manager_pid']}").exists(), "compositor process cleanup")
            assert not (runtime / "workspace-wayland").exists(), "Wayland socket survived clean stop"
            assert not any(path.exists() for path in initial_x_sockets), "XWayland socket survived clean stop"
# Enforced mounts + network namespace must retain the owning XWayland socket.
project = Path("/tmp/legacy-project")
project.mkdir()
profile = Path("/tmp/legacy-profile.json")
profile.write_text(json.dumps({"id":"hyprland-restricted", "session":{"backend":"hyprland"},
    "cwd":"/workspace/project", "require_enforced_policy":True,
    "network":{"mode":"disabled"}, "mounts":[{"host_path":str(project),
    "workspace_path":"/workspace/project", "mode":"read_write"}]}))
command("profile","import","--json",profile)
id="hyprland-restricted"
try:
    workspace(id,"start","--profile","hyprland-restricted","--ack-hidden-workspace", "--purpose","Legacy XWayland sandbox acceptance")
    launch=workspace(id,"launch","--wait-window","--window-timeout-ms",10000,"--","xterm","-fa","monospace","-e","sh","-c",'test ! -e "$XDG_RUNTIME_DIR/hyprland-ipc" && test ! -e "$XDG_RUNTIME_DIR/hyprland-parent" && printf ready > /workspace/project/ready; exec tail -f /dev/null')
    assert launch["windows"], launch
    wait_for(lambda:(project/"ready").exists(),"XWayland terminal in enforced sandbox")
    app=launch["apps"][0]
    assert app["mount_isolation"]=="bubblewrap_mount_namespace",app
    assert app["network_isolation"]=="bubblewrap_unshare_net",app
    private_runtime = workspace(id,"status")["runtime_dir"]
    fd_probe = "import os,sys; root=sys.argv[1]; targets=[os.readlink('/proc/self/fd/'+fd) for fd in os.listdir('/proc/self/fd') if os.path.exists('/proc/self/fd/'+fd) and os.path.isdir('/proc/self/fd/'+fd)]; assert not any(t.startswith(root) and not t.startswith(root+'/client') for t in targets), targets"
    result = workspace(id,"run","--timeout-ms",10000,"--","python3","-c",fd_probe,private_runtime)
    assert result["exit_code"] == 0, result
    print("Hyprland restricted XWayland terminal passed",flush=True)
finally:
    workspace(id,"stop")
(Path.home()/".cache").mkdir(exist_ok=True)
subprocess.run(["python3","/opt/hyprland-probe/runtime_isolation_smoke.py"],
               env=dict(os.environ,AGENT_WORKSPACE_BIN=BIN,AWL_TEST_SESSION="hyprland"),check=True,timeout=180)
print("Hyprland native acceptance passed", flush=True)
