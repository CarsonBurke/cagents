#!/usr/bin/python3
"""Actual development-runner/X11 arm. No app database/proof writes or role clearing."""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import select
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import uuid

HERE = Path(__file__).resolve().parent


def digest(path):
    result = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def natural_user():
    # Presence only; no value access, copy, logging, removal or substitution.
    if any(name.startswith('CAGENTS') or name in ('AUTH', 'AGENT_TOKEN', 'SESSION_TOKEN')
           for name in os.environ):
        raise RuntimeError('A genuine hosted User environment is required')


def main():
    natural_user()  # Before argparse paths, temp roots, SDK, DBus or fixture setup.
    if sys.flags.optimize:
        raise RuntimeError('Optimized Python is forbidden')
    parser = argparse.ArgumentParser()
    parser.add_argument('--arm', choices=['before', 'after'], required=True)
    parser.add_argument('--source', required=True)
    parser.add_argument('--tree', required=True)
    parser.add_argument('--checkout', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--target', required=True)
    args = parser.parse_args()
    checkout = Path(args.checkout).resolve(strict=True)
    output = Path(args.output).resolve(strict=True)
    target = Path(args.target).resolve()
    assert Path.cwd() == checkout
    os.umask(0o077)
    root = Path(tempfile.mkdtemp(prefix='native1757-', dir='/tmp')).resolve()
    assert root.stat().st_uid == os.getuid() and stat.S_IMODE(root.stat().st_mode) == 0o700
    slot = 'native1757-' + args.arm
    nonce = str(uuid.uuid4())
    save(root / '.native1757-owned.json', {'schema': 1, 'purpose': '1757-hosted-native',
         'root': str(root), 'uid': os.getuid(), 'nonce': nonce, 'window': slot})
    save(output / 'profile.json', {'root': str(root), 'window': slot, 'nonce': nonce,
         'backend': 'X11/Xvfb; no Wayland compositor or popup qualification',
         'home': 'Inherited, never assigned', 'commercial_provider': False})
    for name in ['runtime', 'config', 'data', 'cache', 'state', 'tmp', 'account', 'project', 'bin']:
        (root / name).mkdir(mode=0o700)
    logs = output / 'logs'
    logs.mkdir(mode=0o700)
    # These explicit display/build/fixture settings preserve the normal hosted
    # environment and HOME. No role/context variable is removed or rewritten.
    for name, value in [('XDG_RUNTIME_DIR', root / 'runtime'), ('XDG_CONFIG_HOME', root / 'config'),
                        ('XDG_DATA_HOME', root / 'data'), ('XDG_CACHE_HOME', root / 'cache'),
                        ('XDG_STATE_HOME', root / 'state'), ('TMPDIR', root / 'tmp')]:
        os.environ[name] = str(value)
    os.environ['CARGO_BUILD_JOBS'] = '2'
    os.environ['CARGO_TARGET_DIR'] = str(target)
    os.environ['RUSTUP_TOOLCHAIN'] = '1.93.0'
    os.environ['LIBGL_ALWAYS_SOFTWARE'] = '1'
    os.environ['WGPU_BACKEND'] = 'gl'
    os.environ['WINIT_UNIX_BACKEND'] = 'x11'
    os.environ['XDG_SESSION_TYPE'] = 'x11'
    os.environ['GSETTINGS_BACKEND'] = 'memory'
    os.environ['GTK_THEME'] = 'Adwaita:dark'
    os.environ['CLAUDE_CONFIG_DIR'] = str(root / 'account')
    toolchain = {}
    for name in ['cargo', 'rustc', 'rustdoc']:
        path = Path(subprocess.check_output(['rustup', 'which', '--toolchain', '1.93.0', name],
                                          text=True, timeout=5).strip()).resolve(strict=True)
        toolchain[name] = {'path': str(path), 'sha256': digest(path)}
    os.environ['PATH'] = str(root / 'bin') + ':' + str(Path(toolchain['cargo']['path']).parent) + ':' + os.environ['PATH']
    os.environ['RUSTC'] = toolchain['rustc']['path']
    os.environ['RUSTDOC'] = toolchain['rustdoc']['path']
    os.environ['RUSTC_WRAPPER'] = ''
    os.environ['RUSTC_WORKSPACE_WRAPPER'] = ''
    save(output / 'toolchain.json', toolchain)
    shutil.copy2(HERE / 'claude-fixture.py', root / 'bin/claude')
    (root / 'bin/claude').chmod(0o700)
    for name in ['codex', 'omp']:
        (root / 'bin' / name).write_text('#!/bin/sh\nprintf "Offline fixture: unavailable\\n" >&2\nexit 69\n')
        (root / 'bin' / name).chmod(0o700)
    for name in ['claude', 'codex', 'omp']:
        assert Path(shutil.which(name)).resolve() == root / 'bin' / name
    shell = pwd.getpwuid(os.getuid()).pw_shell
    assert Path(shell).name == 'bash', 'Hosted recipe requires its normal bash login shell'
    lookup = subprocess.run([shell, '-i', '-c', 'command -v claude; command -v codex; command -v omp'],
                            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
    save(output / 'provider-lookup.json', {'shell': shell, 'exit': lookup.returncode,
         'stdout': lookup.stdout, 'stderr': lookup.stderr})
    assert lookup.returncode == 0 and lookup.stdout.splitlines() == [str(root / 'bin' / n) for n in ['claude', 'codex', 'omp']]
    events = []
    children = []
    index = 0
    product = None
    writer = None
    window = None
    artifacts = None
    daemon_pin = None
    daemon_fd = None

    def event(kind, **fields):
        events.append({'monotonic': time.monotonic(), 'kind': kind, **fields})
        save(output / 'events.json', events)

    def command(argv, timeout=15, check=True):
        nonlocal index
        index += 1
        prefix = logs / f'command-{index:04d}'
        with prefix.with_suffix('.out').open('wb') as out, prefix.with_suffix('.err').open('wb') as err:
            result = subprocess.run([str(x) for x in argv], stdout=out, stderr=err, timeout=timeout)
        event('command', argv=[str(x) for x in argv], exit=result.returncode, log=str(prefix))
        if check and result.returncode:
            raise RuntimeError('Native command failed; see raw command log')
        return prefix.with_suffix('.out').read_text()

    def spawn(argv, name, pipe=False):
        log = (logs / (name + '.log')).open('wb')
        child = subprocess.Popen([str(x) for x in argv], stdin=subprocess.PIPE if pipe else subprocess.DEVNULL,
                                 stdout=log, stderr=log)
        log.close()
        fd = os.pidfd_open(child.pid)
        children.append((child, fd, name))
        event('spawn', argv=[str(x) for x in argv], pid=child.pid, log=str(logs / (name + '.log')))
        return child

    def wait(predicate, label, seconds=30):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            value = predicate()
            if value:
                return value
            time.sleep(0.15)
        raise RuntimeError('Native condition timed out: ' + label)

    def source():
        current = {key: command(['git', 'rev-parse', value]).strip()
                   for key, value in [('head', 'HEAD'), ('tree', 'HEAD^{tree}')]}
        current['status'] = command(['git', 'status', '--porcelain']).strip()
        assert current == {'head': args.source, 'tree': args.tree, 'status': ''}
        return current

    def at(label):
        raw = command(['/usr/bin/python3', HERE / 'read-at.py'], timeout=12)
        value = json.loads(raw)
        save(output / (label + '.at.json'), value)
        assert value['bounded'] is False, 'Truncated accessibility evidence cannot qualify'
        return value['nodes']

    def visible(row):
        x, y, w, h = row.get('bounds', [-1, -1, 0, 0])
        return w > 0 and h > 0 and 0 <= x < 1278 and 0 <= y < 800

    def rows(name, label):
        return [r for r in at(label) if r.get('name') == name and visible(r)]

    def click(name, label):
        found = wait(lambda: rows(name, label), name)
        assert len(found) == 1, 'Ambiguous native target'
        x, y, w, h = found[0]['bounds']
        command(['xdotool', 'mousemove', '--sync', str(x + w // 2), str(y + h // 2), 'click', '1'])

    def entry(label):
        def find():
            found = [r for r in at(label) if r['role'] == 'entry' and r['name'] == 'Message' and visible(r)]
            return found[0] if len(found) == 1 else None
        return wait(find, 'one visible native composer')

    def identities(label):
        database = root / 'data/cagents-development/windows' / slot / 'sessions.sqlite3'
        assert database.resolve(strict=True).is_relative_to(root)
        # SELECT-only observation of this newly generated synthetic Store.
        with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=2) as connection:
            cards = []
            for pane, generation, payload in connection.execute(
                    "SELECT card_id, generation, content_json FROM cards WHERE kind_tag='chat'"):
                content = json.loads(payload) if payload else {}
                launch = content.get('launch') or {}
                cards.append({'pane_id': pane, 'generation': generation,
                              'app_session_id': content.get('id'),
                              'native_session_id': launch.get('session_id'),
                              'provider': launch.get('provider')})
        save(output / (label + '.identity.json'), {'cards': cards, 'select_only': True})
        return cards

    def type_text(text):
        command(['xdotool', 'type', '--clearmodifiers', '--delay', '15', text])

    def key(value):
        command(['xdotool', 'key', '--clearmodifiers', value])

    def capture(name, viewport=(1278, 800)):
        png = output / (name + '.png')
        command(['import', '-window', str(window), png])
        from PIL import Image
        with Image.open(png) as image:
            assert image.size == viewport, 'Matching client viewport required'
            image.save(output / (name + '.webp'), lossless=True)
        event('capture', name=name, png_sha256=digest(png), webp_sha256=digest(output / (name + '.webp')))

    def starts():
        p = root / 'provider-starts.jsonl'
        return [] if not p.exists() else [json.loads(line) for line in p.read_text().splitlines()]

    def protocol_done(name):
        path = root / 'protocol-states.jsonl'
        return path.is_file() and any(row.get('fixture_command') == name and row.get('state') == 'completed'
                                     for row in (json.loads(line) for line in path.read_text().splitlines()))

    def daemon_alive(label):
        assert daemon_fd is not None and daemon_pin is not None
        assert not select.select([daemon_fd], [], [], 0)[0], 'Original daemon exited'
        fields = Path(f'/proc/{daemon_pin["pid"]}/stat').read_text().rsplit(')', 1)[1].split()
        assert int(fields[19]) == daemon_pin['start_ticks'] and fields[0] != 'Z'
        event('original-daemon-live', label=label, identity=daemon_pin)

    def bind(label):
        nonlocal artifacts, daemon_pin, daemon_fd
        result = {}
        for name in ['cagents', 'cagentsd', 'cagents-ui']:
            p = target / 'dev-runtime' / name
            version = command([p, '--version']).strip()
            assert '+local.' + command(['git', 'rev-parse', '--short', 'HEAD']).strip() in version and '.dirty' not in version
            result[name] = {'path': str(p), 'sha256': digest(p), 'version': version,
                            'device': p.stat().st_dev, 'inode': p.stat().st_ino}
        if artifacts is not None:
            assert {n: v['sha256'] for n, v in artifacts.items()} == {n: v['sha256'] for n, v in result.items()}
        artifacts = result
        # Read-only exact executable inventory, never PPID signal authority.
        running = []
        for p in Path('/proc').iterdir():
            if not p.name.isdecimal():
                continue
            try:
                executable = Path(os.readlink(p / 'exe').removesuffix(' (deleted)'))
                for name in ['cagentsd', 'cagents-ui']:
                    if executable == Path(result[name]['path']):
                        before = (p / 'stat').read_text().rsplit(')', 1)[1].split()
                        identity = (p / 'exe').stat()
                        actual_hash = digest(p / 'exe')
                        after = (p / 'stat').read_text().rsplit(')', 1)[1].split()
                        assert before[19] == after[19] and after[0] != 'Z'
                        assert actual_hash == result[name]['sha256']
                        running.append({'pid': int(p.name), 'name': name, 'sha256': actual_hash,
                                        'start_ticks': int(after[19]), 'device': identity.st_dev,
                                        'inode': identity.st_ino})
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
        assert sum(r['name'] == 'cagentsd' for r in running) == 1
        assert sum(r['name'] == 'cagents-ui' for r in running) == 1
        current_daemon = next(r for r in running if r['name'] == 'cagentsd')
        if daemon_pin is None:
            daemon_pin = current_daemon
            # Read-only liveness pin, never individual-signal authority.
            daemon_fd = os.pidfd_open(daemon_pin['pid'])
        else:
            assert current_daemon == daemon_pin, 'A replacement daemon cannot qualify UI-only reopen'
        daemon_alive(label)
        xpid = int(command(['xdotool', 'getwindowpid', str(window)]).strip())
        assert any(r['name'] == 'cagents-ui' and r['pid'] == xpid for r in running)
        destination = output / ('matched-trio-' + label + '.json')
        assert not destination.exists(), 'Bindings are immutable per phase'
        save(destination, {'source': source(), 'artifacts': artifacts, 'running': running,
                           'original_daemon': daemon_pin, 'original_daemon_pidfd_live': True})

    try:
        save(output / 'source-before.json', source())
        command(['git', '-C', root / 'project', 'init', '--initial-branch=main'])
        (root / 'project/README.txt').write_text('Synthetic native first-send validation project.\n')
        command(['git', '-C', root / 'project', 'add', 'README.txt'])
        # Only this synthetic fixture repository is written; no app/proof seed.
        command(['env', 'GIT_AUTHOR_DATE=2000-01-01T00:00:00+00:00', 'GIT_COMMITTER_DATE=2000-01-01T00:00:00+00:00',
                 'git', '-C', root / 'project', '-c', 'user.name=Native Fixture', '-c', 'user.email=fixture@example.test',
                 'commit', '-m', 'Initialize synthetic first-send fixture'])
        assert command(['git', '-C', root / 'project', 'rev-parse', 'HEAD']).strip() == '6ac6f24869871ac96fc56ad89f553a2b09187d97'
        config = root / 'bus.conf'
        config.write_text('<busconfig><type>session</type><listen>unix:path=' + str(root / 'runtime/bus') +
                          '</listen><policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>' +
                          '<allow own="*"/><allow user="' + str(os.getuid()) + '"/></policy></busconfig>')
        spawn(['dbus-daemon', '--nofork', '--config-file=' + str(config)], 'dbus')
        wait(lambda: (root / 'runtime/bus').is_socket(), 'private session bus')
        os.environ['DBUS_SESSION_BUS_ADDRESS'] = 'unix:path=' + str(root / 'runtime/bus')
        authority = root / 'Xauthority'
        authority.touch(mode=0o600)
        cookie = secrets.token_hex(16)
        subprocess.run(['xauth', '-f', str(authority)], input='add :0 . ' + cookie + '\n',
                       text=True, stdout=subprocess.DEVNULL, check=True, timeout=5)
        os.environ['XAUTHORITY'] = str(authority)
        display_file = root / 'display'
        with display_file.open('wb') as display:
            xvfb = subprocess.Popen(['Xvfb', '-displayfd', str(display.fileno()), '-screen', '0', '1278x800x24',
                                     '-nolisten', 'tcp', '-noreset', '-auth', str(authority), '-dpi', '96'], pass_fds=(display.fileno(),),
                                    stdout=(logs / 'xvfb.log').open('wb'), stderr=subprocess.STDOUT)
        children.append((xvfb, os.pidfd_open(xvfb.pid), 'xvfb'))
        number = wait(lambda: display_file.read_text().strip(), 'Xvfb display')
        assert number.isdecimal()
        os.environ['DISPLAY'] = ':' + number
        subprocess.run(['xauth', '-f', str(authority)], input='add :' + number + ' . ' + cookie + '\n',
                       text=True, stdout=subprocess.DEVNULL, check=True, timeout=5)
        # No inherited Wayland endpoint may silently override this X11 arm.
        assert 'WAYLAND_DISPLAY' not in os.environ and 'WAYLAND_SOCKET' not in os.environ
        spawn(['openbox', '--config-file', str(HERE / 'openbox.xml')], 'openbox')
        launcher = next((p for p in [Path('/usr/libexec/at-spi-bus-launcher'), Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')]
                         if p.is_file()), None)
        registry = next((p for p in [Path('/usr/libexec/at-spi2-registryd'), Path('/usr/lib/at-spi2-core/at-spi2-registryd')]
                         if p.is_file()), None)
        assert launcher and registry
        spawn([launcher, '--launch-immediately'], 'atspi-launcher')
        address_raw = wait(lambda: command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus',
                           '--object-path', '/org/a11y/bus', '--method', 'org.a11y.Bus.GetAddress'], check=False).strip(), 'private AT bus')
        address = ast.literal_eval(address_raw)[0]
        match = re.fullmatch(r'unix:path=([^,;]+)(?:,guid=[0-9a-f]+)?', address)
        assert match and Path(match[1]).resolve(strict=True).is_relative_to(root)
        os.environ['AT_SPI_BUS_ADDRESS'] = address
        spawn([registry], 'atspi-registry')
        for prop in ['IsEnabled', 'ScreenReaderEnabled']:
            command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
                     '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', prop, '<true>'])
        product = spawn([checkout / 'scripts/run-development', '--browserless', '--debug', '--log-file', logs / 'ui.log',
                         '--', '--window', slot], 'development-runner')
        windows = wait(lambda: command(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], check=False).splitlines(),
                       'actual development window after matched builds', 2700)
        assert len(windows) == 1
        window = int(windows[0])
        command(['xdotool', 'windowsize', '--sync', str(window), '1278', '800'])
        command(['xdotool', 'windowmove', '--sync', str(window), '0', '0'])
        command(['xdotool', 'windowactivate', '--sync', str(window)])
        bind('initial')
        cli = target / 'dev-runtime/cagents'
        linked = json.loads(command([cli, '--no-start', '--device', 'local', '--json', 'account', 'link',
                                     'claude', root / 'account'], timeout=30))
        assert linked['ok'] is True and linked['command'] == 'account.link'
        account = linked['data']['account']
        assert re.fullmatch(r'claude/[0-9a-f-]{36}', account)
        command([cli, '--no-start', '--device', 'local', '--json', 'account', 'default', 'claude', account], timeout=30)
        command([cli, '--no-start', '--device', 'local', '--json', 'project', 'open', root / 'project'], timeout=30)
        command([cli, '--no-start', '--device', 'local', '--json', 'channel', 'spawn', '--repository', root / 'project',
                 '--title', 'First send fixture'], timeout=30)
        # External CreateChannel has no panes. The ordinary first click owns
        # creating its default native draft; do not assume a preexisting chat.
        channel = wait(lambda: [r for r in at('fresh-empty-channel') if r['role'] == 'button' and visible(r)
                       and r['name'].startswith('Open channel First send fixture,')], 'fresh channel row')
        assert len(channel) == 1
        click(channel[0]['name'], 'channel-open')
        # Collapse the inspector using its ordinary shortcut, only if mounted.
        nodes = at('channel-state')
        if any(r['name'] == 'Close inspector' for r in nodes):
            click('Close inspector', 'close-inspector')
        click('Choose a model', 'model-open')
        click('Claude default model', 'model-claude')
        click('Message', 'first-compose')
        capture('first-send-empty')
        type_text('FIRST_SEND_1757')
        key('Return')
        wait(lambda: (root / 'accepted-first-send.json').exists(), 'first SDK acceptance')
        assert len(starts()) == 1
        time.sleep(2)
        capture('first-send-accepted')
        identities('first-accepted-before-late-id')
        (root / 'release-native-identity').touch()
        wait(lambda: protocol_done('first-send') and any(r.get('name') == 'First-send fixture complete.'
                                                       for r in at('first-completion')),
             'late native identity and completion')
        capture('first-send-completed')
        key('ctrl+t')
        wait(lambda: entry('second-draft').get('text') == '', 'second native draft')
        click('Choose a model', 'recovery-model-open')
        click('Claude default model', 'recovery-model-claude')
        click('Message', 'recovery-compose-before-failure')
        # Copy an actual synthetic PNG onto this X server's image clipboard.
        from PIL import Image
        image = root / 'synthetic-image.png'
        Image.new('RGB', (64, 64), (24, 160, 96)).save(image)
        # Keep the foreground owner through the completed image insertion,
        # independent of version-specific TARGETS/request loop accounting.
        clipboard = spawn(['xclip', '-selection', 'clipboard', '-t', 'image/png', '-i', image, '-loops', '0', '-quiet'], 'image-clipboard')
        wait(lambda: 'Waiting for selection requests' in (logs / 'image-clipboard.log').read_text(), 'owned image selection ready')
        key('ctrl+v')
        wait(lambda: '[Image 1]' in entry('image-inserted').get('text', ''), 'normal image paste and inline reference')
        clipboard_fd = next(fd for child, fd, name in children if child is clipboard)
        if clipboard.poll() is None:
            signal.pidfd_send_signal(clipboard_fd, signal.SIGTERM)
        clipboard.wait(timeout=5)
        event('owned-image-selection-released-after-paste', png_sha256=digest(image), exit=clipboard.returncode)
        type_text('UNSENT_SAVE_RECOVERY_1757')
        draft_before = entry('draft-before-failure')['text']
        assert '[Image 1]' in draft_before and 'UNSENT_SAVE_RECOVERY_1757' in draft_before
        database = root / 'data/cagents-development/windows' / slot / 'sessions.sqlite3'
        wait(database.is_file, 'actual generated named window Store')
        def settled_cards():
            value = identities('before-failure')
            first_session = json.loads((root / 'accepted-first-send.json').read_text())['session']
            return value if len(value) == 2 and sum(r['native_session_id'] == first_session for r in value) == 1 else None
        cards_before = wait(settled_cards, 'both ordinary panes persisted before writer reservation')
        first_sdk_session = json.loads((root / 'accepted-first-send.json').read_text())['session']
        original_actor = next(r for r in cards_before if r['native_session_id'] == first_sdk_session)
        original_draft = next(r for r in cards_before if r['pane_id'] != original_actor['pane_id'])
        assert original_draft['app_session_id'] and original_draft['native_session_id'] is None
        identity = database.stat()
        writer = spawn(['/usr/bin/python3', HERE / 'hold-store-lock.py', '--root', root, '--database', database,
                        '--device', str(identity.st_dev), '--inode', str(identity.st_ino), '--nonce', nonce], 'writer-lock', pipe=True)
        wait(lambda: '"ready": true' in (logs / 'writer-lock.log').read_text(), 'real transaction-only write reservation')
        assert len(starts()) == 1
        key('Return')
        wait(lambda: any(r.get('name') == 'Starting' for r in at('saving-state')), 'actual Starting state', 10)
        capture('first-save-starting')
        # Pointer motion only: it must not change keyboard ownership after error.
        # Video/screenshots preserve actual hover response; no inferred FPS claim.
        hover = rows('Open settings — Ctrl+Comma', 'hover-target')
        assert len(hover) == 1
        x, y, w, h = hover[0]['bounds']
        video = spawn(['ffmpeg', '-nostdin', '-loglevel', 'warning', '-y', '-f', 'x11grab', '-framerate', '15',
                       '-video_size', '1278x800', '-i', os.environ['DISPLAY'], '-t', '6', '-an',
                       '-c:v', 'libvpx-vp9', '-deadline', 'realtime', output / 'writer-wait-hover.webm'], 'hover-video')
        for point in [(768, 400), (x + w // 2, y + h // 2), (768, 400), (x + w // 2, y + h // 2)]:
            event('hover-input', coordinates=point)
            command(['xdotool', 'mousemove', '--sync', str(point[0]), str(point[1])])
            time.sleep(0.7)
        video.wait(timeout=10)
        assert video.returncode == 0
        wait(lambda: any('The conversation could not be saved, so nothing was started.' in r.get('name', '')
                         for r in at('failed-save')), 'real writer refusal', 45)
        capture('first-save-error')
        before_typing = entry('error-before-typing')
        assert before_typing['text'] == draft_before and len(starts()) == 1
        cards_failed = identities('after-real-writer-failure')
        assert {r['pane_id']: (r['app_session_id'], r['native_session_id']) for r in cards_failed} == {
            r['pane_id']: (r['app_session_id'], r['native_session_id']) for r in cards_before}
        focused = any('FOCUSED' in state for state in before_typing['states'])
        # Deliberately no click/focus operation between refusal and these keys.
        type_text('_FOCUS_RETURNED')
        time.sleep(0.3)
        typed = entry('error-after-no-click-typing')
        expected = draft_before + '_FOCUS_RETURNED'
        save(output / 'no-click-typing.json', {'draft_before': draft_before, 'focused_after_failure': focused,
             'typed': typed, 'expected': expected, 'provider_starts_before_retry': len(starts()),
             'same_draft_text_and_image_reference': typed['text'] == expected and '[Image 1]' in typed['text']})
        capture('first-save-no-click-typing')
        if args.arm == 'after':
            assert focused and typed['text'] == expected and len(starts()) == 1
        elif typed['text'] != expected:
            # Baseline recovery requires the visible extra user action. It does
            # not become after-arm no-click evidence or a waived after failure.
            click('Message', 'baseline-manual-focus-recovery')
            type_text('_FOCUS_RETURNED')
            assert entry('baseline-repaired-text')['text'] == expected
        if writer.poll() is None:
            writer.stdin.write(b'release\n')
            writer.stdin.flush()
        writer.wait(timeout=5)
        assert writer.returncode == 0 and '"released": true' in (logs / 'writer-lock.log').read_text()
        event('cause-repaired', store_device=identity.st_dev, store_inode=identity.st_ino)
        key('Return')  # Explicit same-draft retry, after real transaction release.
        accepted = wait(lambda: (root / 'accepted-recovery.json').exists(), 'recovered SDK acceptance')
        receipt = json.loads((root / 'accepted-recovery.json').read_text())
        assert receipt['images'] == 1 and receipt['image_sha256'] == digest(image) and len(starts()) == 2
        assert receipt['session'] != json.loads((root / 'accepted-first-send.json').read_text())['session']
        capture('retry-accepted')
        identities('retry-accepted-before-late-id')
        (root / 'release-recovery-native-identity').touch()
        wait(lambda: protocol_done('recovery') and any(r.get('name') == 'First-send fixture complete.'
                                                     for r in at('retry-completion')),
             'recovered completion')
        assert len(starts()) == 2
        capture('retry-completed')
        command(['xdotool', 'windowsize', '--sync', str(window), '900', '650'])
        time.sleep(0.4)
        capture('retry-completed-narrow', (900, 650))
        command(['xdotool', 'windowsize', '--sync', str(window), '1278', '800'])
        def completed_cards():
            value = identities('after-retry')
            return value if any(r['pane_id'] == original_draft['pane_id'] and r['native_session_id'] == receipt['session']
                                for r in value) else None
        cards_after = wait(completed_cards, 'recovered native identity persisted in its original pane')
        assert {r['pane_id'] for r in cards_before} == {r['pane_id'] for r in cards_after}
        retained_first = next(r for r in cards_after if r['pane_id'] == original_actor['pane_id'])
        assert (retained_first['app_session_id'], retained_first['native_session_id']) == (
            original_actor['app_session_id'], original_actor['native_session_id'])
        recovered_actor = next(r for r in cards_after if r['pane_id'] == original_draft['pane_id'])
        # The legitimate first-start checkpoint deliberately changes source A
        # to candidate B; failed B is private memory and not claimed observed.
        assert recovered_actor['app_session_id'] != original_draft['app_session_id']
        bind('before-ui-close')
        # Normal UI close, zero-window observation, same-source normal runner.
        key('alt+F4')
        product.wait(timeout=20)
        assert product.returncode == 0
        assert not command(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], check=False).strip()
        daemon_alive('zero-ui-windows')
        starts_before_reopen = starts()
        product = spawn([checkout / 'scripts/run-development', '--browserless', '--debug', '--log-file', logs / 'ui-reopen.log',
                         '--', '--window', slot], 'development-runner-reopen')
        windows = wait(lambda: command(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], check=False).splitlines(),
                       'same daemon UI-only reopen', 300)
        assert len(windows) == 1
        window = int(windows[0])
        command(['xdotool', 'windowsize', '--sync', str(window), '1278', '800'])
        command(['xdotool', 'windowactivate', '--sync', str(window)])
        bind('after-ui-reopen')
        wait(lambda: any('UNSENT_SAVE_RECOVERY_1757_FOCUS_RETURNED' in r.get('name', '') or
                         'UNSENT_SAVE_RECOVERY_1757_FOCUS_RETURNED' in r.get('text', '')
                         for r in at('ui-reopened')), 'original recovered transcript after UI-only reopen')
        assert starts() == starts_before_reopen
        reopened_cards = identities('after-ui-reopen')
        assert {r['pane_id']: (r['app_session_id'], r['native_session_id']) for r in reopened_cards} == {
            r['pane_id']: (r['app_session_id'], r['native_session_id']) for r in cards_after}
        capture('ui-only-reopened')
        save(output / 'source-after.json', source())
        save(output / 'RESULT.json', {'status': 'FLOW_ASSERTIONS_PASS_PENDING_TEARDOWN', 'arm': args.arm, 'head': args.source, 'tree': args.tree,
             'sdk_starts': starts(), 'accepted_first': json.loads((root / 'accepted-first-send.json').read_text()),
             'accepted_recovery': receipt, 'failed_save_started_provider': False, 'retry_starts': 1,
             'identity_scope': 'SELECT-only persisted cards, not open inventory or private failed candidate B',
             'source_A': original_draft, 'successful_candidate_B': recovered_actor,
             'private_failed_candidate_reuse': 'Source/owning R3 regression only, not directly native-observed',
             'hover': 'Raw native6second video and input timestamps; independent responsiveness review pending',
             'backend': 'X11 only; no Wayland popup claim', 'native_review': 'PENDING'})
    finally:
        # Ordinary private UI and daemon controls first. No installed socket or
        # authority override; failures remain failures for the outer bb1 guardian.
        if writer is not None and writer.poll() is None:
            try:
                writer.stdin.write(b'release\n')
                writer.stdin.flush()
                writer.wait(timeout=5)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                pass
        if window is not None and product is not None and product.poll() is None:
            try:
                command(['xdotool', 'windowactivate', '--sync', str(window)], check=False)
                command(['xdotool', 'key', '--clearmodifiers', 'alt+F4'], check=False)
                product.wait(timeout=15)
            except (subprocess.TimeoutExpired, RuntimeError):
                pass
        if artifacts is not None:
            command([target / 'dev-runtime/cagents', '--no-start', '--device', 'local', 'daemon', 'stop-for-rebuild', '--including-subagents'], timeout=15, check=False)
        for child, fd, name in reversed(children):
            if child.poll() is None and name in ['dbus', 'xvfb', 'openbox', 'atspi-launcher', 'atspi-registry', 'image-clipboard']:
                signal.pidfd_send_signal(fd, signal.SIGTERM)
                event('owned-infrastructure-termination', name=name, pid=child.pid)
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                event('unsettled-direct-child', name=name, pid=child.pid)
                raise RuntimeError('Owned child did not settle; outer guardian must retain failure')
            finally:
                os.close(fd)
        if root.exists():
            # Retain declared synthetic SDK/log evidence, never copy app
            # credentials, runtime sockets or an ambient/home profile.
            for name in ['provider-starts.jsonl', 'protocol-states.jsonl', 'accepted-first-send.json', 'accepted-recovery.json']:
                if (root / name).is_file():
                    shutil.copy2(root / name, output / name)
        save(output / 'final-source.json', source())
        if daemon_fd is not None:
            os.close(daemon_fd)
    result = json.loads((output / 'RESULT.json').read_text())
    final_starts = starts()
    save(output / 'sdk-starts-after-teardown.json', final_starts)
    assert final_starts == starts_before_reopen and len(final_starts) == 2, 'Late SDK invocation cannot qualify after teardown'
    result['sdk_starts_after_teardown'] = final_starts
    result['status'] = 'PASS'
    save(output / 'RESULT.json', result)


if __name__ == '__main__':
    main()
