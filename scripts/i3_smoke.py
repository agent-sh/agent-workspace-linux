#!/usr/bin/env python3
"""Real i3 acceptance through public CLI/MCP, with a private Xvfb display."""
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import time
from kwin_mcp import Mcp

BIN = os.environ.get("AGENT_WORKSPACE_BIN", "target/debug/agent-workspace-linux")
BIN = str(Path(BIN).resolve())
ROOT = Path(tempfile.mkdtemp(prefix="ai3-", dir=Path.home()/".cache"))
ENV = dict(os.environ, XDG_RUNTIME_DIR=str(ROOT/"r"), XDG_CONFIG_HOME=str(ROOT/"c"), I3SOCK="/nonexistent-host-i3")
for name in ("r", "c"):
    (ROOT/name).mkdir(mode=0o700)
ID = "i3"
status = None
sequence = 0

def run(*args, env=ENV, expected=True):
    global sequence
    result = subprocess.run(list(map(str,args)), env=env, text=True, capture_output=True, timeout=30)
    sequence += 1
    (ROOT/f"{sequence:03d}.json").write_text(json.dumps({"args":args,"code":result.returncode,"out":result.stdout,"err":result.stderr},default=str))
    if expected:
        assert result.returncode == 0, (args,result.stderr,result.stdout)
    return result

def command(*args):
    value=json.loads(run(BIN,*args).stdout)
    assert value.get("ok",True),value
    return value

def ws(action,*args): return command("workspace",action,"--id",ID,*args)

def wait(probe,label):
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        if probe(): return
        time.sleep(.1)
    raise AssertionError(label)

def stopped(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ",1)[1].split()[0] in ("Z","X")
    except FileNotFoundError:
        return True

def png(path):
    data=Path(path).read_bytes()
    assert data[:8]==b"\x89PNG\r\n\x1a\n"
    return struct.unpack(">II",data[16:24])

mcp = None
try:
    report=command("doctor")
    assert report.get("ready_for_i3_workspace"),report
    denied=run(BIN,"workspace","start","--id",ID,"--session","i3","--scale","1.5","--ack-hidden-workspace",expected=False)
    assert denied.returncode != 0 and "scale 1" in denied.stderr,denied
    status=ws("start","--session","i3","--width",1000,"--height",760,"--ack-hidden-workspace")["status"]
    assert status["session"]["backend"]=="i3"
    assert Path(f"/proc/{status['window_manager_pid']}/comm").read_text().strip()=="i3"
    runtime=Path(status["runtime_dir"])
    native=dict(ENV,DISPLAY=status["display"],XAUTHORITY=status["xauthority_path"])
    config=run("i3-msg","-s",runtime/"i3.sock","-t","get_config",env=native).stdout
    for forbidden in ("bindsym", "exec ", "bar {"):
        assert forbidden not in config,config
    wrong=run("xdpyinfo",env=dict(native,XAUTHORITY=str(ROOT/"missing-auth")),expected=False)
    assert wrong.returncode != 0,"display allowed a client without authority"
    marker=ROOT/"typed"
    fixture=Path(__file__).parent/"fixtures/focus-popup.tcl"
    launched=ws("launch","--wait-window","--window-timeout-ms",10000,"--","wish",fixture,marker,ROOT/"child","managed")
    window=next(w for w in ws("windows")["windows"] if w["title"]=="Managed focus smoke")
    wid=window["id"]
    ws("resize-window",wid,600,400)
    moved=ws("move-window",wid,30,45)["windows"][0]
    assert (moved["geometry"]["x"],moved["geometry"]["y"])==(30,45),moved
    wait(lambda: next(w for w in ws("windows")["windows"] if w["id"]==wid)["geometry"]["width"]==600,"resize applied")
    ws("focus-window",wid)
    ws("type","I3_NATIVE_OK")
    ws("key","Return")
    wait(lambda: marker.exists() and marker.read_text().strip()=="I3_NATIVE_OK","keyboard delivery")
    ws("move-pointer",200,250)
    pointer=ws("pointer")["pointer"]
    assert (pointer["x"],pointer["y"])==(200,250),pointer
    ws("click",200,250)
    ws("drag",200,250,240,260)
    ws("scroll","--amount",2,200,250,"down")
    ws("clipboard-set","i3 clipboard שלום")
    assert ws("clipboard-get")["clipboard"]["content"]=="i3 clipboard שלום"
    ws("raise-window",wid)
    shot=ws("screenshot","--output",ROOT/"root.png")["screenshot"]
    assert png(shot["path"])==(1000,760),shot
    ws("launch","--wait-window","--window-timeout-ms",10000,"--","xmessage","-title","Second i3 window","another window")
    active=ws("active-window")["active_window"]["id"]
    ws("screenshot-window","--window",wid,"--output",ROOT/"window.png")
    assert png(ROOT/"window.png")== (600,400)
    assert ws("active-window")["active_window"]["id"]==active,"capture changed focus"
    ws("minimize-window",wid)
    assert not next(w for w in ws("windows","--all")["windows"] if w["id"]==wid)["visible"]
    ws("show-window",wid)
    wait(lambda: any(w["id"]==wid for w in ws("windows")["windows"]),"show restored window")
    ws("focus-window",wid)
    assert ws("active-window")["active_window"]["id"]==wid
    # Launched apps run on the workspace session bus.
    bus=ws("run","--timeout-ms",10000,"--","sh","-c",'test -S "$AGENT_WORKSPACE_RUNTIME_DIR/session-bus" && printf %s "$DBUS_SESSION_BUS_ADDRESS"')
    assert bus["succeeded"] and bus["stdout"]["content"].endswith(f"/{ID}/session-bus"),bus
    mcp=Mcp(BIN,ROOT/"mcp.log",env=ENV,headless=True)
    terminal=mcp.tool("workspace_run_in_terminal",{"id":ID,"terminal_id":"acceptance","title":"i3 terminal","command":["sh","-c","printf I3_TERMINAL; exec tail -f /dev/null"]})
    assert terminal["ok"],terminal
    terminal_socket=Path(terminal["terminal"]["tmux_socket_path"])
    pane=run("tmux","-S",terminal_socket,"display-message","-p","#{pid}:#{pane_pid}").stdout.strip()
    terminal_pids=list(map(int,pane.split(":")))
    assert "I3_TERMINAL" in run("tmux","-S",terminal_socket,"capture-pane","-p").stdout
    ws("stop")
    wait(lambda: all(stopped(pid) for pid in [status["window_manager_pid"],status["x_server_pid"],*terminal_pids]),"owned session processes stopped")
    assert not (runtime/"i3.sock").exists(), "i3 IPC socket survived stop"
    status=None
    print(f"i3 acceptance passed; evidence {ROOT}",flush=True)
except Exception:
    if status:
        result=run("i3-msg","-s",str(Path(status["runtime_dir"])/"i3.sock"),"-t","get_tree",expected=False)
        (ROOT/"failure-tree.json").write_text(result.stdout)

    raise
finally:
    if mcp: mcp.close()
    if status: ws("stop")
    print(f"i3 evidence: {ROOT}",flush=True)
