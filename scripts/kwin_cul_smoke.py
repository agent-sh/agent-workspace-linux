#!/usr/bin/env python3
"""Validate Computer Use Linux against a real private Qt/KWin 150% session."""
import json
import os
from pathlib import Path
import subprocess
import time
from kwin_mcp import Mcp
from PyQt6.QtGui import QImage

BIN = "/opt/agent-workspace-linux"
CUL = "/opt/computer-use-linux"
ARTIFACTS = Path("/artifacts")
os.environ.update(HOME="/tmp/cul-home", XDG_CONFIG_HOME="/tmp/cul-config", XDG_RUNTIME_DIR="/tmp/cul-runtime", LANG="C.UTF-8")
for name in ("HOME","XDG_CONFIG_HOME","XDG_RUNTIME_DIR"):
    Path(os.environ[name]).mkdir(parents=True, mode=0o700, exist_ok=True)
Path(os.environ["HOME"],"Pictures").mkdir(exist_ok=True)
Path("/tmp/.X11-unix").mkdir(mode=0o1777,exist_ok=True)
id="cul-150"
sequence=0

def awl(*args):
    global sequence
    sequence += 1
    result=subprocess.run([BIN,*map(str,args)],capture_output=True,text=True,timeout=60)
    (ARTIFACTS/f"cul-awl-{sequence:03d}.out").write_text(result.stdout)
    (ARTIFACTS/f"cul-awl-{sequence:03d}.err").write_text(result.stderr)
    assert result.returncode==0,(args,result.stderr)
    value=json.loads(result.stdout)
    assert value.get("ok",True),value
    return value

def ws(*args):
    return awl("workspace",args[0],"--id",id,*args[1:])

def wait_for(probe,label,timeout=15):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        if probe(): return
        time.sleep(0.1)
    raise AssertionError(label)

cul=None
consent=None
started=False
try:
    ws("start","--session","kwin","--scale",1.5,"--width",960,"--height",720,"--ack-hidden-workspace","--purpose","Computer Use Linux Qt150 verification")
    started=True
    status=ws("status")
    env={item["name"]:item["value"] for item in ws("env")["environment"]["variables"]}
    child_env=dict(os.environ,**env)
    child_env.update(COMPUTER_USE_LINUX_INDICATOR="1", COMPUTER_USE_LINUX_FORCE_PORTAL_POINTER="1")
    support=subprocess.run(["qdbus6","org.kde.KWin","/KWin","org.kde.KWin.supportInformation"],env=child_env,capture_output=True,text=True,timeout=5)
    (ARTIFACTS/"cul-kwin-support.txt").write_text(support.stdout+support.stderr)
    record=Path("/tmp/qt-clicks.json")
    fixture=ws("launch","--wait-window","--window-timeout-ms",10000,"--","python3","/opt/kwin-probe/kwin_qt_fixture.py",record)
    pid=fixture["apps"][0]["pid"]
    wait_for(record.exists,"Qt fixture startup")
    wait_for(lambda:json.loads(record.read_text()).get("device_pixel_ratio")==1.5,"Qt window actual DPR150")
    print("Qt output:",record.read_text(),flush=True)
    assert json.loads(record.read_text())["device_pixel_ratio"] == 1.5, record.read_text()
    cul=Mcp(CUL,ARTIFACTS/"cul-mcp.log",env=child_env)
    tree=cul.tool("get_app_state",{"pid":pid,"include_screenshot":False,"max_nodes":100,"max_depth":10},timeout=30)
    (ARTIFACTS/"cul-state.json").write_text(json.dumps(tree,ensure_ascii=False,indent=2))
    assert not tree.get("accessibility_error"),tree.get("accessibility_error")
    target=next(node for node in tree["accessibility_tree"] if node.get("name")=="Target" and node["role"]=="push button")
    result=cul.tool("click",{"element_index":target["index"],"pid":pid},timeout=20)
    (ARTIFACTS/"cul-native-action.json").write_text(json.dumps(result,ensure_ascii=False,indent=2))
    assert result["ok"],result
    wait_for(lambda:record.exists() and json.loads(record.read_text())["count"]==1,"AT-SPI primary action")
    print("CUL real Qt150 accessibility tree and native primary action passed",flush=True)
    # Portal services are explicit managed apps in this unrestricted test
    # workspace. The private bus never auto-activates arbitrary system services.
    def property_value(destination, interface, name):
        result=subprocess.run(["dbus-send","--session","--print-reply","--reply-timeout=1000",
            "--dest="+destination,"/org/freedesktop/portal/desktop","org.freedesktop.DBus.Properties.Get",
            "string:"+interface,"string:"+name],env=child_env,capture_output=True,text=True,timeout=2)
        return result.returncode,result.stdout+result.stderr
    for service in ("/usr/bin/pipewire","/usr/bin/wireplumber","/usr/libexec/xdg-permission-store","/usr/lib/x86_64-linux-gnu/libexec/xdg-desktop-portal-kde"):
        ws("launch","--",service)
    wait_for(lambda:(Path(env["XDG_RUNTIME_DIR"])/"pipewire-0").exists(),"private PipeWire")
    def backend_ready():
        status,value=property_value("org.freedesktop.impl.portal.desktop.kde","org.freedesktop.impl.portal.ScreenCast","AvailableCursorModes")
        (ARTIFACTS/"cul-backend-cursor-modes.txt").write_text(value)
        return status==0 and "uint32 7" in value
    wait_for(backend_ready,"KDE portal ScreenCast interface registration")
    ws("launch","--","/usr/libexec/xdg-desktop-portal")
    def portal_ready():
        status,value=property_value("org.freedesktop.portal.Desktop","org.freedesktop.portal.ScreenCast","AvailableCursorModes")
        (ARTIFACTS/"cul-frontend-cursor-modes.txt").write_text(value)
        return status==0 and "uint32 7" in value
    wait_for(portal_ready,"private portal cursor capabilities")
    # A coordinate request may raise private portal consent dialogs. The test
    # driver captures them for bounded, inspectable approval in the workspace.
    consent=Mcp(CUL,ARTIFACTS/"cul-consent-mcp.log",env=child_env)
    snapshots=0
    last=0
    def inspect_portal():
        global snapshots,last
        if time.monotonic()-last<2: return
        last=time.monotonic()
        windows=ws("windows")["windows"]
        (ARTIFACTS/"cul-portal-windows.json").write_text(json.dumps(windows,indent=2))
        if any("portal" in (w.get("wm_class") or "").lower() for w in windows):
            snapshots+=1
            ws("screenshot","--output",ARTIFACTS/"cul-portal.png")
            dialog=next(w for w in windows if "portal" in (w.get("wm_class") or "").lower())
            state=consent.tool("get_app_state",{"pid":dialog["pid"],"include_screenshot":False,"max_nodes":200,"max_depth":15},timeout=10)
            (ARTIFACTS/f"cul-consent-tree-{snapshots}.json").write_text(json.dumps(state,ensure_ascii=False,indent=2))
            nodes=state.get("accessibility_tree",[])
            for node in nodes:
                if "Virtual" in (node.get("name") or "") and "checked" not in node.get("states",[]) and node.get("actions"):
                    consent.tool("click",{"element_index":node["index"],"pid":dialog["pid"]},timeout=5)
                    return
            for node in nodes:
                if node.get("name") in ("Allow","Approve","Share","Accept","Share selected screens") and "enabled" in node.get("states",[]) and "showing" in node.get("states",[]) and node.get("actions"):
                    consent.tool("click",{"element_index":node["index"],"pid":dialog["pid"]},timeout=5)
                    return

    # Find the unique target in real compositor pixels. This gives independent
    # global-coordinate ground truth, including the decoration/client offset.
    screenshot=ws("screenshot","--output",ARTIFACTS/"cul-target.png")["screenshot"]
    image=QImage(screenshot["path"])
    mask=set()
    for y in range(image.height()):
        for x in range(image.width()):
            color=image.pixelColor(x,y)
            if color.red()>245 and color.green()<10 and color.blue()>245: mask.add((x,y))
    components=[]
    while mask:
        pending=[mask.pop()]; component=[]
        while pending:
            x,y=pending.pop(); component.append((x,y))
            for neighbor in ((x-1,y),(x+1,y),(x,y-1),(x,y+1)):
                if neighbor in mask: mask.remove(neighbor); pending.append(neighbor)
        components.append(component)
    pixels=max(components,key=len)
    left=min(x for x,y in pixels);right=max(x for x,y in pixels)
    top=min(y for x,y in pixels);bottom=max(y for x,y in pixels)
    scale=status["session"]["scale"]
    assert abs((right-left+1)-60*scale)<=2 and abs((bottom-top+1)-40*scale)<=2,(left,top,right,bottom)
    point=(round((left+right)/2/scale),round((top+bottom)/2/scale))
    (ARTIFACTS/"cul-target-point.json").write_text(json.dumps({"physical_bounds":[left,top,right,bottom],"logical_point":point,"atspi_bounds":target["bounds"]}))
    tree=cul.tool("get_app_state",{"pid":pid,"include_screenshot":False,"max_nodes":100,"max_depth":10},timeout=20)
    target=next(node for node in tree["accessibility_tree"] if node.get("name")=="Target" and node["role"]=="push button")
    result=cul.tool("click",{"element_index":target["index"],"pid":pid,
        "x":point[0],"y":point[1],"coordinate_space":"atspi"},timeout=30,on_wait=inspect_portal)
    (ARTIFACTS/"cul-atspi-coordinate-click.json").write_text(json.dumps(result,ensure_ascii=False,indent=2))
    assert result["ok"],result
    wait_for(lambda:json.loads(record.read_text())["count"]==2,"AT-SPI coordinate click delivered to Target")
    click=json.loads(record.read_text())
    assert click["pointer"] and abs(click["pointer"]["x"]-30)<=2 and abs(click["pointer"]["y"]-20)<=2,click
    (ARTIFACTS/"cul-click-proof.json").write_text(json.dumps(click,indent=2))
    tree=cul.tool("get_app_state",{"pid":pid,"include_screenshot":False,"max_nodes":100,"max_depth":10},timeout=20)
    target=next(node for node in tree["accessibility_tree"] if node.get("name")=="Target" and node["role"]=="push button")
    fallback=cul.tool("click",{"element_index":target["index"],"pid":pid,"click_count":2},timeout=20)
    assert fallback["ok"],fallback
    wait_for(lambda:json.loads(record.read_text())["count"]>=3,"client-local AT-SPI element fallback")
    print("CUL explicit AT-SPI coordinate click reached real Qt Target at150%",flush=True)
finally:
    if consent: consent.close()
    if cul: cul.close()
    runtime=Path(os.environ["XDG_RUNTIME_DIR"])/"agent-workspace-linux"/id
    for path in runtime.rglob("*.log"):
        (ARTIFACTS/("cul-"+path.parent.name+"-"+path.name)).write_bytes(path.read_bytes())
    if started: ws("stop")
