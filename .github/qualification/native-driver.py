"""Temporary hosted-X11 actual native evidence; never production-provider evidence."""
import ast
import ctypes
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time

import pyatspi
from gi.repository import GLib
from PIL import Image

control, before, after = map(Path, sys.argv[1:4])
out = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'native'
if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or 'CAGENTS_AGENT_AUTHORITY' in os.environ:
    raise RuntimeError('A genuine GitHub-hosted User context is required; inherited authority is unchanged')
if os.environ.get('WAYLAND_DISPLAY') or os.environ.get('WAYLAND_SOCKET'):
    raise RuntimeError('Hosted X11 qualification refuses inherited Wayland connections')
out.mkdir(parents=True, exist_ok=True)
target = Path(os.environ['CARGO_TARGET_DIR'])
source = control / '.github/qualification/native-fixture.rs'
base = Path(tempfile.mkdtemp(prefix='hs.', dir='/tmp'))
os.chmod(base, 0o700)
receipts = []
children = []
stopped = set()
viewport = (1280, 800)
stale_accessibility_retries = 0


def cancelled(signum, frame):
    raise RuntimeError('Native qualification cancelled; closing owned descendants')


signal.signal(signal.SIGTERM, cancelled)
# Retain orphaned descendants so escaped process groups remain observable and
# can be closed before the shared target is used by another phase.
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
    raise OSError(ctypes.get_errno(), 'Cannot establish owned descendant cleanup')


def run(args, **kwargs):
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise RuntimeError('Inherited caller authority is present; native command refused')
    timeout = kwargs.pop('timeout', 20)
    process = subprocess.Popen(args, start_new_session=True, **kwargs)
    try:
        code = process.wait(timeout=timeout)
    except BaseException:
        stop(process)
        raise
    if code:
        stop(process)
        raise subprocess.CalledProcessError(code, args)
    stop(process)
    return subprocess.CompletedProcess(args, code)


def wait(predicate, timeout=30):
    global stale_accessibility_retries
    end = time.monotonic() + timeout
    last_stale = None
    while time.monotonic() < end:
        try:
            result = predicate()
        except GLib.Error as error:
            if 'Unknown object' not in str(error):
                raise
            last_stale = error
            stale_accessibility_retries += 1
            result = None
        if result:
            return result
        time.sleep(.15)
    raise RuntimeError('Native condition did not settle within its deadline') from last_stale


def accessibility_snapshot(query):
    # An accessible can disappear between traversal and property lookup during
    # a genuine frame update. Retry the whole read, never an input gesture.
    return wait(lambda: (query(),))[0]


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def tree(pid):
    desktop = pyatspi.Registry.getDesktop(0)
    apps = [desktop.getChildAtIndex(i) for i in range(desktop.childCount)]
    apps = [app for app in apps if app.get_process_id() == pid]
    if len(apps) != 1:
        return []
    pending, result = [apps[0]], []
    while pending:
        node = pending.pop()
        result.append(node)
        assert len(result) < 10000 and node.childCount < 1000
        pending.extend(node.getChildAtIndex(i) for i in range(node.childCount))
    return result


def visible(node):
    return node.getState().contains(pyatspi.STATE_SHOWING)


def live_tree(pid):
    os.kill(pid, 0)
    nodes = tree(pid)
    if not nodes:
        raise RuntimeError('The native application accessibility tree disappeared')
    return nodes


def capture(window, name, size):
    global viewport
    run(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
    run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
    viewport = size
    time.sleep(.6)
    png = out / (name + '.png')
    def rendered_frame():
        run(['import', '-screen', '-window', window, str(png)])
        with Image.open(png) as picture:
            assert picture.size == size, (picture.size, size)
            rgb = picture.convert('RGB')
            histogram = rgb.convert('L').histogram()
            levels = [level for level, count in enumerate(histogram) if count]
            lit = sum(histogram[9:]) / (picture.width * picture.height)
            diverse = rgb.getcolors(maxcolors=15) is None
            metrics = {'size': list(picture.size), 'pixels_above_intensity_8_fraction': lit,
                       'luminance_range': max(levels) - min(levels), 'at_least_16_colors': diverse,
                       'backend_requested': 'tiny-skia', 'capture': 'visible screen pixels for owned window'}
            (out / (name + '-frame.json')).write_text(json.dumps(metrics, indent=2) + '\n')
            return diverse and lit > .01 and metrics['luminance_range'] >= 16
    wait(rendered_frame, timeout=20)
    if name.endswith('-wide'):
        run(['import', '-window', window, str(out / (name + '-drawable.png'))])
    with Image.open(png) as picture:
        assert picture.size == size, (picture.size, size)
        picture.save(out / (name + '.webp'), lossless=True)


def click(pid, name, keyboard=False):
    def find():
        nodes = [node for node in tree(pid) if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                 and node.name == name and visible(node)
                 and node.getState().contains(pyatspi.STATE_ENABLED)]
        return nodes[0] if len(nodes) == 1 else None
    node = wait(find)
    if keyboard:
        assert node.queryComponent().grabFocus(), 'Recovery control refused keyboard focus'
        wait(lambda: node.getState().contains(pyatspi.STATE_FOCUSED))
        run(['xdotool', 'key', 'Return'])
        return
    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    assert bounds.width > 0 and bounds.height > 0
    assert 0 <= bounds.x + bounds.width // 2 < viewport[0]
    assert 0 <= bounds.y + bounds.height // 2 < viewport[1]
    run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2),
         str(bounds.y + bounds.height // 2), 'click', '1'])


def copy_exact(pid, name, expected, keyboard=False):
    sentinel = b'held-scope-native-copy-sentinel'
    assert expected != sentinel
    owner = subprocess.Popen(['xclip', '-selection', 'clipboard', '-in', '-quiet'],
                             stdin=subprocess.PIPE, start_new_session=True)
    children.append(owner)
    owner.stdin.write(sentinel)
    owner.stdin.close()
    def clipboard():
        result = subprocess.run(['xclip', '-selection', 'clipboard', '-out'],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
        return result.stdout if result.returncode == 0 else None
    wait(lambda: clipboard() == sentinel)
    click(pid, name, keyboard=keyboard)
    wait(lambda: clipboard() == expected)


def open_ordinary_history(pid):
    plus = wait(lambda: next((node for node in live_tree(pid)
                             if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                             and node.name.startswith('New tab · Thread ')
                             and visible(node)), None))
    click(pid, plus.name)
    click(pid, 'New Resume conversation tab')
    wait(lambda: any(node.name == 'Resume Ordinary retained conversation' and visible(node)
                     for node in live_tree(pid)))


def processes():
    result = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            result[int(path.parent.name)] = (int(fields[1]), int(fields[2]), fields[0], fields[19])
        except (FileNotFoundError, ProcessLookupError):
            continue
    return result


def descendants(table, roots):
    found = set(roots)
    while True:
        more = {pid for pid, row in table.items() if row[0] in found} - found
        if not more:
            return found
        found.update(more)


def close_family(roots, groups=()):
    tracked = {}
    deadline = time.monotonic() + 15
    while True:
        table = processes()
        owned = descendants(table, roots)
        owned.update(pid for pid, row in table.items() if row[1] in groups)
        for pid in owned:
            if pid in table and pid != os.getpid():
                tracked.setdefault(pid, table[pid][3])
        live = [pid for pid, stamp in tracked.items()
                if pid in table and table[pid][3] == stamp and table[pid][2] not in {'Z', 'X'}]
        if not live:
            return
        if time.monotonic() >= deadline:
            raise RuntimeError(f'Owned process family did not close: {live}')
        for pid in live:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(.05)


def stop(process):
    if process.pid in stopped:
        return
    table = processes()
    family = descendants(table, {process.pid})
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    if process.poll() is None:
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    close_family(family, {process.pid})
    stopped.add(process.pid)


def focus_failed_pane(pid):
    node = wait(lambda: next((node for node in live_tree(pid)
                             if node.name == 'Saved pane unavailable' and visible(node)), None))
    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2),
         str(bounds.y + bounds.height // 2), 'click', '1'])


def resource_sample(pid):
    root = Path('/proc') / str(pid)
    fields = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    main = (root / 'task' / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    return {'monotonic': time.monotonic(), 'unix_ns': time.time_ns(), 'pid': pid,
            'cpu_ticks': int(fields[11]) + int(fields[12]),
            'main_cpu_ticks': int(main[11]) + int(main[12]),
            'status': (root / 'status').read_text(),
            'smaps_rollup': (root / 'smaps_rollup').read_text()}


def responsiveness(window, pid, fixture, root, label, helper):
    node = wait(lambda: next((node for node in live_tree(pid)
                             if node.name == 'Attention' and node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                             and visible(node)), None))
    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    assert 0 <= bounds.x and 0 <= bounds.y and bounds.x + bounds.width <= 1280
    samples = []
    for phase in ['quiet', 'model-updates']:
        if phase == 'model-updates':
            (root / 'performance-stream').touch()
            wait(lambda: 'performance-update-0-' in (root / 'receipts.txt').read_text())
        output = out / (label + '-' + phase + '-pixel-response.jsonl')
        with output.open('w') as log:
            probe = subprocess.Popen([str(helper), window, str(bounds.x), str(bounds.y),
                                      str(bounds.width), str(bounds.height), '60'],
                                     stdout=log, start_new_session=True)
        children.append(probe)
        deadline = time.monotonic() + 40
        while probe.poll() is None or (phase == 'model-updates' and 'performance-stream-complete' not in (root / 'receipts.txt').read_text()):
            assert time.monotonic() < deadline, 'Pixel probe exceeded deadline'
            samples.append({'phase': phase, 'ui': resource_sample(pid), 'fixture': resource_sample(fixture.pid)})
            time.sleep(.5)
        assert probe.returncode == 0, 'Common-control hover calibration or response probe failed'
        rows = [json.loads(line) for line in output.read_text().splitlines()]
        assert rows[0]['calibrated'] and len(rows) == 61
        assert not any(row.get('timeout') for row in rows[1:]), 'Common-chrome response stalled for at least one second'
        if phase == 'model-updates':
            updates = [line.split('-') for line in (root / 'receipts.txt').read_text().splitlines()
                       if line.startswith('performance-update-')]
            assert len(updates) == 150 and [int(row[2]) for row in updates] == list(range(150))
            assert all(int(a[3]) < int(b[3]) and int(a[9]) < int(b[9])
                       for a, b in zip(updates, updates[1:]))
    (out / (label + '-responsiveness-resources.json')).write_text(json.dumps({
        'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
        'synthetic_workload': '200-item healthy transcript, 32 managed registry entries (30 idle peers), 150 channel-title events at nominal 5Hz',
        'scope': 'Actual X11 app input-to-observed-window-pixels; pixel probe covers the first approximately 8 seconds, resources cover the full 150-event stream. No physical presentation/GPU or live-provider claim. Baseline blocks healthy transcript admission.',
        'probe_sha256': digest(helper), 'samples': samples,
    }, indent=2) + '\n')


try:
    launcher = next(path for path in [Path('/usr/libexec/at-spi-bus-launcher'),
                                    Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')] if path.exists())
    accessibility = subprocess.Popen([str(launcher), '--launch-immediately'], start_new_session=True)
    children.append(accessibility)
    for property_name in ['IsEnabled', 'ScreenReaderEnabled']:
        run(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
             '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', property_name, '<true>'])
    bus_reply = subprocess.check_output(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus',
                                        '--object-path', '/org/a11y/bus', '--method', 'org.a11y.Bus.GetAddress'], text=True)
    os.environ['AT_SPI_BUS_ADDRESS'] = ast.literal_eval(bus_reply)[0]
    registry_path = next(path for path in [Path('/usr/libexec/at-spi2-registryd'),
                                         Path('/usr/lib/at-spi2-core/at-spi2-registryd')] if path.exists())
    registry_env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=os.environ['AT_SPI_BUS_ADDRESS'])
    registry = subprocess.Popen([str(registry_path)], env=registry_env, start_new_session=True)
    children.append(registry)
    config = base / 'openbox.xml'
    config.write_text('<openbox_config xmlns="http://openbox.org/3.4/rc"><focus><focusNew>yes</focusNew></focus><applications><application class="*"><decor>no</decor><position force="yes"><x>0</x><y>0</y></position></application></applications></openbox_config>')
    wm = subprocess.Popen(['openbox', '--config-file', str(config)], start_new_session=True)
    children.append(wm)
    pixel_helper = base / 'pixel-response'
    run(['cc', '-O2', '-Wall', '-Wextra', '-Werror', str(control / '.github/qualification/pixel-response.c'),
         '-o', str(pixel_helper), '-lX11', '-lXtst'])
    for variant, checkout in [('before', before), ('after', after)]:
        revision = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
        expected_sha = 'c727f886628897c1749290213e9d65037253095a' if variant == 'before' else os.environ['PRODUCT_SHA']
        expected_tree = '6d4f36e2b4dd48fc9744305d34c78469384edcc5' if variant == 'before' else os.environ['PRODUCT_TREE']
        assert revision == expected_sha
        assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}'], text=True).strip() == expected_tree
        assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
        artifacts = out / (variant + '-development-ui.jsonl')
        with artifacts.open('w') as log:
            run(['cargo', 'build', '--locked', '--profile', 'dev-runtime', '--no-default-features',
                 '--features', 'development,ui', '--lib', '--message-format=json'],
                cwd=checkout, stdout=log, timeout=4200)
        rows = [json.loads(line) for line in artifacts.read_text().splitlines() if line.startswith('{')]
        libs = [name for row in rows if row.get('reason') == 'compiler-artifact'
                and row.get('target', {}).get('name') == 'cagents'
                and set(row.get('features', [])) == {'development', 'ui'}
                for name in row.get('filenames', []) if name.endswith('.rlib')]
        assert len(libs) == 1
        helper = base / (variant + '-fixture')
        command = ['rustc', '--edition=2024', str(source), '--extern', 'cagents=' + libs[0],
                   '-L', 'dependency=' + str(target / 'dev-runtime/deps'), '-o', str(helper)]
        if variant == 'after':
            command += ['--cfg', 'held_product_after']
        for row in rows:
            if row.get('reason') == 'build-script-executed':
                for path in row.get('linked_paths', []):
                    command += ['-L', path]
        run(command, timeout=180)
        for surface, mode, payload, hidden, ordinary in [('chat', 'stale', 'short', False, False), ('chat', 'stale', 'long', False, False), ('chat', 'closed', 'short', False, False), ('chat', 'closed', 'empty', False, False), ('terminal', 'closed', 'empty', False, False), ('chat', 'stale', 'short', True, False), ('chat', 'stale', 'short', False, True)]:
            label = f'{variant}-' + ('hidden-' if hidden else '') + ('ordinary-' if ordinary else '') + f'{surface}-{mode}-{payload}'
            root = base / label
            root.mkdir(mode=0o700)
            env = os.environ.copy()
            for key, name in [('HOME', 'home'), ('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'),
                              ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state'), ('TMPDIR', 'tmp'),
                              ('XDG_RUNTIME_DIR', 'runtime')]:
                path = root / name
                path.mkdir(mode=0o700)
                env[key] = str(path)
            env['CARGO_HOME'] = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
            env['RUSTUP_HOME'] = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))
            env['HELD_FIXTURE_PAYLOAD'] = payload
            env['HELD_FIXTURE_SURFACE'] = surface
            env['HELD_FIXTURE_HIDDEN'] = '1' if hidden else '0'
            env['HELD_FIXTURE_ORDINARY'] = '1' if ordinary else '0'
            env['ACCESSIBILITY_ENABLED'] = '1'
            env['NO_AT_BRIDGE'] = '0'
            env['WINIT_UNIX_BACKEND'] = 'x11'
            env['ICED_BACKEND'] = 'tiny-skia'
            env['LIBGL_ALWAYS_SOFTWARE'] = '1'
            with (out / (label + '-fixture.log')).open('w') as log:
                fixture = subprocess.Popen([str(helper), str(root), mode], env=env, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
            children.append(fixture)
            receipt = root / 'receipts.txt'
            wait(lambda: fixture.poll() is None and receipt.exists() and 'fixture-ready' in receipt.read_text())
            with (out / (label + '-runner.log')).open('w') as log:
                ui = subprocess.Popen(['scripts/run-development', '--browserless', '--debug', '--log-file',
                                       str(out / (label + '-ui.log'))], cwd=checkout, env=env, stdout=log,
                                      stderr=subprocess.STDOUT, start_new_session=True)
            children.append(ui)
            def window_ready():
                if ui.poll() is not None:
                    raise RuntimeError('Development runner exited before native window')
                found = subprocess.run(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'],
                                       text=True, stdout=subprocess.PIPE, timeout=5)
                windows = found.stdout.split()
                return windows[0] if len(windows) == 1 else None
            window = wait(window_ready, timeout=1800)
            pid = int(subprocess.check_output(['xdotool', 'getwindowpid', window], text=True).strip())
            executable = Path('/proc') / str(pid) / 'exe'
            assert executable.resolve() == (target / 'dev-runtime/cagents-ui').resolve()
            ui_hash = digest(executable)
            run(['xdotool', 'windowactivate', '--sync', window])
            run(['xdotool', 'key', 'ctrl+shift+i'])
            time.sleep(.4)
            hidden_field = None
            if hidden:
                hidden_field = wait(lambda: next((node for node in tree(pid)
                                                 if node.name == 'Search settings' and visible(node)
                                                 and node.getState().contains(pyatspi.STATE_EDITABLE)), None))
                assert hidden_field.queryComponent().grabFocus()
                wait(lambda: hidden_field.getState().contains(pyatspi.STATE_FOCUSED))
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'active '])
                wait(lambda: hidden_field.queryText().getText(0, -1) == 'active ')
                if variant == 'after':
                    wait(lambda: 'healthy-attach-held-before-ready' in receipt.read_text())
                    (root / 'release-hidden-healthy').touch()
            if variant == 'after':
                if not hidden:
                    wait(lambda: any(node.name == 'Saved pane unavailable' for node in tree(pid)))
                wait(lambda: 'healthy-attach-requested' in receipt.read_text())
            else:
                time.sleep(5)
                baseline_receipts = receipt.read_text()
                assert not any(marker in baseline_receipts for marker in
                               ('healthy-attach-requested', 'healthy-attach-held-before-ready')), \
                    'Baseline did not reproduce whole-scope block'
            if hidden:
                if variant == 'after':
                    wait(lambda: 'hidden-healthy-ready-title-published' in receipt.read_text())
                    wait(lambda: any('Hidden conversation recovered' in node.name and visible(node)
                                     for node in live_tree(pid)))
                assert hidden_field.getState().contains(pyatspi.STATE_FOCUSED), 'Background saved recovery stole native active-pane focus'
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'scope preserved'])
                wait(lambda: hidden_field.queryText().getText(0, -1) == 'active scope preserved')
                assert not any(node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid))
            capture(window, label + '-wide', (1280, 800))
            capture(window, label + '-narrow', (900, 700))
            run(['xdotool', 'key', 'Tab', 'shift+Tab'])
            focused = accessibility_snapshot(lambda: [{'name': node.name, 'role': node.getRoleName()} for node in live_tree(pid)
                       if node.getState().contains(pyatspi.STATE_FOCUSED)])
            (out / (label + '-focused.json')).write_text(json.dumps(focused, indent=2))
            capture(window, label + '-keyboard', (900, 700))
            if payload == 'long':
                if variant == 'after':
                    wait(lambda: any('Synthetic conversation entry 199.' in node.name and visible(node)
                                     for node in live_tree(pid)))
                capture(window, label + '-performance-wide', (1280, 800))
                responsiveness(window, pid, fixture, root, label, pixel_helper)
            if variant == 'after' and not hidden:
                capture(window, label + '-wide-restored', (1280, 800))
                fields = [node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                          and node.getState().contains(pyatspi.STATE_EDITABLE)]
                assert len(fields) == 1, 'Healthy sibling must expose one editable Message field'
                field = fields[0]
                bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2), str(bounds.y + bounds.height // 2), 'click', '1', 'key', 'ctrl+a'])
                healthy_text = 'Healthy sibling remains usable.'
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', healthy_text])
                wait(lambda: field.queryText().getText(0, -1) == healthy_text)
                (root / 'healthy-draft-expected').write_text(healthy_text)
            if ordinary:
                open_ordinary_history(pid)
                capture(window, label + '-ordinary-history', (1280, 800))
                if variant == 'after':
                    run(['xdotool', 'key', 'Escape'])
                    wait(lambda: not any(node.name == 'Resume conversation' and visible(node) for node in live_tree(pid)))
                    focus_failed_pane(pid)
                    click(pid, 'Remove saved pane')
                    wait(lambda: not any(node.name == 'Saved pane unavailable' for node in live_tree(pid)))
                    open_ordinary_history(pid)
                    before_denied = receipt.read_text().count('effect-request-')
                    click(pid, 'Resume Unavailable saved pane')
                    wait(lambda: any('This saved pane could not be verified and cannot be reopened here.' in node.name
                                     and visible(node) for node in live_tree(pid)))
                    assert receipt.read_text().count('effect-request-') == before_denied
                    capture(window, label + '-denied-history', (1280, 800))
                click(pid, 'Resume Ordinary retained conversation')
                wait(lambda: receipt.read_text().count('ordinary-exact-history-start-accepted') == 1)
                wait(lambda: not any(node.name == 'Resume conversation' and visible(node) for node in live_tree(pid)))
                wait(lambda: any('Ordinary History recovery is ready.' in node.name and visible(node)
                                 for node in live_tree(pid)))
                def recovered_field():
                    fields = [node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                              and node.getState().contains(pyatspi.STATE_EDITABLE)]
                    return fields[0] if len(fields) == 1 else None
                field = wait(recovered_field)
                assert field.queryText().getText(0, -1) == '', 'History unexpectedly transferred the old closed draft'
                assert field.queryComponent().grabFocus()
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'Fresh work after ordinary History recovery.'])
                wait(lambda: field.queryText().getText(0, -1) == 'Fresh work after ordinary History recovery.')
                capture(window, label + '-ordinary-recovered', (1280, 800))
            elif variant == 'after' and not hidden and payload == 'short' and mode == 'stale':
                capture(window, label + '-controls', (1280, 800))
                copy_exact(pid, 'Copy saved draft', (root / 'expected-draft.txt').read_bytes(), keyboard=True)
                copy_exact(pid, 'Copy saved attachment locations', (root / 'expected-attachment.txt').read_bytes())
                reads = receipt.read_text().count('explicit-read-replied')
                starts = receipt.read_text().count('explicit-read-started')
                (root / 'hold-check').touch()
                click(pid, 'Check session again')
                wait(lambda: 'held-read-started' in receipt.read_text())
                pending = wait(lambda: next((node for node in live_tree(pid) if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name == 'Checking session…'), None))
                assert not pending.getState().contains(pyatspi.STATE_ENABLED)
                run(['import', '-window', window, str(out / (label + '-pending.png'))])
                with Image.open(out / (label + '-pending.png')) as picture:
                    picture.save(out / (label + '-pending.webp'), lossless=True)
                bounds = pending.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2), str(bounds.y + bounds.height // 2), 'click', '1', 'click', '1'])
                assert receipt.read_text().count('explicit-read-started') == starts + 1
                (root / 'hold-check').unlink()
                wait(lambda: receipt.read_text().count('explicit-read-replied') > reads)
                wait(lambda: any('This saved pane is still unverified.' in node.name for node in live_tree(pid)))
                assert 'repaired-held-attach-requested' not in receipt.read_text()
                capture(window, label + '-failed-check', (1280, 800))
                (root / 'error-check').touch()
                click(pid, 'Check session again')
                wait(lambda: 'scripted-read-disconnect' in receipt.read_text())
                wait(lambda: any('Could not check this saved session.' in node.name for node in live_tree(pid)))
                capture(window, label + '-transport-error', (1280, 800))
                (root / 'error-check').unlink()
                (root / 'repair-evidence').touch()
                click(pid, 'Check session again')
                wait(lambda: 'repaired-held-attach-requested' in receipt.read_text())
                wait(lambda: not any(node.name == 'Saved pane unavailable' for node in live_tree(pid)))
                capture(window, label + '-recovered', (1280, 800))
            elif variant == 'after' and payload == 'long':
                started = time.monotonic()
                copied = (root / 'expected-draft.txt').read_bytes()
                copy_exact(pid, 'Copy saved draft', copied)
                run(['xdotool', 'mousemove', '--sync', '1000', '450', 'click', '--repeat', '25', '--delay', '20', '5'])
                capture(window, label + '-scrolled', (1280, 800))
                wait(lambda: any(node.name == 'Copy saved attachment locations' and visible(node) for node in live_tree(pid)))
                copy_exact(pid, 'Copy saved attachment locations', (root / 'expected-attachment.txt').read_bytes())
                capture(window, label + '-narrow-long', (900, 700))
                run(['xdotool', 'mousemove', '--sync', '760', '450', 'click', '--repeat', '25', '--delay', '20', '5'])
                copy_exact(pid, 'Copy saved attachment locations', (root / 'expected-attachment.txt').read_bytes())
                capture(window, label + '-narrow-scrolled', (900, 700))
                (out / (label + '-long-copy.json')).write_text(json.dumps({
                    'copied_bytes': len(copied), 'elapsed_seconds': time.monotonic() - started,
                    'ui_proc_status': (Path('/proc') / str(pid) / 'status').read_text(),
                }, indent=2))
            elif variant == 'after' and mode == 'closed':
                focus_failed_pane(pid)
                if payload == 'short':
                    wait(lambda: next((node for node in live_tree(pid)
                                       if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                       and node.name.startswith('Close Chat pane Unavailable saved pane')
                                       and visible(node)), None))
                    close = next(node for node in live_tree(pid)
                                 if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                 and node.name.startswith('Close Chat pane Unavailable saved pane')
                                 and visible(node))
                    click(pid, close.name)
                else:
                    click(pid, 'Remove saved pane')
                wait(lambda: not any(node.name == 'Saved pane unavailable' for node in live_tree(pid)))
                wait(lambda: any(node.name == 'Message' and node.getState().contains(pyatspi.STATE_FOCUSED)
                                 for node in live_tree(pid)))
                capture(window, label + '-removed', (1280, 800))
            names = accessibility_snapshot(lambda: [{'name': node.name, 'role': node.getRoleName()} for node in live_tree(pid)])
            (out / (label + '-accessibility.json')).write_text(json.dumps(names, indent=2))
            database = Path((root / 'database-path').read_text())
            key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:quarantined_panes'
            if variant == 'after':
                def durable_settled():
                    try:
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            marker_row = db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()
                            draft_key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:chat_drafts'
                            drafts_row = db.execute('SELECT value FROM metadata WHERE key=?', (draft_key,)).fetchone()
                            card_row = db.execute('SELECT existence_json FROM cards WHERE card_id=?', ('00000000-0000-4000-8000-000000004922',)).fetchone()
                            if not marker_row or not card_row or not drafts_row:
                                return False
                            marker = json.loads(marker_row[0])
                            is_closed = 'closed' in str(json.loads(card_row[0])).lower()
                            should_close = mode == 'closed' or ordinary
                            should_mark = hidden or ordinary or mode != 'stale' or payload != 'short'
                            healthy_saved = hidden or json.loads(drafts_row[0]).get('00000000-0000-4000-8000-000000003922') == healthy_text
                            return healthy_saved and is_closed == should_close and ('00000000-0000-4000-8000-000000004922' in marker) == should_mark
                    except sqlite3.Error:
                        return False
                wait(durable_settled)
            if ordinary:
                def recovered_durable_settled():
                    try:
                        actor = (root / 'ordinary-recovered-chat-id').read_text()
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            row = next((row for row in db.execute('SELECT card_id, content_json, existence_json FROM cards WHERE kind_tag=?', ('chat',))
                                        if json.loads(row[1]).get('id') == actor), None)
                            draft_key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:chat_drafts'
                            drafts = db.execute('SELECT value FROM metadata WHERE key=?', (draft_key,)).fetchone()
                            return (row is not None and 'open' in str(json.loads(row[2])).lower() and drafts is not None
                                    and json.loads(drafts[0]).get(row[0]) == 'Fresh work after ordinary History recovery.')
                    except (sqlite3.Error, FileNotFoundError):
                        return False
                wait(recovered_durable_settled)
            stop(ui)
            stop(fixture)
            expected_existence = 'closed' if variant == 'after' and (mode == 'closed' or ordinary) else 'open'
            with (out / (label + '-typed-persistence.log')).open('w') as log:
                run([str(helper), str(root), mode, '--inspect', expected_existence], env=env, stdout=log)
            database = Path((root / 'database-path').read_text())
            assert database.is_relative_to(root)
            if variant == 'after':
                with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
                    key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:quarantined_panes'
                    marker = json.loads(db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()[0])
                    bad_id = '00000000-0000-4000-8000-000000004922'
                    assert (bad_id in marker) == (hidden or ordinary or mode != 'stale' or payload != 'short')
                    before_rows = list(db.iterdump())
                old_reader = subprocess.run([str(base / 'before-fixture'), str(root), mode, '--inspect', expected_existence],
                                            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
                (out / (label + '-v13-reader-refusal.log')).write_bytes(old_reader.stdout)
                assert old_reader.returncode != 0 and b'UnsupportedSchema' in old_reader.stdout
                with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
                    assert list(db.iterdump()) == before_rows
            events = receipt.read_text()
            (out / (label + '-receipts.txt')).write_text(events)
            assert 'UNEXPECTED-' not in events
            receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                             'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                             'events': events.splitlines()})
            assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
            assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
finally:
    cleanup_errors = []
    for process in reversed(children):
        try:
            stop(process)
        except Exception as error:
            cleanup_errors.append(str(error))
    try:
        close_family({os.getpid()})
        # Reap adopted children; zombies do not execute or retain build locks.
        while True:
            try:
                if os.waitpid(-1, os.WNOHANG)[0] == 0:
                    break
            except ChildProcessError:
                break
        remaining = processes()
        live_owned = [pid for pid in descendants(remaining, {os.getpid()})
                      if pid != os.getpid() and pid in remaining and remaining[pid][2] not in {'Z', 'X'}]
        assert not live_owned, live_owned
    except Exception as error:
        cleanup_errors.append(str(error))
    for root in base.iterdir():
        if root.is_dir():
            for name in ['receipts.txt', 'expected-draft.txt', 'expected-attachment.txt']:
                if (root / name).is_file():
                    (out / (root.name + '-' + name)).write_bytes((root / name).read_bytes())
    (out / 'cleanup.json').write_text(json.dumps({'errors': cleanup_errors, 'subreaper': True}, indent=2) + '\n')
    (out / 'accessibility-retries.json').write_text(json.dumps({'unknown_object_retries': stale_accessibility_retries,
        'scope': 'Bounded whole-query retry for AT-SPI Unknown object only; no input gesture is retried.'}, indent=2) + '\n')
    if not cleanup_errors:
        (out / 'cleanup-complete').write_text('Owned descendant families observed inactive; private native fixture closed.\n')
    (out / 'native-receipts.json').write_text(json.dumps(receipts, indent=2) + '\n')
    (out.parent / 'sha256.txt').write_text(''.join(f'{digest(path)}  {path.relative_to(out.parent)}\n'
        for path in sorted(out.parent.rglob('*')) if path.is_file() and path.name != 'sha256.txt'))
    if cleanup_errors:
        raise RuntimeError('Native process cleanup is unqualified: ' + '; '.join(cleanup_errors))
