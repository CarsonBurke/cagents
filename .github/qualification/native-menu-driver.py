"""Actual hosted X11 project-menu evidence; source only until independently reviewed.

Guard, bounded AT-SPI traversal, bootstrap, pixel conversion and cleanup adapted
from b0089c26473c7dcef6970b3c90ee9785481ff9df native-driver.py, Git blob
d34ad4d8adfd40f7965d0a35f0c6a45abf39f350. Its resizing capture, HOME override,
synthetic IPC fixture and acceptance claims are deliberately absent.
"""
import ast
import ctypes
import csv
import io
import math
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

CONTROL, BEFORE, AFTER = (Path(arg).resolve() for arg in sys.argv[1:4])
M = json.loads((CONTROL / '.github/qualification/project-menu-native-manifest.json').read_text())
OUT = Path(os.environ['MENU_EVIDENCE_ROOT']).resolve()
TARGET = Path(os.environ['CARGO_TARGET_DIR']).resolve()
EVENTS, COMMANDS, ARMS, OWNED, CHILDREN = [], [], [], {}, []
ACTIVE, CRITICAL = None, False
PIXEL_MENU, CAPTURES, CANCEL_SIGNAL = None, {}, None
original_home, original_codex_home = os.environ.get('HOME'), os.environ.get('CODEX_HOME')
menu_cargo_cache = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
menu_rustup_cache = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))


def put(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def guard():
    names = sorted(name for name in os.environ if name.startswith('CAGENTS_'))
    assert os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted' and os.environ.get('GITHUB_ACTIONS') == 'true'
    assert not names, 'Inherited CAGENTS names refused unchanged: ' + ', '.join(names)
    assert not any(name in os.environ for name in ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET'))
    assert os.environ.get('HOME') == original_home and os.environ.get('CODEX_HOME') == original_codex_home


def processes():
    table = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            table[int(path.parent.name)] = dict(parent=int(fields[1]), group=int(fields[2]), state=fields[0], start=fields[19])
        except (FileNotFoundError, ProcessLookupError):
            pass
    return table


def scan():
    table = processes()
    parents = {os.getpid(): ACTIVE}
    for identity, row in OWNED.items():
        if row['pid'] in table and table[row['pid']]['start'] == row['start']:
            parents[row['pid']] = row['arm']
    while True:
        new = [(pid, row) for pid, row in table.items() if pid not in parents and row['parent'] in parents]
        if not new:
            break
        for pid, row in new:
            arm = parents[row['parent']]
            parents[pid] = arm
            OWNED.setdefault(f'{pid}:{row["start"]}', dict(row, pid=pid, arm=arm))
    return table


def live(arm='all'):
    table = scan()
    return [row for row in OWNED.values() if (arm == 'all' or row['arm'] == arm)
            and row['pid'] in table and table[row['pid']]['start'] == row['start']
            and table[row['pid']]['state'] not in ('Z', 'X')]


def close_owned(arm='all'):
    for sig, seconds in ((signal.SIGTERM, 5), (signal.SIGKILL, 10)):
        deadline = time.monotonic() + seconds
        while live(arm) and time.monotonic() < deadline:
            for row in live(arm):
                current = processes().get(row['pid'])
                if current and current['start'] == row['start'] and current['state'] not in ('Z', 'X'):
                    try:
                        os.kill(row['pid'], sig)
                        EVENTS.append(dict(monotonic=time.monotonic(), cleanup_signal=sig, identity=row))
                    except ProcessLookupError:
                        pass
            time.sleep(.05)
    assert not live(arm), 'Owned process identities remain live'
    for child in CHILDREN:
        if child.poll() is not None:
            child.wait()
    while True:
        try:
            if os.waitpid(-1, os.WNOHANG)[0] == 0:
                break
        except ChildProcessError:
            break


def spawn(args, **kwargs):
    guard()
    process = subprocess.Popen([str(arg) for arg in args], start_new_session=True, **kwargs)
    CHILDREN.append(process)
    scan()  # Capture PID/start/group before wait or reaping.
    current = processes().get(process.pid)
    assert current is not None, 'Cannot retain launched process identity'
    process.menu_identity = dict(OWNED[f'{process.pid}:{current["start"]}'])
    return process


def cmd(args, env=None, cwd=None, timeout=20, expected=0):
    guard()
    args = [str(arg) for arg in args]
    if CRITICAL:
        ocr_read = (ACTIVE == 'before' and len(args) == 10 and args[0] == 'tesseract' and
            args[2:] == ['stdout', '-l', 'eng', '--psm', '11', '--oem', '1', 'tsv'] and
            Path(args[1]).resolve() in CAPTURES and CAPTURES[Path(args[1]).resolve()]['arm'] == 'before')
        assert ocr_read or args[:2] == ['import', '-window'] or (args[0] == 'xdotool' and args[1] in
            ('getwindowgeometry', 'getmouselocation', 'getwindowfocus')), 'UI repair command refused during wheel→activation'
    directory = OUT / 'commands' / f'{len(COMMANDS):05d}'
    directory.mkdir(parents=True)
    record = dict(argv=args, cwd=str(cwd) if cwd else None, arm=ACTIVE, started=time.monotonic(), deadline_seconds=timeout)
    COMMANDS.append(record)
    put(directory / 'command.json', record)
    with (directory / 'stdout.log').open('wb') as stdout, (directory / 'stderr.log').open('wb') as stderr:
        process = spawn(args, env=env, cwd=cwd, stdout=stdout, stderr=stderr)
        record['identity'] = process.menu_identity
        deadline = time.monotonic() + timeout
        try:
            while process.poll() is None:
                scan()
                assert time.monotonic() < deadline, 'Native command deadline exceeded'
                time.sleep(.02)
            record['exit'] = process.wait()
        except BaseException as error:
            record.update(exit=None, error=str(error))
            put(directory / 'command.json', record)
            raise
    record.update(returned=time.monotonic(), stdout_sha256=digest(directory / 'stdout.log'), stderr_sha256=digest(directory / 'stderr.log'))
    put(directory / 'command.json', record)
    (directory / 'exit.txt').write_text(str(record['exit']) + '\n')
    assert expected is None or record['exit'] == expected, f'Native command exit {record["exit"]}: {args}'
    return (directory / 'stdout.log').read_text(errors='replace')


def wait(predicate, timeout=30, interval=.1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        scan()
        result = predicate()
        if result:
            return result
        time.sleep(interval)
    raise RuntimeError('Native condition deadline exceeded')


def git(root, *args, fixture=False):
    # Only synthetic fixture Git commands disable system/global config.
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null') if fixture else None
    return cmd(['git', '--no-optional-locks', '-C', root, *args], env=env).strip()


def source(root, pin):
    record = dict(head=git(root, 'rev-parse', 'HEAD'), tree=git(root, 'rev-parse', 'HEAD^{tree}'),
                  status=git(root, 'status', '--porcelain=v1', '--untracked-files=all'))
    assert record == dict(head=pin['head'], tree=pin['tree'], status=''), record
    assert not any(line.startswith('160000 ') for line in git(root, 'ls-files', '--stage').splitlines())
    record['sha256'] = {name: digest(root / name) for name in M['source_paths']}
    return record


def tree(pid):
    desktop = pyatspi.Registry.getDesktop(0)
    apps = [desktop.getChildAtIndex(i) for i in range(desktop.childCount)]
    apps = [app for app in apps if app.get_process_id() == pid]
    if len(apps) != 1:
        return []
    pending, result, deadline = [apps[0]], [], time.monotonic() + 5
    while pending:
        assert time.monotonic() < deadline and len(result) < 10000
        node = pending.pop()
        result.append(node)
        assert node.childCount < 1000
        pending.extend(node.getChildAtIndex(i) for i in range(node.childCount))
    return result


class PixelRow:
    """An explicit OCR observation, never an accessibility node or product fixture."""
    def __init__(self, row):
        self.name, self.native_bounds = row['text'], tuple(row['bounds'])
        self.observation = row


def tsv_lines(raw, size):
    # TSV schema: https://tesseract-ocr.github.io/tessdoc/Command-Line-Usage.html#tsv-output
    reader = csv.DictReader(io.StringIO(raw), delimiter='\t')
    assert reader.fieldnames == ['level', 'page_num', 'block_num', 'par_num', 'line_num', 'word_num',
                                'left', 'top', 'width', 'height', 'conf', 'text'], 'Unknown TSV schema'
    grouped, identities = {}, set()
    for row in reader:
        assert None not in row, 'Malformed TSV columns'
        if int(row['level']) != 5 or not row['text'].strip():
            continue
        key = tuple(int(row[name]) for name in ('page_num', 'block_num', 'par_num', 'line_num'))
        identity = (*key, int(row['word_num']))
        assert key[0] == 1 and identity not in identities
        identities.add(identity)
        confidence = float(row['conf'])
        bounds = tuple(int(row[name]) for name in ('left', 'top', 'width', 'height'))
        assert math.isfinite(confidence) and 0 <= confidence <= 100 and on_screen(bounds, size)
        assert not any(ord(character) < 32 for character in row['text'])
        grouped.setdefault(key, []).append(dict(text=row['text'], bounds=bounds, confidence=confidence, word_num=identity[-1]))
    result = []
    for key, words in grouped.items():
        words.sort(key=lambda word: word['word_num'])
        left, top = min(word['bounds'][0] for word in words), min(word['bounds'][1] for word in words)
        right = max(word['bounds'][0] + word['bounds'][2] for word in words)
        bottom = max(word['bounds'][1] + word['bounds'][3] for word in words)
        result.append(dict(text=' '.join(word['text'] for word in words), bounds=(left, top, right-left, bottom-top),
                           min_confidence=min(word['confidence'] for word in words), words=words, line_id=key))
    assert result, 'No readable native OCR lines'
    return result


def exact_pixel_label(lines, label):
    matches = [row for row in lines if row['text'] == label]
    assert len(matches) <= 1, 'Duplicate exact OCR label: ' + label
    if not matches:
        return None
    assert matches[0]['min_confidence'] >= M['ocr']['minimum_word_confidence'], 'Low-confidence exact OCR label: ' + label
    return matches[0]


def ocr_frame(window, size, name):
    assert ACTIVE == 'before', 'OCR is baseline-only; product accessibility gates are unchanged'
    record = capture(window, name, size)
    png = (OUT / (name + '.png')).resolve()
    assert png in CAPTURES and CAPTURES[png]['arm'] == 'before'
    raw = cmd(['tesseract', png, 'stdout', '-l', 'eng', '--psm', '11', '--oem', '1', 'tsv'], timeout=20)
    (OUT / (name + '.tsv')).write_text(raw)
    assert digest(png) == record['png_sha256'], 'OCR input changed after native capture'
    rows = tsv_lines(raw, size)
    put(OUT / (name + '-ocr.json'), dict(native_capture=record, rows=rows,
        policy='Exact full literal labels only; no fuzzy, punctuation aliases, scaling or image preprocessing'))
    return dict(name=name, rows=rows, png=png, capture=record)


class BaselinePixels:
    def __init__(self, pid, window, size, label):
        assert ACTIVE == 'before'
        self.pid, self.window, self.size, self.label = pid, window, size, label
        self.opened, self.number, self.context, self.last = False, 0, None, None

    def frame(self, purpose):
        self.number += 1
        return ocr_frame(self.window, self.size, f'{self.label}-pixel-{self.number:03d}-{purpose}')

    def column(self, frame):
        left, top = self.context['left'], self.context['top']
        # A conservative observation domain, not a guessed activation rectangle.
        # Unknown text in this source-supported label column invalidates the read.
        return [row for row in frame['rows'] if left-4 <= row['bounds'][0] <= left + M['ocr']['column_start_span_pixels']
                and row['bounds'][1] >= top-4 and sum(character.isalpha() for character in row['text']) >= 3]

    def validate_slice(self, frame):
        candidates = self.column(frame)
        assert len(candidates) >= 2, 'Insufficient readable menu labels; OCR absence is not a feature negative'
        names = set()
        for row in candidates:
            assert row['text'] in M['baseline_menu_labels'], 'Unknown/partial OCR menu label: ' + row['text']
            assert row['min_confidence'] >= M['ocr']['minimum_word_confidence'], 'Low-confidence menu text'
            assert row['text'] not in names, 'Ambiguous repeated native menu label'
            names.add(row['text'])
        frame['menu_rows'] = candidates
        self.last = frame
        return frame

    def read(self, purpose='slice'):
        assert self.opened
        return self.validate_slice(self.frame(purpose))

    def lookup(self, label):
        frame = self.read('lookup')
        row = exact_pixel_label(frame['menu_rows'], label)
        return PixelRow(row) if row else None

    def open(self, inputs, rect):
        assert not CRITICAL and not self.opened
        x, y, width, height = rect
        inputs.move(x + width // 2, y + height // 2)
        time.sleep(.7)
        before = self.frame('before-open')
        inputs.click(3)
        time.sleep(.6)
        opened = self.frame('opened')
        labels = ['Open folder', 'New terminal', 'New channel']
        rows = [exact_pixel_label(opened['rows'], label) for label in labels]
        assert all(rows), 'Native menu anchors unavailable; no guessed baseline coordinates'
        assert all(exact_pixel_label(before['rows'], label) is None for label in labels), 'Menu labels already present before opening; ambiguous menu binding'
        assert rows[0]['bounds'][1] < rows[1]['bounds'][1] < rows[2]['bounds'][1]
        left = min(row['bounds'][0] for row in rows)
        assert max(row['bounds'][0] for row in rows)-left <= 8, 'Initial menu labels are not one independent native column'
        self.context = dict(left=left, top=rows[0]['bounds'][1], anchor=rect,
                            before_capture=before['name'], open_capture=opened['name'])
        self.opened = True
        self.validate_slice(opened)
        put(OUT / (opened['name'] + '-binding.json'), self.context)
        return opened

    def crop_hashes(self, frame, rows):
        with Image.open(frame['png']) as image:
            return [hashlib.sha256(image.crop((row['bounds'][0], row['bounds'][1],
                row['bounds'][0]+row['bounds'][2], row['bounds'][1]+row['bounds'][3])).tobytes()).hexdigest() for row in rows]

    def closed(self):
        if not self.opened:
            return True
        old = self.last
        assert old and len(old['menu_rows']) >= 2
        frame = self.frame('after-escape')
        candidates = self.column(frame)
        for row in candidates:
            if row['text'] in M['baseline_menu_labels']:
                assert row['min_confidence'] >= M['ocr']['minimum_word_confidence'], 'Uncertain OCR after dismissal'
                return False
        assert not candidates, 'Unknown native text prevents proving menu dismissal'
        changed = sum(a != b for a,b in zip(self.crop_hashes(old, old['menu_rows']), self.crop_hashes(frame, old['menu_rows'])))
        assert changed >= 2, 'OCR absence without changed native menu glyph regions is ambiguous'
        self.opened = False
        put(OUT / (frame['name'] + '-closed.json'), dict(previous=old['name'], changed_glyph_regions=changed,
            status='No menu text remains in independently bound column; native glyph regions changed'))
        return True

    def modal_closed(self, label):
        # Called only after the unchanged strict native exact-target modal oracle.
        self.opened = False
        put(OUT / (label + '-pixel-menu-closed-by-modal.json'), dict(reason='Exact native modal/Cancel oracle passed; image retained separately'))

    def calibrate(self, inputs, rect, label):
        self.open(inputs, rect)
        first = exact_pixel_label(self.last['menu_rows'], 'Open folder')
        assert first
        b = first['bounds']
        wheel_point = (b[0]+b[2]//2, b[1]+b[3]//2)
        inputs.move(*wheel_point)
        seen, slices, event_start = set(), [], len(EVENTS)
        final = None
        for index in range(21):
            frame = self.read(f'calibration-{index}')
            seen.update(row['text'] for row in frame['menu_rows'] if row['text'] in M['editor_labels'])
            final = exact_pixel_label(frame['menu_rows'], 'Remove project…')
            slices.append(dict(capture=frame['name'], rows=frame['menu_rows'], pixel_hashes=self.crop_hashes(frame, frame['menu_rows'])))
            if final and set(M['editor_labels']).issubset(seen):
                break
            if index < 20:
                inputs.wheel(5, 3)
        if final is None:
            assert len(slices) >= 3 and slices[-1]['rows'] == slices[-2]['rows'] == slices[-3]['rows']
            assert slices[-1]['pixel_hashes'] == slices[-2]['pixel_hashes'] == slices[-3]['pixel_hashes'], 'Unstable/unreadable end state is not unreachable-row evidence'
            assert (pointer()['X'], pointer()['Y']) == wheel_point, 'Calibration pointer drift is not unreachable-row evidence'
            observation = dict(activation_attempted=False, final_target=None, anchor=rect, point=wheel_point,
                geometry=geometry(self.window), event_start=event_start, event_end=len(EVENTS), slices=slices,
                observed_editor_labels=sorted(seen), missing_editor_labels=sorted(set(M['editor_labels'])-seen),
                missing_before_states=['reachable Remove row','keyboard/click activation at that row','exact target dialog'],
                negative_basis='No literal Remove row observed after bounded real wheel; readable native slices and glyph regions stayed unchanged. Independent pixel review must confirm physical unreachability.')
            put(OUT / (label + '-unreachable.json'), observation)
            raise BaselineRemoveUnreachable(observation)
        assert set(M['editor_labels']).issubset(seen), 'Missing editor OCR coverage is not a passing calibration'
        inputs.wheel(5, 60)
        frame = self.read('calibration-bottom')
        final = exact_pixel_label(frame['menu_rows'], 'Remove project…')
        assert final, 'Previously reached row became unavailable; no coordinate repair'
        b = final['bounds']
        point = (b[0]+b[2]//2, b[1]+b[3]//2)
        put(OUT / (label + '-calibration.json'), dict(anchor=rect, stationary_point=point, final_bounds=b,
            coordinate_source='This baseline native PNG/TSV only', observed_editor_labels=sorted(seen), slices=slices))
        inputs.key('Escape')
        wait(self.closed)
        return rect, point


def box(node):
    if isinstance(node, PixelRow):
        return node.native_bounds
    b = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    return (b.x, b.y, b.width, b.height)


def visible(node):
    return node.getState().contains(pyatspi.STATE_SHOWING)


def describe(node):
    state = node.getState()
    row = dict(name=node.name, role=node.getRoleName(), showing=visible(node),
               enabled=state.contains(pyatspi.STATE_ENABLED), focused=state.contains(pyatspi.STATE_FOCUSED))
    try:
        row['bounds'] = box(node)
    except NotImplementedError:
        row['bounds'] = None
    return row


def snapshot(pid, name):
    nodes = tree(pid)
    assert nodes, 'PID-filtered native accessibility tree disappeared'
    put(OUT / (name + '-a11y.json'), dict(pid=pid, monotonic=time.monotonic(), nodes=[describe(node) for node in nodes]))


def unique(pid, name, enabled=True):
    if ACTIVE == 'before' and PIXEL_MENU is not None and name in M['baseline_menu_labels']:
        return PIXEL_MENU.lookup(name) if PIXEL_MENU.opened else None
    found = [node for node in tree(pid) if node.name == name and visible(node)
             and (not enabled or node.getState().contains(pyatspi.STATE_ENABLED))]
    actionable = [node for node in found if node.getRole() in (pyatspi.ROLE_PUSH_BUTTON, pyatspi.ROLE_MENU_ITEM)]
    found = actionable or found
    return found[0] if len(found) == 1 else None


def query(tool):
    return cmd(['xdotool', *tool]).strip()


def geometry(window):
    data = dict(line.split('=', 1) for line in query(['getwindowgeometry', '--shell', window]).splitlines() if '=' in line)
    return {key: int(data[key]) for key in ('X', 'Y', 'WIDTH', 'HEIGHT', 'SCREEN')}


def pointer():
    data = dict(line.split('=', 1) for line in query(['getmouselocation', '--shell']).splitlines() if '=' in line)
    return {key: int(data[key]) for key in ('X', 'Y', 'SCREEN')}


def capture(window, name, size):
    # Pure pixel read: never resize/move/focus/change pointer or invoke AT-SPI actions.
    actual, point = geometry(window), pointer()
    assert (actual['WIDTH'], actual['HEIGHT']) == size
    png = OUT / (name + '.png')
    cmd(['import', '-window', window, png])
    with Image.open(png) as image:
        assert image.size == size
        image.save(OUT / (name + '.webp'), lossless=True)
    assert geometry(window) == actual and pointer() == point
    result = dict(geometry=actual, pointer=point, png_sha256=digest(png), webp_sha256=digest(OUT / (name + '.webp')),
                  monotonic=time.monotonic(), critical=CRITICAL, capture='Actual native pixels, lossless WebP, no resize')
    result.update(arm=ACTIVE, window=window)
    CAPTURES[png.resolve()] = result
    put(OUT / (name + '-capture.json'), result)
    return result


class ClientData(ctypes.Union):
    _fields_ = [('bytes', ctypes.c_char * 20), ('shorts', ctypes.c_short * 10), ('longs', ctypes.c_long * 5)]


class ClientMessage(ctypes.Structure):
    _fields_ = [('type', ctypes.c_int), ('serial', ctypes.c_ulong), ('send_event', ctypes.c_int),
                ('display', ctypes.c_void_p), ('window', ctypes.c_ulong), ('message_type', ctypes.c_ulong),
                ('format', ctypes.c_int), ('data', ClientData)]


class XEvent(ctypes.Union):
    _fields_ = [('client', ClientMessage), ('pad', ctypes.c_long * 24)]


class Inputs:
    def __init__(self):
        self.x, self.t = ctypes.CDLL('libX11.so.6'), ctypes.CDLL('libXtst.so.6')
        self.x.XOpenDisplay.argtypes, self.x.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
        self.display = self.x.XOpenDisplay(None)
        assert self.display
        self.x.XStringToKeysym.argtypes, self.x.XStringToKeysym.restype = [ctypes.c_char_p], ctypes.c_ulong
        self.x.XKeysymToKeycode.argtypes, self.x.XKeysymToKeycode.restype = [ctypes.c_void_p, ctypes.c_ulong], ctypes.c_ubyte
        self.x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.x.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.t.XTestQueryExtension.argtypes = [ctypes.c_void_p, *([ctypes.POINTER(ctypes.c_int)] * 4)]
        self.t.XTestFakeKeyEvent.argtypes = self.t.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
        values = [ctypes.c_int() for _ in range(4)]
        assert self.t.XTestQueryExtension(self.display, *[ctypes.byref(v) for v in values])
        put(OUT / 'xtest.json', dict(event_base=values[0].value, error_base=values[1].value, major=values[2].value, minor=values[3].value))
        self.keys, self.buttons = set(), set()

    def event(self, kind, value, down, purpose='control'):
        if CRITICAL:
            assert (purpose == 'wheel' and kind == 'button' and value in (4, 5)) or (purpose == 'activation' and
                ((kind == 'key' and value in ('Return', 'space')) or (kind == 'button' and value == 1)))
        if kind == 'key':
            symbol = self.x.XStringToKeysym(value.encode('ascii'))
            code = self.x.XKeysymToKeycode(self.display, symbol)
            assert symbol and code
            pressed = self.keys
            if down:
                pressed.add(value)  # Pending injection is owned even if interrupted before its receipt.
            result = self.t.XTestFakeKeyEvent(self.display, code, int(down), 0)
        else:
            code = value
            pressed = self.buttons
            if down:
                pressed.add(value)
            result = self.t.XTestFakeButtonEvent(self.display, code, int(down), 0)
        self.x.XSync(self.display, 0)
        if result and not down:
            pressed.discard(value)
        EVENTS.append(dict(arm=ACTIVE, monotonic=time.monotonic(), kind=kind, value=value, actual_code=code,
                           pressed=down, injection_return=result, purpose=purpose, critical=CRITICAL,
                           pointer=pointer(), focus=query(['getwindowfocus'])))
        put(OUT / 'inputs.json', EVENTS)
        assert result, 'XTest injection refused'

    def key(self, value, purpose='control'):
        try:
            self.event('key', value, True, purpose)
            time.sleep(.04)
        finally:
            if value in self.keys:
                self.event('key', value, False, purpose)

    def click(self, button, purpose='control'):
        try:
            self.event('button', button, True, purpose)
            time.sleep(.04)
        finally:
            if button in self.buttons:
                self.event('button', button, False, purpose)

    def wheel(self, button, count):
        assert button in (4, 5) and 0 < count <= 80
        for _ in range(count):
            self.click(button, 'wheel')
        time.sleep(.35)

    def move(self, x, y):
        assert not CRITICAL, 'Pointer repair refused'
        query(['mousemove', '--sync', str(x), str(y)])
        EVENTS.append(dict(arm=ACTIVE, monotonic=time.monotonic(), kind='motion', x=x, y=y))

    def shortcut(self, keys):
        assert not CRITICAL
        try:
            for key in keys:
                self.event('key', key, True)
        finally:
            for key in reversed(keys):
                if key in self.keys:
                    self.event('key', key, False)

    def type_text(self, text):
        assert not CRITICAL and len(text) <= 64 and re.fullmatch(r'[a-z0-9-]+', text)
        for character in text:
            self.key('minus' if character == '-' else character)

    def request_close(self, window):
        assert not CRITICAL
        protocols = cmd(['xprop', '-id', window, 'WM_PROTOCOLS'])
        assert 'WM_DELETE_WINDOW' in protocols, 'App has no supported native close protocol'
        self.x.XInternAtom.argtypes, self.x.XInternAtom.restype = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int], ctypes.c_ulong
        self.x.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(XEvent)]
        event = XEvent()
        event.client.type, event.client.send_event = 33, 1  # ClientMessage
        event.client.display, event.client.window = self.display, int(window)
        event.client.message_type = self.x.XInternAtom(self.display, b'WM_PROTOCOLS', 0)
        event.client.format = 32
        event.client.data.longs[0] = self.x.XInternAtom(self.display, b'WM_DELETE_WINDOW', 0)
        event.client.data.longs[1] = 0  # CurrentTime, ICCCM WM_DELETE_WINDOW
        result = self.x.XSendEvent(self.display, int(window), 0, 0, ctypes.byref(event))
        self.x.XSync(self.display, 0)
        EVENTS.append(dict(arm=ACTIVE, monotonic=time.monotonic(), kind='WM_DELETE_WINDOW', window=window, injection_return=result))
        assert result, 'Native close request refused'

    def release_held(self):
        global CRITICAL
        CRITICAL = False
        errors = []
        for kind, values in [('key', self.keys), ('button', self.buttons)]:
            for value in list(values):
                try:
                    self.event(kind, value, False, 'cleanup')
                except BaseException as error:
                    errors.append(str(error))
        assert not errors and not self.keys and not self.buttons, 'Input release unqualified: ' + '; '.join(errors)

    def close(self):
        try:
            self.release_held()
        finally:
            self.x.XCloseDisplay(self.display)


def inside(rect, point):
    x, y, width, height = rect
    return width > 0 and height > 0 and x <= point[0] < x + width and y <= point[1] < y + height


def on_screen(rect, size):
    x, y, width, height = rect
    return width > 0 and height > 0 and x >= 0 and y >= 0 and x + width <= size[0] and y + height <= size[1]


def click_node(inputs, node):
    assert not CRITICAL and node.name != 'Remove project', 'Never activate destructive confirmation'
    x, y, width, height = box(node)
    assert width > 0 and height > 0
    inputs.move(x + width // 2, y + height // 2)
    inputs.click(1)


def cli(binary, env, args, label):
    result = json.loads(cmd([binary, '--json', '--no-start', *args], env=env, timeout=30))
    put(OUT / (label + '-cli.json'), result)
    assert result.get('ok') is True
    return result['data']


def files(projects):
    return {str(p): dict(files={str(f.relative_to(p)): digest(f) for f in sorted(p.rglob('*'))
                               if f.is_file() and '.git' not in f.relative_to(p).parts},
        head=git(p, 'rev-parse', 'HEAD', fixture=True), refs=git(p, 'show-ref', '--head', fixture=True),
        status=git(p, 'status', '--porcelain=v1', '--untracked-files=all', fixture=True), remotes=git(p, 'remote', '-v', fixture=True)) for p in projects}


def conserve(binary, env, projects, expected_rows, expected_files, stub, label):
    rows = cli(binary, env, ['project', 'list'], label + '-projects')['projects']
    terminals = cli(binary, env, ['terminal', 'list', '--all'], label + '-terminals')
    channels = cli(binary, env, ['channel', 'list'], label + '-channels')
    capacity = cli(binary, env, ['chat', 'capacity'], label + '-capacity')
    actual_files = files(projects)
    put(OUT / (label + '-fixture.json'), actual_files)
    assert rows == expected_rows, 'Unexpected project membership/order/metadata change'
    assert terminals['terminals'] == [], 'Unexpected terminal created'
    assert channels['channels'] == [], 'Unexpected channel created'
    assert all(capacity[key] == 0 for key in ('used', 'resident', 'spawning')), 'Unexpected native chat residency/startup'
    assert actual_files == expected_files, 'Fixture files/refs/status changed'
    assert not stub.read_text(), 'Unexpected editor invocation; original receipt retained'


def menu_slice(pid, size):
    if ACTIVE == 'before' and PIXEL_MENU is not None:
        return [(row['text'], tuple(row['bounds'])) for row in PIXEL_MENU.read()['menu_rows']]
    return [(node.name, box(node)) for node in tree(pid) if node.name in M['menu_labels'] and visible(node) and on_screen(box(node), size)]


def menu_gone(pid):
    if ACTIVE == 'before' and PIXEL_MENU is not None:
        return PIXEL_MENU.closed()
    return unique(pid, 'Open folder') is None and unique(pid, 'Remove project…') is None


def dismiss(inputs, pid):
    assert not CRITICAL
    if unique(pid, 'Remove project from cagents?', False):
        click_node(inputs, wait(lambda: unique(pid, 'Cancel')))
    inputs.key('Escape')
    wait(lambda: menu_gone(pid) and unique(pid, 'Remove project from cagents?', False) is None)


def search(pid):
    nodes = [node for node in tree(pid) if visible(node) and node.getState().contains(pyatspi.STATE_EDITABLE)
             and node.name == 'Search projects, branches, channels']
    return nodes[0] if len(nodes) == 1 else None


def clear_search(inputs, pid):
    inputs.shortcut(['Control_L', 'Shift_L', 'f'])
    field = wait(lambda: search(pid))
    wait(lambda: field.getState().contains(pyatspi.STATE_FOCUSED))
    inputs.shortcut(['Control_L', 'a'])
    inputs.key('BackSpace')
    wait(lambda: field.queryText().getText(0, -1) == '')
    click_node(inputs, wait(lambda: unique(pid, 'Search projects')))
    wait(lambda: search(pid) is None)


def anchor(inputs, pid, size):
    clear_search(inputs, pid)
    rows = [node for node in tree(pid) if node.name.startswith('Open project menu-project-') and visible(node) and on_screen(box(node), size)]
    assert rows
    x, y, width, height = box(rows[0])
    inputs.move(x + width // 2, y + height // 2)
    inputs.wheel(4, 60)
    for _ in range(40):
        rows = [node for node in tree(pid) if node.name == 'Open project menu-project-12' and visible(node)
                and on_screen(box(node), size) and box(node)[1] >= size[1] * .55]
        if len(rows) == 1:
            rect = box(rows[0])
            assert rect[2] >= 30 and rect[3] >= 10
            return rect
        inputs.wheel(5, 2)
    raise RuntimeError('Unique target identity near lower edge not established')


def open_menu(inputs, rect, pid):
    if ACTIVE == 'before' and PIXEL_MENU is not None:
        return PIXEL_MENU.open(inputs, rect)
    x, y, width, height = rect
    inputs.move(x + width // 2, y + height // 2)
    inputs.click(3)
    wait(lambda: unique(pid, 'Open folder'))


def calibrate(inputs, pid, window, size, label):
    rect = anchor(inputs, pid, size)
    snapshot(pid, label + '-anchor')
    capture(window, label + '-anchor', size)
    if ACTIVE == 'before' and PIXEL_MENU is not None:
        return PIXEL_MENU.calibrate(inputs, rect, label)
    open_menu(inputs, rect, pid)
    first = box(wait(lambda: unique(pid, 'Open folder')))
    inputs.move(first[0] + first[2] // 2, first[1] + first[3] // 2)
    seen, slices = set(), []
    for index in range(21):
        exposed = [(node.name, box(node)) for node in tree(pid) if node.name in M['editor_labels']
                   and visible(node) and on_screen(box(node), size)]
        seen.update(name for name, _ in exposed)
        slices.append(dict(index=index, exposed=exposed, seen=sorted(seen)))
        snapshot(pid, label + f'-calibration-slice-{index}')
        remove = unique(pid, 'Remove project…')
        if set(M['editor_labels']).issubset(seen) and remove and on_screen(box(remove), size):
            break
        inputs.wheel(5, 3)
    assert set(M['editor_labels']).issubset(seen), 'Not all six real editor labels observed across bounded calibration slices'
    inputs.wheel(5, 60)  # Establish final lower-limit geometry before accepted replay.
    final = box(wait(lambda: unique(pid, 'Remove project…')))
    assert on_screen(final, size)
    point = (final[0] + final[2] // 2, final[1] + final[3] // 2)
    put(OUT / (label + '-calibration.json'), dict(anchor=rect, stationary_point=point, final_bounds=final,
        observed_editor_labels=sorted(seen), slices=slices))
    inputs.key('Escape')
    wait(lambda: menu_gone(pid))
    return rect, point


def modal(pid, target, size, label):
    nodes = tree(pid)
    titles = [node for node in nodes if node.name == 'Remove project from cagents?' and visible(node)]
    cancel, confirm = unique(pid, 'Cancel'), unique(pid, 'Remove project')
    assert len(titles) == 1 and cancel and confirm, 'Expected one exact Remove confirmation with safe Cancel'
    assert cancel.getState().contains(pyatspi.STATE_FOCUSED), 'Cancel was not initially focused'
    title_box, cancel_box, confirm_box = map(box, (titles[0], cancel, confirm))
    left = min(title_box[0], cancel_box[0], confirm_box[0]) - 2
    right = max(b[0] + b[2] for b in (title_box, cancel_box, confirm_box)) + 2
    top, bottom = title_box[1] - 2, max(cancel_box[1] + cancel_box[3], confirm_box[1] + confirm_box[3]) + 2
    region = (left, top, right - left, bottom - top)
    assert on_screen(region, size)
    scoped = []
    for node in nodes:
        if visible(node):
            try:
                b = box(node)
            except NotImplementedError:
                continue
            if inside(region, (b[0], b[1])) and inside(region, (b[0] + b[2] - 1, b[1] + b[3] - 1)):
                scoped.append(node)
    names = [node.name for node in scoped]
    assert target.name in names and str(target) in names, 'Exact target name/path absent from bounded modal region'
    assert not [name for name in names if re.fullmatch(r'menu-project-\d\d', name) and name != target.name]
    put(OUT / (label + '-modal.json'), dict(title_count=1, target=str(target), region=region,
        initially_focused_cancel=describe(cancel), destructive_control_observed_never_activated=describe(confirm),
        scoped_nodes=[describe(node) for node in scoped],
        scope='Bounded actual modal-content region, not whole PID tree; independent image review required'))
    return cancel


def recovery(inputs, pid, window, size, label):
    inputs.key('Escape')
    wait(lambda: menu_gone(pid))
    inputs.shortcut(['Control_L', 'Shift_L', 'f'])
    field = wait(lambda: search(pid))
    wait(lambda: field.getState().contains(pyatspi.STATE_FOCUSED))
    inputs.shortcut(['Control_L', 'a'])
    inputs.type_text('menu-project-12')
    wait(lambda: field.queryText().getText(0, -1) == 'menu-project-12')
    snapshot(pid, label + '-recovery')
    capture(window, label + '-recovery', size)
    inputs.shortcut(['Control_L', 'a'])
    inputs.key('BackSpace')
    wait(lambda: field.queryText().getText(0, -1) == '')
    wait(lambda: any(node.name == 'Open project menu-project-01' for node in tree(pid)))
    click_node(inputs, wait(lambda: unique(pid, 'Search projects')))
    wait(lambda: search(pid) is None)


class BaselineKeyboardNegative(RuntimeError):
    def __init__(self, observation):
        self.observation = observation
        super().__init__('Observed no Remove confirmation after baseline keyboard activation; not a passing case')


class BaselineRemoveUnreachable(BaselineKeyboardNegative):
    def __init__(self, observation):
        self.observation = observation
        RuntimeError.__init__(self, 'Observed no reachable Remove row during bounded baseline wheel; activation states are unrun')


def observe_remove(pid, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        assert any(row['pid'] == pid for row in live(ACTIVE)), 'Owned native UI identity disappeared'
        nodes = tree(pid)
        assert nodes, 'Native accessibility tree unavailable; cannot classify feature-negative'
        titles = [node for node in nodes if node.name == 'Remove project from cagents?' and visible(node)]
        assert len(titles) <= 1, 'Ambiguous multiple confirmations'
        if titles:
            return True
        time.sleep(.1)
    return False


def reset_baseline_negative(error, inputs, pid, window, size, label, activation,
                            binary, env, projects, rows, initial_files, stub, checkout, pin):
    unreachable = isinstance(error, BaselineRemoveUnreachable)
    assert ACTIVE == 'before' and (unreachable or activation in ('Return', 'space')) and not CRITICAL
    assert not inputs.keys and not inputs.buttons, 'Held input prevents safe negative reset'
    observation = dict(error.observation, status='observed_negative_reset_UNVERIFIED', product_oracle_passed=False,
                       negative_kind='unreachable_row' if unreachable else 'keyboard_nonactivation')
    # Preserve the original post-input truth before any dismissal/reset input.
    snapshot(pid, label + '-post-activation-negative')
    capture(window, label + '-post-activation-negative', size)
    put(OUT / (label + '-negative.json'), observation)
    assert geometry(window) == observation['geometry']
    assert (pointer()['X'], pointer()['Y']) == tuple(observation['point'])
    assert any(row['pid'] == pid for row in live('before'))
    nodes = tree(pid)
    assert nodes and not any(visible(node) and (node.getRole() == pyatspi.ROLE_DIALOG or
        node.name in ('Cancel', 'Remove project', 'Remove project from cagents?')) for node in nodes), 'Other/ambiguous modal prevents safe baseline reset'
    conserve(binary, env, projects, rows, initial_files, stub, label + '-negative-before-reset')
    observation['source_before_reset'] = source(checkout, pin)
    dismiss(inputs, pid)  # Outside the completed accepted interval; never repairs its outcome.
    assert unique(pid, 'Search projects') and unique(pid, 'Open project menu-project-12'), 'Underlying controls are not usable'
    recovery(inputs, pid, window, size, label + '-negative-reset')
    conserve(binary, env, projects, rows, initial_files, stub, label + '-negative-after-reset')
    restored_anchor = anchor(inputs, pid, size)
    assert restored_anchor == tuple(observation['anchor']), 'Independent target sidebar state was not restored'
    assert geometry(window) == observation['geometry']
    observation['restored_anchor'] = restored_anchor
    observation.update(status='observed_negative_safely_reset', reset='Escape and useful search typing/clear verified',
                       source_after_reset=source(checkout, pin))
    put(OUT / (label + '-negative.json'), observation)
    return observation


def trial(inputs, pid, window, size, label, activation, target, binary, env, projects, rows, initial_files, stub):
    global CRITICAL
    dismiss(inputs, pid)
    rect, point = calibrate(inputs, pid, window, size, label)
    open_menu(inputs, rect, pid)
    inputs.move(*point)  # The LAST pointer placement, before the accepted wheel.
    time.sleep(.25)
    initial_geometry, initial_focus, initial_pointer = geometry(window), query(['getwindowfocus']), pointer()
    assert (initial_pointer['X'], initial_pointer['Y']) == point
    top_slice = menu_slice(pid, size)
    top_image = capture(window, label + '-top', size)
    snapshot(pid, label + '-top')
    event_start = len(EVENTS)
    CRITICAL = True
    try:
        inputs.wheel(5, 60)
        final = box(wait(lambda: unique(pid, 'Remove project…')))
        assert on_screen(final, size) and inside(final, point), 'Pointer missed final row; discarded without repair'
        assert pointer() == initial_pointer and geometry(window) == initial_geometry and query(['getwindowfocus']) == initial_focus
        bottom_slice = menu_slice(pid, size)
        bottom_image = capture(window, label + '-bottom', size)
        snapshot(pid, label + '-bottom')
        overflow = top_slice != bottom_slice and top_image['png_sha256'] != bottom_image['png_sha256']
        if size == (900, 600) and ACTIVE == 'after':
            assert overflow and not any(name == 'Remove project…' for name, _ in top_slice), 'Narrow clipped final-row reachability not observed'
        assert unique(pid, 'Remove project from cagents?', False) is None, 'Wheel unexpectedly activated a dialog'
        assert not stub.read_text(), 'Wheel unexpectedly invoked editor'
        if activation == 'click':
            inputs.click(1, 'activation')
        else:
            inputs.key(activation, 'activation')
        assert pointer() == initial_pointer and geometry(window) == initial_geometry
    finally:
        CRITICAL = False
    observation = dict(activation=activation, anchor=rect, point=point, geometry=initial_geometry,
        final_bounds=final, top_slice=top_slice, bottom_slice=bottom_slice, overflow_observed=overflow,
        event_start=event_start, event_end=len(EVENTS), expected_modal_observed=False)
    put(OUT / (label + '-trial.json'), observation)
    if not observe_remove(pid, timeout=5):
        if ACTIVE == 'before' and activation in ('Return', 'space'):
            raise BaselineKeyboardNegative(observation)
        raise RuntimeError('Expected Remove confirmation absent after native activation')
    cancel = modal(pid, target, size, label)
    if ACTIVE == 'before' and PIXEL_MENU is not None:
        PIXEL_MENU.modal_closed(label)
    snapshot(pid, label + '-dialog')
    capture(window, label + '-dialog', size)
    put(OUT / (label + '-trial.json'), dict(activation=activation, anchor=rect, point=point, geometry=initial_geometry,
        final_bounds=final, top_slice=top_slice, bottom_slice=bottom_slice, overflow_observed=overflow,
        event_start=event_start, event_end=len(EVENTS),
        oracle='One exact-target modal; Cancel initially focused; actual key/button release completed without confirmation',
        visual_review='Required; no source-only native acceptance'))
    click_node(inputs, cancel)
    wait(lambda: unique(pid, 'Remove project from cagents?', False) is None)
    conserve(binary, env, projects, rows, initial_files, stub, label)
    current_row = wait(lambda: unique(pid, 'Open project menu-project-12'))
    assert box(current_row) == rect, 'Target row moved after Cancel; fresh Escape control refused'
    open_menu(inputs, rect, pid)
    recovery(inputs, pid, window, size, label)


def limits(inputs, pid, window, size, label):
    dismiss(inputs, pid)
    rect, point = calibrate(inputs, pid, window, size, label)
    open_menu(inputs, rect, pid)
    inputs.move(*point)
    inputs.wheel(5, 60)
    bottom = menu_slice(pid, size)
    assert inside(box(wait(lambda: unique(pid, 'Remove project…'))), point)
    capture(window, label + '-bottom', size)
    inputs.wheel(5, 20)
    assert menu_slice(pid, size) == bottom and unique(pid, 'Remove project from cagents?', False) is None
    inputs.wheel(4, 60)
    top = menu_slice(pid, size)
    if size == (900, 600) and ACTIVE == 'after':
        assert top != bottom, 'Reverse wheel did not expose a different narrow slice'
    capture(window, label + '-top', size)
    inputs.wheel(5, 60)
    assert menu_slice(pid, size) == bottom
    capture(window, label + '-bottom-restored', size)
    put(OUT / (label + '-limits.json'), dict(top=top, bottom=bottom, slices_changed=top != bottom,
        lower_limit_stable=True, scope='Native XTest line wheel only; not touchpad pixel deltas'))
    inputs.key('Escape')
    wait(lambda: menu_gone(pid))


def tooltip(inputs, pid, window, size, label):
    button = wait(lambda: unique(pid, 'Search projects'))
    b = box(button)
    center = (b[0] + b[2] // 2, b[1] + b[3] // 2)
    inputs.move(size[0] - 20, size[1] // 2)
    time.sleep(.3)
    inputs.move(*center)
    time.sleep(.8)
    capture(window, label + '-before-press', size)
    snapshot(pid, label + '-before-press')
    inputs.event('button', 1, True)
    time.sleep(.8)
    capture(window, label + '-held', size)
    snapshot(pid, label + '-held')
    inputs.event('button', 1, False)
    time.sleep(.8)
    capture(window, label + '-released', size)
    snapshot(pid, label + '-released')
    inputs.move(size[0] - 20, size[1] // 2)
    time.sleep(.8)
    capture(window, label + '-left', size)
    inputs.move(*center)
    time.sleep(.8)
    capture(window, label + '-reentered', size)
    snapshot(pid, label + '-reentered')
    put(OUT / (label + '-visual-review.json'), dict(status='UNQUALIFIED pending independent five-image review',
        button_bounds=b, expected='Tooltip before press; absent held/released; returns after leave/reenter',
        note='Missing AT-SPI tooltip label is not absence proof; distinguish normal search toggle.'))
    dismiss(inputs, pid)


def private_env(root):
    env = os.environ.copy()
    for key, leaf in [('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'),
                      ('XDG_STATE_HOME', 'state'), ('XDG_RUNTIME_DIR', 'run'), ('TMPDIR', 'tmp')]:
        path = root / leaf
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.update(CARGO_HOME=menu_cargo_cache, RUSTUP_HOME=menu_rustup_cache, CARGO_BUILD_JOBS='2',
        ACCESSIBILITY_ENABLED='1', NO_AT_BRIDGE='0', WINIT_UNIX_BACKEND='x11', LIBGL_ALWAYS_SOFTWARE='1',
        RUST_LOG='cagents=debug,cagentsd=debug,winit_x11=info,warn')
    assert env.get('HOME') == original_home and env.get('CODEX_HOME') == original_codex_home
    return env


def bootstrap(base):
    root = base / 'bootstrap'
    root.mkdir(mode=0o700)
    env = private_env(root)
    launcher = next(p for p in [Path('/usr/libexec/at-spi-bus-launcher'), Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')] if p.exists())
    log = (OUT / 'accessibility.log').open('wb')
    spawn([launcher, '--launch-immediately'], env=env, stdout=log, stderr=subprocess.STDOUT)
    for prop in ('IsEnabled', 'ScreenReaderEnabled'):
        cmd(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
             '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', prop, '<true>'], env=env)
    reply = cmd(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
                 '--method', 'org.a11y.Bus.GetAddress'], env=env)
    os.environ['AT_SPI_BUS_ADDRESS'] = ast.literal_eval(reply)[0]
    registry = next(p for p in [Path('/usr/libexec/at-spi2-registryd'), Path('/usr/lib/at-spi2-core/at-spi2-registryd')] if p.exists())
    spawn([registry], env=dict(env, DBUS_SESSION_BUS_ADDRESS=os.environ['AT_SPI_BUS_ADDRESS']), stdout=log, stderr=subprocess.STDOUT)
    config = base / 'openbox.xml'
    config.write_text('<openbox_config xmlns="http://openbox.org/3.4/rc"><focus><focusNew>yes</focusNew></focus><applications><application class="*"><decor>no</decor><position force="yes"><x>0</x><y>0</y></position></application></applications></openbox_config>')
    spawn(['openbox', '--config-file', config], env=env, stdout=(OUT / 'openbox.log').open('wb'), stderr=subprocess.STDOUT)


def matched(window, pin, env, label):
    pid = int(query(['getwindowpid', window]))
    ui_identity = [row for row in live(label) if row['pid'] == pid]
    assert len(ui_identity) == 1, 'Native window PID is not an owned arm identity'
    actual = (Path('/proc') / str(pid) / 'exe').resolve()
    assert actual == (TARGET / 'dev-runtime/cagents-ui').resolve(), 'Wrong native UI binary'
    records = {}
    for name in ('cagents', 'cagentsd', 'cagents-ui'):
        path = actual.parent / name
        assert path.is_file() and os.access(path, os.X_OK) and path.resolve().is_relative_to(TARGET)
        version = cmd([path, '--version'], env=env).strip()
        assert 'local.' + pin['head'][:7] in version, 'Executable metadata does not match source'
        records[name] = dict(path=str(path), sha256=digest(path), version=version)
    binary = actual.parent / 'cagents'
    status = cli(binary, env, ['daemon', 'status'], label + '-daemon')
    assert status['running'] and status['instance'] == 'development'
    socket = Path(status['socket'])
    assert socket.is_relative_to(Path(env['XDG_RUNTIME_DIR'])) and len(os.fsencode(socket)) < 108
    assert status['daemon_version'] and 'local.' + pin['head'][:7] in status['daemon_version']
    daemons = []
    for row in live(label):
        proc = Path('/proc') / str(row['pid'])
        try:
            if (proc / 'exe').resolve() == (actual.parent / 'cagentsd').resolve():
                assert ('XDG_RUNTIME_DIR=' + env['XDG_RUNTIME_DIR']).encode() in (proc / 'environ').read_bytes().split(b'\0')
                daemons.append(dict(row, executable_sha256=digest(proc / 'exe')))
        except (FileNotFoundError, ProcessLookupError):
            pass
    assert len(daemons) == 1, 'Actual private matched daemon identity is ambiguous'
    put(OUT / (label + '-binaries.json'), dict(ui_pid=pid, ui_identity=ui_identity[0], executables=records, daemon=daemons[0], status=status))
    return pid, binary


def run_arm(label, checkout, pin, base, projects, initial_files, inputs, stubs):
    global ACTIVE, CRITICAL, PIXEL_MENU
    ACTIVE = label
    PIXEL_MENU = None
    result = dict(arm=label, pin=pin, status='running', cases=[], unrun_cases=list(M['cases']))
    ARMS.append(result)
    root = base / label
    root.mkdir(mode=0o700)
    env = private_env(root)
    stub = root / 'editor-invocations.jsonl'
    stub.write_text('')
    env.update(PATH=str(stubs) + os.pathsep + os.environ['PATH'], MENU_STUB_LOG=str(stub))
    settings = Path(env['XDG_CONFIG_HOME']) / 'cagents-development/settings.toml'
    settings.parent.mkdir(mode=0o700)
    settings.write_text(M['settings'])
    result.update(source_before=source(checkout, pin), settings_sha256=digest(settings), home_unchanged=True,
        codex_home_unchanged=True, private_paths={k: env[k] for k in env if k.startswith('XDG_') or k == 'TMPDIR'})
    runner, binary, pid, window = None, None, None, None
    cleanup_errors = []
    try:
        args = [str(checkout / 'scripts/run-development'), '--browserless', '--debug', '--log-file', str(OUT / (label + '-ui.log'))]
        result.update(runner_command=args, startup_deadline_seconds=5400)
        runner = spawn(args, cwd=checkout, env=env, stdout=(OUT / (label + '-runner.log')).open('wb'), stderr=subprocess.STDOUT)
        def ready():
            assert runner.poll() is None, 'Runner exited before native window'
            windows = cmd(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], expected=None).split()
            return windows[0] if len(windows) == 1 else None
        window = wait(ready, timeout=5400, interval=2)
        pid, binary = matched(window, pin, env, label)
        wait(lambda: tree(pid))
        for project in reversed(projects):  # Newest-first production ordering puts12 last.
            cli(binary, env, ['project', 'open', str(project)], label + '-seed-' + project.name)
        rows = cli(binary, env, ['project', 'list'], label + '-initial-projects')['projects']
        assert [row['repository'] for row in rows] == [str(project) for project in projects]
        wait(lambda: any(node.name.startswith('Open project menu-project-') for node in tree(pid)))
        conserve(binary, env, projects, rows, initial_files, stub, label + '-initial')
        for size_name, size in [('wide', (1280, 800)), ('narrow', (900, 600))]:
            assert not CRITICAL
            query(['windowactivate', '--sync', window])
            query(['windowsize', '--sync', window, *map(str, size)])
            query(['windowmove', '--sync', window, '0', '0'])
            wait(lambda: geometry(window) == dict(X=0, Y=0, WIDTH=size[0], HEIGHT=size[1], SCREEN=0))
            time.sleep(.6)
            configured = settings.read_text()
            assert re.search(r'^ui_scale\s*=\s*1\.5\s*$', configured, re.M)
            assert re.search(r'^app_theme\s*=\s*"dark"\s*$', configured, re.M)
            (OUT / (label + '-' + size_name + '-settings.toml')).write_text(configured)
            put(OUT / (label + '-' + size_name + '-geometry.json'), dict(physical_window=geometry(window),
                configured_ui_scale=1.5, settings_sha256=digest(settings), current_native_scale_factor='UNMEASURED',
                startup_scale_guess='Supporting log only, never a current per-window measurement',
                display_configuration={k: os.environ.get(k) for k in ('DISPLAY', 'WINIT_X11_SCALE_FACTOR', 'GDK_SCALE', 'GDK_DPI_SCALE')}))
            PIXEL_MENU = BaselinePixels(pid, window, size, label + '-' + size_name) if label == 'before' else None
            blocked_by_unreachable = None
            for activation, short in [('Return', 'return'), ('space', 'space'), ('click', 'click'), (None, 'limits'), (None, 'tooltip')]:
                if CANCEL_SIGNAL is not None:
                    raise QualificationCancelled('Collection remains cancelled; next case refused')
                case = size_name + '-' + short
                if label == 'before' and blocked_by_unreachable and short in ('return', 'space', 'click', 'limits'):
                    result['cases'].append(dict(case=case, status='UNRUN_no_observed_activation_target', reason=blocked_by_unreachable))
                    continue
                result['unrun_cases'].remove(case)
                row = dict(case=case, status='running')
                result['cases'].append(row)
                case_label = label + '-project-menu-' + case
                if activation:
                    try:
                        trial(inputs, pid, window, size, case_label, activation, projects[-1], binary, env, projects, rows, initial_files, stub)
                        row['status'] = 'machine_oracle_complete_visual_review_pending'
                    except BaselineKeyboardNegative as error:
                        negative = reset_baseline_negative(error, inputs, pid, window, size, case_label, activation,
                            binary, env, projects, rows, initial_files, stub, checkout, pin)
                        row.update(status='observed_negative_unreachable_row' if isinstance(error, BaselineRemoveUnreachable) else 'observed_negative_missing_keyboard_activation',
                                   product_oracle_passed=False, failure=str(error), reset=negative['reset'],
                                   negative_kind=negative['negative_kind'])
                        if isinstance(error, BaselineRemoveUnreachable):
                            blocked_by_unreachable = case_label + '-unreachable.json'
                            row['missing_before_states'] = negative['missing_before_states']
                elif short == 'limits':
                    try:
                        limits(inputs, pid, window, size, case_label)
                        row['status'] = 'machine_oracle_complete_visual_review_pending'
                    except BaselineRemoveUnreachable as error:
                        negative = reset_baseline_negative(error, inputs, pid, window, size, case_label, activation,
                            binary, env, projects, rows, initial_files, stub, checkout, pin)
                        row.update(status='observed_negative_unreachable_row', product_oracle_passed=False,
                                   failure=str(error), reset=negative['reset'], missing_before_states=negative['missing_before_states'])
                        blocked_by_unreachable = case_label + '-unreachable.json'
                else:
                    tooltip(inputs, pid, window, size, case_label)
                    row['status'] = 'images_recorded_visual_oracle_UNQUALIFIED'
                conserve(binary, env, projects, rows, initial_files, stub, case_label + '-final')
        result['status'] = ('baseline_observations_complete_with_explicit_negative_or_unrun_states' if
            any(row['status'].startswith('observed_negative') for row in result['cases'])
            else 'machine_flow_complete_visual_review_pending')
    except QualificationCancelled as error:
        result.update(status='cancelled', error=str(error), cancellation_signal=CANCEL_SIGNAL)
        if result['cases'] and result['cases'][-1]['status'] == 'running':
            result['cases'][-1]['status'] = 'cancelled'
        raise
    except Exception as error:
        if CANCEL_SIGNAL is not None:
            result.update(status='cancelled', error=str(error), cancellation_signal=CANCEL_SIGNAL)
            if result['cases'] and result['cases'][-1]['status'] == 'running':
                result['cases'][-1]['status'] = 'cancelled'
            raise QualificationCancelled('Original cancellation remains fatal after input/receipt failure') from error
        result.update(status='failed', error=str(error))
        if result['cases'] and result['cases'][-1]['status'] == 'running':
            result['cases'][-1].update(status='failed', error=str(error))
        if window and pid:
            try:
                current = geometry(window)
                capture(window, label + '-failure', (current['WIDTH'], current['HEIGHT']))
                snapshot(pid, label + '-failure')
            except Exception as capture_error:
                result['missing_failure_capture'] = str(capture_error)
    finally:
        CRITICAL = False
        inputs_released = False
        try:
            inputs.release_held()
            inputs_released = True
        except BaseException as error:
            cleanup_errors.append('Held input release failed: ' + str(error))
        if binary:
            try:
                inventory = cli(binary, env, ['project', 'list'], label + '-cleanup-projects')
                result['final_project_inventory'] = inventory
                if 'rows' in locals():
                    result['project_conservation'] = inventory['projects'] == rows
                    if not result['project_conservation']:
                        result.update(status='failed', error='Unexpected project change; original inventory retained')
            except Exception as error:
                cleanup_errors.append('Final project inventory unavailable: ' + str(error))
        if inputs_released and runner and runner.poll() is None and window and pid:
            try:
                dismiss(inputs, pid)
                inputs.request_close(window)  # Never XDestroyWindow/windowclose.
                wait(lambda: runner.poll() is not None, timeout=30)
                result['runner_exit'] = runner.wait()
                assert result['runner_exit'] == 0
            except Exception as error:
                cleanup_errors.append('Native UI did not close normally: ' + str(error))
        if binary:
            try:
                terminal_rows = cli(binary, env, ['terminal', 'list', '--all'], label + '-cleanup-terminals')
                assert terminal_rows['terminals'] == [], 'Unexpected private sessions; guarded shutdown cannot be waived'
                cli(binary, env, ['daemon', 'stop', '--require-empty'], label + '-stop-daemon')
                wait(lambda: not (Path(env['XDG_RUNTIME_DIR']) / 'cagents-development/cagentsd.sock').exists(), timeout=15)
            except Exception as error:
                cleanup_errors.append('Guarded private daemon stop failed: ' + str(error))
        try:
            close_owned(label)
        except Exception as error:
            cleanup_errors.append(str(error))
        if binary:
            original = json.loads((OUT / (label + '-binaries.json')).read_text())['executables']
            result['executable_sha256_after'] = {name: digest(Path(row['path'])) for name, row in original.items()}
            assert result['executable_sha256_after'] == {name: row['sha256'] for name, row in original.items()}, 'Matched binaries changed during arm'
        result.update(cleanup_errors=cleanup_errors, source_after=source(checkout, pin), stub_receipt=stub.read_text())
        (OUT / (label + '-editor-invocations.jsonl')).write_text(stub.read_text())
        observed = files(projects)
        put(OUT / (label + '-fixture-after.json'), observed)
        put(OUT / (label + '-result.json'), result)
        assert observed == initial_files, 'Fixture conservation failed; next arm refused'
        assert not cleanup_errors and not live(label), 'Cleanup failed; next arm/build refused'
        ACTIVE = None
    return result


class QualificationCancelled(BaseException):
    pass


def cancelled(signum, frame):
    global CANCEL_SIGNAL
    CANCEL_SIGNAL = signum
    put(OUT / 'cancellation.json', dict(signal=signum, monotonic=time.monotonic(), arm=ACTIVE, critical=CRITICAL))
    raise QualificationCancelled('Native control cancelled; stop collection and close owned identities')


def main():
    global pyatspi, Image
    guard()
    OUT.mkdir(mode=0o700, parents=True, exist_ok=True)
    put(OUT / 'context.json', dict(hosted=True, inherited_cagents_names=[], wayland_names=[],
        home_unchanged=True, codex_home_unchanged=True,
        display_configuration={k: os.environ.get(k) for k in ('DISPLAY', 'WINIT_X11_SCALE_FACTOR', 'GDK_SCALE', 'GDK_DPI_SCALE')},
        policy='No inherited authority or scale variable removed/replaced'))
    assert M['never_merge'] is True and len(M['cases']) == 10
    assert TARGET.is_absolute() and TARGET.is_relative_to(Path(os.environ['RUNNER_TEMP']).resolve())
    assert os.environ.get('RUSTUP_TOOLCHAIN') == '1.93.0'
    libc = ctypes.CDLL(None, use_errno=True)
    assert libc.prctl(36, 1, 0, 0, 0) == 0
    signal.signal(signal.SIGTERM, cancelled)
    signal.signal(signal.SIGINT, cancelled)
    base = Path(tempfile.mkdtemp(prefix='mn.', dir='/tmp'))
    base.chmod(0o700)
    put(OUT / 'task-root.json', dict(root=str(base), target=str(TARGET), mode='0700'))
    inputs, failed, cleanup_errors = None, False, []
    try:
        display = cmd(['xdpyinfo'])
        assert re.search(r'dimensions:\s+1280x800 pixels', display)
        assert re.search(r'resolution:\s+96x96 dots per inch', display)
        cmd(['xrandr', '--current'])
        cmd(['xprop', '-root', 'RESOURCE_MANAGER'], expected=None)
        (OUT / 'tesseract-version.txt').write_text(cmd(['tesseract', '--version']))
        languages = cmd(['tesseract', '--list-langs'])
        (OUT / 'tesseract-languages.txt').write_text(languages)
        assert 'eng' in languages.splitlines(), 'Exact English OCR model unavailable'
        bootstrap(base)
        import pyatspi
        from PIL import Image
        inputs = Inputs()
        projects = []
        project_root = base / 'projects'
        project_root.mkdir(mode=0o700)
        for i in range(1, 13):
            project = project_root / f'menu-project-{i:02d}'
            project.mkdir(mode=0o700)
            (project / 'fixture.txt').write_text(f'Synthetic project-menu fixture {i:02d}.\n')
            git(project, 'init', '--initial-branch=main', fixture=True)
            git(project, 'add', 'fixture.txt', fixture=True)
            git(project, '-c', 'user.name=Menu Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false',
                'commit', '-m', 'Seed synthetic project', fixture=True)
            projects.append(project)
        initial_files = files(projects)
        put(OUT / 'fixture-before.json', initial_files)
        stubs = base / 'editors'
        stubs.mkdir(mode=0o700)
        for editor in M['editor_executables']:
            path = stubs / editor
            path.write_text('#!/usr/bin/python3\nimport json,os,sys\nwith open(os.environ["MENU_STUB_LOG"],"a") as stream:\n stream.write(json.dumps({"editor":' + repr(editor) + ',"argv":sys.argv[1:]})+"\\n")\n')
            path.chmod(0o700)
        put(OUT / 'editor-stubs.json', {name: digest(stubs / name) for name in M['editor_executables']})
        for label, checkout in [('before', BEFORE), ('after', AFTER)]:
            if CANCEL_SIGNAL is not None:
                raise QualificationCancelled('Collection remains cancelled; next arm refused')
            record = run_arm(label, checkout, M[label], base, projects, initial_files, inputs, stubs)
            if label == 'after' and record['status'] != 'machine_flow_complete_visual_review_pending':
                failed = True
        observed = files(projects)
        put(OUT / 'fixture-after.json', observed)
        assert observed == initial_files
    except BaseException as error:
        failed = True
        put(OUT / 'global-error.json', dict(error=str(error), type=type(error).__name__))
        raise
    finally:
        try:
            if inputs:
                inputs.close()
        except Exception as error:
            cleanup_errors.append(str(error))
        try:
            close_owned()
        except Exception as error:
            cleanup_errors.append(str(error))
        put(OUT / 'owned-processes.json', OWNED)
        put(OUT / 'inputs.json', EVENTS)
        put(OUT / 'commands.json', COMMANDS)
        put(OUT / 'direct-child-exits.json', [dict(identity=child.menu_identity, exit=child.poll()) for child in CHILDREN])
        put(OUT / 'cleanup.json', dict(errors=cleanup_errors, subreaper=True, live_owned=live(),
            identity_rule='PID/start/group recorded before wait; each signal revalidates start identity'))
        baseline_failed = any(arm['arm'] == 'before' and arm['status'] == 'failed' for arm in ARMS)
        baseline_negatives = sum(row['status'].startswith('observed_negative') for arm in ARMS for row in arm['cases'])
        put(OUT / 'result.json', dict(collection_status='failed' if failed or cleanup_errors else ('completed_with_baseline_failure' if baseline_failed else ('completed_with_baseline_negatives' if baseline_negatives else 'completed')), arms=ARMS, baseline_negative_count=baseline_negatives,
            acceptance='UNQUALIFIED pending independent actual-image/UX review and remaining PR gates', limits=M['limits']))
        (OUT / 'sha256.txt').write_text(''.join(f'{digest(p)}  {p.relative_to(OUT)}\n' for p in sorted(OUT.rglob('*'))
            if p.is_file() and p.name != 'sha256.txt'))
        if cleanup_errors:
            raise RuntimeError('Native cleanup unqualified: ' + '; '.join(cleanup_errors))
    if failed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
