"""Temporary hosted-X11 actual native evidence; never production-provider evidence."""
import ast
import ctypes
import difflib
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import traceback

import pyatspi
from gi.repository import GLib
from PIL import Image

control, before, after, before_main, before_image = map(Path, sys.argv[1:6])
out = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'native'
if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted' or 'CAGENTS_AGENT_AUTHORITY' in os.environ:
    raise RuntimeError('A genuine GitHub-hosted User context is required; inherited authority is unchanged')
if os.environ.get('WAYLAND_DISPLAY') or os.environ.get('WAYLAND_SOCKET'):
    raise RuntimeError('Hosted X11 qualification refuses inherited Wayland connections')
out.mkdir(parents=True, exist_ok=True)
target = Path(os.environ['CARGO_TARGET_DIR'])
source = control / '.github/qualification/native-fixture.rs'
review_exercise = runpy.run_path(str(control / '.github/qualification/review-native.py'))['exercise']
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


def private_inspection_file(root, path):
    assert root.is_absolute() and root.resolve() == root
    assert path.is_absolute() and path != root and path.is_relative_to(root)
    current = root
    for part in (None, *path.relative_to(root).parts):
        if part is not None:
            assert part not in ('', '.', '..')
            current = current / part
        info = current.lstat()
        assert info.st_uid == os.getuid() and not stat.S_ISLNK(info.st_mode)
        if current == path:
            assert stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600
            assert info.st_nlink == 1
        else:
            assert stat.S_ISDIR(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o700
    return {'path': str(path), 'device': info.st_dev, 'inode': info.st_ino,
            'uid': info.st_uid, 'mode': stat.S_IMODE(info.st_mode), 'links': info.st_nlink,
            'bytes': info.st_size, 'sha256': digest(path)}


def inspection_database_snapshot(root, path, artifact):
    identity = private_inspection_file(root, path)
    with sqlite3.connect('file:' + str(path) + '?mode=ro', uri=True, timeout=.2) as db:
        db.execute('BEGIN')
        sql = list(db.iterdump())
        schema = list(db.execute('SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name, tbl_name'))
        pragmas = {name: db.execute('PRAGMA ' + name).fetchone()[0]
                   for name in ('user_version', 'schema_version', 'application_id', 'journal_mode')}
        metadata = dict(db.execute('SELECT key, value FROM metadata'))
        tables = {}
        for name, in db.execute("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name"):
            identifier = '"' + name.replace('"', '""') + '"'
            tables[name] = [[{'blob_hex': value.hex()} if isinstance(value, bytes) else value
                             for value in row] for row in db.execute('SELECT * FROM ' + identifier)]
    assert private_inspection_file(root, path) == identity, 'Database identity changed during inspection snapshot'
    artifact.write_text('\n'.join(sql) + '\n')
    return {'identity': identity, 'sql': sql, 'schema': schema, 'pragmas': pragmas,
            'metadata': metadata, 'tables': tables, 'dump': str(artifact), 'dump_sha256': digest(artifact)}


def create_inspection_clone(root, original, clone):
    original_identity = private_inspection_file(root, original)
    directory = root / 'inspection'
    directory.mkdir(mode=0o700, exist_ok=True)
    assert clone in (directory / 'typed.sqlite3', directory / 'refusal.sqlite3')
    directory_info = directory.lstat()
    assert stat.S_ISDIR(directory_info.st_mode) and stat.S_IMODE(directory_info.st_mode) == 0o700
    assert directory_info.st_uid == os.getuid()
    fd = os.open(clone, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    os.close(fd)
    clone_identity = private_inspection_file(root, clone)
    assert (clone_identity['device'], clone_identity['inode']) != (original_identity['device'], original_identity['inode'])
    deadline = time.monotonic() + 20
    def progress(status, remaining, total):
        if time.monotonic() >= deadline:
            raise TimeoutError('Private SQLite inspection backup exceeded its deadline')
    with sqlite3.connect('file:' + str(original) + '?mode=ro', uri=True, timeout=.2) as source_db:
        with sqlite3.connect(clone, timeout=.2) as clone_db:
            source_db.backup(clone_db, pages=128, progress=progress, sleep=.05)
            # StateStore configures WAL before checking the schema. Prepare
            # only the clone now, so a refused reader has no profile to change.
            assert source_db.execute('PRAGMA journal_mode').fetchone()[0] == 'wal'
            assert clone_db.execute('PRAGMA journal_mode=WAL').fetchone()[0] == 'wal'
    assert private_inspection_file(root, original) == original_identity


def invoke_snapshot_inspector(helper, root, mode, existence, env, clone, log, refusal):
    args = [str(helper), str(root), mode, '--inspect', existence,
            '--inspection-database', str(clone)]
    if refusal:
        result = subprocess.run(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=20)
        log.write_bytes(result.stdout)
        return result
    with log.open('w') as stream:
        return run(args, env=env, stdout=stream, stderr=subprocess.STDOUT)


def inspect_saved_snapshot(helper, root, mode, existence, env, label, *, refusal=False):
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ or 'CAGENTS_AGENT_AUTHORITY' in env:
        raise RuntimeError('Inherited caller authority is present; inspection refused')
    original = Path((root / 'database-path').read_text())
    kind = 'refusal' if refusal else 'typed'
    clone = root / 'inspection' / (kind + '.sqlite3')
    prefix = label + '-' + kind + '-inspection'
    log = out / (label + ('-v13-reader-refusal.log' if refusal else '-typed-persistence.log'))
    snapshots, audit_errors = {}, []
    audit = {'kind': kind, 'helper': str(helper), 'helper_sha256': digest(helper),
             'original': str(original), 'clone': str(clone), 'environment_changed': False,
             'argv': [str(helper), str(root), mode, '--inspect', existence, '--inspection-database', str(clone)],
             'snapshot_transport': 'SQLite mode=ro source backup; clone-only WAL profile preparation before clone-before receipt. Source must already use WAL; user_version is never changed.',
             'root_argument': str(root), 'scope': 'Original SQLite reads and backup are read-only. Typed decoding opens a fresh writable clone; its exact startup writes are audited.'}
    result = None
    try:
        snapshots['original-before'] = inspection_database_snapshot(root, original, out / (prefix + '-original-before.sql'))
        create_inspection_clone(root, original, clone)
        snapshots['clone-before'] = inspection_database_snapshot(root, clone, out / (prefix + '-clone-before.sql'))
        before, copied = snapshots['original-before'], snapshots['clone-before']
        assert copied['sql'] == before['sql'] and copied['schema'] == before['schema']
        assert all(copied['pragmas'][name] == before['pragmas'][name]
                   for name in ('user_version', 'application_id', 'journal_mode'))
        assert copied['identity']['path'] != before['identity']['path']
        assert (copied['identity']['device'], copied['identity']['inode']) != (before['identity']['device'], before['identity']['inode'])
        if refusal:
            assert copied['pragmas']['user_version'] == 14
        audit['invocation_started_unix_ms'] = time.time_ns() // 1_000_000
        result = invoke_snapshot_inspector(helper, root, mode, existence, env, clone, log, refusal)
        audit['invocation_finished_unix_ms'] = time.time_ns() // 1_000_000
        audit['exit_code'] = result.returncode
        if refusal:
            assert result.returncode != 0 and b'UnsupportedSchema' in result.stdout
            assert re.search(rb'(?m)^Error: UnsupportedSchema \{ found: 14, supported: 13 \}\r?$', result.stdout)
        else:
            assert result.returncode == 0
    finally:
        operation_error = sys.exc_info()[1]
        audit.setdefault('invocation_finished_unix_ms', time.time_ns() // 1_000_000)
        if operation_error is not None:
            audit['operation_error'] = {'type': type(operation_error).__name__, 'message': str(operation_error)}
            if isinstance(operation_error, subprocess.CalledProcessError):
                audit['exit_code'] = operation_error.returncode
        for name, path in [('clone-after', clone), ('original-after', original)]:
            try:
                snapshots[name] = inspection_database_snapshot(root, path, out / (prefix + '-' + name + '.sql'))
            except BaseException as error:
                audit_errors.append(name + ': ' + repr(error))
        try:
            before, after = snapshots['original-before'], snapshots['original-after']
            (out / (prefix + '-original-delta.diff')).write_text(''.join(difflib.unified_diff(
                [line + '\n' for line in before['sql']], [line + '\n' for line in after['sql']],
                fromfile='original-before.sql', tofile='original-after.sql')))
            assert before['sql'] == after['sql'], 'Original SQL changed'
            assert before['identity'] == after['identity'], 'Original file/hash/inode changed'
            assert before['schema'] == after['schema'] and before['pragmas'] == after['pragmas'], 'Original schema/header changed'
        except BaseException as error:
            audit_errors.append('original immutability: ' + repr(error))
        if 'clone-before' in snapshots and 'clone-after' in snapshots:
            before, after = snapshots['clone-before'], snapshots['clone-after']
            (out / (prefix + '-clone-delta.diff')).write_text(''.join(difflib.unified_diff(
                [line + '\n' for line in before['sql']], [line + '\n' for line in after['sql']],
                fromfile='clone-before.sql', tofile='clone-after.sql')))
            try:
                assert before['schema'] == after['schema'] and before['pragmas'] == after['pragmas'], 'Clone schema/header changed'
                assert all(before['identity'][key] == after['identity'][key]
                           for key in ('path', 'device', 'inode', 'uid', 'mode', 'links')), 'Clone identity changed'
                if refusal:
                    assert before['sql'] == after['sql'] and before['identity'] == after['identity'], 'Refusal clone changed'
                else:
                    expected = before['metadata'].copy()
                    assert expected['last_boot_id'] == Path('/proc/sys/kernel/random/boot_id').read_text().strip(), 'Inspection boot changed'
                    startup = after['metadata']['last_startup_unix_ms']
                    assert isinstance(startup, str) and startup.isascii() and startup.isdecimal()
                    assert audit['invocation_started_unix_ms'] <= int(startup) <= audit['invocation_finished_unix_ms']
                    assert expected['clean_shutdown'] in ('0', '1')
                    expected.update(clean_shutdown='0', last_startup_unix_ms=startup)
                    assert after['metadata'] == expected, 'Unexpected clone metadata normalization'
                    assert {key: rows for key, rows in before['tables'].items() if key != 'metadata'} == {
                        key: rows for key, rows in after['tables'].items() if key != 'metadata'}, 'Clone payload/card normalization'
            except BaseException as error:
                audit_errors.append('clone audit: ' + repr(error))
        audit['snapshots'] = {name: {key: value for key, value in snapshot.items() if key not in ('sql', 'tables')}
                              for name, snapshot in snapshots.items()}
        audit['errors'] = audit_errors
        audit['outcome'] = 'failed' if operation_error is not None or audit_errors else 'complete'
        (out / (prefix + '.json')).write_text(json.dumps(audit, indent=2) + '\n')
        if audit_errors:
            raise RuntimeError('Private inspection snapshot audit failed: ' + '; '.join(audit_errors)) from operation_error


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


def capture(window, name, size, *, neutral_pointer=False):
    global viewport
    run(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
    run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
    viewport = size
    if neutral_pointer:
        # Match comparison frames without hovering unrelated navigation. This
        # moves only the pointer; keyboard focus and all gestures stay intact.
        # An outer-border leave can retain the previous client hover. The
        # fixture has no provider chips in the middle of its 24px status strip.
        point = (size[0] // 2, size[1] - 13)
        focus = subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5).strip()
        assert int(subprocess.check_output(['xdotool', 'getwindowpid', focus], text=True, timeout=5)) == pid
        run(['xdotool', 'mousemove', '--sync', *map(str, point)])
        location = dict(line.split('=', 1) for line in subprocess.check_output(
            ['xdotool', 'getmouselocation', '--shell'], text=True, timeout=5).splitlines())
        assert (int(location['X']), int(location['Y'])) == point
        assert subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5).strip() == focus
        event({'event': 'comparison-pointer-parked', 'capture': name, 'point': list(point),
               'actual_pointer': location, 'unchanged_input_focus': focus, 'focus_pid': pid})
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
            tooltip_free = True
            if name in ('before-terminal-closed-empty-wide', 'after-terminal-closed-empty-wide'):
                # Both regions are empty backgrounds in this fixed fixture.
                # The previously observed Work/Search tips occupy these pixels
                # and lack an accessibility tooltip node. Observe full frames;
                # never alter pixels or retry an input gesture to hide them.
                assert size == (1280, 800)
                metrics['comparison_tooltip_regions'] = []
                for label, box, reference in [
                    ('work', (1016, 94, 1264, 105), (1260, 132)),
                    ('search', (164, 94, 212, 105), (240, 100)),
                ]:
                    color = rgb.getpixel(reference)
                    delta = max(max(abs(a - b) for a, b in zip(pixel, color))
                                for pixel in rgb.crop(box).getdata())
                    metrics['comparison_tooltip_regions'].append({
                        'label': label, 'box': list(box), 'reference': list(reference),
                        'maximum_background_delta': delta, 'tolerance': 8})
                    tooltip_free &= delta <= 8
            (out / (name + '-frame.json')).write_text(json.dumps(metrics, indent=2) + '\n')
            return diverse and lit > .01 and metrics['luminance_range'] >= 16 and tooltip_free
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


def copy_exact(pid, name, expected, keyboard=False, gesture=None):
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
        if gesture is None:
            click(pid, name, keyboard=keyboard)
        else:
            gesture()
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


def image_only_observation(root, label, phase, *, sources=None, image=None, retired=False,
                           before_loss=False, read=None, previous_generation=-1):
    """One actual read-only transaction; no caller supplies a saved image."""
    healthy = '00000000-0000-4000-8000-000000003922'
    bad = '00000000-0000-4000-8000-000000004922'
    channel = '00000000-0000-4000-8000-000000002922'
    workspace = (root / 'workspace-id').read_text().strip()
    database = Path((root / 'database-path').read_text())
    private_inspection_file(root, database)
    prefix = 'workspace:' + workspace + ':navigation:'
    def observe():
        try:
            with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                db.execute('BEGIN')
                row = db.execute('SELECT generation FROM workspaces WHERE workspace_id=?', (workspace,)).fetchone()
                cards = {card: {'workspace': owner, 'kind': kind, 'source': json.loads(content),
                                'existence': json.loads(existence)}
                         for card, owner, kind, content, existence in db.execute(
                             'SELECT card_id,workspace_id,kind_tag,content_json,existence_json FROM cards WHERE card_id IN (?,?)',
                             (healthy, bad))}
                values = {key.removeprefix(prefix): json.loads(value) for key, value in db.execute(
                    'SELECT key,value FROM metadata WHERE key LIKE ?', (prefix + '%',))}
                required = ('threads', 'active_thread', 'chat_drafts', 'chat_draft_attachments',
                            'pending_chat_admissions', 'quarantined_panes', 'local_only_cards')
                if not row or row[0] <= previous_generation or set(cards) != {healthy, bad} or not all(key in values for key in required):
                    return None
                owners = [owner for owner in values['threads'] if owner['id'] == channel]
                if len(owners) != 1:
                    return None
                owner = owners[0]
                current_sources = {card: entry['source'] for card, entry in cards.items()}
                if sources is not None and current_sources != sources:
                    raise AssertionError('Image-only exact saved sources changed')
                if any(entry['workspace'] != workspace or entry['kind'] != 'chat' for entry in cards.values()):
                    raise AssertionError('Image-only card ownership changed')
                if cards[bad]['existence'] != 'open' or values['active_thread'] != channel:
                    return None
                if values['quarantined_panes'] != {bad: dict(kind='chat', **cards[bad]['source'])}:
                    return None
                if values['pending_chat_admissions'] != {bad: cards[bad]['source']} or values['chat_drafts'] != {}:
                    return None
                if bad in values['local_only_cards'] or bad in values['chat_draft_attachments']:
                    raise AssertionError('Rejected sibling gained local or image custody')
                expected_members = [bad] if retired and before_loss else [healthy, bad]
                if owner['cards'] != expected_members:
                    return None
                if retired and before_loss:
                    existence = cards[healthy]['existence']
                    if not isinstance(existence, dict) or set(existence) != {'closed'} or values['local_only_cards'] != [] or values['chat_draft_attachments'] != {}:
                        return None
                else:
                    if cards[healthy]['existence'] != 'open' or set(values['local_only_cards']) != ({healthy} if retired else set()):
                        return None
                    images = values['chat_draft_attachments']
                    if phase != 'bootstrap':
                        if set(images) != {healthy} or len(images[healthy]) != 1:
                            return None
                        if image is not None and images[healthy] != [image]:
                            raise AssertionError('Exact managed-image association changed')
                    elif images:
                        raise AssertionError('Bootstrap unexpectedly contains a draft image')
                if read is not None and owner['title'] != 'Image custody check ' + str(read['sequence']):
                    return None
                offered = (root / 'image-only-offered.png').read_bytes()
                managed = None
                if phase != 'bootstrap':
                    actual_image = image if image is not None else values['chat_draft_attachments'][healthy][0]
                    path = Path(actual_image['path'])
                    identity = private_inspection_file(root, path)
                    data = path.read_bytes()
                    assert path.is_relative_to(root / 'data') and data == offered, 'Managed PNG bytes changed'
                    managed = {'attachment': actual_image, 'identity': identity, 'bytes': len(data),
                               'sha256': hashlib.sha256(data).hexdigest()}
                dump = out / (label + '-' + phase + '-storage.sql')
                dump.write_text('\n'.join(db.iterdump()) + '\n')
                value = {'phase': phase, 'database': str(database), 'workspace': workspace,
                         'workspace_generation': row[0], 'cards': cards, 'navigation': values,
                         'read': read, 'dump': dump.name, 'dump_sha256': digest(dump),
                         'offered_png': {'bytes': len(offered), 'sha256': hashlib.sha256(offered).hexdigest()},
                         'managed_asset': managed,
                         'scope': 'Read-only SQLite transaction including committed WAL; storage reload is not a cold native UI restart.'}
                (out / (label + '-' + phase + '-storage.json')).write_text(json.dumps(value, indent=2) + '\n')
                event({'event': 'image-only-storage-phase', 'phase': phase, 'workspace_generation': row[0],
                       'read': read, 'receipt': label + '-' + phase + '-storage.json'})
                return value
        except (sqlite3.Error, FileNotFoundError):
            return None
    return wait(observe)


def exercise_image_only(pid, window, root, label, receipt, before_loss):
    global viewport
    ready = 'Verified conversation ready for image-only retirement.'
    wait(lambda: accessibility_snapshot(lambda: any(ready in node.name and visible(node) for node in live_tree(pid))))
    wait(lambda: any(node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid)))
    def binding():
        # CardSurface and IdContainer forward/join their children in the
        # actual accessibility tree. Bind A's real private-source header and
        # named resize edge to its column, then the actual Entry and Ready
        # sentinel inside that column; do not invent an ancestor/widget ID.
        nodes = live_tree(pid)
        directory = str(root.parent / 'project')
        headers = [node for node in nodes if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                   and node.name.startswith('Chat pane ') and ', ' + directory + ', ' in node.name
                   and not node.name.startswith('Chat pane Unavailable saved pane, ')]
        assert len(headers) <= 1, 'Only admitted A may have an available Chat header'
        if not headers:
            return None
        header = headers[0]
        title, suffix = header.name.removeprefix('Chat pane ').split(', ' + directory + ', ', 1)
        assert suffix in ('focused', 'not focused')
        assert title in ('Saved recovery model received', 'Image retirement received') or re.fullmatch(r'Image custody check [0-9]+', title)
        edges = [node for node in nodes if node.getRole() == pyatspi.ROLE_SEPARATOR and visible(node)
                 and node.name == 'Resize Chat pane ' + title]
        sidebars = [node for node in nodes if node.getRole() == pyatspi.ROLE_SEPARATOR and visible(node)
                    and node.name == 'Resize sidebar']
        if len(edges) != 1 or len(sidebars) != 1:
            return None
        edge = edges[0].queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        sidebar = sidebars[0].queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        left, right = sidebar.x + sidebar.width, edge.x
        if not 0 <= left < right <= viewport[0]:
            return None
        def in_column(node):
            bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
            return (bounds.width > 0 and bounds.height > 0 and left <= bounds.x
                    and bounds.x + bounds.width <= right and 0 <= bounds.y
                    and bounds.y + bounds.height <= viewport[1])
        fields = [node for node in nodes if node.getRole() == pyatspi.ROLE_ENTRY
                  and node.name == 'Message' and visible(node) and in_column(node)
                  and node.getState().contains(pyatspi.STATE_EDITABLE)
                  and node.getState().contains(pyatspi.STATE_ENABLED)]
        sentinels = [node for node in nodes if ready in node.name and visible(node) and in_column(node)]
        assert len(fields) <= 1, 'A column must expose exactly one native Message Entry'
        assert len(sentinels) <= 1, 'A column must expose one unique Ready sentinel'
        if len(fields) != 1 or len(sentinels) != 1:
            return None
        field = fields[0]
        bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        header_bounds = header.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        return {'field': field, 'header': header.name, 'header_role': header.getRoleName(),
                'header_bounds': [header_bounds.x, header_bounds.y, header_bounds.width, header_bounds.height],
                'column': [left, right], 'resize_anchor': edges[0].name,
                'editor_role': field.getRoleName(), 'editor_name': field.name,
                'editor_bounds': [bounds.x, bounds.y, bounds.width, bounds.height]}
    def composer():
        value = binding()
        return value['field'] if value else None
    def no_pending_paste():
        return not any(node.name == 'Pasting…' and visible(node) for node in live_tree(pid))
    def input_receipt(phase, expected):
        def read():
            value = binding()
            if value is None:
                return None
            field = value.pop('field')
            field.clearCache()
            assert field.queryText().getText(0, -1) == expected
            assert field.getState().contains(pyatspi.STATE_FOCUSED)
            assert no_pending_paste()
            return value
        value = wait(lambda: accessibility_snapshot(read))
        focus = int(subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5).strip())
        focus_pid = int(subprocess.check_output(['xdotool', 'getwindowpid', str(focus)], text=True, timeout=5).strip())
        assert focus == int(window) and focus_pid == pid, 'Native key input left the owned A window'
        event({'event': 'image-only-key-target', 'phase': phase, 'window': focus, 'pid': focus_pid,
               'editor_text': expected, 'pending_paste_visible': False, **value})
    def chip():
        value = binding()
        if value is None:
            return False
        left, right = value['column']
        def in_column(node):
            bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
            return (left <= bounds.x and bounds.x + bounds.width <= right and bounds.width > 0
                    and 0 <= bounds.y and bounds.y + bounds.height <= viewport[1] and bounds.height > 0)
        nodes = live_tree(pid)
        controls = [node for node in nodes if node.name == 'Remove attachment'
                    and node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node) and in_column(node)]
        assert len(controls) <= 1
        return len(controls) == 1 and any(node.name == 'Image 1' and visible(node) and in_column(node) for node in nodes)
    stamp = time.localtime(1_700_000_000)
    stamp_labels = {
        'message-time': time.strftime('%Y-%m-%d %H:%M', stamp),
        'finished-time': 'Finished at ' + time.strftime('%H:%M', stamp),
    }
    def tooltip_targets():
        value = binding()
        if value is None:
            return None
        left, right = value['column']
        nodes = live_tree(pid)
        result = {}
        for kind, name, role in [
            ('image', 'Image 1', pyatspi.ROLE_PARAGRAPH),
            ('remove', 'Remove attachment', pyatspi.ROLE_PUSH_BUTTON),
            ('copy', 'Copy this message', pyatspi.ROLE_PUSH_BUTTON),
            *[(kind, name, pyatspi.ROLE_PARAGRAPH) for kind, name in stamp_labels.items()],
        ]:
            matches = []
            for node in nodes:
                if node.name != name or node.getRole() != role or not visible(node):
                    continue
                bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                if (left <= bounds.x and bounds.x + bounds.width <= right
                        and bounds.width > 0 and bounds.height > 0 and bounds.y >= 0
                        and bounds.y + bounds.height <= viewport[1]):
                    matches.append((node, bounds))
            assert len(matches) <= 1, ('Ambiguous native tooltip owner', kind)
            if len(matches) != 1:
                return None
            node, bounds = matches[0]
            result[kind] = {'name': name, 'role': node.getRoleName(),
                            'enabled': node.getState().contains(pyatspi.STATE_ENABLED),
                            'bounds': [bounds.x, bounds.y, bounds.width, bounds.height]}
        assert result['remove']['enabled'] and result['copy']['enabled']
        return result
    def tooltip_evidence(expected, *, timestamps):
        global viewport
        # This adds pointer-only diagnostic frames to the original custody
        # flow. Never refocus, activate a control, repeat paste, or delete the
        # marker here. Native IDs can legitimately change on a fresh frame;
        # each target is rebound to A's actual source/column/editor instead.
        wait(lambda: receipt.read_text().splitlines().count('explicit-read-started')
             == receipt.read_text().splitlines().count('explicit-read-replied'))
        def observed_requests():
            return [line for line in receipt.read_text().splitlines()
                    if line.startswith(('effect-request-', 'explicit-read-', 'image-read-delivered '))]
        effects = observed_requests()
        phase = 'marked' if expected else 'empty'
        for size, suffix in [((1280, 800), 'wide'), ((900, 700), 'narrow')]:
            prefix = label + '-tooltip-' + phase
            run(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
            run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
            viewport = size
            targets = wait(lambda: accessibility_snapshot(tooltip_targets))
            input_receipt('tooltip-' + phase + '-ready-' + suffix, expected)
            capture(window, prefix + '-rest-' + suffix, size, neutral_pointer=True)
            input_receipt('tooltip-' + phase + '-rest-' + suffix, expected)
            targets = wait(lambda: accessibility_snapshot(tooltip_targets))
            event({'event': 'native-tooltip-rest', 'phase': phase, 'viewport': list(size),
                   'targets': targets, 'editor_text': expected, 'capture': prefix + '-rest-' + suffix})
            kinds = ['image', 'remove', 'message-time', 'finished-time'] if timestamps else []
            for kind in kinds:
                targets = wait(lambda: accessibility_snapshot(tooltip_targets))
                owner = targets[kind]
                x, y, width, height = owner['bounds']
                point = (x + width // 2, y + height // 2)
                if kind == 'image':
                    rx, ry, rw, rh = targets['remove']['bounds']
                    assert not (rx <= point[0] < rx + rw and ry <= point[1] < ry + rh)
                entered = time.monotonic()
                run(['xdotool', 'mousemove', '--sync', *map(str, point)])
                location = dict(line.split('=', 1) for line in subprocess.check_output(
                    ['xdotool', 'getmouselocation', '--shell'], text=True, timeout=5).splitlines())
                assert (int(location['X']), int(location['Y'])) == point
                time.sleep(.5)  # Established tooltip: 300ms dwell plus 120ms fade.
                input_receipt('tooltip-' + phase + '-' + kind + '-dwell-' + suffix, expected)
                name = prefix + '-' + kind + '-dwell-' + suffix
                capture(window, name, size)  # Keep the pointer on the owner.
                input_receipt('tooltip-' + phase + '-' + kind + '-captured-' + suffix, expected)
                actual_pointer = dict(line.split('=', 1) for line in subprocess.check_output(
                    ['xdotool', 'getmouselocation', '--shell'], text=True, timeout=5).splitlines())
                assert (int(actual_pointer['X']), int(actual_pointer['Y'])) == point
                def captured_owner():
                    value = binding()
                    if value is None:
                        return None
                    left, right = value['column']
                    owners = []
                    for node in live_tree(pid):
                        if node.name != owner['name'] or node.getRoleName() != owner['role'] or not visible(node):
                            continue
                        bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                        rectangle = [bounds.x, bounds.y, bounds.width, bounds.height]
                        if (rectangle == owner['bounds'] and left <= bounds.x
                                and bounds.x + bounds.width <= right):
                            owners.append({'name': node.name, 'role': node.getRoleName(), 'bounds': rectangle,
                                           'enabled': node.getState().contains(pyatspi.STATE_ENABLED)})
                    assert len(owners) <= 1, 'The captured tooltip owner became ambiguous'
                    return owners[0] if owners else None
                observed_owner = wait(lambda: accessibility_snapshot(captured_owner))
                assert observed_owner == owner, 'Capture moved or changed the actual tooltip owner'
                assert chip() and no_pending_paste()
                event({'event': 'native-tooltip-dwell', 'phase': phase, 'kind': kind,
                       'viewport': list(size), 'owner': owner, 'pointer': location,
                       'captured_owner': observed_owner, 'captured_pointer': actual_pointer,
                       'entered_monotonic': entered, 'dwell_seconds': time.monotonic() - entered,
                       'editor_text': expected, 'capture': name,
                       'capture_sha256': digest(out / (name + '.webp')),
                       'scope': 'Actual pixels; nested attachment/remove tooltip coexistence requires independent UX review.'})
                exit_name = prefix + '-' + kind + '-exit-' + suffix
                capture(window, exit_name, size, neutral_pointer=True)
                input_receipt('tooltip-' + phase + '-' + kind + '-exit-' + suffix, expected)
                assert wait(lambda: accessibility_snapshot(tooltip_targets))
            assert observed_requests() == effects, 'Tooltip observation issued or received an extra model/provider request'
        # Restore the original custody flow's viewport without touching focus.
        capture(window, label + '-tooltip-' + phase + '-returned-wide', (1280, 800), neutral_pointer=True)
        input_receipt('tooltip-' + phase + '-returned-wide', expected)
        assert observed_requests() == effects, 'Returning the tooltip viewport changed model/provider requests'
    wait(lambda: (field := composer()) is not None and field.queryText().getText(0, -1) == '')
    bootstrap = image_only_observation(root, label, 'bootstrap')
    sources = {card: value['source'] for card, value in bootstrap['cards'].items()}
    assert sources['00000000-0000-4000-8000-000000003922']['id'] == '00000000-0000-4000-8000-000000005922'
    assert sources['00000000-0000-4000-8000-000000004922']['id'] == '00000000-0000-4000-8000-000000006922'
    png = (root / 'image-only-offered.png').read_bytes()
    assert png.startswith(b'\x89PNG\r\n\x1a\n') and len(png) < 32768
    (out / (label + '-offered.png')).write_bytes(png)
    owner = subprocess.Popen(['xclip', '-selection', 'clipboard', '-target', 'image/png', '-in', '-quiet'],
                             stdin=subprocess.PIPE, start_new_session=True)
    children.append(owner)
    owner.stdin.write(png)
    owner.stdin.close()
    def offered():
        assert owner.poll() is None, 'Owned PNG selection exited before paste'
        result = subprocess.run(['xclip', '-selection', 'clipboard', '-out', '-target', 'image/png'],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
        return result.returncode == 0 and result.stdout == png
    wait(offered)
    targets = subprocess.run(['xclip', '-selection', 'clipboard', '-out', '-target', 'TARGETS'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5)
    assert targets.returncode == 0 and b'image/png' in targets.stdout.splitlines()
    names = targets.stdout.decode('ascii').splitlines()
    assert not any(name.upper() in ('UTF8_STRING', 'STRING', 'TEXT') or name.lower().startswith('text/') for name in names), 'PNG owner also offered a text fallback'
    event({'event': 'image-only-png-offered', 'selection_owner_pid': owner.pid, 'mime': 'image/png',
           'bytes': len(png), 'sha256': hashlib.sha256(png).hexdigest(), 'targets': targets.stdout.decode()})
    def image_editor_bounds():
        field = composer()
        if field is None:
            return None
        bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
        if (bounds.width > 0 and bounds.height > 0 and bounds.x >= 0 and bounds.y >= 0
                and bounds.x + bounds.width <= viewport[0] and bounds.y + bounds.height <= viewport[1]):
            return bounds.x, bounds.y, bounds.width, bounds.height
        return None
    x, y, width, height = wait(image_editor_bounds)
    event({'event': 'image-only-editor-pointer', 'bounds': [x, y, width, height]})
    run(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])
    wait(lambda: (field := composer()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED))
    input_receipt('before-paste', '')
    run(['xdotool', 'key', '--clearmodifiers', 'ctrl+v'])
    wait(lambda: (field := composer()) is not None and field.queryText().getText(0, -1) == '[Image 1] ' and chip())
    event({'event': 'image-only-native-paste-settled', 'editor': '[Image 1] ', 'image_label': 'Image 1'})
    assert offered()
    tooltip_evidence('[Image 1] ', timestamps=True)
    # Delete only the marker, with the exact A editor still focused. A real
    # managed chip remains; neither unsent text nor an invalid path masks 1955.
    wait(no_pending_paste)
    input_receipt('before-marker-delete', '[Image 1] ')
    run(['xdotool', 'key', '--clearmodifiers', 'ctrl+a', 'BackSpace'])
    wait(lambda: (field := composer()) is not None and field.queryText().getText(0, -1) == '' and chip())
    wait(no_pending_paste)
    event({'event': 'image-only-paste-settled', 'phase': 'marker-deleted', 'pending_paste_visible': False})
    event({'event': 'image-only-native-marker-deleted', 'editor': '', 'image_label': 'Image 1'})
    tooltip_evidence('', timestamps=False)
    stop(owner)
    def guarded_check(phase, *, retired, image=None, previous_generation):
        wait(lambda: receipt.read_text().splitlines().count('explicit-read-started')
             == receipt.read_text().splitlines().count('explicit-read-replied'))
        lines = receipt.read_text().splitlines()
        reads = sum(line.startswith('image-read-delivered ') for line in lines)
        starts = lines.count('explicit-read-started')
        click(pid, 'Check session again')
        def new_read():
            rows = [line for line in receipt.read_text().splitlines() if line.startswith('image-read-delivered ')]
            assert len(rows) <= reads + 1, 'One guarded click must cause one exact Read'
            if len(rows) != reads + 1:
                return None
            match = re.fullmatch(r'image-read-delivered sequence=([0-9]+) revision=([0-9]+) retired=(true|false)', rows[-1])
            assert match is not None
            value = {'sequence': int(match[1]), 'revision': int(match[2]), 'retired': match[3] == 'true'}
            assert value['retired'] == retired
            return value
        read = wait(new_read)
        wait(lambda: receipt.read_text().splitlines().count('explicit-read-replied') == starts + 1)
        title = 'Open channel Image custody check ' + str(read['sequence']) + ', '
        labels = wait(lambda: accessibility_snapshot(lambda: [node.name for node in live_tree(pid)
            if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name.startswith(title) and visible(node)]))
        assert len(labels) == 1
        wait(lambda: any(node.name == 'Check session again' and visible(node)
                         and node.getState().contains(pyatspi.STATE_ENABLED) for node in live_tree(pid)))
        event({'event': 'image-only-read-processed', 'phase': phase, 'read': read, 'sidebar_label': labels[0]})
        return image_only_observation(root, label, phase, sources=sources, image=image, retired=retired,
                                      before_loss=before_loss, read=read, previous_generation=previous_generation)
    pre = guarded_check('pre-remove', retired=False, previous_generation=bootstrap['workspace_generation'])
    healthy = '00000000-0000-4000-8000-000000003922'
    image = pre['navigation']['chat_draft_attachments'][healthy][0]
    reference = image['reference']
    assert set(image) == {'path', 'reference'}
    assert set(reference) == {'id', 'number', 'device', 'name', 'sha256', 'extension'}
    assert reference['number'] == 1 and reference['name'] == 'Pasted image' and reference['extension'] == 'png'
    assert reference['sha256'] == hashlib.sha256(png).hexdigest()
    image_path = Path(image['path'])
    private_inspection_file(root, image_path)
    assert image_path.is_relative_to(root / 'data') and image_path.read_bytes() == png
    fields = [image['path'], reference['id'], str(reference['number']), reference['device'], reference['name'],
              reference['sha256'], reference['extension']]
    assert all(field and not any(ord(character) < 32 or ord(character) == 127 for character in field) for field in fields)
    observation = root / 'image-only-attachment.txt'
    fd = os.open(observation, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        stream.write('\n'.join(fields) + '\n')
    private_inspection_file(root, observation)
    (out / (label + '-pre-remove-image.json')).write_text(json.dumps({
        'attachment': image, 'png_bytes': len(png), 'png_sha256': hashlib.sha256(png).hexdigest(),
        'observed_after_read': pre['read'], 'observation_sha256': digest(observation),
        'source': sources[healthy], 'scope': 'Actual native paste, then exact saved image observed through read-only SQLite.'}, indent=2) + '\n')
    wait(lambda: (field := composer()) is not None and field.queryText().getText(0, -1) == '' and chip())
    wait(no_pending_paste)
    event({'event': 'image-only-paste-settled', 'phase': 'pre-remove', 'pending_paste_visible': False})
    capture(window, label + '-pre-remove-image-only-wide', (1280, 800), neutral_pointer=True)
    (root / 'image-remove').touch()
    event({'event': 'image-only-removal-triggered', 'input': 'External exact-scope watcher; no application input'})
    wait(lambda: any(line.startswith('image-removal-event-delivered ') for line in receipt.read_text().splitlines()))
    wait(lambda: any(re.fullmatch(r'image-read-delivered sequence=[0-9]+ revision=[0-9]+ retired=true', line)
                     for line in receipt.read_text().splitlines()))
    def post_state():
        nodes = live_tree(pid)
        if before_loss:
            return (not any(visible(node) and (node.name == 'Message' or node.name == 'Remove attachment'
                            or ready in node.name) for node in nodes)
                    and len([node for node in nodes if visible(node) and node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                             and node.name.startswith('Chat pane ')]) == 1)
        return (any(visible(node) and 'Closed in another window.' in node.name for node in nodes)
                and len([node for node in nodes if visible(node) and node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                         and node.name.startswith('Chat pane ')]) == 2
                and (field := composer()) is not None and field.queryText().getText(0, -1) == '' and chip())
    wait(lambda: accessibility_snapshot(post_state))
    first = guarded_check('post-remove-check-1', retired=True, image=image, previous_generation=pre['workspace_generation'])
    second = guarded_check('post-remove-check-2', retired=True, image=image, previous_generation=first['workspace_generation'])
    assert second['read']['sequence'] > first['read']['sequence'] > pre['read']['sequence']
    assert second['read']['revision'] == first['read']['revision'] == pre['read']['revision'] + 1
    assert first['navigation']['threads'][0]['ordinal'] == pre['navigation']['threads'][0]['ordinal']
    for size, suffix in [((1280, 800), 'wide'), ((900, 700), 'narrow')]:
        # Assert at the requested viewport before retaining its paired frame.
        run(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
        run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
        viewport = size
        wait(lambda: accessibility_snapshot(post_state))
        capture(window, label + '-retired-' + suffix, size, neutral_pointer=True)
        wait(lambda: accessibility_snapshot(post_state))
    return {'before_loss': before_loss, 'image': image,
            'phases': ['bootstrap', 'pre-remove', 'post-remove-check-1', 'post-remove-check-2'],
            'scope': 'Synthetic provider transport; native clipboard/ImageSaved and repeated storage saves, not a cold UI restart.'}


def open_ordinary_history(pid):
    plus_name = wait(lambda: next((node.name for node in live_tree(pid)
                                  if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                  and node.name.startswith('New tab · Thread ')
                                  and visible(node)), None))
    click(pid, plus_name)
    click(pid, 'New Resume conversation tab')
    wait(lambda: any(node.name == 'Resume Ordinary retained conversation' and visible(node)
                     for node in live_tree(pid)))


def composition_context_controls(pid):
    """The System Notice is one independently disclosed commentary step."""
    def read():
        controls = []
        for node in live_tree(pid):
            if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name == 'Claude context' and visible(node):
                bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                if (bounds.width > 0 and bounds.height > 0 and 0 <= bounds.x
                        and bounds.x + bounds.width <= viewport[0]
                        and 0 <= bounds.y and bounds.y + bounds.height <= viewport[1]):
                    controls.append((bounds.x, bounds.y, bounds.width, bounds.height))
        assert len(controls) <= 1, 'Composition has exactly one Claude context notice'
        return sorted(controls, key=lambda bounds: bounds[1])
    return accessibility_snapshot(read)


def composition_context_gesture(pid, *, copy=False):
    def target():
        controls = composition_context_controls(pid)
        if not controls:
            return None
        x, y, width, height = controls[0]
        if not copy:
            return (x, y, width, height)
        candidates = []
        for node in live_tree(pid):
            if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name == 'Copy this message' and visible(node):
                bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                if (bounds.x >= x + width and y <= bounds.y + bounds.height // 2 <= y + height
                        and bounds.width > 0 and bounds.height > 0
                        and bounds.x + bounds.width <= viewport[0]):
                    candidates.append((bounds.x, bounds.y, bounds.width, bounds.height))
        assert len(candidates) <= 1, 'Context Copy must belong to the exact context header row'
        return candidates[0] if candidates else None
    bounds = wait(target)
    x, y, width, height = bounds
    event({'event': 'composition-context-gesture', 'copy': copy, 'bounds': list(bounds)})
    run(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])


def observe_composition_created_scope(root, created):
    database = Path((root / 'database-path').read_text())
    assert database.is_relative_to(root)
    key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:active_thread'
    def read():
        try:
            with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                row = db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()
                active = json.loads(row[0]) if row else None
                return {'active_thread': active} if active == created else None
        except (sqlite3.Error, json.JSONDecodeError):
            return None
    observation = wait(read)
    event({'event': 'composition-created-channel-persisted', 'created_channel_id': created,
           'database': str(database), 'metadata_key': key, **observation,
           'scope': 'Read-only private SQLite observation before the New channel reveal capture.'})


def composition_flow(pid, window, root, label):
    user = 'Actual user request: keep my saved work while I open another channel.'
    ready = 'Composition transcript is ready.'
    body = (root / 'composition-context.txt').read_bytes()
    run(['xdotool', 'key', 'ctrl+shift+i'])  # Match the ordinary arms' closed inspector.
    wait(lambda: any(ready in node.name and visible(node) for node in live_tree(pid)))
    wait(lambda: any(user in node.name and visible(node) for node in live_tree(pid)))
    wait(lambda: any(node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid)))
    copy_exact(pid, 'Copy saved draft', (root / 'expected-draft.txt').read_bytes())
    copy_exact(pid, 'Copy saved attachment locations', (root / 'expected-attachment.txt').read_bytes())
    def composer():
        fields = [node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                  and node.getState().contains(pyatspi.STATE_EDITABLE)]
        assert len(fields) <= 1
        return fields[0] if fields else None
    def channel_control(prefix):
        names = [node.name for node in live_tree(pid)
                 if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                 and node.name.startswith(prefix)]
        assert len(names) <= 1, 'Composition navigation must identify one exact channel'
        return names[0] if names else None
    field = wait(composer)
    bounds = field.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    run(['xdotool', 'mousemove', '--sync', str(bounds.x + bounds.width // 2),
         str(bounds.y + bounds.height // 2), 'click', '1'])
    wait(lambda: (field := composer()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED))
    assert accessibility_snapshot(lambda: composer().queryText().getText(0, -1)) == ''
    draft = 'Healthy work survives a new channel and Claude context.'
    run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', draft])
    wait(lambda: composer().queryText().getText(0, -1) == draft)
    (root / 'healthy-draft-expected').write_text(draft)
    assert len(composition_context_controls(pid)) == 1
    assert not accessibility_snapshot(lambda: any('SYNTHETIC_CONTEXT_BEGIN' in node.name
                                                  and visible(node) for node in live_tree(pid)))
    capture(window, label + '-context-collapsed-wide', (1280, 800), neutral_pointer=True)
    (root / 'composition-create-allowed').touch()
    click(pid, 'New channel for project')
    receipt = root / 'receipts.txt'
    wait(lambda: 'composition-ranked-response-delivered' in receipt.read_text().splitlines())
    created = (root / 'composition-channel-id').read_text().strip()
    new_control = wait(lambda: channel_control('Open channel New channel, '))
    wait(lambda: not any(node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid)))
    wait(lambda: (field := composer()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED)
         and field.queryText().getText(0, -1) == '')
    observe_composition_created_scope(root, created)
    event({'event': 'composition-new-channel-revealed', 'channel_id': created, 'control': new_control})
    capture(window, label + '-new-channel-revealed', (1280, 800), neutral_pointer=True)
    saved_control = wait(lambda: channel_control('Open channel Saved recovery model received, '))
    click(pid, saved_control)
    wait(lambda: any(node.name == 'Saved pane unavailable' and visible(node) for node in live_tree(pid)))
    wait(lambda: (field := composer()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED)
         and field.queryText().getText(0, -1) == draft)
    event({'event': 'composition-original-scope-and-draft-restored', 'draft_sha256': hashlib.sha256(draft.encode()).hexdigest()})
    wait(lambda: len(composition_context_controls(pid)) == 1)
    assert not accessibility_snapshot(lambda: any('SYNTHETIC_CONTEXT_BEGIN' in node.name
                                                  and visible(node) for node in live_tree(pid)))
    composition_context_gesture(pid)
    wait(lambda: any('SYNTHETIC_CONTEXT_BEGIN' in node.name and visible(node) for node in live_tree(pid)))
    capture(window, label + '-context-expanded-wide', (1280, 800), neutral_pointer=True)
    copy_exact(pid, 'Copy original Claude context', body,
               gesture=lambda: composition_context_gesture(pid, copy=True))
    capture(window, label + '-context-expanded-narrow', (900, 700), neutral_pointer=True)
    wait(lambda: any('SYNTHETIC_CONTEXT_BEGIN' in node.name and visible(node) for node in live_tree(pid)))
    copy_exact(pid, 'Copy original Claude context narrow', body,
               gesture=lambda: composition_context_gesture(pid, copy=True))
    composition_context_gesture(pid)
    wait(lambda: not any('SYNTHETIC_CONTEXT_BEGIN' in node.name and visible(node) for node in live_tree(pid)))
    assert accessibility_snapshot(lambda: composer().queryText().getText(0, -1)) == draft
    capture(window, label + '-context-collapsed-narrow', (900, 700), neutral_pointer=True)
    # A normal navigation saves composer state; no provider command is sent.
    click(pid, wait(lambda: channel_control('Open channel New channel, ')))
    wait(lambda: (field := composer()) is not None and field.queryText().getText(0, -1) == '')
    click(pid, wait(lambda: channel_control('Open channel Saved recovery model received, ')))
    wait(lambda: (field := composer()) is not None and field.getState().contains(pyatspi.STATE_FOCUSED)
         and field.queryText().getText(0, -1) == draft)


def composition_main_before_flow(pid, window, root, label):
    """Capture the same task on main without inventing its blocked controls."""
    run(['xdotool', 'key', 'ctrl+shift+i'])
    observations = []
    def blocked(target, capture_suffix):
        def read():
            nodes = [node for node in live_tree(pid) if visible(node)]
            names = [node.name for node in nodes]
            pending = [name for name in names if name.startswith('This saved pane has not been reopened.')]
            tabs = [node for node in nodes if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                    and node.name.startswith('Chat pane ')]
            editable = [node.name for node in nodes if node.name == 'Message'
                        and node.getState().contains(pyatspi.STATE_EDITABLE)]
            if not pending or len(tabs) != 2:
                return None
            for tab in tabs:
                assert ', ' + str(root.parent / 'project') + ', ' in tab.name
            forbidden = [name for name in names if name in ('Claude context', 'Copy this message')
                         or 'Composition transcript is ready.' in name
                         or 'Actual user request: keep my saved work while I open another channel.' in name
                         or 'SYNTHETIC_CONTEXT_BEGIN' in name or 'SYNTHETIC_CONTEXT_END' in name]
            assert not editable and not forbidden, 'Main no longer reproduces the mixed-scope admission block'
            events = (root / 'receipts.txt').read_text().splitlines()
            assert not any(line in events for line in (
                'healthy-attach-requested', 'healthy-attach-held-before-ready',
                'repaired-held-attach-requested', 'composition-context-copy-delivered'))
            return {'target': target, 'unavailable_before': True, 'interaction_performed': False,
                    'paired_capture': label + '-' + capture_suffix + '.webp', 'viewport': list(viewport),
                    'pending_labels': pending, 'saved_chat_tabs': [node.name for node in tabs],
                    'editable_message_fields': editable, 'unexpected_transcript_controls': forbidden,
                    'visible_labels': names, 'receipt_lines': len(events),
                    'scope': 'The same synthetic transcript is configured but not admitted; main renders the saved panes awaiting admission. No healthy typing, context disclosure or context-copy gesture was possible.'}
        value = wait(read)
        observations.append(value)
        event({'event': 'composition-target-unavailable-before', **value})
    def channel_control(prefix):
        names = [node.name for node in live_tree(pid)
                 if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                 and node.name.startswith(prefix)]
        assert len(names) <= 1, 'Main composition navigation must identify one exact channel'
        return names[0] if names else None
    def new_composer():
        fields = [node for node in live_tree(pid) if node.name == 'Message' and visible(node)
                  and node.getState().contains(pyatspi.STATE_EDITABLE)]
        assert len(fields) <= 1
        return (len(fields) == 1 and fields[0].getState().contains(pyatspi.STATE_FOCUSED)
                and fields[0].queryText().getText(0, -1) == '')
    blocked('Healthy draft entry and collapsed Claude context', 'context-collapsed-wide')
    capture(window, label + '-context-collapsed-wide', (1280, 800), neutral_pointer=True)
    (root / 'composition-create-allowed').touch()
    click(pid, 'New channel for project')
    wait(lambda: 'composition-ranked-response-delivered' in (root / 'receipts.txt').read_text().splitlines())
    created = (root / 'composition-channel-id').read_text().strip()
    new_control = wait(lambda: channel_control('Open channel New channel, '))
    wait(new_composer)
    wait(lambda: not any(node.name.startswith('This saved pane has not been reopened.')
                         and visible(node) for node in live_tree(pid)))
    observe_composition_created_scope(root, created)
    event({'event': 'composition-new-channel-revealed', 'channel_id': created, 'control': new_control})
    capture(window, label + '-new-channel-revealed', (1280, 800), neutral_pointer=True)
    click(pid, wait(lambda: channel_control('Open channel Saved recovery model received, ')))
    blocked('Expand Claude context and copy its original bytes at wide width', 'context-expanded-wide-request-unavailable')
    capture(window, label + '-context-expanded-wide-request-unavailable', (1280, 800), neutral_pointer=True)
    capture(window, label + '-context-expanded-narrow-request-unavailable', (900, 700), neutral_pointer=True)
    blocked('Expand Claude context and copy its original bytes at narrow width', 'context-expanded-narrow-request-unavailable')
    blocked('Collapse Claude context and continue the healthy composer at narrow width', 'context-collapsed-narrow')
    capture(window, label + '-context-collapsed-narrow', (900, 700), neutral_pointer=True)
    (out / (label + '-baseline-unavailable.json')).write_text(json.dumps(observations, indent=2) + '\n')



def history_row_has_status(pid, title, status):
    def read():
        rows = [node for node in live_tree(pid) if node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                and node.name == 'Resume ' + title and visible(node)]
        assert len(rows) <= 1, 'History row must be unique'
        if not rows:
            return False
        pending, seen = [rows[0]], 0
        while pending:
            node = pending.pop()
            if node is None:
                raise IncompleteAccessibilityTree('History row changed during traversal')
            seen += 1
            assert seen < 100 and node.childCount < 100
            if visible(node) and node.name.endswith(' · ' + status):
                return True
            pending.extend(node.getChildAtIndex(index) for index in range(node.childCount))
        return False
    return accessibility_snapshot(read)


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
    review_helpers = {}
    for variant, checkout in [('before', before), ('after', after), ('before-main', before_main), ('before-image', before_image)]:
        revision = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
        expected_sha, expected_tree = {
            'before': ('c727f886628897c1749290213e9d65037253095a', '6d4f36e2b4dd48fc9744305d34c78469384edcc5'),
            'after': (os.environ['PRODUCT_SHA'], os.environ['PRODUCT_TREE']),
            'before-main': ('c4e75601ae2e8c6f11f810d36af94726477efbb4', '69dacc7a72689e6d24f0a3d4122f9ddfe6ed23c8'),
            'before-image': ('bb88099d1037a04e5c914a6f2b055e1445cd2a47', '0309bedf50697d303396147dd3d728c6eae4a733'),
        }[variant]
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
        serde_libs = [name for row in rows if row.get('reason') == 'compiler-artifact'
                      and row.get('target', {}).get('name') == 'serde_json'
                      and 'lib' in row.get('target', {}).get('kind', [])
                      for name in row.get('filenames', []) if name.endswith('.rlib')]
        assert len(serde_libs) == 1, 'Each matched UI graph must supply exactly one existing serde_json library'
        helper = base / (variant + '-fixture')
        command = ['rustc', '--edition=2024', str(source), '--extern', 'cagents=' + libs[0],
                   '--extern', 'serde_json=' + serde_libs[0],
                   '-L', 'dependency=' + str(target / 'dev-runtime/deps'), '-o', str(helper)]
        if variant == 'after':
            command += ['--cfg', 'held_product_after']
        elif variant == 'before-main':
            command += ['--cfg', 'held_composition_main_before']
        elif variant == 'before-image':
            command += ['--cfg', 'held_product_after', '--cfg', 'held_image_before']
        for row in rows:
            if row.get('reason') == 'build-script-executed':
                for path in row.get('linked_paths', []):
                    command += ['-L', path]
        run(command, timeout=180)
        fixtures[variant] = (revision, helper, expected_sha, expected_tree)
        if variant in ('before-image', 'after'):
            review_helper = base / (variant + '-review-fixture')
            review_command = ['rustc', '--edition=2024',
                              str(control / '.github/qualification/review-fixture.rs'),
                              '--extern', 'cagents=' + libs[0],
                              '-L', 'dependency=' + str(target / 'dev-runtime/deps'),
                              '-o', str(review_helper)]
            for row in rows:
                if row.get('reason') == 'build-script-executed':
                    for path in row.get('linked_paths', []):
                        review_command += ['-L', path]
            run(review_command, timeout=180)
            review_helpers[variant] = review_helper
        if variant == 'before-main':
            (out / 'before-main-source.json').write_text(json.dumps({
                'revision': revision, 'tree': expected_tree, 'cargo_lock_sha256': digest(checkout / 'Cargo.lock'),
                'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                'helper_cfg': 'held_composition_main_before',
                'scope': 'Current-main native composition comparison only; original c727 comparison remains separate.',
            }, indent=2) + '\n')
        if variant == 'before-image':
            (out / 'before-image-source.json').write_text(json.dumps({
                'revision': revision, 'tree': expected_tree, 'cargo_lock_sha256': digest(checkout / 'Cargo.lock'),
                'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                'helper_cfg': ['held_product_after', 'held_image_before'],
                'scope': 'Image-only association-loss comparison on 9ead plus shared tooltip accessibility repair bb88099; this is a modified baseline. Historical c727 and c4 comparisons remain separate.',
            }, indent=2) + '\n')
    # Compile each matched helper first. The original c727 reader still checks
    # candidate schema refusal; current main owns only the last comparison arm.
    for variant, checkout, composition in [('after', after, False), ('before', before, False), ('after', after, True), ('before-main', before_main, True)] + [('before-image', before_image, 'image-only'), ('after', after, 'image-only')]:
        image_only = composition == 'image-only'
        composition = composition is True
        revision, helper, expected_sha, expected_tree = fixtures[variant]
        assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
        assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}'], text=True).strip() == expected_tree
        assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
        arms = [('chat', 'stale', 'short', False, True), ('chat', 'stale', 'short', False, False), ('chat', 'stale', 'long', False, False), ('chat', 'closed', 'short', False, False), ('chat', 'closed', 'empty', False, False), ('terminal', 'closed', 'empty', False, False), ('chat', 'stale', 'short', True, False), ('chat', 'peer-removal', 'empty', False, False)]
        if composition:
            arms = [('chat', 'composition', 'short', False, False)]
        if image_only:
            arms = [('chat', 'image-only', 'empty', False, False)]
        for surface, mode, payload, hidden, ordinary in arms:
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
            env['HELD_FIXTURE_COMPOSITION'] = '1' if composition else '0'
            env['HELD_FIXTURE_IMAGE_ONLY'] = '1' if image_only else '0'
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
            if variant == 'before-main':
                assert composition and mode == 'composition'
                composition_main_before_flow(pid, window, root, label)
                database = Path((root / 'database-path').read_text())
                assert database.is_relative_to(root)
                prefix = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:'
                def composition_main_before_durable():
                    try:
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            values = dict(db.execute('SELECT key, value FROM metadata WHERE key LIKE ?', (prefix + '%',)))
                            if not all(prefix + key in values for key in ['chat_drafts', 'pending_chat_admissions', 'active_thread', 'threads']):
                                return False
                            drafts = json.loads(values[prefix + 'chat_drafts'])
                            pending = json.loads(values[prefix + 'pending_chat_admissions'])
                            channels = json.loads(values[prefix + 'threads'])
                            created = (root / 'composition-channel-id').read_text().strip()
                            return (not drafts.get('00000000-0000-4000-8000-000000003922', '')
                                    and drafts.get('00000000-0000-4000-8000-000000004922') == (root / 'expected-draft.txt').read_text()
                                    and all(card in pending for card in ['00000000-0000-4000-8000-000000003922', '00000000-0000-4000-8000-000000004922'])
                                    and json.loads(values[prefix + 'active_thread']) == '00000000-0000-4000-8000-000000002922'
                                    and any(channel['id'] == created and channel['shared'] for channel in channels))
                    except (sqlite3.Error, FileNotFoundError):
                        return False
                wait(composition_main_before_durable)
                stop(ui)
                stop(fixture)
                assert icon_config.read_text() == '\"Adwaita\"\n'
                inspect_saved_snapshot(helper, root, mode, 'open', env, label)
                events = receipt.read_text()
                assert 'UNEXPECTED-' not in events
                assert events.splitlines().count('composition-ranked-response-delivered') == 1
                assert not any(line in events.splitlines() for line in (
                    'healthy-attach-requested', 'healthy-attach-held-before-ready',
                    'repaired-held-attach-requested', 'composition-context-copy-delivered'))
                assert not any(line.startswith('effect-request-') for line in events.splitlines())
                (out / (label + '-receipts.txt')).write_text(events)
                receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                                 'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                                 'events': events.splitlines(), 'scope': 'Current-main synthetic mixed-scope comparison. Healthy draft entry and context disclosure/copy were unavailable; only New channel creation/reveal and return navigation were exercised.'})
                assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
                assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
                event({'event': 'arm-completed', 'revision': revision})
                active_arm = None
                continue
            if composition:
                assert variant == 'after' and mode == 'composition'
                composition_flow(pid, window, root, label)
                database = Path((root / 'database-path').read_text())
                assert database.is_relative_to(root)
                prefix = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:'
                def composition_durable():
                    try:
                        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
                            values = dict(db.execute('SELECT key, value FROM metadata WHERE key LIKE ?', (prefix + '%',)))
                            if not all(prefix + key in values for key in ['chat_drafts', 'quarantined_panes', 'active_thread', 'threads']):
                                return False
                            drafts = json.loads(values[prefix + 'chat_drafts'])
                            channels = json.loads(values[prefix + 'threads'])
                            created = (root / 'composition-channel-id').read_text().strip()
                            return (drafts.get('00000000-0000-4000-8000-000000003922') == (root / 'healthy-draft-expected').read_text()
                                    and drafts.get('00000000-0000-4000-8000-000000004922') == (root / 'expected-draft.txt').read_text()
                                    and '00000000-0000-4000-8000-000000004922' in json.loads(values[prefix + 'quarantined_panes'])
                                    and json.loads(values[prefix + 'active_thread']) == '00000000-0000-4000-8000-000000002922'
                                    and any(channel['id'] == created and channel['shared'] for channel in channels))
                    except (sqlite3.Error, FileNotFoundError):
                        return False
                wait(composition_durable)
                stop(ui)
                stop(fixture)
                assert icon_config.read_text() == '"Adwaita"\n'
                inspect_saved_snapshot(helper, root, mode, 'open', env, label)
                inspect_saved_snapshot(base / 'before-fixture', root, mode, 'open', env, label, refusal=True)
                events = receipt.read_text()
                assert 'UNEXPECTED-' not in events
                assert events.splitlines().count('composition-ranked-response-delivered') == 1
                assert events.splitlines().count('composition-context-copy-delivered') == 2
                assert 'repaired-held-attach-requested' not in events
                assert not any(line.startswith('effect-request-') and line != 'effect-request-chat-attach'
                               for line in events.splitlines())
                (out / (label + '-receipts.txt')).write_text(events)
                receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                                 'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                                 'events': events.splitlines(), 'scope': 'After-only synthetic composition supplement; no provider execution or adapter-classification claim.'})
                assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
                assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
                event({'event': 'arm-completed', 'revision': revision})
                active_arm = None
                continue
            if image_only:
                image_contract = exercise_image_only(pid, window, root, label, receipt, variant == 'before-image')
                stop(ui)
                stop(fixture)
                assert icon_config.read_text() == '"Adwaita"\n'
                inspect_saved_snapshot(helper, root, mode, 'open', env, label)
                inspect_saved_snapshot(base / 'before-fixture', root, mode, 'open', env, label, refusal=True)
                events = receipt.read_text()
                lines = events.splitlines()
                assert 'UNEXPECTED-' not in events
                assert lines.count('healthy-attach-requested') == 1
                assert lines.count('effect-request-chat-attach') == 1
                assert sum(line.startswith('image-removal-event-delivered ') for line in lines) == 1
                assert sum(line.startswith('image-pre-remove-asset-validated ') for line in lines) == 1
                assert not any(line.startswith(('effect-request-chat-start', 'effect-request-chat-prepare',
                                               'effect-request-other-provider-mutation', 'effect-request-chat-close',
                                               'repaired-held-attach-requested')) for line in lines)
                (out / (label + '-receipts.txt')).write_text(events)
                receipts.append({'arm': label, 'revision': revision, 'ui_sha256': ui_hash,
                                 'fixture_source_sha256': digest(source), 'helper_sha256': digest(helper),
                                 'events': lines, 'image_only_contract': image_contract})
                assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == expected_sha
                assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
                event({'event': 'arm-completed', 'revision': revision})
                active_arm = None
                review_exercise(globals(), checkout, review_helpers[variant], variant, revision, expected_tree)
                continue
            if mode == 'peer-removal':
                ready = 'Verified conversation ready for peer removal.'
                def peer_inside_viewport(node):
                    bounds = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    return (bounds.width > 0 and bounds.height > 0 and bounds.x >= 0 and bounds.y >= 0
                            and bounds.x + bounds.width <= viewport[0] and bounds.y + bounds.height <= viewport[1])
                # Display titles follow the manual channel title and change
                # again when the peer edit arrives. Bind the sole Chat to its
                # private source directory instead of freezing presentation.
                (out / (label + '-bootstrap-accessibility.json')).write_text(json.dumps(
                    accessibility_snapshot(lambda: [{'name': node.name, 'role': node.getRoleName()}
                        for node in live_tree(pid) if visible(node)]), indent=2) + '\n')
                ready_labels = wait(lambda: accessibility_snapshot(lambda: [node.name
                    for node in live_tree(pid) if ready in node.name and visible(node)]))
                event({'event': 'peer-ready-sentinel-visible', 'labels': ready_labels})
                def peer_chat_tab():
                    def read():
                        tabs = [node for node in live_tree(pid)
                                if node.getRole() == pyatspi.ROLE_PUSH_BUTTON and visible(node)
                                and node.name.startswith('Chat pane ')]
                        assert len(tabs) <= 1, 'Peer fixture must have exactly one Chat pane'
                        if not tabs or not peer_inside_viewport(tabs[0]):
                            return None
                        name = tabs[0].name
                        assert ', ' + str(root.parent / 'project') + ', ' in name, 'Unexpected Chat source directory'
                        return name
                    return accessibility_snapshot(read)
                event({'event': 'peer-chat-tab-identified', 'label': wait(peer_chat_tab)})
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
                    return (not any(visible(node) and (ready in node.name or node.name == 'Message'
                                    or (node.getRole() == pyatspi.ROLE_PUSH_BUTTON
                                        and node.name.startswith('Chat pane '))) for node in nodes)
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
                capture(window, label + '-removed-wide', (1280, 800), neutral_pointer=True)
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
                inspect_saved_snapshot(helper, root, mode, 'closed', env, label)
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
            capture(window, label + '-wide', (1280, 800), neutral_pointer=True)
            capture(window, label + '-narrow', (900, 700), neutral_pointer=True)
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
                wait(lambda: history_row_has_status(pid, 'Healthy saved pane', 'Open'))
                wait(lambda: history_row_has_status(pid, 'Unavailable saved pane',
                     'Unavailable' if variant == 'after' else 'Open'))
                capture(window, label + '-ordinary-history', (1280, 800), neutral_pointer=True)
                if variant == 'after':
                    run(['xdotool', 'key', 'Escape'])
                    wait(lambda: not any(node.name == 'Resume conversation' and visible(node) for node in live_tree(pid)))
                    focus_failed_pane(pid)
                    click(pid, 'Remove saved pane')
                    wait(lambda: not any(node.name == 'Saved pane unavailable' for node in live_tree(pid)))
                    open_ordinary_history(pid)
                    wait(lambda: history_row_has_status(pid, 'Unavailable saved pane', 'Unavailable'))
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
                capture(window, label + '-ordinary-recovered', (1280, 800), neutral_pointer=True)
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
            inspect_saved_snapshot(helper, root, mode, expected_existence, env, label)
            database = Path((root / 'database-path').read_text())
            assert database.is_relative_to(root)
            if variant == 'after':
                with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
                    key = 'workspace:' + (root / 'workspace-id').read_text().strip() + ':navigation:quarantined_panes'
                    marker = json.loads(db.execute('SELECT value FROM metadata WHERE key=?', (key,)).fetchone()[0])
                    bad_id = '00000000-0000-4000-8000-000000004922'
                    assert (bad_id in marker) == (hidden or ordinary or mode != 'stale' or payload != 'short')
                inspect_saved_snapshot(base / 'before-fixture', root, mode, expected_existence, env, label, refusal=True)
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
    retention_errors = []
    if native_failure is not None:
        try:
            (out / 'native-failure-traceback.txt').write_text(''.join(traceback.format_exception(
                type(native_failure), native_failure, native_failure.__traceback__)))
        except BaseException as error:
            diagnostic_errors.append({'kind': 'traceback', 'error': repr(error)})
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
            if root.name in ('before-image-review-tooltip', 'after-review-tooltip'):
                # Retain raw synthetic service/transport data even when a
                # later native observation fails. Incomplete data never
                # qualifies a diagnostic or the original twenty-arm gate.
                for relative in ['gh-calls.jsonl', 'git-trace.jsonl', 'gh-data/manifest.json']:
                    try:
                        diagnostic_source = root / relative
                        if diagnostic_source.exists():
                            info = diagnostic_source.lstat()
                            assert stat.S_ISREG(info.st_mode) and not stat.S_ISLNK(info.st_mode)
                            assert info.st_uid == os.getuid() and info.st_nlink == 1
                            assert diagnostic_source.resolve(strict=True).is_relative_to(root)
                            assert info.st_size <= 8 * 1024 * 1024
                            (out / (root.name + '-' + relative.replace('/', '-'))).write_bytes(diagnostic_source.read_bytes())
                    except Exception as error:
                        retention_errors.append({'arm': root.name, 'file': relative, 'error': repr(error)})
    (out / 'cleanup.json').write_text(json.dumps({'errors': cleanup_errors, 'subreaper': True}, indent=2) + '\n')
    (out / 'native-outcome.json').write_text(json.dumps({
        'outcome': 'failed' if native_failure is not None or cleanup_errors or retention_errors else 'complete',
        'incomplete_arm': active_arm, 'error_type': type(native_failure).__name__ if native_failure else None,
        'error': str(native_failure)[:2000] if native_failure else None, 'cleanup_errors': cleanup_errors,
        'diagnostic_errors': diagnostic_errors,
        'retention_errors': retention_errors,
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
    if retention_errors and native_failure is None:
        raise RuntimeError('Supplemental native receipt retention is unqualified: ' + repr(retention_errors))
