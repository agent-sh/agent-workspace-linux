#!/usr/bin/env python3
"""Sandboxed apps keep display/terminal access without daemon runtime or host proc."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import socket
import struct
import time
from kwin_mcp import Mcp

BIN=str(Path(os.environ.get("AGENT_WORKSPACE_BIN","target/debug/agent-workspace-linux")).resolve())
ROOT=Path(tempfile.mkdtemp(prefix="ari-",dir=Path.home()/".cache"))
ENV=dict(os.environ,XDG_RUNTIME_DIR=str(ROOT/"r"),XDG_CONFIG_HOME=str(ROOT/"c"))
(ROOT/"r").mkdir(mode=0o700)
(ROOT/"project").mkdir()
sequence=0

def cmd(*args):
    global sequence
    r=subprocess.run([BIN,*map(str,args)],env=ENV,capture_output=True,text=True,timeout=30)
    sequence+=1
    (ROOT/f"{sequence:03d}.json").write_text(json.dumps({"args":args,"code":r.returncode,"stdout":r.stdout,"stderr":r.stderr},default=str))
    assert r.returncode==0,(args,r.stdout,r.stderr)
    v=json.loads(r.stdout)
    assert v.get("ok",True),v
    return v

mcp=Mcp(BIN,ROOT/"mcp.log",env=ENV,headless=True)
try:
    for mounted in (False,True):
        id="mounts" if mounted else "network"
        p={"id":id,"session":{"backend":os.environ.get("AWL_TEST_SESSION","x11")},"network":{"mode":"disabled"},"require_enforced_policy":True}
        if mounted:
            p.update(cwd="/workspace/project",mounts=[{"host_path":str(ROOT/"project"),"workspace_path":"/workspace/project","mode":"read_write"}])
        profile=ROOT/f"{id}.profile.json";profile.write_text(json.dumps(p))
        cmd("profile","import","--json",profile)
        status=cmd("workspace","start","--id",id,"--profile",id,"--ack-hidden-workspace")["status"]
        sibling=cmd("workspace","start","--id",f"{id}-sibling","--ack-hidden-workspace")["status"]
        try:
            probe="""import os,sys,json,socket
from pathlib import Path
runtime=Path(sys.argv[1]); hostpid=sys.argv[2]; own_x_socket='X'+sys.argv[3].split(':',1)[1].split('.')[0]
paths=[runtime/'control.sock',runtime/'session-bus',runtime/'i3.sock',Path('/proc')/hostpid/'root'/str(runtime).lstrip('/')/'control.sock',*map(Path,sys.argv[4:])]
ibus=Path.home()/'.cache'/'ibus'
paths+=[p for p in ibus.glob('dbus-*')] if ibus.is_dir() else []
reachable=[]
for path in paths:
 s=socket.socket(socket.AF_UNIX);s.settimeout(1)
 try: s.connect(str(path));reachable.append(str(path))
 except OSError: pass
 finally: s.close()
foreign_x=[p.name for p in Path('/tmp/.X11-unix').iterdir() if p.name!=own_x_socket]
host_bus=any(p.exists() for p in [Path('/run/dbus/system_bus_socket'),Path('/run/user')/str(os.getuid())/'bus'])
print(json.dumps({'reachable':reachable,'pid':os.getpid(),'host_proc':Path('/proc',hostpid).exists(),'foreign_x':foreign_x,'host_bus':host_bus}))
raise SystemExit(42 if reachable or Path('/proc',hostpid).exists() or foreign_x or host_bus else 0)
"""
            terminal=mcp.tool("workspace_run_in_terminal",{"id":id,"terminal_id":"private","command":["sh","-c","printf PRIVATE_TERMINAL_OK; exec tail -f /dev/null"],"wait_window":True,"timeout_ms":10000})
            assert terminal["ok"],terminal
            # Another sandboxed app must not reach the terminal's tmux server, a
            # sibling workspace's control socket, or the host input-method bus.
            run=cmd("workspace","run","--id",id,"--timeout-ms",10000,"--","python3","-c",probe,status["runtime_dir"],status["daemon_pid"],status["display"],
                    terminal["terminal"]["tmux_socket_path"],Path(sibling["runtime_dir"])/"control.sock")
            assert run["exit_code"]==0,run
            screen=mcp.tool("workspace_terminal_read",{"id":id,"terminal_id":"private"})
            assert "PRIVATE_TERMINAL_OK" in screen["screen"]["text"],screen
            windows=cmd("workspace","windows","--id",id)["windows"]
            appid=terminal["terminal"]["app_id"]
            assert any(w.get("app_id")==appid for w in windows),windows
            wid=next(w["id"] for w in windows if w.get("app_id")==appid)
            peer=socket.socket(socket.AF_UNIX);peer.connect(terminal["terminal"]["tmux_socket_path"])
            server_pid=struct.unpack("3i",peer.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))[0];peer.close()
            children=Path(f"/proc/{server_pid}/task/{server_pid}/children").read_text().split()
            terminal_pids=[server_pid,*map(int,children)]
            cmd("workspace","close-window","--id",id,wid)
            screen=mcp.tool("workspace_terminal_read",{"id":id,"terminal_id":"private"})
            assert "PRIVATE_TERMINAL_OK" in screen["screen"]["text"],"detached terminal lost its private PID namespace"

        finally:
            cmd("workspace","stop","--id",id)
            cmd("workspace","stop","--id",f"{id}-sibling")
        deadline=time.monotonic()+10
        while time.monotonic()<deadline:
            live=[]
            for pid in terminal_pids:
                try:
                    state=Path(f"/proc/{pid}/stat").read_text().split(") ",1)[1].split()[0]
                    if state not in ("Z","X"): live.append(pid)
                except FileNotFoundError: pass
            if not live: break
            time.sleep(.1)
        assert not live,live
    print(f"runtime isolation passed; evidence {ROOT}")
finally:
    mcp.close()
    print(f"runtime isolation evidence: {ROOT}")
