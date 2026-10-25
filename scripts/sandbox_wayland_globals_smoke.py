#!/usr/bin/env python3
"""Sandboxed apps must not see or bind privileged Wayland globals.

Runs wayland_globals_probe.py inside a network-only and a mount-restricted
sandbox, and once unsandboxed as a control that shows what the compositor's
main socket offers. Usage: sandbox_wayland_globals_smoke.py BACKEND
(AGENT_WORKSPACE_BIN, SANDBOX_GLOBALS_ARTIFACTS optional).
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
BIN = str(Path(os.environ.get("AGENT_WORKSPACE_BIN", "target/debug/agent-workspace-linux")).resolve())
BACKEND = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("AWL_TEST_SESSION", "kwin")
ROOT = Path(tempfile.mkdtemp(prefix="swg-", dir=Path.home() / ".cache"))
ARTIFACTS = Path(os.environ.get("SANDBOX_GLOBALS_ARTIFACTS", ROOT))
ARTIFACTS.mkdir(parents=True, exist_ok=True)
ENV = dict(os.environ, XDG_RUNTIME_DIR=str(ROOT / "r"), XDG_CONFIG_HOME=str(ROOT / "c"))
(ROOT / "r").mkdir(mode=0o700)
(ROOT / "project").mkdir()
PROBE = (HERE / "wayland_globals_probe.py").read_text()
sys.path.insert(0, str(HERE))
from wayland_globals_probe import PRIVILEGED  # noqa: E402
sequence = 0


def cmd(*args):
    global sequence
    result = subprocess.run([BIN, *map(str, args)], env=ENV, capture_output=True, text=True, timeout=60)
    sequence += 1
    (ROOT / f"{sequence:03d}.json").write_text(json.dumps(
        {"args": args, "code": result.returncode, "stdout": result.stdout, "stderr": result.stderr}, default=str))
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    value = json.loads(result.stdout)
    assert value.get("ok", True), value
    return value


def run_probe(id, profile):
    start = ["workspace", "start", "--id", id, "--ack-hidden-workspace"]
    if profile is None:
        start += ["--session", BACKEND]
    else:
        path = ROOT / f"{id}.profile.json"
        path.write_text(json.dumps(profile))
        cmd("profile", "import", "--json", path)
        start += ["--profile", id]
    status = cmd(*start)["status"]
    try:
        # The test process itself reaches the unfiltered main socket. Its
        # privileged names are what a sandboxed app would try to guess.
        main = subprocess.run(["python3", "-c", PROBE], capture_output=True, text=True, timeout=30,
                              env=dict(ENV, XDG_RUNTIME_DIR=status["runtime_dir"], WAYLAND_DISPLAY="workspace-wayland"))
        assert main.returncode == 0, main
        names = json.loads(main.stdout)["names"]
        guesses = {name: interface for name, interface in names.items() if interface in PRIVILEGED}
        run = cmd("workspace", "run", "--id", id, "--timeout-ms", 20000, "--tail-bytes", 262144,
                  "--", "python3", "-c", PROBE, json.dumps(guesses))
        assert run["exit_code"] == 0, run
        if profile is not None:
            app = run["launch"]["apps"][0]
            assert app["network_isolation"] == "bubblewrap_unshare_net", app
            expected = "bubblewrap_mount_namespace" if profile.get("mounts") else "host"
            assert app["mount_isolation"] == expected, app
        result = json.loads(run["stdout"]["content"].strip().splitlines()[-1])
        if shutil.which("grim"):
            # Capture must work for the control and fail inside sandboxes.
            shot = cmd("workspace", "run", "--id", id, "--timeout-ms", 20000, "--", "grim", "/tmp/shot.png")
            result["grim_exit_code"] = shot["exit_code"]
            assert (shot["exit_code"] == 0) == (profile is None), shot
        if profile is not None and shutil.which("kwrite"):
            # A real Qt Wayland app still renders, takes input and pastes
            # through the filter.
            host_note = ROOT / "project" / f"{id}.txt"
            host_note.write_text("")
            note = f"/workspace/project/{id}.txt" if profile.get("mounts") else str(host_note)
            cmd("workspace", "launch", "--id", id, "--wait-window", "--window-timeout-ms", 15000, "--", "kwrite", note)
            window = next(w for w in cmd("workspace", "windows", "--id", id)["windows"] if host_note.name in w["title"])
            cmd("workspace", "focus-window", "--id", id, window["id"])
            cmd("workspace", "type", "--id", id, "filtered Wayland input")
            cmd("workspace", "paste", "--id", id, " and paste")
            deadline = time.monotonic() + 15
            while host_note.read_text().rstrip("\n") != "filtered Wayland input and paste":
                assert time.monotonic() < deadline, host_note.read_text()
                cmd("workspace", "key", "--id", id, "ctrl+s")
                time.sleep(0.5)
            result["kwrite"] = "typed, pasted and saved"
        return result
    finally:
        cmd("workspace", "stop", "--id", id)


def main():
    cases = {
        "unsandboxed": None,
        "network": {"id": "swg-network", "session": {"backend": BACKEND}, "network": {"mode": "disabled"},
                    "require_enforced_policy": True},
        "mounts": {"id": "swg-mounts", "session": {"backend": BACKEND}, "network": {"mode": "disabled"},
                   "require_enforced_policy": True, "cwd": "/workspace/project",
                   "mounts": [{"host_path": str(ROOT / "project"), "workspace_path": "/workspace/project",
                               "mode": "read_write"}]},
    }
    report = {}
    for label, profile in cases.items():
        report[label] = run_probe(f"swg-{label}" if profile is None else profile["id"], profile)
    (ARTIFACTS / f"sandbox-wayland-globals-{BACKEND}.json").write_text(json.dumps(report, indent=2, sort_keys=True))
    for label, result in report.items():
        print(f"{BACKEND} {label}: {len(result['globals'])} globals, privileged visible "
              f"{result['privileged_visible']}, binds {result['privileged_bind']}, "
              f"guessed hidden binds {result['hidden_bind']}", flush=True)
    leaks = {label: result["privileged_visible"] for label, result in report.items()
             if label != "unsandboxed" and result["privileged_visible"]}
    bound = {label: [name for name, outcome in {**result["privileged_bind"], **result["hidden_bind"]}.items()
                     if outcome == "bound"]
             for label, result in report.items() if label != "unsandboxed"}
    assert not leaks and not any(bound.values()), f"sandboxed apps reach privileged Wayland globals: {leaks} {bound}"
    print(f"{BACKEND} sandboxed Wayland globals passed; evidence {ROOT}", flush=True)


if __name__ == "__main__":
    main()
