"""UNRUN #1984 native observation/input primitives; never merge.
Exact AST bodies retained from frozen #1900 control664cb4d. Only reusable
custody/capture/guard/input/desktop primitives transfer; no donor pin/result,
provider, installation, scope or UI assertions transfer. Native use stays HELD.
"""
import ast
import csv
import ctypes
import hashlib
import io
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import time

original_home, original_codex_home = os.environ.get('HOME'), os.environ.get('CODEX_HOME')
editor_cargo_cache = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
editor_rustup_cache = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))
EVENTS, COMMANDS, ARMS, OWNED, CHILDREN = [], [], [], {}, []
ACTIVE, CRITICAL = None, False
CAPTURES, CANCEL_SIGNAL, SCREEN, CLIENT_RECT = {}, None, None, None
A11Y_READ, A11Y_QUERIES = None, []


def configure(manifest, evidence_root, target):
    global M, OUT, TARGET, REQUESTED_RENDERER
    # Configuration itself grants no authority and launches nothing.
    M, OUT, TARGET = manifest, Path(evidence_root), Path(target)
    REQUESTED_RENDERER = M['backend']['requested_environment']


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
    process.native_identity = dict(OWNED[f'{process.pid}:{current["start"]}'])
    if 'coordinate-install' in [str(arg) for arg in args]:
        try:
            process.native_custody = identity(process.pid, Path(args[0]), kwargs.get('env'))
        except (OSError, AssertionError) as error:
            raise NamespaceCustodyUnavailable('UNQUALIFIED_namespace_custody: cannot retain actual coordinator identity: '+str(error)) from error
    return process


def cmd(args, env=None, cwd=None, timeout=20, expected=0):
    guard()
    args = [str(arg) for arg in args]
    directory = OUT / 'commands' / f'{len(COMMANDS):05d}'
    directory.mkdir(parents=True)
    record = dict(argv=args, cwd=str(cwd) if cwd else None, arm=ACTIVE, started=time.monotonic(), deadline_seconds=timeout)
    COMMANDS.append(record)
    put(directory / 'command.json', record)
    with (directory / 'stdout.log').open('wb') as stdout, (directory / 'stderr.log').open('wb') as stderr:
        process = spawn(args, env=env, cwd=cwd, stdout=stdout, stderr=stderr)
        record['identity'] = process.native_identity
        if hasattr(process, 'native_custody'):
            record['coordinator_custody'] = process.native_custody
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


def stale_a11y(error):
    get_name = getattr(error, 'get_dbus_name', None)
    if callable(get_name) and get_name() == 'org.freedesktop.DBus.Error.UnknownObject':
        return True
    return (isinstance(error, GLib.Error) and error.domain == 'atspi_error' and error.code == 1 and
            re.fullmatch(r"Unknown object '/org/a11y/atspi/(?:accessible|cache)(?:/[A-Za-z0-9_]+)*'", error.message) is not None)


def a11y_read(function):
    # Only decorate complete, read-only queries. Never an input/gesture/reset.
    def query(*args, **kwargs):
        global A11Y_READ
        if A11Y_READ is not None:
            return function(*args, **kwargs)
        record = dict(query=function.__name__, arm=ACTIVE, started=time.monotonic(), attempts=[])
        A11Y_QUERIES.append(record)
        A11Y_READ = record
        deadline = time.monotonic() + 5
        try:
            for attempt in range(3):
                if CANCEL_SIGNAL is not None:
                    raise QualificationCancelled('Cancelled accessibility query')
                assert attempt == 0 or time.monotonic() < deadline, 'Whole accessibility query retry deadline exceeded'
                try:
                    value = function(*args, **kwargs)
                    record['attempts'].append(dict(attempt=attempt+1, status='complete'))
                    return value
                except Exception as error:
                    get_name = getattr(error, 'get_dbus_name', None)
                    name = get_name() if callable(get_name) else None
                    record['attempts'].append(dict(attempt=attempt+1, type=type(error).__name__, name=name, error=str(error)))
                    if CANCEL_SIGNAL is not None:
                        raise QualificationCancelled('Cancelled accessibility query') from error
                    if not stale_a11y(error) or attempt == 2 or time.monotonic() >= deadline:
                        raise
                    time.sleep(.05)
        finally:
            record['returned'] = time.monotonic()
            put(OUT / 'accessibility-queries.json', A11Y_QUERIES)
            A11Y_READ = None
    return query


@a11y_read
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
        assert math.isfinite(confidence) and 0 <= confidence <= 100 and on_screen(bounds, size, origin=(0, 0))
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


def box(node):
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


@a11y_read
def snapshot(pid, name):
    nodes = tree(pid)
    assert nodes, 'PID-filtered native accessibility tree disappeared'
    put(OUT / (name + '-a11y.json'), dict(pid=pid, monotonic=time.monotonic(), nodes=[describe(node) for node in nodes]))


def query(tool):
    return cmd(['xdotool', *tool]).strip()


def geometry(window):
    return SCREEN.client(window)


def pointer():
    data = dict(line.split('=', 1) for line in query(['getmouselocation', '--shell']).splitlines() if '=' in line)
    return {key: int(data[key]) for key in ('X', 'Y', 'SCREEN')}


def pixel_metrics(image):
    rgb = image.convert('RGB')
    luminance = rgb.convert('L')
    histogram = luminance.histogram()
    count = sum(histogram)
    mean = sum(value*number for value, number in enumerate(histogram))/count
    deviation = math.sqrt(sum(number*(value-mean)**2 for value, number in enumerate(histogram))/count)
    occupied = [value for value, number in enumerate(histogram) if number]
    colors = rgb.getcolors(maxcolors=256)
    return dict(luminance_min=occupied[0], luminance_max=occupied[-1], luminance_stddev=deviation,
                nonblack_fraction=sum(histogram[9:])/count, distinct_rgb_colors=len(colors) if colors is not None else 257)


def nonblank(metrics):
    thresholds = M['capture']['nonblank']
    return (metrics['luminance_max']-metrics['luminance_min'] >= thresholds['minimum_luminance_span'] and
            metrics['luminance_stddev'] >= thresholds['minimum_luminance_stddev'] and
            metrics['nonblack_fraction'] >= thresholds['minimum_nonblack_fraction'] and
            metrics['distinct_rgb_colors'] >= thresholds['minimum_distinct_rgb_colors'])


def capture(window, name, size):
    # Pure visible-screen pixels: no resize/move/focus/pointer or accessibility action.
    state, point, focus = SCREEN.state(window), pointer(), query(['getwindowfocus'])
    actual = state['client']
    result = dict(arm=ACTIVE, window=window, geometry=actual, pointer=point, focus=focus,
        x11_before=state, xdotool_relative_diagnostic=geometry_diagnostic(window), critical=CRITICAL,
        capture='Visible root pixels cropped at independently verified absolute client bounds; no scale/repair')
    put(OUT / (name + '-capture.json'), result)
    root_png, direct_png, png = (OUT / (name + suffix) for suffix in ('-root.png', '-drawable-diagnostic.png', '.png'))
    cmd(['import', '-window', 'root', root_png])
    direct_command = len(COMMANDS)
    cmd(['import', '-window', window, direct_png], expected=None)
    result.update(root_png_sha256=digest(root_png), drawable_command_index=direct_command,
                  drawable_exit=COMMANDS[direct_command]['exit'])
    if direct_png.exists():
        result['drawable_png_sha256'] = digest(direct_png)
        with Image.open(direct_png) as diagnostic:
            result['drawable_size'] = diagnostic.size
            result['drawable_metrics'] = pixel_metrics(diagnostic)
    with Image.open(root_png) as root:
        result['root_size'] = root.size
        bounds = (actual['X'], actual['Y'], actual['X']+actual['WIDTH'], actual['Y']+actual['HEIGHT'])
        result['client_crop_bounds'] = bounds
        put(OUT / (name + '-capture.json'), result)
        assert root.size == (state['root']['WIDTH'], state['root']['HEIGHT'])
        assert on_screen((actual['X'], actual['Y'], actual['WIDTH'], actual['HEIGHT']), root.size, origin=(0, 0)), 'Client crop is outside visible root'
        client = root.crop(bounds)
        client.save(png)
        client.save(OUT / (name + '.webp'), lossless=True)
        result.update(client_crop_bounds=bounds, client_metrics=pixel_metrics(client), client_size=client.size)
    result.update(x11_after=SCREEN.state(window), pointer_after=pointer(), focus_after=query(['getwindowfocus']),
                  png_sha256=digest(png), webp_sha256=digest(OUT / (name + '.webp')), monotonic=time.monotonic())
    put(OUT / (name + '-capture.json'), result)
    assert actual == CLIENT_RECT and (actual['WIDTH'], actual['HEIGHT']) == size, 'Unexpected absolute client geometry'
    assert result['client_size'] == size and not state['occluders'], 'Mapped sibling obscures the native client'
    assert result['x11_after'] == state and result['pointer_after'] == point and result['focus_after'] == focus, 'Capture state changed'
    assert nonblank(result['client_metrics']), 'Blank/ambiguous visible client pixels; no OCR or acceptance'
    CAPTURES[png.resolve()] = result
    return result


def geometry_diagnostic(window):
    # xdotool may report parent-relative geometry under a reparenting WM; diagnostic only.
    return query(['getwindowgeometry', '--shell', window])


class WindowAttributes(ctypes.Structure):
    # Xlib.h XWindowAttributes, including the fields needed to preserve ABI layout.
    _fields_ = [(name, ctypes.c_int) for name in ('x', 'y', 'width', 'height', 'border_width', 'depth')] + [
        ('visual', ctypes.c_void_p), ('root', ctypes.c_ulong), ('window_class', ctypes.c_int),
        ('bit_gravity', ctypes.c_int), ('win_gravity', ctypes.c_int), ('backing_store', ctypes.c_int),
        ('backing_planes', ctypes.c_ulong), ('backing_pixel', ctypes.c_ulong), ('save_under', ctypes.c_int),
        ('colormap', ctypes.c_ulong), ('map_installed', ctypes.c_int), ('map_state', ctypes.c_int),
        ('all_event_masks', ctypes.c_long), ('your_event_mask', ctypes.c_long), ('do_not_propagate_mask', ctypes.c_long),
        ('override_redirect', ctypes.c_int), ('screen', ctypes.c_void_p)]


class XError(ctypes.Structure):
    _fields_ = [('type', ctypes.c_int), ('display', ctypes.c_void_p), ('resourceid', ctypes.c_ulong),
                ('serial', ctypes.c_ulong), ('error_code', ctypes.c_ubyte), ('request_code', ctypes.c_ubyte), ('minor_code', ctypes.c_ubyte)]


class NativeScreen:
    """Read-only Xlib geometry/stacking queries; no geometry or focus mutations."""
    def __init__(self):
        self.x = ctypes.CDLL('libX11.so.6')
        self.x.XOpenDisplay.argtypes, self.x.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
        self.display = self.x.XOpenDisplay(None)
        assert self.display
        self.x.XDefaultRootWindow.argtypes, self.x.XDefaultRootWindow.restype = [ctypes.c_void_p], ctypes.c_ulong
        self.root = self.x.XDefaultRootWindow(self.display)
        self.x.XDefaultScreen.argtypes, self.x.XDefaultScreen.restype = [ctypes.c_void_p], ctypes.c_int
        assert self.x.XDefaultScreen(self.display) == 0
        self.x.XGetWindowAttributes.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(WindowAttributes)]
        self.x.XTranslateCoordinates.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_int,
            ctypes.c_int, ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong)]
        self.x.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]
        self.x.XFree.argtypes = [ctypes.c_void_p]
        self.x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.x.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.x.XSetErrorHandler.argtypes, self.x.XSetErrorHandler.restype = [ctypes.c_void_p], ctypes.c_void_p
        self.errors = []
        def error(display, event):
            e = event.contents
            self.errors.append(dict(resource=e.resourceid, serial=e.serial, code=e.error_code,
                request=e.request_code, minor=e.minor_code))
            return 0
        self.handler = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(XError))(error)
        self.previous = self.x.XSetErrorHandler(ctypes.cast(self.handler, ctypes.c_void_p))

    def checked(self, status):
        self.x.XSync(self.display, 0)
        if self.errors:
            put(OUT / 'x11-query-errors.json', self.errors)
        assert status and not self.errors, 'Ambiguous/stale X11 query; no native capture acceptance'

    def attributes(self, window):
        a = WindowAttributes()
        self.checked(self.x.XGetWindowAttributes(self.display, window, ctypes.byref(a)))
        assert a.root == self.root
        return a

    def client(self, window):
        window = int(window)
        a = self.attributes(window)
        x, y, child = ctypes.c_int(), ctypes.c_int(), ctypes.c_ulong()
        self.checked(self.x.XTranslateCoordinates(self.display, window, self.root, 0, 0,
            ctypes.byref(x), ctypes.byref(y), ctypes.byref(child)))
        assert a.map_state == 2 and a.window_class == 1, 'Client is not viewable InputOutput'
        return dict(X=x.value, Y=y.value, WIDTH=a.width, HEIGHT=a.height, SCREEN=0)

    def children(self, window):
        root, parent, count = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_uint()
        children = ctypes.POINTER(ctypes.c_ulong)()
        try:
            self.checked(self.x.XQueryTree(self.display, window, ctypes.byref(root), ctypes.byref(parent),
                ctypes.byref(children), ctypes.byref(count)))
            assert root.value == self.root and count.value < 10000
            return parent.value, [children[i] for i in range(count.value)]
        finally:
            if children:
                self.x.XFree(children)

    def state(self, window):
        client, root = self.client(window), self.client(self.root)
        path, occluders, above = [int(window)], [], []
        current = int(window)
        while current != self.root:
            assert len(path) <= 16, 'Ambiguous X11 ancestor chain'
            parent, _ = self.children(current)
            _, siblings = self.children(parent)
            assert current in siblings and parent not in path
            # XQueryTree returns siblings bottom-to-top; inspect every level,
            # including WM frame/override-redirect windows absent from EWMH lists.
            for sibling in siblings[siblings.index(current)+1:]:
                a = self.attributes(sibling)
                item = dict(window=sibling, parent=parent, map_state=a.map_state, window_class=a.window_class)
                if a.map_state == 2 and a.window_class == 1:
                    rect = self.client(sibling)
                    # Include native borders as potential visible occluders.
                    item['outer'] = dict(X=rect['X']-a.border_width, Y=rect['Y']-a.border_width,
                        WIDTH=rect['WIDTH']+2*a.border_width, HEIGHT=rect['HEIGHT']+2*a.border_width)
                    if rectangles_overlap(client, item['outer']):
                        occluders.append(item)
                above.append(item)
            current = parent
            path.append(current)
        return dict(client=client, root=root, ancestor_path=path, siblings_above=above, occluders=occluders)

    def close(self):
        try:
            self.checked(True)
        finally:
            self.x.XCloseDisplay(self.display)
            self.x.XSetErrorHandler(self.previous)


def rectangles_overlap(a, b):
    return (a['X'] < b['X']+b['WIDTH'] and b['X'] < a['X']+a['WIDTH'] and
            a['Y'] < b['Y']+b['HEIGHT'] and b['Y'] < a['Y']+a['HEIGHT'])


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


def on_screen(rect, size, origin=None):
    x, y, width, height = rect
    if origin is None:
        origin = (CLIENT_RECT['X'], CLIENT_RECT['Y']) if CLIENT_RECT is not None else (0, 0)
    return (width > 0 and height > 0 and x >= origin[0] and y >= origin[1]
            and x+width <= origin[0]+size[0] and y+height <= origin[1]+size[1])


def click_node(inputs, node):
    assert not CRITICAL
    x, y, width, height = box(node)
    assert width > 0 and height > 0
    inputs.move(x + width // 2, y + height // 2)
    inputs.click(1)


def private_env(root):
    env = os.environ.copy()
    for key, leaf in [('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache'),
                      ('XDG_STATE_HOME', 'state'), ('XDG_RUNTIME_DIR', 'run'), ('TMPDIR', 'tmp')]:
        path = root / leaf
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.update(CARGO_HOME=editor_cargo_cache, RUSTUP_HOME=editor_rustup_cache, CARGO_BUILD_JOBS='2',
        ACCESSIBILITY_ENABLED='1', NO_AT_BRIDGE='0', WINIT_UNIX_BACKEND='x11',
        RUST_LOG='cagents=debug,cagentsd=debug,winit_x11=info,iced_wgpu=info,warn')
    env.update(REQUESTED_RENDERER)
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


class QualificationCancelled(BaseException):
    pass


def cancelled(signum, frame):
    global CANCEL_SIGNAL
    CANCEL_SIGNAL = signum
    put(OUT / 'cancellation.json', dict(signal=signum, monotonic=time.monotonic(), arm=ACTIVE, critical=CRITICAL))
    raise QualificationCancelled('Native control cancelled; stop collection and close owned identities')


class NamespaceCustodyUnavailable(RuntimeError):
    pass


def identity(pid, executable=None, env=None):
    """Read only the owned process's identity and selected nonsecret settings."""
    row = next((row for row in live(ACTIVE) if row['pid'] == pid), None)
    assert row is not None, 'Process is outside this arm custody'
    proc = Path('/proc') / str(pid)
    before = processes().get(pid)
    actual = (proc / 'exe').resolve(strict=True)
    namespace = (proc / 'ns/pid').stat()
    ours = Path('/proc/self/ns/pid').stat()
    if (namespace.st_dev, namespace.st_ino) != (ours.st_dev, ours.st_ino):
        raise NamespaceCustodyUnavailable('UNQUALIFIED_namespace_custody: different kernel PID namespace')
    if executable is not None:
        assert actual == executable.resolve(strict=True), 'Unexpected actual process executable'
    selected = {}
    if env is not None:
        entries = (proc / 'environ').read_bytes().split(b'\0')
        for name in ['XDG_RUNTIME_DIR', *REQUESTED_RENDERER]:
            values = [item.split(b'=', 1)[1].decode() for item in entries if item.startswith(name.encode()+b'=')]
            assert values == [env[name]], 'Owned native process selected environment differs: ' + name
            selected[name] = values[0]
        runtime = Path(selected['XDG_RUNTIME_DIR']).stat()
        expected = Path(env['XDG_RUNTIME_DIR']).stat()
        if (runtime.st_dev, runtime.st_ino) != (expected.st_dev, expected.st_ino):
            raise NamespaceCustodyUnavailable('UNQUALIFIED_namespace_custody: service runtime inode differs')
        selected['runtime_inode'] = [runtime.st_dev, runtime.st_ino]
    after = processes().get(pid)
    assert before and after and before['start'] == after['start'] == row['start']
    assert actual == (proc / 'exe').resolve(strict=True), 'Executable changed during identity receipt'
    return dict(row, executable=str(actual), executable_sha256=digest(proc / 'exe'),
                pid_namespace=[namespace.st_dev, namespace.st_ino], selected_environment=selected)


def owned_window(executable, previous_pid=None):
    matches = []
    for row in live(ACTIVE):
        try:
            if row['pid'] != previous_pid and (Path('/proc') / str(row['pid']) / 'exe').resolve() == executable.resolve():
                windows = cmd(['xdotool', 'search', '--onlyvisible', '--pid', str(row['pid'])], expected=None).split()
                if windows:
                    assert len(windows) == 1, 'Ambiguous owned native top-level windows'
                    assert int(query(['getwindowpid', windows[0]])) == row['pid']
                    matches.append((row['pid'], windows[0]))
        except (FileNotFoundError, ProcessLookupError):
            pass
    assert len(matches) <= 1, 'Multiple owned native candidate processes'
    return matches[0] if matches else None


def viewport(window, size):
    global CLIENT_RECT
    query(['windowactivate', '--sync', window])
    query(['windowsize', '--sync', window, *map(str, size)])
    query(['windowmove', '--sync', window, '0', '0'])
    def ready():
        value = geometry(window)
        return ((value['WIDTH'], value['HEIGHT']) == tuple(size) and value['SCREEN'] == 0 and
                on_screen((value['X'], value['Y'], *size), tuple(M['capture']['root_size']), origin=(0, 0)))
    wait(ready)
    CLIENT_RECT = geometry(window)
    time.sleep(.6)


def renderer(log, owner):
    # Only a log belonging to this exact owned process is admissible, never donor output.
    raw = log.read_text(errors='replace') if log.exists() else ''
    selected = [line for line in raw.splitlines() if 'Selected: AdapterInfo {' in line]
    observed = dict(status='UNMEASURED', adapter=None, backend=None)
    if len(selected) == 1:
        backend = re.search(r'\bbackend: (\w+)\s*}', selected[0])
        adapter = re.search(r'\bname: "([^"\n]+)"', selected[0])
        if backend and adapter:
            observed = dict(status='OBSERVED_owned_log', adapter=adapter[1], backend=backend[1], raw_line=selected[0])
    return dict(owner=owner, log=str(log), log_sha256=digest(log) if log.exists() else None,
                requested_environment=REQUESTED_RENDERER,
                inherited_environment={name: os.environ.get(name) for name in REQUESTED_RENDERER},
                observed_renderer=observed)


