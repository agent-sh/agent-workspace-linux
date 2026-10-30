#!/usr/bin/env python3
"""Exercise the prepared GNOME 46 runtime through the public MCP and CLI."""
import json
import os
from pathlib import Path
import shutil
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
ENV = dict(os.environ, XDG_CONFIG_HOME=str(ARTIFACTS / 'config'))
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


client = Mcp(str(BIN), ARTIFACTS / 'mcp.stderr', headless=True, env=ENV)
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
    print('GNOME 46/Mutter 46, 125% physical input, Unicode, clipboard, window controls, capture and bus denial passed.', flush=True)
finally:
    if status:
        runtime = Path(status['runtime_dir'])
        for path in [*runtime.glob('*.log'), runtime / 'output-config.json']:
            if path.is_file():
                shutil.copy2(path, ARTIFACTS / path.name)
        ws('stop')
        cache = Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / '.cache')))
        userspace = cache / 'agent-workspace-linux/runtime-sessions' / ID
        wait(lambda: not userspace.exists(), 'prepared userspace cleanup')
    client.close()
print('Selected-runtime shutdown and writable-layer cleanup passed.', flush=True)
