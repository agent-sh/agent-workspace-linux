#!/usr/bin/env python3
"""Native GNOME acceptance. Run in gnome_probe's disposable container."""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import time
from kwin_mcp import Mcp

BIN = '/opt/agent-workspace-linux'
ARTIFACTS = Path('/artifacts')
os.environ.update(XDG_RUNTIME_DIR='/tmp/awl-runtime', XDG_CONFIG_HOME='/tmp/awl-config', HOME='/tmp/awl-home', LANG='C.UTF-8')
for key in ('XDG_RUNTIME_DIR', 'XDG_CONFIG_HOME', 'HOME'):
    Path(os.environ[key]).mkdir(mode=0o700, exist_ok=True)
sequence = 0


def command(*args, expected=True, failure=None):
    global sequence
    sequence += 1
    result = subprocess.run([BIN, *map(str, args)], capture_output=True, text=True, timeout=60)
    (ARTIFACTS / f'{sequence:03d}.json').write_text(result.stdout)
    (ARTIFACTS / f'{sequence:03d}.stderr').write_text(result.stderr)
    if not expected:
        # Denials are either a nonzero exit or a JSON {ok:false}; both must
        # carry the expected reason so an unrelated error cannot pass.
        assert result.returncode != 0 or not json.loads(result.stdout).get('ok', True), (args, result.stdout)
        assert failure and failure in result.stdout + result.stderr, (args, failure, result.stdout, result.stderr)
        return
    assert result.returncode == 0, (args, result.stderr)
    value = json.loads(result.stdout)
    assert value.get('ok', True), (args, value)
    return value


def ws(id, *args):
    return command('workspace', args[0], '--id', id, *args[1:])


def wait(probe, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if probe():
            return
        time.sleep(.1)
    raise AssertionError(f'timeout: {label}')


def process_stopped(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()[0] in ('Z', 'X')
    except FileNotFoundError:
        return True


def png_size(path):
    data = Path(path).read_bytes()
    assert data[:8] == b'\x89PNG\r\n\x1a\n' and len(data) > 1000
    return struct.unpack('>II', data[16:24])


def save_logs(runtime, id):
    for path in [*runtime.glob('*.log'), *runtime.glob('apps/*.stderr.log'), *runtime.glob('*-bus.conf'), runtime / 'output-config.json']:
        if path.is_file():
            (ARTIFACTS / f'{id}-{path.name}').write_bytes(path.read_bytes())


assert 'DISPLAY' not in os.environ and 'WAYLAND_DISPLAY' not in os.environ
assert not Path('/dev/dri').exists() and not Path('/dev/input').exists()
Path('/tmp/.X11-unix').mkdir(mode=0o1777, exist_ok=True)
assert command('doctor')['ready_for_gnome_workspace']
command('workspace', 'start', '--id', 'gnome-invalid-scale', '--ack-hidden-workspace',
    '--session', 'gnome', '--scale', 1.5, '--width', 960, '--height', 720, expected=False,
    failure='GNOME monitor does not support scale 1.5')
wait(lambda: 'gnome-shell' not in subprocess.check_output(['ps', '-eo', 'comm='], text=True).splitlines(),
    'failed startup compositor cleanup')
for scale in (1, 1.5):
    id = f'gnome-{int(scale * 100)}'
    runtime = Path(os.environ['XDG_RUNTIME_DIR']) / 'agent-workspace-linux' / id
    status = None
    mcp = None
    terminal_pids = []
    # Stale service activation must not escape app namespaces after a restart.
    stale = runtime / 'session-services'
    stale.mkdir(parents=True, exist_ok=True)
    marker = Path(f'/tmp/forbidden-{id}')
    (stale / 'org.agent_sh.Forbidden.service').write_text(
        f'[D-BUS Service]\nName=org.agent_sh.Forbidden\nExec=/usr/bin/touch {marker}\n')
    try:
        if scale == 1.5:
            mcp = Mcp(BIN, ARTIFACTS / f'{id}-mcp.log', headless=True)
            start = mcp.tool('workspace_start', {'id': id, 'session': {'backend': 'gnome', 'scale': scale},
                'width': 1200, 'height': 900, 'acknowledge_hidden_workspace': True, 'open_viewer': False})
            assert start['ok'], start
        else:
            ws(id, 'start', '--ack-hidden-workspace', '--session', 'gnome', '--scale', scale, '--width', 1200, '--height', 900)
        status = ws(id, 'status')
        assert status['display'].startswith(':') and status['x_server_pid'] == 0, status
        assert status['session']['backend'] == 'gnome' and status['session'].get('scale', 1) == scale
        config = json.loads((runtime / 'output-config.json').read_text())
        assert config['scale'] == scale and config['width'] == 1200 and config['height'] == 900
        assert not config['has_run_dialog'] and not config['has_overview']
        env = {v['name']: v['value'] for v in ws(id, 'env')['environment']['variables']}
        bus_env = dict(os.environ, **env)
        control_env = dict(bus_env, DBUS_SESSION_BUS_ADDRESS=f'unix:path={runtime}/session-bus')
        assert env['DBUS_SYSTEM_BUS_ADDRESS'].endswith(f'/{id}/system-bus')
        assert env['GDK_BACKEND'] == 'wayland'
        # An application on this private bus cannot impersonate its daemon.
        unauth = subprocess.run(['gdbus', 'call', '--session', '--dest', 'org.agent_sh.Workspace.Gnome',
            '--object-path', '/org/agent_sh/Workspace/Gnome', '--method', 'org.agent_sh.Workspace.Gnome.Command',
            '{"op":"probe"}'], env=control_env, capture_output=True, text=True, timeout=5)
        assert 'Only the workspace daemon' in unauth.stdout, unauth
        for interface, method in [('org.gnome.Shell', 'Eval'), ('org.gnome.Shell.Extensions', 'InstallRemoteExtension')]:
            denied = subprocess.run(['dbus-send', '--session', '--print-reply', '--reply-timeout=1000',
                '--dest=org.gnome.Shell', '/org/gnome/Shell', f'{interface}.{method}', 'string:blocked'],
                env=control_env, capture_output=True, text=True, timeout=5)
            assert denied.returncode != 0 and 'AccessDenied' in denied.stderr, denied
        for address in {env['DBUS_SESSION_BUS_ADDRESS'], control_env['DBUS_SESSION_BUS_ADDRESS'], env['DBUS_SYSTEM_BUS_ADDRESS'], env['AT_SPI_BUS_ADDRESS']}:
            denied = subprocess.run(['dbus-send', f'--bus={address}', '--print-reply', '--reply-timeout=1000',
                '--dest=org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus.ReloadConfig'],
                env=bus_env, capture_output=True, text=True, timeout=5)
            assert denied.returncode != 0 and 'AccessDenied' in denied.stderr, denied
            forbidden = subprocess.run(['dbus-send', f'--bus={address}', '--print-reply', '--reply-timeout=1000',
                '--dest=org.agent_sh.Forbidden', '/org/agent_sh/Forbidden', 'org.agent_sh.Forbidden.Ping'],
                env=bus_env, capture_output=True, text=True, timeout=5)
            assert forbidden.returncode != 0 and 'ServiceUnknown' in forbidden.stderr, forbidden
        assert not marker.exists()
        evidence = Path(f'/tmp/fixture-{scale}.json')
        ws(id, 'launch', '--wait-window', '--window-timeout-ms', 10000, '--', 'python3', '/opt/gnome-probe/gnome_fixture.py', evidence)
        wait(lambda: evidence.exists() and len(ws(id, 'windows')['windows']) == 1, 'native fixture ready')
        wid = ws(id, 'windows')['windows'][0]['id']
        assert wid.startswith('gnome:')
        ws(id, 'focus-window', wid)
        text = f'Native {scale} café שלום 日本語'
        ws(id, 'type', text)
        wait(lambda: json.loads(evidence.read_text())['text'] == text, 'native Unicode delivery')
        assert ws(id, 'active-window')['active_window']['id'] == wid
        ws(id, 'key', 'ctrl+a')
        ws(id, 'clipboard-set', 'clipboard café שלום')
        assert ws(id, 'clipboard-get')['clipboard']['content'] == 'clipboard café שלום'
        ws(id, 'key', 'ctrl+v')
        wait(lambda: json.loads(evidence.read_text())['text'] == 'clipboard café שלום', 'native clipboard paste')
        ws(id, 'move-window', wid, 30, 45)
        ws(id, 'resize-window', wid, 600, 450)
        window = ws(id, 'windows')['windows'][0]
        g = window['geometry']
        assert (g['x'], g['y'], g['width'], g['height']) == (30, 45, 600, 450), window
        wait(lambda: abs(json.loads(evidence.read_text())['width'] * scale - 600) <= 1, 'client allocation')
        fixture = json.loads(evidence.read_text())
        bx, by, bw, bh = fixture['button_rect']
        x, y = round(g['x'] + (bx + bw / 2) * scale), round(g['y'] + (by + bh / 2) * scale)
        ws(id, 'move-pointer', x, y)
        pointer = ws(id, 'pointer')['pointer']
        assert abs(pointer['x'] - x) <= 1 and abs(pointer['y'] - y) <= 1, pointer
        ws(id, 'click', x, y)
        wait(lambda: json.loads(evidence.read_text())['clicks'] == 1, 'native button delivery at physical coordinates')
        ws(id, 'scroll', x, y, 'down')
        wait(lambda: bool(json.loads(evidence.read_text())['scrolls']), 'native scroll delivery')
        ws(id, 'drag', x, y, x + 20, y)
        ws(id, 'raise-window', wid)
        ws(id, 'minimize-window', wid)
        assert not next(w for w in ws(id, 'windows', '--all')['windows'] if w['id'] == wid)['visible']
        ws(id, 'show-window', wid)
        ws(id, 'focus-window', wid)
        shot = ws(id, 'screenshot')['screenshot']
        assert png_size(shot['path']) == (1200, 900), shot
        shutil.copy2(shot['path'], ARTIFACTS / f'{id}-desktop.png')
        shot = ws(id, 'screenshot-window', '--window', wid)['screenshot']
        assert png_size(shot['path']) == (600, 450), shot
        shutil.copy2(shot['path'], ARTIFACTS / f'{id}-window.png')
        # The host viewer uses this exact control IPC contract.
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(10)
            client.connect(status['socket_path'])
            client.sendall((json.dumps({'method': 'screenshot', 'output_path': 'viewer-frame.png', 'source': 'viewer_stream'}) + '\n').encode())
            frame = json.loads(client.makefile().readline())
            assert frame['ok'] and frame['screenshot']['source'] == 'viewer_stream', frame
            assert png_size(frame['screenshot']['path']) == (1200, 900)
        # Run-dialog shortcut must remain inert under the restricted Shell mode.
        ws(id, 'key', 'alt+F2')
        assert len(ws(id, 'windows')['windows']) == 1
        ws(id, 'key', 'Escape')
        (ARTIFACTS / f'{id}-fixture.json').write_text(evidence.read_text())
        ws(id, 'close-window', wid)
        if mcp:
            terminal = mcp.tool('workspace_run_in_terminal', {'id': id, 'terminal_id': 'cleanup',
                'command': ['sleep', '600'], 'window_timeout_ms': 15000, 'timeout_ms': 10000})
            assert terminal['ok'], terminal
            terminal = terminal['terminal']
            terminal_pids = [int(pid) for pid in subprocess.check_output(['tmux', '-S', terminal['tmux_socket_path'],
                'display-message', '-p', '#{pid} #{pane_pid}'], env=bus_env, text=True, timeout=5).split()]
            assert mcp.tool('workspace_kill_app', {'id': id, 'app_id': terminal['app_id']})['ok']
        print(f'GNOME {scale}: native client input, clipboard, windows, scale, capture and viewer IPC passed', flush=True)
    finally:
        if mcp:
            mcp.close()
        save_logs(runtime, id)
        fixture_file=Path(f'/tmp/fixture-{scale}.json')
        if fixture_file.exists(): (ARTIFACTS/f'{id}-fixture.json').write_text(fixture_file.read_text())
        if status:
            ws(id, 'stop')
            wait(lambda: process_stopped(status['window_manager_pid']), 'compositor shutdown')
            wait(lambda: all(process_stopped(pid) for pid in terminal_pids), 'managed terminal cleanup')
            assert not (runtime / 'workspace-wayland').exists(), 'stale Wayland socket'
            for name in ('session-bus','client-bus','system-bus','at-spi/bus'):
                assert not (runtime / name).exists(), f'stale private bus {name}'
            assert not Path('/tmp/.X11-unix/X' + status['display'][1:]).exists(), 'stale XWayland socket'
        else:
            subprocess.run([BIN, 'workspace', 'cleanup', '--id', id], capture_output=True, timeout=10)

# The daemon must retain enforced mount/network restrictions for native and
# legacy XWayland clients, while its own bus cannot activate arbitrary services.
project = Path('/tmp/gnome-project')
project.mkdir()
profile = Path('/tmp/gnome-profile.json')
profile.write_text(json.dumps({'id': 'gnome-restricted', 'session': {'backend': 'gnome'},
    'cwd': '/workspace/project', 'require_enforced_policy': True, 'network': {'mode': 'disabled'},
    'mounts': [{'host_path': str(project), 'workspace_path': '/workspace/project', 'mode': 'read_write'}]}))
command('profile', 'import', '--json', profile)
id = 'gnome-restricted'
try:
    ws(id, 'start', '--profile', id, '--ack-hidden-workspace')
    native = ws(id, 'launch', '--wait-window', '--window-timeout-ms', 10000, '--', 'python3', '/opt/gnome-probe/gnome_fixture.py', '/workspace/project/native.json')
    wait(lambda: (project / 'native.json').exists(), 'restricted native client')
    app = native['apps'][0]
    assert app['mount_isolation'] == 'bubblewrap_mount_namespace' and app['network_isolation'] == 'bubblewrap_unshare_net', app
    legacy_fixture = ws(id, 'launch', '--wait-window', '--window-timeout-ms', 10000, '--',
        'env', 'GDK_BACKEND=x11', 'GTK_IM_MODULE=simple', 'GNOME_FIXTURE_APP_ID=org.agent_sh.GnomeLegacyFixture',
        'python3', '/opt/gnome-probe/gnome_fixture.py', '/workspace/project/x11.json')
    legacy_id = legacy_fixture['windows'][0]['id']
    ws(id, 'focus-window', legacy_id)
    ws(id, 'type', 'Legacy café שלום 日本語')
    wait(lambda: (project/'x11.json').exists() and json.loads((project/'x11.json').read_text())['text']=='Legacy café שלום 日本語', 'XWayland Unicode delivery')
    (ARTIFACTS/'legacy-fixture.json').write_text((project/'x11.json').read_text())
    ws(id,'key','ctrl+a')
    ws(id,'type','Replacement 😀')
    wait(lambda: json.loads((project/'x11.json').read_text())['text']=='Replacement 😀','GTK selection replacement')
    ws(id,'key','Home')
    ws(id,'key','Right')
    ws(id,'type','א')
    wait(lambda: json.loads((project/'x11.json').read_text())['text']=='Rאeplacement 😀','GTK caret insertion')
    qt=ws(id,'launch','--wait-window','--window-timeout-ms',10000,'--','env','QT_QPA_PLATFORM=xcb','QT_IM_MODULE=compose',
        'python3','/opt/gnome-probe/gnome_qt_fixture.py','/workspace/project/qt.json')
    ws(id,'focus-window',qt['windows'][0]['id'])
    ws(id,'type','Qt café שלום 日本語')
    wait(lambda:(project/'qt.json').exists() and json.loads((project/'qt.json').read_text())['text']=='Qt café שלום 日本語','cold Qt Unicode insertion')
    ws(id,'key','ctrl+a')
    ws(id,'type','Replacement 😀')
    wait(lambda:json.loads((project/'qt.json').read_text())['text']=='Replacement 😀','Qt selection replacement')
    ws(id,'key','Home')
    ws(id,'key','Right')
    ws(id,'type','א')
    wait(lambda:json.loads((project/'qt.json').read_text())['text']=='Rאeplacement 😀','Qt caret insertion')
    legacy = ws(id, 'launch', '--wait-window', '--window-timeout-ms', 10000, '--', 'xterm', '-e', 'sh', '-c', 'printf ready > /workspace/project/legacy; exec tail -f /dev/null')
    assert legacy['windows'], legacy
    wait(lambda: (project / 'legacy').exists(), 'restricted XWayland client')
    terminal=ws(id,'launch','--wait-window','--window-timeout-ms',10000,'--','xterm','-e','python3','-c',
        "import pathlib,time; pathlib.Path('/workspace/project/terminal.txt').write_text(input()); time.sleep(60)")
    ws(id,'focus-window',terminal['windows'][0]['id'])
    ws(id,'clipboard-set','CLIPBOARD SENTINEL')
    ws(id,'type','Terminal café שלום 日本語\n')
    wait(lambda:(project/'terminal.txt').exists() and (project/'terminal.txt').read_text()=='Terminal café שלום 日本語','xterm PRIMARY Unicode paste')
    assert ws(id,'clipboard-get')['clipboard']['content']=='CLIPBOARD SENTINEL'

finally:
    save_logs(Path(os.environ['XDG_RUNTIME_DIR']) / 'agent-workspace-linux' / id, id)
    for name in ('native','x11','qt'):
        if (project/f'{name}.json').exists(): (ARTIFACTS/f'restricted-{name}.json').write_text((project/f'{name}.json').read_text())
    ws(id, 'stop')

# Spawn-time app ceiling remains effective after the originating MCP exits.
permissions = Path('/tmp/gnome-permissions.json')
permissions.write_text(json.dumps({'apps': {'allow': ['/usr/bin/true']}}))
wrapper = ARTIFACTS / 'gnome-ceiling-bin'
wrapper.write_text('#!/bin/sh\nexec /opt/agent-workspace-linux "$@" --permissions /tmp/gnome-permissions.json\n')
wrapper.chmod(0o700)
mcp = Mcp(str(wrapper), ARTIFACTS / 'ceiling-mcp.log', headless=True)
id = 'gnome-ceiling'
try:
    start = mcp.tool('workspace_start', {'id': id, 'session': {'backend': 'gnome'},
        'acknowledge_hidden_workspace': True, 'open_viewer': False})
    assert start['ok'], start
    mcp.close()
    mcp = None
    command('workspace', 'launch', '--id', id, '--', '/usr/bin/sh', '-c', 'true', expected=False,
        failure='workspace launch exceeds the MCP permission ceiling')
    ws(id, 'launch', '--', '/usr/bin/true')
finally:
    if mcp:
        mcp.close()
    ws(id, 'stop')
# A daemon crash must not leave its private compositor or bus services alive.
id = 'gnome-crash'
ws(id, 'start', '--session', 'gnome', '--ack-hidden-workspace')
status = ws(id, 'status')
children = {status['daemon_pid']}
processes = [tuple(map(int, line.split())) for line in subprocess.check_output(['ps','-eo','pid=,ppid='],text=True).splitlines()]
while True:
    expanded = children | {pid for pid, parent in processes if parent in children}
    if expanded == children:
        break
    children = expanded
os.kill(status['daemon_pid'], signal.SIGKILL)
wait(lambda: all(process_stopped(pid) for pid in children), 'crash-owned process cleanup')
command('workspace', 'cleanup', '--id', id)
print('GNOME native acceptance passed', flush=True)
