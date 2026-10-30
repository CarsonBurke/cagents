"""UNRUN PR1984 caller bindings for the single shared private desktop capsule.

This module creates no endpoint. It binds the unchanged capsule to the original
probe's exact OWNED/CHILDREN objects and adds only caller budget/effect guards.
Import/call requires fresh exact source review and actual EC admission.
"""
import contextlib
import ctypes
import os
from pathlib import Path
import subprocess
import time


def bind(native, desktop, context):
    original_guard, original_env = native.guard, native.private_env
    original_inputs, original_screen = native.Inputs, native.NativeScreen
    state = dict(cleanup=False, cleanup_deadline=None, epoch=0,
                 cleanup_epoch=None, aborted=False, unconfirmed=False)
    native.CALLER_STATE = state

    def budget(deadline=None):
        limits = [context['final_deadline'] if state['cleanup'] else context['work_deadline']]
        if state['cleanup_deadline'] is not None:
            limits.append(state['cleanup_deadline'])
        if deadline is not None:
            limits.append(deadline)
        remaining = min(limits) - time.monotonic()
        if (state['aborted'] or
                (state['cleanup'] and state['epoch'] != state['cleanup_epoch']) or
                (native.CANCEL_SIGNAL is not None and not state['cleanup']) or remaining <= 0):
            state['unconfirmed'] = True
            if state['cleanup']:
                state['aborted'] = True
            raise native.QualificationCancelled('PR1984 absolute budget/cancellation stops later effects')
        return remaining

    def guard():
        budget()
        original_guard()
        assert not desktop.PIDFD_FAILURES, 'Original pidfd admission/release failure remains HOLD'
        desktop.verify_private_desktop()
        budget()

    def begin_cleanup():
        if state['aborted']:
            budget()
        if not state['cleanup']:
            state.update(cleanup=True, cleanup_epoch=state['epoch'],
                cleanup_deadline=min(context['final_deadline'],
                    time.monotonic() + context['lease']['cleanup_seconds']))
        return budget()

    @contextlib.contextmanager
    def cleanup_scope():
        if native.CANCEL_SIGNAL is not None and not state['cleanup']:
            begin_cleanup()
        budget()
        try:
            yield
        except BaseException:
            state['unconfirmed'] = True
            if state['cleanup']:
                state['aborted'] = True
            raise
        else:
            budget()

    def cancelled(signum, frame):
        native.CANCEL_SIGNAL = signum
        state['epoch'] += 1
        state['unconfirmed'] = True
        if state['cleanup']:
            state['aborted'] = True
        native.put(native.OUT / 'cancellation.json', dict(signal=signum,
            epoch=state['epoch'], cleanup_epoch=state['cleanup_epoch'],
            cleanup=state['cleanup'], monotonic=time.monotonic()))
        raise native.QualificationCancelled('PR1984 cancelled; retain original custody and stop effects')

    def digest(path):
        import hashlib
        budget()
        result = hashlib.sha256()
        with Path(path).open('rb') as stream:
            while True:
                budget()
                block = stream.read(1024 * 1024)
                budget()
                if not block:
                    break
                result.update(block)
        return result.hexdigest()

    def processes():
        budget()
        table = {}
        for path in Path('/proc').glob('[0-9]*/stat'):
            budget()
            try:
                raw = path.read_text()
                budget()
                fields = raw.rsplit(')', 1)[1].split()
                table[int(path.parent.name)] = dict(parent=int(fields[1]), group=int(fields[2]),
                    state=fields[0], start=fields[19])
            except (FileNotFoundError, ProcessLookupError):
                pass
        budget()
        return table

    def private_env(root):
        guard()
        value = original_env(root)
        guard()
        return value

    def ax_read(function):
        desktop.require_private_desktop()
        value = function()
        desktop.require_private_desktop()
        return value

    class GuardedState:
        """Only the existing read-only state query; no protocol fallback."""
        def __init__(self, value):
            self.value = value

        def contains(self, flag):
            return ax_read(lambda: self.value.contains(flag))

    class GuardedComponent:
        def __init__(self, value):
            self.value = value

        def getExtents(self, coordinates):
            return ax_read(lambda: self.value.getExtents(coordinates))

    class GuardedNode:
        """Exact AX reads used by this probe, individually guarded."""
        def __init__(self, value):
            self.value = value

        @property
        def name(self):
            return ax_read(lambda: self.value.name)

        @property
        def childCount(self):
            return ax_read(lambda: self.value.childCount)

        def getChildAtIndex(self, index):
            return GuardedNode(ax_read(lambda: self.value.getChildAtIndex(index)))

        def get_process_id(self):
            return ax_read(self.value.get_process_id)

        def getRole(self):
            return ax_read(self.value.getRole)

        def getRoleName(self):
            return ax_read(self.value.getRoleName)

        def getState(self):
            return GuardedState(ax_read(self.value.getState))

        def queryComponent(self):
            return GuardedComponent(ax_read(self.value.queryComponent))

    def a11y_read(function):
        def query(*args, **kwargs):
            desktop.require_private_desktop()
            if native.A11Y_READ is not None:
                return function(*args, **kwargs)
            deadline = time.monotonic() + 5
            record = dict(query=function.__name__, arm=native.ACTIVE, started=time.monotonic(), attempts=[])
            native.A11Y_QUERIES.append(record)
            native.A11Y_READ = record
            try:
                for attempt in range(3):
                    budget(deadline)
                    desktop.require_private_desktop()
                    try:
                        value = function(*args, **kwargs)
                        budget(deadline)
                        record['attempts'].append(dict(attempt=attempt + 1, status='complete'))
                        return value
                    except Exception as error:
                        name = getattr(error, 'get_dbus_name', None)
                        record['attempts'].append(dict(attempt=attempt + 1, type=type(error).__name__,
                            name=name() if callable(name) else None, error=str(error)))
                        budget(deadline)
                        if not native.stale_a11y(error) or attempt == 2:
                            raise
                        sleep(.05, deadline)
            finally:
                record['returned'] = time.monotonic()
                native.put(native.OUT / 'accessibility-queries.json', native.A11Y_QUERIES)
                native.A11Y_READ = None
        return query

    def tree(pid):
        @a11y_read
        def read_tree():
            desktop.require_private_desktop()
            root = GuardedNode(ax_read(lambda: native.pyatspi.Registry.getDesktop(0)))
            apps = []
            for index in range(root.childCount):
                desktop.require_private_desktop()
                app = root.getChildAtIndex(index)
                if app.get_process_id() == pid:
                    apps.append(app)
            if len(apps) != 1:
                return []
            pending, result, deadline = [apps[0]], [], time.monotonic() + 5
            while pending:
                budget(deadline)
                desktop.require_private_desktop()
                assert len(result) < 10000
                node = pending.pop()
                result.append(node)
                assert node.childCount < 1000
                for index in range(node.childCount):
                    desktop.require_private_desktop()
                    pending.append(node.getChildAtIndex(index))
            return result
        return read_tree()

    @a11y_read
    def snapshot(pid, name):
        nodes = tree(pid)
        assert nodes, 'PID-filtered native accessibility tree disappeared'
        native.put(native.OUT / (name + '-a11y.json'), dict(pid=pid, monotonic=time.monotonic(),
            nodes=[native.describe(node) for node in nodes]))

    def viewport(window, size):
        desktop.require_private_desktop()
        native.query(['windowactivate', '--sync', window])
        native.query(['windowsize', '--sync', window, *map(str, size)])
        native.query(['windowmove', '--sync', window, '0', '0'])
        def ready():
            value = native.geometry(window)
            return ((value['WIDTH'], value['HEIGHT']) == tuple(size) and value['SCREEN'] == 0 and
                native.on_screen((value['X'], value['Y'], *size), tuple(native.M['capture']['root_size']), origin=(0, 0)))
        wait(ready)
        native.CLIENT_RECT = native.geometry(window)
        sleep(.6)

    def spawn(args, bootstrap=False, desktop_capability=None, effect_deadline=None, **kwargs):
        guard()
        assert desktop_capability is None, 'PR1984 Cancel probe admits only its owned X11 endpoints'
        args = [str(arg) for arg in args]
        assert kwargs.get('env') is not None, 'Implicit child environment refused'
        desktop.child_desktop_environment(args, kwargs['env'], bootstrap=bootstrap,
            capability=desktop_capability, cwd=kwargs.get('cwd'))
        budget(effect_deadline)
        child = subprocess.Popen(args, start_new_session=True, **kwargs)
        native.CHILDREN.append(child)
        current = processes().get(child.pid)
        assert current is not None, 'Original Popen retained; cannot stamp actual child identity'
        child.native_identity = dict(current, pid=child.pid, arm=native.ACTIVE)
        desktop.admit_owned(child.pid, current, native.ACTIVE)
        native.scan()
        budget(effect_deadline)
        return child

    def poll(child, deadline=None):
        budget(deadline)
        value = child.poll()
        budget(deadline)
        return value

    def reap(child, deadline=None, receipt=None):
        remaining = budget(deadline)
        value = child.wait(timeout=remaining)
        if receipt is not None:
            receipt.update(exit=value, original_wait_exit=value, original_wait_settled=True)
        budget(deadline)
        desktop.settle_direct(child)
        budget(deadline)
        return value

    def sleep(seconds, deadline=None):
        remaining = budget(deadline)
        assert seconds > 0
        time.sleep(min(seconds, remaining))
        budget(deadline)

    def cmd(args, env=None, cwd=None, timeout=20, expected=0, bootstrap=False, desktop_capability=None):
        guard()
        args = [str(arg) for arg in args]
        deadline = time.monotonic() + timeout
        directory = native.OUT / 'commands' / f'{len(native.COMMANDS):05d}'
        directory.mkdir(parents=True)
        record = dict(argv=args, cwd=str(cwd) if cwd else None, arm=native.ACTIVE,
            started=time.monotonic(), deadline_seconds=timeout, absolute_deadline=deadline)
        native.COMMANDS.append(record)
        native.put(directory / 'command.json', record)
        with (directory / 'stdout.log').open('wb') as stdout, (directory / 'stderr.log').open('wb') as stderr:
            budget(deadline)
            child = spawn(args, env=dict(os.environ) if env is None else env, cwd=cwd,
                stdout=stdout, stderr=stderr, bootstrap=bootstrap,
                desktop_capability=desktop_capability, effect_deadline=deadline)
            record['identity'] = child.native_identity
            try:
                while poll(child, deadline) is None:
                    native.scan()
                    budget(deadline)
                    sleep(.02, deadline)
                record['exit'] = reap(child, deadline, record)
            except BaseException as error:
                record.update(exit=record.get('exit'), error=str(error))
                native.put(directory / 'command.json', record)
                raise
        record.update(returned=time.monotonic(), stdout_sha256=digest(directory / 'stdout.log'),
            stderr_sha256=digest(directory / 'stderr.log'))
        native.put(directory / 'command.json', record)
        (directory / 'exit.txt').write_text(str(record['exit']) + '\n')
        assert expected is None or record['exit'] == expected, f'Native command exit {record["exit"]}: {args}'
        budget(deadline)
        raw = (directory / 'stdout.log').read_text(errors='replace')
        budget(deadline)
        return raw

    def wait(predicate, timeout=30, interval=.1):
        deadline = time.monotonic() + timeout
        while True:
            budget(deadline)
            native.scan()
            budget(deadline)
            result = predicate()
            budget(deadline)
            if result:
                return result
            sleep(interval, deadline)

    class GuardedScreen(original_screen):
        """Original geometry/capture algorithms, explicit proved connection."""
        def __init__(self):
            desktop.require_private_desktop()
            self.x = ctypes.CDLL('libX11.so.6')
            self.x.XOpenDisplay.argtypes, self.x.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
            desktop.require_private_desktop()
            self.display = self.x.XOpenDisplay(os.environ['DISPLAY'].encode('ascii'))
            assert self.display
            self.x.XDefaultRootWindow.argtypes, self.x.XDefaultRootWindow.restype = [ctypes.c_void_p], ctypes.c_ulong
            desktop.require_private_desktop()
            self.root = self.x.XDefaultRootWindow(self.display)
            self.x.XDefaultScreen.argtypes, self.x.XDefaultScreen.restype = [ctypes.c_void_p], ctypes.c_int
            desktop.require_private_desktop()
            assert self.x.XDefaultScreen(self.display) == 0
            self.x.XGetWindowAttributes.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(native.WindowAttributes)]
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
                value = event.contents
                self.errors.append(dict(resource=value.resourceid, serial=value.serial, code=value.error_code,
                    request=value.request_code, minor=value.minor_code))
                return 0
            self.handler = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(native.XError))(error)
            desktop.require_private_desktop()
            self.previous = self.x.XSetErrorHandler(ctypes.cast(self.handler, ctypes.c_void_p))

        def checked(self, status):
            desktop.require_private_desktop()
            self.x.XSync(self.display, 0)
            desktop.require_private_desktop()
            if self.errors:
                native.put(native.OUT / 'x11-query-errors.json', self.errors)
            assert status and not self.errors, 'Ambiguous/stale X11 query; no native capture acceptance'

        def attributes(self, window):
            attributes = native.WindowAttributes()
            desktop.require_private_desktop()
            self.checked(self.x.XGetWindowAttributes(self.display, window, ctypes.byref(attributes)))
            assert attributes.root == self.root
            return attributes

        def client(self, window):
            window = int(window)
            attributes = self.attributes(window)
            x, y, child = ctypes.c_int(), ctypes.c_int(), ctypes.c_ulong()
            desktop.require_private_desktop()
            self.checked(self.x.XTranslateCoordinates(self.display, window, self.root, 0, 0,
                ctypes.byref(x), ctypes.byref(y), ctypes.byref(child)))
            assert attributes.map_state == 2 and attributes.window_class == 1, 'Client is not viewable InputOutput'
            return dict(X=x.value, Y=y.value, WIDTH=attributes.width, HEIGHT=attributes.height, SCREEN=0)

        def children(self, window):
            desktop.require_private_desktop()
            root, parent, count = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_uint()
            children = ctypes.POINTER(ctypes.c_ulong)()
            desktop.require_private_desktop()
            self.checked(self.x.XQueryTree(self.display, window, ctypes.byref(root), ctypes.byref(parent),
                ctypes.byref(children), ctypes.byref(count)))
            assert root.value == self.root and count.value < 10000
            result = parent.value, [children[i] for i in range(count.value)]
            if children:
                desktop.require_private_desktop()
                self.x.XFree(children)
            return result

        def state(self, window):
            desktop.require_private_desktop()
            return super().state(window)

        def close(self):
            self.checked(True)
            desktop.require_private_desktop()
            self.x.XCloseDisplay(self.display)
            desktop.require_private_desktop()
            self.x.XSetErrorHandler(self.previous)

    class GuardedInputs(original_inputs):
        """Original event semantics, owned endpoint and effect entry guards."""
        def __init__(self):
            desktop.require_private_desktop()
            self.x, self.t = ctypes.CDLL('libX11.so.6'), ctypes.CDLL('libXtst.so.6')
            self.x.XOpenDisplay.argtypes, self.x.XOpenDisplay.restype = [ctypes.c_char_p], ctypes.c_void_p
            desktop.require_private_desktop()
            self.display = self.x.XOpenDisplay(os.environ['DISPLAY'].encode('ascii'))
            assert self.display
            self.x.XStringToKeysym.argtypes, self.x.XStringToKeysym.restype = [ctypes.c_char_p], ctypes.c_ulong
            self.x.XKeysymToKeycode.argtypes, self.x.XKeysymToKeycode.restype = [ctypes.c_void_p, ctypes.c_ulong], ctypes.c_ubyte
            self.x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
            self.x.XCloseDisplay.argtypes = [ctypes.c_void_p]
            self.t.XTestQueryExtension.argtypes = [ctypes.c_void_p, *([ctypes.POINTER(ctypes.c_int)] * 4)]
            self.t.XTestFakeKeyEvent.argtypes = self.t.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
            values = [ctypes.c_int() for _ in range(4)]
            desktop.require_private_desktop()
            assert self.t.XTestQueryExtension(self.display, *[ctypes.byref(value) for value in values])
            native.put(native.OUT / 'xtest.json', dict(event_base=values[0].value, error_base=values[1].value,
                major=values[2].value, minor=values[3].value))
            self.keys, self.buttons = set(), set()

        def event(self, kind, value, down, purpose='control'):
            if native.CRITICAL:
                assert (purpose == 'wheel' and kind == 'button' and value in (4, 5)) or (purpose == 'activation' and
                    ((kind == 'key' and value in ('Return', 'space')) or (kind == 'button' and value == 1)))
            if kind == 'key':
                desktop.require_private_desktop()
                symbol = self.x.XStringToKeysym(value.encode('ascii'))
                desktop.require_private_desktop()
                code = self.x.XKeysymToKeycode(self.display, symbol)
                assert symbol and code
                pressed = self.keys
                if down:
                    pressed.add(value)
                desktop.require_private_desktop()
                result = self.t.XTestFakeKeyEvent(self.display, code, int(down), 0)
            else:
                code, pressed = value, self.buttons
                if down:
                    pressed.add(value)
                desktop.require_private_desktop()
                result = self.t.XTestFakeButtonEvent(self.display, code, int(down), 0)
            desktop.require_private_desktop()
            self.x.XSync(self.display, 0)
            if result and not down:
                pressed.discard(value)
            desktop.require_private_desktop()
            native.EVENTS.append(dict(arm=native.ACTIVE, monotonic=time.monotonic(), kind=kind, value=value,
                actual_code=code, pressed=down, injection_return=result, purpose=purpose, critical=native.CRITICAL,
                pointer=native.pointer(), focus=native.query(['getwindowfocus'])))
            native.put(native.OUT / 'inputs.json', native.EVENTS)
            assert result, 'XTest injection refused'

        def key(self, value, purpose='control'):
            self.event('key', value, True, purpose)
            sleep(.04)
            if value in self.keys:
                self.event('key', value, False, purpose)

        def click(self, button, purpose='control'):
            self.event('button', button, True, purpose)
            sleep(.04)
            if button in self.buttons:
                self.event('button', button, False, purpose)

        def shortcut(self, keys):
            assert not native.CRITICAL
            for key in keys:
                self.event('key', key, True)
            for key in reversed(keys):
                if key in self.keys:
                    self.event('key', key, False)

        def wheel(self, button, count):
            assert button in (4, 5) and 0 < count <= 80
            for _ in range(count):
                self.click(button, 'wheel')
            sleep(.35)

        def move(self, x, y):
            desktop.require_private_desktop()
            return super().move(x, y)

        def request_close(self, window):
            desktop.require_private_desktop()
            assert not native.CRITICAL
            protocols = cmd(['xprop', '-id', window, 'WM_PROTOCOLS'])
            assert 'WM_DELETE_WINDOW' in protocols, 'App has no supported native close protocol'
            self.x.XInternAtom.argtypes, self.x.XInternAtom.restype = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int], ctypes.c_ulong
            self.x.XSendEvent.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_long, ctypes.POINTER(native.XEvent)]
            event = native.XEvent()
            event.client.type, event.client.send_event = 33, 1
            event.client.display, event.client.window = self.display, int(window)
            desktop.require_private_desktop()
            event.client.message_type = self.x.XInternAtom(self.display, b'WM_PROTOCOLS', 0)
            event.client.format = 32
            desktop.require_private_desktop()
            event.client.data.longs[0] = self.x.XInternAtom(self.display, b'WM_DELETE_WINDOW', 0)
            event.client.data.longs[1] = 0
            desktop.require_private_desktop()
            result = self.x.XSendEvent(self.display, int(window), 0, 0, ctypes.byref(event))
            desktop.require_private_desktop()
            self.x.XSync(self.display, 0)
            native.EVENTS.append(dict(arm=native.ACTIVE, monotonic=time.monotonic(), kind='WM_DELETE_WINDOW',
                window=window, injection_return=result))
            assert result, 'Native close request refused'

        def close(self):
            native.CRITICAL = False
            for kind, values in [('key', self.keys), ('button', self.buttons)]:
                for value in list(values):
                    self.event(kind, value, False, 'cleanup')
            assert not self.keys and not self.buttons
            desktop.require_private_desktop()
            self.x.XCloseDisplay(self.display)

    # Explicit callback/state closure. No copied process table or implicit
    # supervisor exists in the shared capsule.
    for name, value in dict(guard=guard, check_native_budget=budget,
            native_cleanup_scope=cleanup_scope, spawn=spawn, cmd=cmd, wait=wait,
            processes=processes, digest=digest, private_env=private_env,
            put=native.put, OUT=native.OUT, ACTIVE=native.ACTIVE,
            OWNED=native.OWNED, CHILDREN=native.CHILDREN, EVENTS=native.EVENTS,
            native_budget_seconds=budget,
            original_home=native.original_home, original_codex_home=native.original_codex_home).items():
        setattr(desktop, name, value)
    assert desktop.OWNED is native.OWNED and desktop.CHILDREN is native.CHILDREN
    assert desktop.EVENTS is native.EVENTS
    native.PIDFDS, native.PIDFD_FAILURES = desktop.PIDFDS, desktop.PIDFD_FAILURES
    native.scan, native.live, native.close_owned = desktop.scan, desktop.live, desktop.close_owned
    native.guard, native.digest, native.processes = guard, digest, processes
    native.private_env, native.spawn, native.cmd = private_env, spawn, cmd
    native.tree, native.a11y_read, native.viewport = tree, a11y_read, viewport
    native.snapshot = snapshot
    native.wait, native.cancelled = wait, cancelled
    native.check_native_budget, native.begin_probe_cleanup = budget, begin_cleanup
    native.poll_owned, native.reap_owned, native.bounded_sleep = poll, reap, sleep
    native.bootstrap = desktop.bootstrap
    native.require_private_desktop = desktop.require_private_desktop
    native.PRIVATE_DESKTOP = desktop
    native.NativeScreen, native.Inputs = GuardedScreen, GuardedInputs
    return native
