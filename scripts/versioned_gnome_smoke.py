#!/usr/bin/env python3
"""Exercise the prepared GNOME 46 runtime through the public MCP and CLI."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time

from kwin_mcp import Mcp

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get('AGENT_WORKSPACE_BIN', ROOT / 'target/debug/agent-workspace-linux')).resolve()
ARTIFACTS = Path(sys.argv[1]).resolve()
ARTIFACTS.mkdir(parents=True, exist_ok=True)
ID = 'versioned-gnome-46-smoke'
runtime_target = Path(os.environ.get('GNOME46_RUNTIME_DIR', ARTIFACTS / 'r'))
runtime_target.mkdir(parents=True, exist_ok=True, mode=0o700)
runtime_link = None
if 'GNOME46_RUNTIME_DIR' not in os.environ:
    # Exercise a /tmp runtime path while storing its files on disk.
    runtime_link = Path('/tmp') / f'awl-v46-{os.getpid()}'
    runtime_link.symlink_to(runtime_target.parent, target_is_directory=True)
ENV = dict(os.environ, XDG_CONFIG_HOME=str(ARTIFACTS / 'config'),
           XDG_RUNTIME_DIR=str(runtime_link / runtime_target.name if runtime_link else runtime_target))
sequence = 0


def cli(*args, expected=True):
    global sequence
    sequence += 1
    result = subprocess.run([str(BIN), *map(str, args)], capture_output=True, text=True, timeout=90, env=ENV)
    (ARTIFACTS / f'{sequence:03d}.stdout').write_text(result.stdout)
    (ARTIFACTS / f'{sequence:03d}.stderr').write_text(result.stderr)
    if not expected:
        assert result.returncode != 0, result.stdout
        return result.stderr
    assert result.returncode == 0, result.stderr
    value = json.loads(result.stdout)
    assert value.get('ok', True), value
    return value


def ws(action, *args):
    return cli('workspace', action, '--id', ID, *args)


def wait(probe, label):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if probe():
            return
        time.sleep(.05)
    raise AssertionError('timeout: ' + label)


def png_size(path):
    data = Path(path).read_bytes()
    assert data[:8] == b'\x89PNG\r\n\x1a\n' and len(data) > 1000
    return struct.unpack('>II', data[16:24])


def ipc_click(status, x, y):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(status['socket_path'])
        connection.sendall((json.dumps({'method': 'click', 'x': x, 'y': y, 'button': 1, 'count': 1})+'\n').encode())
        return json.loads(connection.makefile().readline())


client = Mcp(str(BIN), ARTIFACTS / 'mcp.stderr', headless=True, env=ENV)
# Deliberately leave one child unreaped until cleanup finishes. A zombie has
# no mount namespace and must not obstruct another workspace's collection.
zombie = subprocess.Popen(['true'])
wait(lambda: Path(f'/proc/{zombie.pid}/stat').read_text().rsplit(') ', 1)[1].startswith('Z'),
     'unreaped regression child')
status = None
try:
    profile = {'id': 'gnome46-regression', 'session': {'backend': 'gnome', 'version': '46', 'scale': 1.25}}
    saved = client.tool('profile_put', {'profile': profile})
    assert saved['ok'], saved
    started = client.tool('workspace_start', {
        'id': ID, 'profile': profile['id'],
        'width': 1600, 'height': 1000, 'acknowledge_hidden_workspace': True,
        'purpose': 'Validate prepared GNOME 46 with private display and buses', 'open_viewer': False,
    })
    assert started['ok'], started
    status = ws('status')
    (ARTIFACTS / 'status.json').write_text(json.dumps(status, indent=2))
    assert status['session'] == {'backend': 'gnome', 'version': '46', 'scale': 1.25}, status
    assert (status['width'], status['height']) == (1600, 1000)
    runtime = Path(status['runtime_dir'])
    compositor = (runtime / 'gnome.log').read_text()
    assert 'mutter 46.' in compositor and 'Created surfaceless renderer without GPU' in compositor
    reported = client.tool('workspace_run_app', {'id': ID, 'command': ['gnome-shell', '--version'],
                            'timeout_ms': 10000, 'kill_on_timeout': True})
    assert reported['run']['succeeded'] and 'GNOME Shell 46.' in reported['run']['stdout']['content'], reported
    inherited = client.tool('workspace_run_app', {'id': ID, 'command': ['python3', '-c',
        'import os; assert "AGENT_WORKSPACE_SHARED_CONTROL" not in os.environ; '
        'assert not any("owner.json" in os.readlink("/proc/self/fd/"+f) '
        'for f in os.listdir("/proc/self/fd") if os.path.exists("/proc/self/fd/"+f))'],
        'timeout_ms': 10000, 'kill_on_timeout': True})
    assert inherited['run']['succeeded'], inherited
    mismatch = cli('workspace', 'start', '--id', ID, '--ack-hidden-workspace', '--session', 'gnome', expected=False)
    assert 'differs' in mismatch, mismatch
    evidence = ARTIFACTS / 'gtk.json'
    ws('launch', '--wait-window', '--window-timeout-ms', 10000, '--', 'python3', ROOT / 'scripts/gnome_fixture.py', evidence)
    wait(lambda: evidence.exists(), 'GTK fixture')
    window = ws('windows')['windows'][0]['id']
    ws('focus-window', window)
    text = 'GNOME 46 version selection: café שלום 日本語'
    ws('type', text)
    wait(lambda: json.loads(evidence.read_text())['text'] == text, 'Unicode input')
    ws('key', 'ctrl+a')
    ws('clipboard-set', 'GNOME 46 clipboard')
    assert ws('clipboard-get')['clipboard']['content'] == 'GNOME 46 clipboard'
    ws('key', 'ctrl+v')
    wait(lambda: json.loads(evidence.read_text())['text'] == 'GNOME 46 clipboard', 'clipboard paste')
    ws('move-window', window, 30, 45)
    ws('resize-window', window, 600, 450)
    geometry = ws('windows')['windows'][0]['geometry']
    assert (geometry['x'], geometry['y'], geometry['width'], geometry['height']) == (30, 45, 600, 450), geometry
    wait(lambda: abs(json.loads(evidence.read_text())['width'] * 1.25 - 600) <= 1, 'physical window sizing')
    fixture = json.loads(evidence.read_text())
    bx, by, bw, bh = fixture['button_rect']
    x, y = round(geometry['x'] + (bx + bw / 2) * 1.25), round(geometry['y'] + (by + bh / 2) * 1.25)
    ws('move-pointer', x, y)
    pointer = ws('pointer')['pointer']
    assert abs(pointer['x'] - x) <= 1 and abs(pointer['y'] - y) <= 1, pointer
    ws('click', x, y)
    wait(lambda: json.loads(evidence.read_text())['clicks'] == 1, 'scaled click delivery')
    ws('scroll', x, y, 'down')
    wait(lambda: json.loads(evidence.read_text())['scrolls'], 'scroll delivery')
    ws('drag', x, y, x + 20, y)
    ws('minimize-window', window)
    assert not next(item for item in ws('windows', '--all')['windows'] if item['id'] == window)['visible']
    ws('show-window', window)
    ws('focus-window', window)
    for action, args, filename, expected in [
        ('screenshot', [], 'desktop.png', (1600, 1000)),
        ('screenshot-window', ['--window', window], 'window.png', (600, 450)),
    ]:
        shot = ws(action, *args)['screenshot']
        assert png_size(shot['path']) == expected, shot
        shutil.copy2(shot['path'], ARTIFACTS / filename)
    denied = client.tool('workspace_run_app', {'id': ID, 'command': ['gdbus', 'call', '--session',
        '--dest', 'org.gnome.Shell', '--object-path', '/org/gnome/Shell', '--method', 'org.gnome.Shell.Eval', '1+1'],
        'timeout_ms': 10000, 'kill_on_timeout': True})
    assert not denied['run']['succeeded'] and 'AccessDenied' in denied['run']['stderr']['content'], denied
    # Bypass the MCP front end to prove the daemon sees atomic host pause
    # updates through its namespace. This control file is test-private.
    paused = client.tool('mcp_control_update', {'mode': 'paused', 'reason': 'private runtime regression'})
    assert paused['ok'], paused
    blocked = ipc_click(status, x, y)
    assert not blocked['ok'] and 'paused' in blocked['message'], blocked
    resumed = client.tool('mcp_control_update', {'mode': 'active', 'confirmed_user_request': True,
                          'reason': 'resume private test fixture'})
    assert resumed['ok'], resumed
    assert ipc_click(status, x, y)['ok']
    print('GNOME 46/Mutter 46, 125% physical input, Unicode, clipboard, window controls, capture and bus denial passed.', flush=True)
    runtime = Path(status['runtime_dir'])
    for path in [*runtime.glob('*.log'), runtime / 'output-config.json']:
        if path.is_file():
            shutil.copy2(path, ARTIFACTS / path.name)
    ws('stop')
    cache = Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache')))
    userspace = cache / 'agent-workspace-linux/runtime-sessions' / ID
    wait(lambda: not userspace.exists(), 'prepared userspace cleanup')
    # Interrupt only the verified supervisor of this test workspace.
    restarted = client.tool('workspace_start', {
        'id': ID, 'profile': profile['id'], 'acknowledge_hidden_workspace': True,
        'open_viewer': False,
    })
    assert restarted['ok'], restarted
    holder = ARTIFACTS / 'holder-thread'
    app = ws('launch', '--', 'python3', '-c',
        'import ctypes,threading,time; from pathlib import Path; '
        't=threading.Thread(target=lambda: '
        f'(Path({str(holder)!r}).write_text(str(threading.get_native_id())),time.sleep(300))); '
        't.start(); ctypes.CDLL(None).pthread_exit(None)')['apps'][0]
    wait(lambda: holder.exists() and Path(f"/proc/{app['pid']}/stat").read_text()
         .rsplit(') ', 1)[1].startswith('Z'), 'dead leader with live worker')
    layers = list(userspace.iterdir())
    assert len(layers) == 1, layers
    owner = json.loads((layers[0] / 'owner.json').read_text())
    pid = owner['pid']
    proc = Path('/proc') / str(pid)
    assert ID.encode() in (proc / 'cmdline').read_bytes().split(b'\0')
    assert int((proc / 'stat').read_text().rsplit(') ', 1)[1].split()[19]) == owner['start_ticks']
    os.kill(pid, signal.SIGTERM)
    wait(lambda: not Path(status['socket_path']).exists() or
         subprocess.run([str(BIN), 'workspace', 'status', '--id', ID],
                        env=ENV, capture_output=True, timeout=5).returncode != 0,
         'interrupted daemon exit')
    retained = cli('workspace', 'start', '--id', ID, '--session', 'gnome',
                   '--session-version', '46', '--ack-hidden-workspace', expected=False)
    assert 'still mounted' in retained and layers[0].exists(), retained
    # The worker is still alive, so its writable layer must remain intact.
    assert Path(f"/proc/{app['pid']}/task/{holder.read_text()}/mountinfo").exists()
    preview = ws('cleanup', '--dry-run')
    assert preview['candidates'] and userspace.exists(), preview
    cleaned = ws('cleanup')
    assert cleaned['removed'] and not cleaned['skipped'], cleaned
    assert not userspace.exists(), userspace
    print('Interrupted-supervisor dry-run and verified stale-layer cleanup passed.', flush=True)
finally:
    try:
        # Cleanup is scoped to this test's isolated runtime and workspace id.
        current = subprocess.run([str(BIN), 'workspace', 'status', '--id', ID],
                                 env=ENV, capture_output=True, timeout=10)
        if current.returncode == 0:
            ws('stop')
        preview = ws('cleanup', '--dry-run')
        if preview.get('candidates'):
            cleaned = ws('cleanup')
            assert not cleaned['skipped'], cleaned
    finally:
        client.close()
        zombie.wait(timeout=5)
        if runtime_link:
            runtime_link.unlink()
print('Selected-runtime shutdown and writable-layer cleanup passed.', flush=True)
