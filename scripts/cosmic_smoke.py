#!/usr/bin/env python3
"""Real COSMIC native acceptance tests in the isolated software-only container."""
import json
import os
import re
from pathlib import Path
import socket
import struct
import subprocess
import time
import shutil
from kwin_mcp import Mcp

BIN = os.environ.get("COSMIC_PROBE_BIN", "/opt/agent-workspace-linux")
ARTIFACTS = Path(os.environ.get("COSMIC_PROBE_ARTIFACT_DIR", "/artifacts"))
ARTIFACTS.mkdir(parents=True, exist_ok=True)
ARTIFACTS.joinpath("scratch").mkdir(exist_ok=True)
os.environ.update(XDG_RUNTIME_DIR="/artifacts/scratch/awl-runtime", XDG_CONFIG_HOME="/artifacts/scratch/awl-config", HOME="/artifacts/scratch/awl-home", LANG="C.UTF-8")
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
        # Daemon refusals exit 0 with ok=false; CLI refusals exit nonzero.
        refused = result.returncode != 0 or json.loads(result.stdout).get("ok") is False
        assert refused, (args, result.stdout)
        return result.stdout + result.stderr
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


# Known upstream COSMIC 8cb1802 winit capture signature. An output capture
# blits inside render_output, which leaves the EGL context surfaceless before
# eglSwapBuffers. Smithay's recreate attempt then fails because Mesa refuses a
# second surface on the same wl_egl_window, and COSMIC immediately re-renders.
# Toplevel capture renders offscreen between frames, so only the next frame's
# buffer-age query fails. Any other ERROR line, or more of these than captures
# can explain, fails acceptance. See docs/native-cosmic.md.
# Smithay swaps with eglSwapBuffersWithDamageKHR when the frame has damage and
# with eglSwapBuffers otherwise. Mesa applies the same current-surface check to
# both, so each failed swap logs exactly one of the two.
SWAP_FAILURES = (
    "smithay::backend::egl::ffi: [EGL] 0x300d (BAD_SURFACE) eglSwapBuffers: eglSwapBuffers",
    "smithay::backend::egl::ffi: [EGL] 0x300d (BAD_SURFACE) eglSwapBuffersWithDamageKHR: _eglSwapBuffersWithDamageCommon",
)
CAPTURE_SWAP = (
    "smithay::backend::egl::ffi: [EGL] 0x3003 (BAD_ALLOC) eglCreatePlatformWindowSurfaceEXT: _eglCreateWindowSurfaceCommon",
    "smithay::backend::egl::surface: error=EGL failed to allocate resources for the requested operation.",
    "cosmic_comp::backend::winit: Failed to render frame. err=Failed to submit buffer for display",
)
BUFFER_AGE = "smithay::backend::egl::ffi: [EGL] 0x300d (BAD_SURFACE) eglQuerySurface: eglQuerySurface"
LOCALE = "i18n_embed::requester: Unable to parse your locale: ParserError(InvalidLanguage)"


def check_compositor_log(path, output_captures, window_captures):
    import re
    counts = {}
    for line in Path(path).read_text(errors="replace").splitlines():
        line = re.sub(r"\x1b\[[0-9;]*m", "", line)
        match = re.match(r"\S+Z\s+ERROR\s+(.*)", line)
        if match:
            message = match.group(1).strip()
            counts[message] = counts.get(message, 0) + 1
    unexpected = {m: n for m, n in counts.items() if m not in (*SWAP_FAILURES, *CAPTURE_SWAP, BUFFER_AGE, LOCALE)}
    assert not unexpected, f"unexpected COSMIC errors: {unexpected}"
    swaps = {counts.get(m, 0) for m in CAPTURE_SWAP} | {sum(counts.get(m, 0) for m in SWAP_FAILURES)}
    assert len(swaps) == 1, f"incomplete COSMIC capture swap signature: {counts}"
    swaps = swaps.pop()
    assert swaps <= output_captures, f"{swaps} failed swaps for {output_captures} output captures"
    queries = counts.get(BUFFER_AGE, 0)
    assert queries <= 1 + swaps + window_captures, f"{queries} buffer-age errors: {counts}"
    assert counts.get(LOCALE, 0) <= 1, counts
    return {"failed_swaps": swaps, "buffer_age_errors": queries,
            "output_captures": output_captures, "window_captures": window_captures}


# Launched apps write what their own Wayland connection advertises.
WAYLAND_PROBE = r'''import json,os,re,subprocess,sys
info=subprocess.run(["wayland-info"],capture_output=True,text=True,timeout=10)
open(sys.argv[1]+".staging","w").write(json.dumps({"returncode":info.returncode,"stderr":info.stderr[-500:],
    "wayland_display":os.environ.get("WAYLAND_DISPLAY"),"dbus":os.environ.get("DBUS_SESSION_BUS_ADDRESS"),
    "globals":sorted(set(re.findall(r"interface: '([^']+)'",info.stdout)))}))
os.replace(sys.argv[1]+".staging",sys.argv[1])
'''


def capture_image(id):
    from PyQt6.QtGui import QImage
    shot = workspace(id, "screenshot")["screenshot"]
    return shot, QImage(shot["path"])


def color_bounds(image, predicate):
    points = [(x, y) for y in range(image.height()) for x in range(image.width()) if predicate(image.pixelColor(x, y))]
    assert points, "colored fixture region absent from capture"
    return (min(p[0] for p in points), min(p[1] for p in points),
            max(p[0] for p in points), max(p[1] for p in points), points)


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
assert report["ready_for_cosmic_workspace"], report
assert not report["ready_for_x11_workspace"], "COSMIC must not need an X11 window manager"
for scale in (1, 1.5):
    id = f"cosmic-{int(scale * 100)}"
    note = Path(f"/artifacts/scratch/cosmic-note-{scale}.txt")
    note.write_text("")
    mcp = None
    runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "agent-workspace-linux" / id
    # The workspace bus must not activate services from the standard directories.
    standard_services = runtime / "session-data/dbus-1/services"
    standard_services.mkdir(parents=True, exist_ok=True)
    marker = Path(f"/artifacts/scratch/forbidden-service-{id}")
    (standard_services / "org.agent_sh.Forbidden.service").write_text(
        f"[D-BUS Service]\nName=org.agent_sh.Forbidden\nExec=/usr/bin/touch {marker}\n")
    # A stopped session may leave service files behind. Startup must discard them.
    stale_services = runtime / "session-services"
    stale_services.mkdir(parents=True, exist_ok=True)
    shutil.copy(standard_services / "org.agent_sh.Forbidden.service", stale_services)
    status = None
    terminal_pids = []
    captures = {"output": 0, "window": 0}
    try:
        if scale == 1.5:
            mcp = Mcp(BIN, ARTIFACTS / f"{id}-mcp.log", headless=True)
            start = mcp.tool("workspace_start", {"id": id, "session": {"backend":"cosmic", "scale":scale},
                "width":960, "height":720, "acknowledge_hidden_workspace":True,
                "purpose":"Native MCP acceptance", "open_viewer":False})
            assert start["ok"], start
        else:
            start = workspace(id, "start", "--session", "cosmic", "--scale", scale, "--width", 960, "--height", 720,
                              "--ack-hidden-workspace", "--purpose", "Native COSMIC acceptance test")
        status = workspace(id, "status")
        assert status["session"]["backend"] == "cosmic"
        assert status["session"].get("scale", 1) == scale
        output=command("__cosmic-control", str(runtime/json.loads((runtime/"cosmic-client.json").read_text())["WAYLAND_DISPLAY"]), "snapshot", "{}")
        assert output["scale"] == scale, output
        assert status["x_server_pid"] == 0
        assert status["window_manager_pid"]
        initial_x_sockets = set(Path("/tmp/.X11-unix").iterdir())
        processes = subprocess.check_output(["ps", "-eo", "comm="], text=True).splitlines()
        assert not any(name.strip() == "Xvfb" for name in processes)
        env = workspace(id, "env")["environment"]["variables"]
        env = {entry["name"]: entry["value"] for entry in env}
        assert env["XDG_SESSION_TYPE"] == "wayland"
        # Attached controllers (computer-use-linux) and launched apps share the
        # workspace session bus.
        assert env["DBUS_SESSION_BUS_ADDRESS"].endswith(f"/{id}/session-bus")
        assert env["WAYLAND_DISPLAY"].startswith("wayland-")
        assert env["QT_QPA_PLATFORM"] == "wayland"
        bus_env = dict(os.environ, **env)
        client_metadata=json.loads((runtime/"cosmic-client.json").read_text())
        assert client_metadata["launcher_restricted"] is True
        process_args=subprocess.check_output(["ps","-eo","args="],text=True)
        xwayland_args=[line for line in process_args.splitlines() if line.startswith("/usr/bin/Xwayland ")]
        assert xwayland_args and all("-auth "+str(status["xauthority_path"]) in line for line in xwayland_args),xwayland_args
        authorized=subprocess.run(["xdpyinfo"],env=bus_env,capture_output=True,timeout=5)
        assert authorized.returncode==0,authorized.stderr
        unauthorized=subprocess.run(["xdpyinfo"],env=dict(bus_env,XAUTHORITY=str(runtime/"absent-authority")),capture_output=True,timeout=5)
        assert unauthorized.returncode!=0,"XWayland accepted a client without the workspace cookie"
        # Attached controllers and launched apps use the compositor socket and
        # session bus; desktop services such as portals need capture.
        attached = subprocess.run(["wayland-info"], env=bus_env, capture_output=True, text=True, timeout=10)
        attached_globals = set(re.findall(r"interface: '([^']+)'", attached.stdout))
        assert {"zcosmic_toplevel_manager_v1", "ext_image_copy_capture_manager_v1"} <= attached_globals, attached
        app_receipt = Path(f"/artifacts/scratch/{id}-app-globals.json")
        workspace(id, "launch", "--", "python3", "-c", WAYLAND_PROBE, app_receipt)
        wait_for(app_receipt.exists, "launched app Wayland registry", timeout=15)
        app_view = json.loads(app_receipt.read_text())
        assert app_view["wayland_display"] == env["WAYLAND_DISPLAY"], app_view
        assert app_view["dbus"] == env["DBUS_SESSION_BUS_ADDRESS"], app_view
        assert "zcosmic_toplevel_manager_v1" in app_view["globals"], app_view
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
        receipt=ARTIFACTS/f"{id}-fixture.json"
        launch=workspace(id,"launch","--wait-window","--window-timeout-ms",10000,"--","python3","/opt/cosmic-probe/cosmic_fixture.py",receipt)
        window=next(w for w in workspace(id,"windows")["windows"] if w["title"]=="COSMIC native acceptance fixture")
        wid=window["id"]
        # Workspace app handles, not Wayland app IDs, attribute PID-less windows.
        assert window.get("app_id")==launch["apps"][0]["id"] and window["wm_class"]=="python3",window
        workspace(id,"focus-window",wid)
        before,image=capture_image(id); captures["output"]+=1
        before_bytes=Path(before["path"]).read_bytes()
        target=color_bounds(image,lambda c:c.red()>240 and c.blue()>240 and c.green()<20)[4]
        x=round(sum(p[0] for p in target)/len(target));y=round(sum(p[1] for p in target)/len(target))
        workspace(id,"click",x,y)
        wait_for(lambda:json.loads(receipt.read_text())["clicks"]==1,"physical screenshot pixel reached native button")
        point=workspace(id,"pointer")["pointer"]
        assert abs(point["x"]-x)<=1 and abs(point["y"]-y)<=1,point
        assert json.loads(receipt.read_text())["screen_logical"]==[round(960/scale),round(720/scale)]
        workspace(id,"key","ctrl+l")
        text="Native COSMIC: שלום café 日本語\nSecond line"
        workspace(id,"type",text)
        wait_for(lambda:json.loads(receipt.read_text())["text"]==text,"native Unicode input")
        workspace(id,"key","shift+a")
        workspace(id,"key","exclam")
        wait_for(lambda:json.loads(receipt.read_text())["text"]==text+"A!","shifted symbols from native keymap")
        # COSMIC would drop or reinterpret control characters; typing must fail instead.
        assert "U+0001" in command("workspace", "type", "--id", id, "bad\u0001", expected=False)
        time.sleep(0.5)
        assert json.loads(receipt.read_text())["text"]==text+"A!","rejected text reached the app"
        after=workspace(id,"screenshot")["screenshot"]; captures["output"]+=1
        assert before_bytes!=Path(after["path"]).read_bytes(),"native frame did not update after typing"
        workspace(id,"clipboard-set","clipboard שלום")
        assert workspace(id,"clipboard-get")["clipboard"]["content"]=="clipboard שלום"
        workspace(id,"key","ctrl+a")
        workspace(id,"paste","Pasted via private COSMIC clipboard")
        wait_for(lambda:"Pasted via private COSMIC clipboard" in json.loads(receipt.read_text())["text"],"native paste")
        moved = workspace(id, "move-window", wid, 30, 45)
        assert (moved["windows"][0]["geometry"]["x"], moved["windows"][0]["geometry"]["y"]) == (30,45), moved
        workspace(id, "resize-window", wid, 600, 450)
        def resized():
            current = next(w for w in workspace(id,"windows")["windows"] if w["id"]==wid)
            return abs(current["geometry"]["width"]-600)<=1 and abs(current["geometry"]["height"]-450)<=1
        wait_for(resized, "native asynchronous resize")
        workspace(id, "raise-window", wid)
        workspace(id, "minimize-window", wid)
        assert not next(w for w in workspace(id, "windows", "--all")["windows"] if w["id"] == wid)["visible"]
        workspace(id, "show-window", wid)
        workspace(id, "focus-window", wid)
        workspace(id, "move-pointer", 180, 220)
        pointer = workspace(id, "pointer")["pointer"]
        assert abs(pointer["x"] - 180) <= 1 and abs(pointer["y"] - 220) <= 1, pointer
        # Locate the editor in a native capture, then require real effects:
        # a pointer drag selects its text and a wheel event reaches it.
        left,top,right,bottom,_=color_bounds(capture_image(id)[1],lambda c:c.red()<20 and c.green()>240 and c.blue()>240)
        captures["output"]+=1
        line_y=top+round(10*scale)
        workspace(id, "drag", left+round(4*scale), line_y, right-round(4*scale), line_y)
        wait_for(lambda:json.loads(receipt.read_text())["selection"]!="","native pointer drag selection")
        state=json.loads(receipt.read_text())
        assert state["selection"] in state["text"],state
        workspace(id, "click", (left+right)//2, (top+bottom)//2)
        workspace(id, "scroll", (left+right)//2, (top+bottom)//2, "down")
        wait_for(lambda:json.loads(receipt.read_text())["wheel_events"]>0,"native wheel event")
        shot = workspace(id, "screenshot")["screenshot"]; captures["output"]+=1
        assert check_png(shot["path"]) == (960, 720), shot
        shutil.copy2(shot["path"],ARTIFACTS/f"{id}-desktop.png")
        shot = workspace(id, "screenshot-window", "--window", wid)["screenshot"]; captures["window"]+=1
        assert shot["target"] == "window" and shot["window_id"] == wid
        geometry=next(w for w in workspace(id,"windows")["windows"] if w["id"]==wid)["geometry"]
        assert check_png(shot["path"]) == (geometry["width"],geometry["height"]), shot
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
            captures["output"]+=1
        command("workspace", "start", "--id", id, "--session", "x11", "--ack-hidden-workspace", expected=False)
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
        print(f"COSMIC {scale}: native Qt, Unicode input, clipboard, window control, pointer, capture and viewer frames passed", flush=True)
    finally:
        if mcp:
            mcp.close()
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
        if status:
            wait_for(lambda: not Path(f"/proc/{status['window_manager_pid']}").exists(), "compositor process cleanup")
            assert not any(runtime.glob("wayland-[0-9]")), "Wayland socket survived clean stop"
            assert not any(path.exists() for path in initial_x_sockets), "XWayland socket survived clean stop"
    gate = check_compositor_log(ARTIFACTS / f"{id}-cosmic.log", captures["output"], captures["window"])
    print(f"COSMIC {scale}: compositor log gate {json.dumps(gate)}", flush=True)
    cleanup = command("workspace", "cleanup", "--id", id)
    assert not runtime.exists(), f"stopped COSMIC runtime survived cleanup: {cleanup}"
# XWayland clients type through the seat layout.
project = Path("/artifacts/scratch/xwayland-project")
project.mkdir(parents=True)
id="cosmic-xwayland"
try:
    workspace(id,"start","--session","cosmic","--ack-hidden-workspace", "--purpose","XWayland terminal acceptance")
    launch=workspace(id,"launch","--wait-window","--window-timeout-ms",10000,"--","xterm","-fa","monospace","-e","sh","-c",'printf ready > "$1/ready"; exec cat > "$1/typed"',"sh",project)
    assert launch["windows"], launch
    wait_for(lambda:(project/"ready").exists(),"XWayland terminal")
    # X11 clients have no text-input. They get the seat layout's own keys, and
    # characters outside that layout are refused before anything is typed.
    workspace(id,"focus-window",launch["windows"][0]["id"])
    typed="XWayland typed: Hello, World! ~|{}<>?_+ 123\tdone\n"
    workspace(id,"type",typed)
    wait_for(lambda:(project/"typed").exists() and (project/"typed").read_text(encoding="utf-8")==typed,"XWayland layout typing")
    refused=command("workspace","type","--id",id,"café שלום\n",expected=False)
    assert "not in the COSMIC keyboard layout" in refused,refused
    assert "U+0001" in command("workspace","type","--id",id,"bad\u0001",expected=False)
    workspace(id,"type","after refusal\n")
    wait_for(lambda:(project/"typed").read_text(encoding="utf-8")==typed+"after refusal\n","refused text typed nothing")
    print("COSMIC XWayland terminal passed",flush=True)
finally:
    workspace(id,"stop")

# A crashed daemon leaves a stale COSMIC runtime that cleanup must remove.
id="cosmic-crash"
workspace(id,"start","--session","cosmic","--ack-hidden-workspace")
status=workspace(id,"status")
runtime=Path(status["runtime_dir"])
def process_gone(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(") ",1)[1].split()[0] in ("Z","X")
    except FileNotFoundError:
        return True
os.kill(status["daemon_pid"],9)
wait_for(lambda:process_gone(status["daemon_pid"]),"killed COSMIC daemon")
wait_for(lambda:process_gone(status["window_manager_pid"]),"compositor exits with its daemon")
cleanup=command("workspace","cleanup","--id",id)
assert not runtime.exists(),f"crashed COSMIC runtime survived cleanup: {cleanup}"
print("COSMIC crashed runtime cleanup passed",flush=True)

print("COSMIC native acceptance passed", flush=True)
