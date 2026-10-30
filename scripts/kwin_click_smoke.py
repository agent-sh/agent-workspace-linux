#!/usr/bin/env python3
"""Assert client delivery through CLI/MCP inside the isolated KWin probe."""
import json
import os
from pathlib import Path
import subprocess
import time
from kwin_mcp import Mcp

BIN = os.environ.get('KWIN_PROBE_BIN', '/opt/agent-workspace-linux')
ARTIFACTS = Path('/artifacts')
os.environ.update(XDG_RUNTIME_DIR='/tmp/awl-r', XDG_CONFIG_HOME='/tmp/awl-c',
                  HOME='/tmp/awl-home', LANG='C.UTF-8')
for name in ['XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'HOME']:
    Path(os.environ[name]).mkdir(mode=0o700, parents=True, exist_ok=True)
assert 'DISPLAY' not in os.environ and 'WAYLAND_DISPLAY' not in os.environ
Path('/tmp/.X11-unix').mkdir(mode=0o1777, exist_ok=True)
sequence = 0
def cli(*args):
    global sequence
    sequence += 1
    result = subprocess.run([BIN, 'workspace', *map(str, args)], capture_output=True,
                            text=True, timeout=60)
    (ARTIFACTS / f'{sequence:03d}.stdout').write_text(result.stdout)
    (ARTIFACTS / f'{sequence:03d}.stderr').write_text(result.stderr)
    assert result.returncode == 0, result.stderr
    value = json.loads(result.stdout)
    assert value.get('ok', True), value
    return value

def wait(probe, label):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if probe(): return
        time.sleep(.05)
    raise AssertionError('timeout: ' + label)

for scale, toolkit in [(1, 'qt'), (1, 'gtk'), (1.5, 'qt'), (1.5, 'gtk')]:
    id = f'click-{toolkit}-{int(scale*100)}'
    output = ARTIFACTS / f'{id}.json'
    client = Mcp(BIN, ARTIFACTS / f'{id}-mcp.log', headless=True)
    status = None
    try:
        start = client.tool('workspace_start', {'id': id, 'session': {'backend': 'kwin', 'scale': scale},
            'acknowledge_hidden_workspace': True, 'open_viewer': False, 'width': 960, 'height': 720})
        assert start['ok'], start
        status = cli('status', '--id', id)
        cli('launch', '--id', id, '--wait-window', '--', 'python3',
            Path(__file__).with_name('kwin_click_fixture.py' if toolkit == 'qt' else 'kwin_click_gtk_fixture.py'), output)
        wait(lambda: output.exists(), 'fixture')
        window = next(w for w in cli('windows', '--id', id)['windows']
                      if w['title'] == 'KWin click regression')
        cli('focus-window', '--id', id, window['id'])
        points = json.loads(output.read_text())['targets']
        for index in range(8):
            local = points['left' if index % 2 == 0 else 'right']
            x = window['geometry']['x'] + round(local[0] * scale)
            y = window['geometry']['y'] + round(local[1] * scale)
            cli('move-pointer', '--id', id, 5, 5)
            if index % 2 == 0:
                cli('click', '--id', id, x, y)
            else:
                clicked = client.tool('workspace_click', {'id': id, 'x': x, 'y': y})
                assert clicked['ok'], clicked
        tick = json.loads(output.read_text())['ticks']
        wait(lambda: json.loads(output.read_text())['ticks'] >= tick + 2, 'client event-loop barrier')
        observed = json.loads(output.read_text())
        counts = {kind: sum(e['kind'] == kind for e in observed['events']) for kind in ['press', 'release']}
        print(f'KWin {scale} {toolkit}: click events={counts}, widget activations={observed["activations"]}', flush=True)
        assert counts == {'press': 8, 'release': 8} and observed['activations'] == 8, observed
        # Control from the report: drag at the last target must produce a pair.
        before = len(observed['events'])
        cli('move-pointer', '--id', id, 5, 5)
        cli('drag', '--id', id, x, y, x, y)
        wait(lambda: len(json.loads(output.read_text())['events']) >= before + 2, 'zero-length drag control')
        cli('move-pointer', '--id', id, 5, 5)
        cli('scroll', '--id', id, x, y, 'down')
        wait(lambda: json.loads(output.read_text())['scrolls'] > 0, 'new-target wheel delivery')
        print(f'KWin {scale} {toolkit}: CLI/MCP delivered eight complete clicks and widget activations', flush=True)
    finally:
        if status:
            runtime = Path(status['runtime_dir'])
            for path in runtime.glob('*.log'):
                (ARTIFACTS / f'{id}-{path.name}').write_bytes(path.read_bytes())
            cli('stop', '--id', id)
            cli('cleanup', '--id', id)
        client.close()
