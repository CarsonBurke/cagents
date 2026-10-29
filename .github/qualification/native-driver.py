"""Temporary hosted-X11 actual native evidence; never production-provider evidence."""
import ast
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
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
native_renderer = {'ICED_BACKEND': 'wgpu', 'WGPU_BACKEND': 'gl', 'LIBGL_ALWAYS_SOFTWARE': '1'}
stale_accessibility_retries = 0
incomplete_accessibility_retries = 0
active_arm = None
window = pid = None
command_sequence = 0


def event(value):
    value = {'arm': active_arm, 'monotonic': time.monotonic(), 'unix_ns': time.time_ns(), **value}
    with (out / 'native-events.jsonl').open('a') as stream:
        stream.write(json.dumps(value) + '\n')


def cancelled(signum, frame):
    raise RuntimeError('Native qualification cancelled; closing owned descendants')


signal.signal(signal.SIGTERM, cancelled)
# Retain orphaned descendants so escaped process groups remain observable and
# can be closed before the shared target is used by another phase.
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
    raise OSError(ctypes.get_errno(), 'Cannot establish owned descendant cleanup')


def run(args, **kwargs):
    global command_sequence
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise RuntimeError('Inherited caller authority is present; native command refused')
    timeout = kwargs.pop('timeout', 20)
    command_sequence += 1
    command_id = command_sequence
    event({'event': 'command-started', 'command_id': command_id, 'argv': list(map(str, args)),
           'cwd': str(kwargs.get('cwd', Path.cwd()))})
    process = subprocess.Popen(args, start_new_session=True, **kwargs)
    try:
        code = process.wait(timeout=timeout)
    except BaseException as error:
        event({'event': 'command-failed', 'command_id': command_id,
               'error_type': type(error).__name__, 'error': str(error)[:2000]})
        stop(process)
        raise
    event({'event': 'command-finished', 'command_id': command_id, 'exit_code': code})
    if code:
        stop(process)
        raise subprocess.CalledProcessError(code, args)
    stop(process)
    return subprocess.CompletedProcess(args, code)


class IncompleteAccessibilityTree(RuntimeError):
    pass


def stale_accessibility_object(error):
    return 'Unknown object' in str(error) or (
        error.domain == 'atspi_error' and error.code == 1
        and re.fullmatch(r'/org/a11y/atspi/accessible/[0-9]+/[0-9]+', error.message) is not None)


def wait(predicate, timeout=30):
    global stale_accessibility_retries, incomplete_accessibility_retries
    end = time.monotonic() + timeout
    last_stale = None
    while time.monotonic() < end:
        try:
            result = predicate()
        except (GLib.Error, IncompleteAccessibilityTree) as error:
            if isinstance(error, GLib.Error):
                if not stale_accessibility_object(error):
                    raise
                stale_accessibility_retries += 1
            else:
                incomplete_accessibility_retries += 1
            last_stale = error
            result = None
        if result and time.monotonic() < end:
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
    os.kill(pid, 0)
    desktop = pyatspi.Registry.getDesktop(0)
    if desktop is None:
        raise IncompleteAccessibilityTree('Accessibility desktop is not published yet')
    apps = [desktop.getChildAtIndex(i) for i in range(desktop.childCount)]
    if any(app is None for app in apps):
        raise IncompleteAccessibilityTree('Accessibility desktop child changed during traversal')
    apps = [app for app in apps if app.get_process_id() == pid]
    if len(apps) != 1:
        return []
    pending, result = [apps[0]], []
    while pending:
        node = pending.pop()
        if node is None:
            raise IncompleteAccessibilityTree('Accessibility child changed during traversal')
        result.append(node)
        assert len(result) < 10000 and node.childCount < 1000
        pending.extend(node.getChildAtIndex(i) for i in range(node.childCount))
    return result


def visible(node):
    return node.getState().contains(pyatspi.STATE_SHOWING)


def live_tree(pid):
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
                       'backend_requested': native_renderer['ICED_BACKEND'],
                       'wgpu_backend_requested': native_renderer['WGPU_BACKEND'],
                       'software_gl_requested': native_renderer['LIBGL_ALWAYS_SOFTWARE'],
                       'capture': 'visible screen pixels for owned window'}
            (out / (name + '-frame.json')).write_text(json.dumps(metrics, indent=2) + '\n')
            return diverse and lit > .01 and metrics['luminance_range'] >= 16
    wait(rendered_frame, timeout=20)
    if name.endswith('-wide'):
        run(['import', '-window', window, str(out / (name + '-drawable.png'))])
    with Image.open(png) as picture:
        assert picture.size == size, (picture.size, size)
        picture.save(out / (name + '.webp'), lossless=True)


def click(pid, name, keyboard=False):
    names = (name,) if isinstance(name, str) else name
    def find():
        nodes = [node for node in tree(pid) if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                 and node.name in names and visible(node)
                 and node.getState().contains(pyatspi.STATE_ENABLED)]
        return nodes[0] if len(nodes) == 1 else None
    def target_snapshot():
        node = find()
        if node is None:
            return None
        selected_name = node.name
        bounds = None if keyboard else node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        return node, selected_name, bounds
    node, selected_name, bounds = wait(target_snapshot)
    if keyboard:
        def input_focus():
            return int(subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5).strip())
        def focus_state(target):
            states = target.getState()
            return {'object': repr(target), 'focused': states.contains(pyatspi.STATE_FOCUSED),
                    'enabled': states.contains(pyatspi.STATE_ENABLED), 'showing': visible(target),
                    'native_input_focus': input_focus(), 'owned_window': int(window)}
        before_focus = focus_state(node)
        event({'event': 'control-gesture', 'pid': pid, 'name': selected_name, 'input': 'keyboard',
               'before_focus': before_focus})
        assert node.queryComponent().grabFocus(), 'Recovery control refused keyboard focus'
        last_focus = {}
        def focused_control():
            nonlocal last_focus
            current = find()
            if current is None:
                last_focus = {'target_missing_or_ambiguous': True, 'native_input_focus': input_focus()}
                return None
            current.clearCache()
            last_focus = focus_state(current)
            return current if (last_focus['focused'] and last_focus['enabled'] and last_focus['showing']
                               and last_focus['native_input_focus'] == int(window)) else None
        try:
            wait(focused_control)
        finally:
            event({'event': 'keyboard-focus-readback', 'pid': pid, 'name': name, 'state': last_focus})
        run(['xdotool', 'key', 'Return'])
        return
    assert bounds.width > 0 and bounds.height > 0
    assert 0 <= bounds.x + bounds.width // 2 < viewport[0]
    assert 0 <= bounds.y + bounds.height // 2 < viewport[1]
    event({'event': 'control-gesture', 'pid': pid, 'name': selected_name, 'input': 'pointer',
           'bounds': [bounds.x, bounds.y, bounds.width, bounds.height]})
    run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2),
         str(bounds.y + bounds.height // 2), 'click', '1'])
    return selected_name


def capture_notice(pid, window, name, message):
    def notice_control():
        nodes = [node for node in live_tree(pid) if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                 and node.name in ('Application notices: success', 'Application notices', 'Application errors')
                 and visible(node) and node.getState().contains(pyatspi.STATE_ENABLED)]
        assert len(nodes) <= 1, 'Application notices must expose one current control'
        return nodes[0] if nodes else None
    control = wait(notice_control)
    bounds = control.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    geometry = dict(line.split('=', 1) for line in subprocess.check_output(
        ['xdotool', 'getwindowgeometry', '--shell', window], text=True, timeout=5).splitlines())
    assert int(geometry['X']) == 0 and int(geometry['Y']) == 0
    assert bounds.width >= 18 and bounds.height >= 18
    center_x, center_y = bounds.x + bounds.width // 2, bounds.y + bounds.height // 2
    assert 9 <= center_x <= viewport[0] - 9 and 9 <= center_y <= viewport[1] - 9
    glyph_frame = out / (name + '-notice-control.png')
    run(['import', '-screen', '-window', window, str(glyph_frame)])
    with Image.open(glyph_frame) as picture:
        assert picture.size == viewport
        glyph = picture.crop((center_x - 9, center_y - 9, center_x + 9, center_y + 9)).convert('RGB')
        low, high = glyph.convert('L').getextrema()
        metrics = {'luminance_range': high - low, 'at_least_4_colors': glyph.getcolors(maxcolors=3) is None,
                   'desktop_bounds': [bounds.x, bounds.y, bounds.width, bounds.height],
                   'roi': [center_x - 9, center_y - 9, center_x + 9, center_y + 9]}
    event({'event': 'notice-control-glyph', 'pid': pid, 'capture': glyph_frame.name, **metrics})
    assert metrics['luminance_range'] >= 16 and metrics['at_least_4_colors'], 'Notices control glyph is blank'
    control_name = click(pid, ('Application notices: success', 'Application notices', 'Application errors'))
    wait(lambda: any(node.name == 'Close application notices' and visible(node) for node in live_tree(pid)))
    wait(lambda: any(node.name == message and visible(node) for node in live_tree(pid)))
    event({'event': 'application-notice-readback', 'pid': pid, 'control': control_name, 'message': message})
    capture(window, name, (1280, 800))
    click(pid, 'Close application notices')
    wait(lambda: not any(node.name == 'Close application notices' and visible(node) for node in live_tree(pid)))


def copy_exact(pid, name, expected, keyboard=False):
    sentinel = b'held-scope-native-copy-sentinel'
    assert expected != sentinel
    owner = subprocess.Popen(['xclip', '-selection', 'clipboard', '-in', '-quiet'],
                             stdin=subprocess.PIPE, start_new_session=True)
    children.append(owner)
    owner.stdin.write(sentinel)
    owner.stdin.close()
    last_read = {}
    def clipboard():
        result = subprocess.run(['xclip', '-selection', 'clipboard', '-out'],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
        last_read.update({'monotonic': time.monotonic(), 'exit_code': result.returncode,
                          'bytes': len(result.stdout), 'sha256': hashlib.sha256(result.stdout).hexdigest(),
                          'matches_sentinel': result.stdout == sentinel,
                          'matches_expected': result.stdout == expected})
        return result.stdout if result.returncode == 0 else None
    phase = 'await-sentinel'
    try:
        wait(lambda: clipboard() == sentinel)
        phase = 'gesture'
        click(pid, name, keyboard=keyboard)
        phase = 'await-expected'
        wait(lambda: clipboard() == expected)
    except BaseException:
        try:
            event({'event': 'clipboard-incomplete', 'pid': pid, 'name': name, 'phase': phase,
                   'last_read': last_read, 'sentinel_owner_exit_code': owner.poll()})
        except BaseException as diagnostic_error:
            print('Clipboard failure diagnostics unavailable: ' + repr(diagnostic_error), file=sys.stderr)
        raise
    event({'event': 'clipboard-match', 'pid': pid, 'name': name, 'bytes': len(expected),
           'sha256': hashlib.sha256(expected).hexdigest()})


def open_ordinary_history(pid):
    plus_name = wait(lambda: next((node.name for node in live_tree(pid)
                                  if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                  and node.name.startswith('New tab · Thread ')
                                  and visible(node)), None))
    click(pid, plus_name)
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
    def common_control():
        nodes = [node for node in live_tree(pid)
                 if node.name == 'Search projects' and node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)]
        assert len(nodes) <= 1, f'The common control must have one native target, found {len(nodes)}'
        return nodes[0] if nodes else None
    node = wait(common_control)
    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    assert 0 <= bounds.x and 0 <= bounds.y and bounds.x + bounds.width <= 1280 and bounds.y + bounds.height <= 800
    (out / (label + '-probe-target.json')).write_text(json.dumps({
        'name': node.name, 'role': node.getRoleName(),
        'enabled': node.getState().contains(pyatspi.STATE_ENABLED),
        'desktop_bounds': [bounds.x, bounds.y, bounds.width, bounds.height],
        'focused': node.getState().contains(pyatspi.STATE_FOCUSED), 'window': window,
    }, indent=2) + '\n')
    assert node.getState().contains(pyatspi.STATE_ENABLED), 'The common control is disabled'
    samples = []
    completed_phases = []
    try:
        for phase in ['quiet', 'model-updates']:
            if phase == 'model-updates':
                (root / 'performance-stream').touch()
                wait(lambda: 'performance-update-0-' in (root / 'receipts.txt').read_text())
            output = out / (label + '-' + phase + '-pixel-response.jsonl')
            with output.open('w') as log, (out / (label + '-' + phase + '-probe.log')).open('w') as errors:
                probe = subprocess.Popen([str(helper), window, str(bounds.x), str(bounds.y),
                                          str(bounds.width), str(bounds.height), '60', str(output.with_suffix(''))],
                                         stdout=log, stderr=errors, start_new_session=True)
            children.append(probe)
            deadline = time.monotonic() + 40
            while probe.poll() is None or (phase == 'model-updates' and 'performance-stream-complete' not in (root / 'receipts.txt').read_text()):
                assert time.monotonic() < deadline, 'Pixel probe exceeded deadline'
                samples.append({'phase': phase, 'ui': resource_sample(pid), 'fixture': resource_sample(fixture.pid)})
                time.sleep(.5)
            if probe.returncode:
                try:
                    run(['import', '-screen', '-window', window, str(out / (label + '-' + phase + '-probe-failed.png'))])
                except Exception as error:
                    (out / (label + '-' + phase + '-probe-diagnostic-error.txt')).write_text(repr(error) + '\n')
            assert probe.returncode == 0, 'Common-control hover calibration or response probe failed'
            rows = [json.loads(line) for line in output.read_text().splitlines()]
            assert rows[0]['calibrated'] and len(rows) == 61
            with Image.open(str(output.with_suffix('')) + '-calibration-0.ppm') as reference:
                low, high = reference.convert('L').getextrema()
                assert high - low >= 16, 'The common control glyph must be visibly rendered'
            assert all(row['timeout'] is False and 0 <= row['seconds'] < 1
                       and 0 <= row['max_poll_gap_seconds'] < 1 for row in rows[1:]), (
                           'Common-chrome response or observation stalled for at least one second')
            if phase == 'model-updates':
                updates = [line.split('-') for line in (root / 'receipts.txt').read_text().splitlines()
                           if line.startswith('performance-update-')]
                assert len(updates) == 150 and [int(row[2]) for row in updates] == list(range(150))
                assert all(int(a[3]) < int(b[3]) and int(a[9]) < int(b[9])
                           for a, b in zip(updates, updates[1:]))
            capture(window, label + '-' + phase + '-hover', (1280, 800))
            completed_phases.append(phase)
    finally:
        (out / (label + '-responsiveness-resources.json')).write_text(json.dumps({
            'qualified': sys.exc_info()[0] is None and completed_phases == ['quiet', 'model-updates'],
            'completed_phases': completed_phases,
            'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
            'synthetic_workload': '200-item healthy transcript, 32 managed registry entries (30 idle peers), 150 channel-title events at nominal 5Hz',
            'scope': 'Actual X11 app input-to-observed-window-pixels; pixel probe and resources cover their recorded sample intervals during the 150-event stream. No physical presentation/GPU or live-provider claim. Baseline blocks healthy transcript admission.',
            'probe_sha256': digest(helper), 'samples': samples,
        }, indent=2) + '\n')


try:
    icon_root = Path('/usr/share/icons/Adwaita')
    assert (icon_root / 'index.theme').is_file()
    icon_resources = {}
    for icon_name in ['dialog-warning-symbolic', 'dialog-error-symbolic', 'emblem-ok-symbolic']:
        candidates = sorted(path for path in icon_root.rglob(icon_name + '.*') if path.is_file())
        assert candidates, 'Missing installed Adwaita resource: ' + icon_name
        icon_resources[icon_name] = [{'path': str(path), 'sha256': digest(path)} for path in candidates]
    with (out / 'adwaita-package-version.txt').open('w') as package_version:
        run(['dpkg-query', '-W', '-f=${Version}\n', 'adwaita-icon-theme'], stdout=package_version)
    (out / 'icon-theme-resources.json').write_text(json.dumps({
        'requested_theme': 'Adwaita', 'index_sha256': digest(icon_root / 'index.theme'),
        'installed_resource_candidates': icon_resources,
    }, indent=2) + '\n')
    font_paths = [Path(path) for path in subprocess.check_output(
        ['dpkg-query', '-L', 'fonts-noto-cjk'], text=True).splitlines()
        if Path(path).suffix.lower() in {'.ttc', '.ttf', '.otf'}]
    assert font_paths and all(path.is_file() for path in font_paths)
    japanese_font = Path(subprocess.check_output(
        ['fc-match', '-f', '%{file}', 'sans:lang=ja'], text=True))
    assert japanese_font in font_paths, 'Japanese fallback must resolve to installed CJK fonts'
    (out / 'cjk-font-resources.json').write_text(json.dumps({
        'package_version': subprocess.check_output(
            ['dpkg-query', '-W', '-f=${Version}', 'fonts-noto-cjk'], text=True),
        'japanese_fallback': str(japanese_font),
        'installed_resources': [{'path': str(path), 'sha256': digest(path)} for path in font_paths],
        'scope': 'Same installed desktop fonts for every before/after arm; rendered coverage still requires native image review.',
    }, indent=2) + '\n')
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
    fixtures = {}
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
        fixtures[variant] = (revision, helper, expected_sha, expected_tree)
    # Compile both immutable helpers first so candidate evidence can use the
    # schema-13 reader while exercising the changed interaction first.
    for variant, checkout in [('after', after), ('before', before)]:
        revision, helper, expected_sha, expected_tree = fixtures[variant]
        assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
        assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}'], text=True).strip() == expected_tree
        assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
        for surface, mode, payload, hidden, ordinary in [('chat', 'stale', 'short', False, True), ('chat', 'stale', 'short', False, False), ('chat', 'stale', 'long', False, False), ('chat', 'closed', 'short', False, False), ('chat', 'closed', 'empty', False, False), ('terminal', 'closed', 'empty', False, False), ('chat', 'stale', 'short', True, False), ('chat', 'peer-removal', 'empty', False, False)]:
            label = f'{variant}-' + ('hidden-' if hidden else '') + ('ordinary-' if ordinary else '') + f'{surface}-{mode}-{payload}'
            active_arm = label
            window = pid = None
            event({'event': 'arm-started', 'revision': revision})
            root = base / label
            root.mkdir(mode=0o700)
            env = os.environ.copy()
            for key, name in [('HOME', 'home'), ('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'),
                              ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state'), ('TMPDIR', 'tmp'),
                              ('XDG_RUNTIME_DIR', 'runtime')]:
                path = root / name
                path.mkdir(mode=0o700)
                env[key] = str(path)
            assert 'FLATPAK_ID' not in env, 'Private icon configuration requires the ordinary XDG config route'
            icon_config = Path(env['XDG_CONFIG_HOME']) / 'cosmic/com.system76.CosmicTk/v1/icon_theme'
            icon_config.parent.mkdir(parents=True)
            icon_config.write_text('"Adwaita"\n')
            (out / (label + '-icon-theme.json')).write_text(json.dumps({
                'requested_theme': 'Adwaita', 'private_config_path': str(icon_config),
                'config_sha256': digest(icon_config),
                'installed_resources_sha256': digest(out / 'icon-theme-resources.json'),
            }, indent=2) + '\n')
            env['CARGO_HOME'] = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
            env['RUSTUP_HOME'] = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))
            env['HELD_FIXTURE_PAYLOAD'] = payload
            env['HELD_FIXTURE_SURFACE'] = surface
            env['HELD_FIXTURE_HIDDEN'] = '1' if hidden else '0'
            env['HELD_FIXTURE_ORDINARY'] = '1' if ordinary else '0'
            env['HELD_FIXTURE_PEER'] = '1' if mode == 'peer-removal' else '0'
            env['ACCESSIBILITY_ENABLED'] = '1'
            env['NO_AT_BRIDGE'] = '0'
            env['WINIT_UNIX_BACKEND'] = 'x11'
            env.update(native_renderer)
            env['RUST_LOG'] = 'cagents=debug,cagentsd=debug,iced_wgpu=info,warn'
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
            (out / (label + '-launch.json')).write_text(json.dumps({
                'arm': label, 'revision': revision, 'window_id': window, 'pid': pid,
                'executable': str(executable.resolve()), 'ui_sha256': ui_hash,
                'backend_requested': env['ICED_BACKEND'],
                'wgpu_backend_requested': env['WGPU_BACKEND'],
                'software_gl_requested': env['LIBGL_ALWAYS_SOFTWARE'],
                'fixture_source_sha256': digest(source),
                'helper_sha256': digest(helper), 'scope': 'Owned native window binding before application input',
            }, indent=2) + '\n')
            with (out / (label + '-window.txt')).open('w') as log:
                run(['xwininfo', '-id', window, '-tree', '-stats'], stdout=log)
            with (out / (label + '-root-window.txt')).open('w') as log:
                run(['xwininfo', '-root', '-stats'], stdout=log)
            run(['xdotool', 'windowactivate', '--sync', window])
            wait(lambda: 'full-model-delivered' in receipt.read_text())
            wait(lambda: any(node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                             and node.name.startswith('Open channel Saved recovery model received, ')
                             and visible(node)
                             for node in tree(pid)))
            capture(window, label + '-launch-wide', (1280, 800))
            if mode == 'peer-removal':
                ready = 'Verified conversation ready for peer removal.'
                wait(lambda: any(node.name == ready and visible(node) for node in live_tree(pid)))
                wait(lambda: any(visible(node) and node.name.startswith('Chat pane Healthy saved pane,')
                                 for node in live_tree(pid)))
                def peer_inside_viewport(node):
                    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    return (bounds.width > 0 and bounds.height > 0 and bounds.x >= 0 and bounds.y >= 0
                            and bounds.x + bounds.width <= viewport[0] and bounds.y + bounds.height <= viewport[1])
                def empty_peer_composer():
                    fields = [node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                              and node.getState().contains(pyatspi.STATE_EDITABLE)]
                    assert len(fields) <= 1
                    return len(fields) == 1 and fields[0].queryText().getText(0, -1) == ''
                wait(empty_peer_composer)
                capture(window, label + '-ready-wide', (1280, 800))
                settings_focused = 'Settings pane Active settings, Application-wide, focused'
                settings_unfocused = 'Settings pane Active settings, Application-wide, not focused'
                settings_tab = wait(lambda: next((node.name for node in live_tree(pid)
                    if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                    and node.name in (settings_focused, settings_unfocused)
                    and peer_inside_viewport(node)
                    and node.getState().contains(pyatspi.STATE_ENABLED)), None))
                click(pid, settings_tab)
                def peer_field():
                    fields = [node for node in live_tree(pid) if node.name == 'Search settings' and visible(node)
                              and peer_inside_viewport(node)
                              and node.getState().contains(pyatspi.STATE_EDITABLE)]
                    assert len(fields) <= 1
                    return fields[0] if fields else None
                def peer_field_bounds():
                    field = peer_field()
                    if field is None:
                        return None
                    bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    return (bounds.x, bounds.y, bounds.width, bounds.height) if bounds.width > 0 and bounds.height > 0 else None
                x, y, width, height = wait(peer_field_bounds)
                run(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])
                wait(lambda: (field := peer_field()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED))
                assert accessibility_snapshot(lambda: peer_field().queryText().getText(0, -1)) == ''
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'peer scope preserved'])
                wait(lambda: (field := peer_field()) is not None and field.queryText().getText(0, -1) == 'peer scope preserved')
                assert receipt.read_text().splitlines().count('healthy-attach-requested') == 1
                (root / 'peer-remove').touch()
                event({'event': 'peer-removal-triggered', 'input': 'external model watcher; no application input'})
                wait(lambda: 'peer-removal-event-delivered' in receipt.read_text().splitlines())
                wait(lambda: 'peer-removal-model-delivered' in receipt.read_text().splitlines())
                wait(lambda: any(node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                                and node.name.startswith('Open channel Peer removal received, ') for node in live_tree(pid)))
                def peer_removed():
                    nodes = live_tree(pid)
                    field = peer_field()
                    return (not any(visible(node) and (node.name == ready or node.name == 'Message'
                                    or node.name.startswith('Chat pane Healthy saved pane,')) for node in nodes)
                            and any(visible(node) and node.name == settings_focused for node in nodes)
                            and field is not None and field.getState().contains(pyatspi.STATE_FOCUSED)
                            and field.queryText().getText(0, -1) == 'peer scope preserved')
                wait(peer_removed)
                assert not accessibility_snapshot(lambda: any(visible(node) and node.name == 'Saved pane unavailable'
                                                               for node in live_tree(pid)))
                # No focus repair after the external removal. Confirm actual
                # keyboard routing before capturing the retained task.
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', ' and still active'])
                wait(lambda: (field := peer_field()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED)
                     and field.queryText().getText(0, -1) == 'peer scope preserved and still active')
                capture(window, label + '-removed-wide', (1280, 800))
                database = Path((root / 'database-path').read_text())
                assert database.is_relative_to(root)
                workspace_id = (root / 'workspace-id').read_text().strip()
                prefix = 'workspace:' + workspace_id + ':navigation:'
                def peer_durable():
                    try:
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            channels = db.execute('SELECT value FROM metadata WHERE key=?', (prefix + 'threads',)).fetchone()
                            card = db.execute('SELECT existence_json FROM cards WHERE card_id=?',
                                              ('00000000-0000-4000-8000-000000003922',)).fetchone()
                            if not channels or not card:
                                return False
                            owners = [channel for channel in json.loads(channels[0])
                                      if channel['id'] == '00000000-0000-4000-8000-000000002922']
                            return (len(owners) == 1 and owners[0]['cards'] == ['00000000-0000-4000-8000-000000004922']
                                    and 'closed' in json.loads(card[0]))
                    except (sqlite3.Error, FileNotFoundError):
                        return False
                wait(peer_durable)
                stop(ui)
                stop(fixture)
                assert icon_config.read_text() == '"Adwaita"\n'
                with (out / (label + '-typed-persistence.log')).open('w') as log:
                    run([str(helper), str(root), mode, '--inspect', 'closed'], env=env, stdout=log, stderr=subprocess.STDOUT)
                events = receipt.read_text()
                assert 'UNEXPECTED-' not in events
                assert events.splitlines().count('healthy-attach-requested') == 1
                assert events.splitlines().count('peer-removal-event-delivered') == 1
                assert not any(line.startswith(('effect-request-chat-start', 'effect-request-chat-prepare',
                                               'effect-request-other-provider-mutation', 'effect-request-chat-close'))
                               for line in events.splitlines())
                (out / (label + '-receipts.txt')).write_text(events)
                receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                                 'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                                 'events': events.splitlines()})
                assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
                assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
                event({'event': 'arm-completed', 'revision': revision})
                active_arm = None
                continue
            run(['xdotool', 'key', 'ctrl+shift+i'])
            time.sleep(.4)
            hidden_field = None
            if hidden:
                settings_focused = 'Settings pane Active settings, Application-wide, focused'
                settings_unfocused = 'Settings pane Active settings, Application-wide, not focused'
                def inside_viewport(node):
                    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    return (bounds.width > 0 and bounds.height > 0 and bounds.x >= 0 and bounds.y >= 0
                            and bounds.x + bounds.width <= viewport[0] and bounds.y + bounds.height <= viewport[1])
                settings_tab = wait(lambda: next((node.name for node in live_tree(pid)
                                                  if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                                  and node.name in (settings_focused, settings_unfocused)
                                                  and visible(node) and inside_viewport(node)
                                                  and node.getState().contains(pyatspi.STATE_ENABLED)), None))
                click(pid, settings_tab)
                wait(lambda: any(node.name == settings_focused and visible(node) and inside_viewport(node)
                                 for node in live_tree(pid)))
                previous_settings_bounds = [None]
                def current_settings_field():
                    fields = [node for node in live_tree(pid)
                              if node.name == 'Search settings' and visible(node) and inside_viewport(node)
                              and node.getState().contains(pyatspi.STATE_EDITABLE)]
                    assert len(fields) <= 1, 'Settings must expose one visible editable search field'
                    if not fields:
                        return None
                    bounds = fields[0].queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    current = (bounds.x, bounds.y, bounds.width, bounds.height)
                    stable = previous_settings_bounds[0] == current
                    previous_settings_bounds[0] = current
                    return fields[0] if stable else None
                hidden_field = wait(current_settings_field)
                bounds = hidden_field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2),
                     str(bounds.y + bounds.height // 2), 'click', '1'])
                wait(lambda: hidden_field.getState().contains(pyatspi.STATE_FOCUSED))
                assert hidden_field.queryText().getText(0, -1) == ''
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'active '])
                wait(lambda: hidden_field.queryText().getText(0, -1) == 'active ')
                capture(window, label + '-active-input-ready', (1280, 800))
                assert hidden_field.getState().contains(pyatspi.STATE_FOCUSED)
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
                assert accessibility_snapshot(lambda: any(
                    node.name == settings_focused and visible(node) and inside_viewport(node)
                    for node in live_tree(pid))), 'Background saved recovery changed the selected Settings pane'
                hidden_field = wait(current_settings_field)  # Reacquire only; never repair focus after Ready.
                assert hidden_field.getState().contains(pyatspi.STATE_FOCUSED), 'Background saved recovery stole native active-pane focus'
                assert hidden_field.queryText().getText(0, -1) == 'active '
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', 'scope preserved'])
                wait(lambda: hidden_field.queryText().getText(0, -1) == 'active scope preserved')
                assert not accessibility_snapshot(lambda: any(
                    node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid)))
            capture(window, label + '-wide', (1280, 800))
            capture(window, label + '-narrow', (900, 700))
            run(['xdotool', 'key', 'Tab', 'shift+Tab'])
            focused = accessibility_snapshot(lambda: [{'name': node.name, 'role': node.getRoleName()} for node in live_tree(pid)
                       if node.getState().contains(pyatspi.STATE_FOCUSED)])
            (out / (label + '-focused.json')).write_text(json.dumps(focused, indent=2))
            capture(window, label + '-keyboard', (900, 700))
            if payload == 'long':
                if variant == 'after':
                    delivered = 'healthy-long-snapshot-delivered items=200 first=synthetic-history-0 last=synthetic-history-199'
                    wait(lambda: delivered in receipt.read_text().splitlines())
                    wait(lambda: any('Synthetic conversation entry 0.' in node.name and visible(node)
                                     for node in live_tree(pid)))
                    (out / (label + '-long-workload-admission.json')).write_text(json.dumps({
                        'successful_snapshot_write_receipt': delivered,
                        'visible_transcript_entry': 0,
                        'scope': 'Exact healthy 200-item snapshot delivery and visible transcript admission; no latest-position or restoration correctness claim.',
                    }, indent=2) + '\n')
                capture(window, label + '-performance-wide', (1280, 800))
                responsiveness(window, pid, fixture, root, label, pixel_helper)
            if variant == 'after' and not hidden:
                capture(window, label + '-wide-restored', (1280, 800))
                fields = accessibility_snapshot(lambda: [
                    node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                    and node.getState().contains(pyatspi.STATE_EDITABLE)])
                assert len(fields) == 1, 'Healthy sibling must expose one editable Message field'
                field = fields[0]
                bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2), str(bounds.y + bounds.height // 2), 'click', '1', 'key', 'ctrl+a'])
                healthy_text = 'Healthy sibling remains usable.'
                run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', healthy_text])
                wait(lambda: field.queryText().getText(0, -1) == healthy_text)
                (root / 'healthy-draft-expected').write_text(healthy_text)
            if ordinary:
                assert 'ordinary-exact-history-start-accepted' not in receipt.read_text(), (
                    'Ordinary saved work started before an explicit Resume')
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
                assert 'ordinary-exact-history-start-accepted' not in receipt.read_text(), (
                    'Ordinary saved work started before its Resume control')
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
                # Ordinary typing has no idle snapshot timer in either
                # revision. Exercise the real save-on-navigation contract.
                def saved_channel_control():
                    candidates = [node for node in live_tree(pid)
                                  if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                  and node.name.startswith('Open channel Saved recovery model received, ')
                                  and visible(node) and node.getState().contains(pyatspi.STATE_ENABLED)]
                    assert len(candidates) <= 1, 'The intended saved channel must be unique'
                    return candidates[0].name if candidates else None
                click(pid, wait(saved_channel_control))
                wait(lambda: not any('Ordinary History recovery is ready.' in node.name and visible(node)
                                     for node in live_tree(pid)))
                capture(window, label + '-ordinary-returned-to-saved-scope', (1280, 800))
            elif variant == 'after' and not hidden and payload == 'short' and mode == 'stale':
                capture(window, label + '-controls', (1280, 800))
                copy_exact(pid, 'Copy saved draft', (root / 'expected-draft.txt').read_bytes(), keyboard=True)
                copy_exact(pid, 'Copy saved attachment locations', (root / 'expected-attachment.txt').read_bytes())
                reads = receipt.read_text().count('explicit-read-replied')
                starts = receipt.read_text().count('explicit-read-started')
                (root / 'hold-check').touch()
                click(pid, 'Check session again')
                wait(lambda: 'held-read-started' in receipt.read_text())
                pending_state = {}
                def disabled_pending():
                    candidates = [node for node in live_tree(pid)
                                  if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                  and node.name == 'Checking session…']
                    assert len(candidates) <= 1, 'Pending recheck must expose one control'
                    if not candidates:
                        pending_state.update({'target_missing': True})
                        return None
                    node = candidates[0]
                    node.clearCache()
                    states = node.getState()
                    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    pending_state.update({
                        'target_missing': False, 'monotonic': time.monotonic(),
                        'enabled': states.contains(pyatspi.STATE_ENABLED),
                        'sensitive': states.contains(pyatspi.STATE_SENSITIVE),
                        'defunct': states.contains(pyatspi.STATE_DEFUNCT),
                        'showing': states.contains(pyatspi.STATE_SHOWING),
                        'bounds': [bounds.x, bounds.y, bounds.width, bounds.height],
                    })
                    return node if (pending_state['showing'] and not pending_state['defunct']
                                    and not pending_state['enabled'] and not pending_state['sensitive']
                                    and bounds.width > 0 and bounds.height > 0) else None
                try:
                    wait(disabled_pending)
                finally:
                    event({'event': 'pending-recheck-state', 'pid': pid, 'state': pending_state})
                assert not pending_state['enabled'] and not pending_state['sensitive']
                capture(window, label + '-pending', (1280, 800))
                x, y, width, height = pending_state['bounds']
                run(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1', 'click', '1'])
                assert receipt.read_text().count('explicit-read-started') == starts + 1
                (root / 'hold-check').unlink()
                wait(lambda: receipt.read_text().count('explicit-read-replied') > reads)
                assert 'repaired-held-attach-requested' not in receipt.read_text()
                capture_notice(pid, window, label + '-failed-check',
                               'This saved pane is still unverified. Its session and drafts are retained.')
                (root / 'error-check').touch()
                click(pid, 'Check session again')
                wait(lambda: 'scripted-read-disconnect' in receipt.read_text())
                capture_notice(pid, window, label + '-transport-error',
                               'Could not check this saved session. Its pane and drafts are retained.')
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
                    close_name = wait(lambda: next((node.name for node in live_tree(pid)
                                                    if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                                    and node.name.startswith('Close Chat pane Unavailable saved pane')
                                                    and visible(node)), None))
                    click(pid, close_name)
                else:
                    click(pid, 'Remove saved pane')
                wait(lambda: not any(node.name == 'Saved pane unavailable' for node in live_tree(pid)))
                wait(lambda: any(node.name == 'Message' and node.getState().contains(pyatspi.STATE_FOCUSED)
                                 for node in live_tree(pid)))
                capture(window, label + '-removed', (1280, 800))
                capture_notice(pid, window, label + '-removed-notice',
                               'Saved pane removed from this window. Its saved session and drafts remain stored.')
            names = accessibility_snapshot(lambda: [{'name': node.name, 'role': node.getRoleName()} for node in live_tree(pid)])
            (out / (label + '-accessibility.json')).write_text(json.dumps(names, indent=2))
            database = Path((root / 'database-path').read_text())
            assert database.is_relative_to(root)
            key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:quarantined_panes'
            def observe_durability(stage, checks, db=None, error=None):
                healthy_id = '00000000-0000-4000-8000-000000003922'
                bad_id = '00000000-0000-4000-8000-000000004922'
                observation = {
                    'stage': stage, 'monotonic': time.monotonic(), 'checks': checks,
                    'source_workspace': (root / 'workspace-id').read_text().strip(),
                    'read_error': str(error) if error else None,
                    'read_error_type': type(error).__name__ if error else None,
                    'scope': 'Read-only private fixture observations; payloads are represented only by byte counts and hashes. Multiple SELECTs are not an atomic snapshot.',
                }
                if db is not None:
                    actor_path = root / 'ordinary-recovered-chat-id'
                    actor = actor_path.read_text() if actor_path.exists() else None
                    observation['recovered_actor'] = actor
                    selected = []
                    for card, owner, kind, content, existence in db.execute(
                            'SELECT card_id, workspace_id, kind_tag, content_json, existence_json FROM cards WHERE card_id IN (?, ?) OR kind_tag=?',
                            (healthy_id, bad_id, 'chat')):
                        # Terminal descriptors use normalized side tables and
                        # legitimately have no inline content_json.
                        source = json.loads(content) if content is not None else {}
                        if card in {healthy_id, bad_id} or (kind == 'chat' and actor and source.get('id') == actor):
                            selected.append({
                                'card_id': card, 'workspace_id': owner, 'kind': kind,
                                'source_actor': source.get('id'),
                                'source_sha256': hashlib.sha256(content.encode()).hexdigest() if content is not None else None,
                                'existence': json.loads(existence),
                            })
                    observation['cards'] = selected
                    ids = {row['card_id'] for row in selected}
                    navigation = []
                    for name, value in db.execute("SELECT key, value FROM metadata WHERE key LIKE 'workspace:%:navigation:%'"):
                        if name.endswith(':chat_drafts'):
                            drafts = json.loads(value)
                            navigation.append({'key': name, 'selected_drafts': {
                                card: {'bytes': len(draft.encode()), 'sha256': hashlib.sha256(draft.encode()).hexdigest()}
                                for card, draft in drafts.items() if card in ids}})
                        elif name.endswith(':quarantined_panes') or name.endswith(':local_only_cards'):
                            navigation.append({'key': name, 'contains_rejected': bad_id in json.loads(value)})
                        elif name.endswith(':threads'):
                            navigation.append({'key': name, 'selected_memberships': [
                                {'channel_id': channel['id'], 'cards': [card for card in channel['cards'] if card in ids]}
                                for channel in json.loads(value) if any(card in ids for card in channel['cards'])]})
                    observation['navigation'] = navigation
                path = out / (label + '-' + stage + '-durability.json')
                path.write_text(json.dumps(observation, indent=2) + '\n')
            if variant == 'after':
                def durable_settled():
                    try:
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            marker_row = db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()
                            draft_key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:chat_drafts'
                            drafts_row = db.execute('SELECT value FROM metadata WHERE key=?', (draft_key,)).fetchone()
                            card_row = db.execute('SELECT existence_json FROM cards WHERE card_id=?', ('00000000-0000-4000-8000-000000004922',)).fetchone()
                            if not marker_row or not card_row or not drafts_row:
                                observe_durability('saved', {'marker_row': bool(marker_row), 'rejected_card_row': bool(card_row),
                                                           'drafts_row': bool(drafts_row)}, db)
                                return False
                            marker = json.loads(marker_row[0])
                            is_closed = 'closed' in str(json.loads(card_row[0])).lower()
                            should_close = mode == 'closed' or ordinary
                            should_mark = hidden or ordinary or mode != 'stale' or payload != 'short'
                            healthy_saved = hidden or json.loads(drafts_row[0]).get('00000000-0000-4000-8000-000000003922') == healthy_text
                            checks = {'healthy_draft_matches': healthy_saved, 'rejected_existence_matches': is_closed == should_close,
                                      'rejected_marker_matches': ('00000000-0000-4000-8000-000000004922' in marker) == should_mark}
                            observe_durability('saved', checks, db)
                            return all(checks.values())
                    except sqlite3.Error as error:
                        observe_durability('saved', {}, error=error)
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
                            scope_key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:active_thread'
                            scope = db.execute('SELECT value FROM metadata WHERE key=?', (scope_key,)).fetchone()
                            settled = (row is not None and 'open' in str(json.loads(row[2])).lower() and drafts is not None
                                       and json.loads(drafts[0]).get(row[0]) == 'Fresh work after ordinary History recovery.')
                            returned = scope is not None and json.loads(scope[0]) == '00000000-0000-4000-8000-000000002922'
                            observe_durability('recovered', {'fresh_recovered_card_and_draft_match': settled,
                                                           'returned_to_saved_channel': returned}, db)
                            return settled and returned
                    except (sqlite3.Error, FileNotFoundError) as error:
                        observe_durability('recovered', {}, error=error)
                        return False
                wait(recovered_durable_settled)
            stop(ui)
            stop(fixture)
            assert icon_config.read_text() == '"Adwaita"\n', 'Private icon setting changed during native interaction'
            expected_existence = 'closed' if variant == 'after' and (mode == 'closed' or ordinary) else 'open'
            with (out / (label + '-typed-persistence.log')).open('w') as log:
                run([str(helper), str(root), mode, '--inspect', expected_existence], env=env, stdout=log, stderr=subprocess.STDOUT)
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
            assert 'UNEXPECTED-' not in events, '\n'.join(
                line for line in events.splitlines() if line.startswith('UNEXPECTED-'))
            receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                             'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                             'events': events.splitlines()})
            assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
            assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
            event({'event': 'arm-completed', 'revision': revision})
            active_arm = None
finally:
    native_failure = sys.exc_info()[1]
    diagnostic_errors = []
    if native_failure is not None and active_arm and window is not None and pid is not None:
        try:
            assert (Path('/proc') / str(pid) / 'exe').resolve() == (target / 'dev-runtime/cagents-ui').resolve()
        except BaseException as error:
            diagnostic_errors.append({'kind': 'ownership', 'error': repr(error)})
        else:
            try:
                run(['import', '-screen', '-window', window,
                     str(out / (active_arm + '-failure-diagnostic.png'))], timeout=5)
            except BaseException as error:
                diagnostic_errors.append({'kind': 'screen', 'error': repr(error)})
            try:
                def diagnostic_tree():
                    assert (Path('/proc') / str(pid) / 'exe').resolve() == (target / 'dev-runtime/cagents-ui').resolve()
                    values = []
                    for node in live_tree(pid):
                        states = node.getState()
                        focused = states.contains(pyatspi.STATE_FOCUSED)
                        editable = states.contains(pyatspi.STATE_EDITABLE)
                        if node.getRole() != pyatspi.ROLE_PUSH_BUTTON and not (focused or editable):
                            continue
                        value = {'name': node.name[:400], 'role': node.getRoleName(), 'focused': focused,
                                 'editable': editable, 'showing': states.contains(pyatspi.STATE_SHOWING)}
                        bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                        value['desktop_bounds'] = [bounds.x, bounds.y, bounds.width, bounds.height]
                        if editable:
                            value['text_preview'] = node.queryText().getText(0, -1)[:256]
                        values.append(value)
                    return values
                (out / (active_arm + '-failure-accessibility.json')).write_text(
                    json.dumps(accessibility_snapshot(diagnostic_tree), indent=2) + '\n')
            except BaseException as error:
                diagnostic_errors.append({'kind': 'accessibility', 'error': repr(error)})
        try:
            event({'event': 'failure-diagnostics', 'errors': diagnostic_errors})
        except BaseException as error:
            diagnostic_errors.append({'kind': 'event', 'error': repr(error)})
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
    (out / 'native-outcome.json').write_text(json.dumps({
        'outcome': 'failed' if native_failure is not None or cleanup_errors else 'complete',
        'incomplete_arm': active_arm, 'error_type': type(native_failure).__name__ if native_failure else None,
        'error': str(native_failure)[:2000] if native_failure else None, 'cleanup_errors': cleanup_errors,
        'diagnostic_errors': diagnostic_errors,
    }, indent=2) + '\n')
    (out / 'accessibility-retries.json').write_text(json.dumps({'unknown_object_retries': stale_accessibility_retries,
        'incomplete_tree_retries': incomplete_accessibility_retries,
        'scope': 'Bounded whole-query retry for AT-SPI Unknown object or explicit missing-child snapshots; no input gesture is retried.'}, indent=2) + '\n')
    if not cleanup_errors:
        (out / 'cleanup-complete').write_text('Owned descendant families observed inactive; private native fixture closed.\n')
    (out / 'native-receipts.json').write_text(json.dumps(receipts, indent=2) + '\n')
    (out.parent / 'sha256.txt').write_text(''.join(f'{digest(path)}  {path.relative_to(out.parent)}\n'
        for path in sorted(out.parent.rglob('*')) if path.is_file() and path.name != 'sha256.txt'))
    if cleanup_errors:
        raise RuntimeError('Native process cleanup is unqualified: ' + '; '.join(cleanup_errors))
