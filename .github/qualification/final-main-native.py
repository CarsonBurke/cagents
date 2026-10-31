"""One real chooser/peer-recipe flow on each exact revision, never full100.

Only observations retry stale accessibility references. Native input is issued
once. Real GTK Cancel and the portal Response drive the production callback.
"""
import ast
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import traceback

import pyatspi
from gi.repository import Gio, GLib
from PIL import Image

A = '00000000-0000-4000-8000-000000002018'
B = '00000000-0000-4000-8000-000000002019'
TEXT = 'Keep this unsent café image draft through the actual chooser.'
SUFFIX = ' Keyboard focus stayed here.'
BAD_TEXT = 'Rejected sibling keeps its saved café draft.\nNever submit or start it.'
RENDERER = {'ICED_BACKEND': 'wgpu', 'WGPU_BACKEND': 'gl', 'LIBGL_ALWAYS_SOFTWARE': '1'}
children = []
stopped = set()
out = None
arm = None


def user():
    assert os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
    assert 'CAGENTS_AGENT_AUTHORITY' not in os.environ, 'Inherited authority is never removed'
    assert not os.environ.get('WAYLAND_DISPLAY') and not os.environ.get('WAYLAND_SOCKET')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def emit(event, **values):
    with (out / 'events.jsonl').open('a') as stream:
        stream.write(json.dumps(dict(event=event, arm=arm, unix_ns=time.time_ns(), **values)) + '\n')


def processes():
    values = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            values[int(path.parent.name)] = dict(state=fields[0], parent=int(fields[1]), group=int(fields[2]), stamp=fields[19])
        except (OSError, ValueError, IndexError):
            pass
    return values


def family(table, roots):
    result = set(roots)
    while True:
        expanded = result | {pid for pid, value in table.items() if value['parent'] in result}
        if expanded == result:
            return result
        result = expanded


def register(process):
    value = processes().get(process.pid)
    process.owned_identity = dict(pid=process.pid, stamp=value['stamp'], group=value['group']) if value else None
    if value:
        assert value['group'] == process.pid, 'Owned Popen must have its own new session'
    children.append(process)
    return process


def stop(process):
    identity = process.owned_identity
    key = (process.pid, identity['stamp'] if identity else None)
    if key in stopped:
        return
    table = processes()
    current = table.get(process.pid)
    if identity is None:
        assert process.poll() is not None, 'No process-start receipt; refusing a borrowed PID'
        stopped.add(key)
        return
    # Never discover a new family through a reused/reaped root PID.
    original = current is not None and current['stamp'] == identity['stamp']
    if current is not None and not original:
        assert process.poll() is not None, 'Original process is unresolved after PID reuse'
        stopped.add(key)
        return
    owned = {pid: table[pid]['stamp'] for pid in family(table, {process.pid}) if pid in table} if original else {}
    def live_owned():
        current_table = processes()
        return {pid: stamp for pid, stamp in owned.items() if pid in current_table
                and current_table[pid]['stamp'] == stamp and current_table[pid]['state'] not in ('Z','X')}
    for signum in (signal.SIGTERM, signal.SIGKILL):
        if not live_owned():
            process.poll()
            stopped.add(key)
            return
        current = processes().get(process.pid)
        if current and current['stamp'] == identity['stamp'] and current['group'] == identity['group']:
            try:
                os.killpg(identity['group'], signum)
            except ProcessLookupError:
                pass
        for pid, stamp in live_owned().items():
            value = processes().get(pid)
            if value and value['stamp'] == stamp and value['state'] not in ('Z', 'X'):
                try:
                    os.kill(pid, signum)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and live_owned():
            process.poll()
            time.sleep(.05)
    if live_owned():
        raise RuntimeError('Owned descendants survived cleanup: ' + str(live_owned()))
    process.poll()
    stopped.add(key)


def drain_adopted():
    # The subreaper owns only descendants of this driver. Snapshot their real
    # start stamps; never borrow unrelated same-user processes or PID groups.
    table = processes()
    owned = {pid: table[pid]['stamp'] for pid in family(table, {os.getpid()}) if pid != os.getpid()}
    for signum in (signal.SIGTERM, signal.SIGKILL):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            table = processes()
            for pid in family(table, {os.getpid()}) - {os.getpid()}:
                owned.setdefault(pid, table[pid]['stamp'])
            for pid, stamp in owned.items():
                value = processes().get(pid)
                if value and value['stamp'] == stamp and value['state'] not in ('Z','X'):
                    try:
                        os.kill(pid, signum)
                    except ProcessLookupError:
                        pass
            for pid in owned:
                try:
                    os.waitpid(pid, os.WNOHANG)
                except ChildProcessError:
                    pass
            table = processes()
            # A shutdown handler can fork before its original process exits.
            # A fresh whole-descendant scan, not just the initial snapshot,
            # must be empty before a cleanup marker can be issued.
            live = [pid for pid in family(table, {os.getpid()}) - {os.getpid()} if table[pid]['state'] not in ('Z','X')]
            if not live:
                return []
            time.sleep(.05)
    return live


def cleanup_arm(resources):
    errors = []
    for name in ('ui','clipboard','fixture'):
        if resources.get(name) is not None:
            try:
                stop(resources[name])
            except BaseException as error:
                errors.append(name + ': ' + repr(error))
    if resources.get('monitor') is not None:
        try:
            resources['monitor'].close()
        except BaseException as error:
            errors.append('monitor: ' + repr(error))
    for name in ('frontend','backend'):
        if resources.get(name) is not None:
            try:
                stop(resources[name])
            except BaseException as error:
                errors.append(name + ': ' + repr(error))
    documents = resources.get('documents')
    graceful = None
    if documents is not None:
        try:
            identity = documents.owned_identity
            actual = processes().get(documents.pid)
            assert identity and actual and actual['stamp'] == identity['stamp'], 'Documents owner disappeared before normal shutdown'
            os.kill(documents.pid, signal.SIGTERM)
            code = documents.wait(timeout=15)
            assert code == 0, ('Documents did not complete normal signal/unmount cleanup', code)
            assert not resources['mount'].is_mount(), 'Documents mount survived graceful process exit'
            graceful = dict(exit_code=code, mount=str(resources['mount']), mounted_after=False, identity=identity)
            stopped.add((documents.pid, identity['stamp']))
        except BaseException as error:
            errors.append('documents graceful-unmount: ' + repr(error))
            try:
                stop(documents)
            except BaseException as forced:
                errors.append('documents forced cleanup: ' + repr(forced))
    if resources.get('permissions') is not None:
        try:
            stop(resources['permissions'])
        except BaseException as error:
            errors.append('permission-store: ' + repr(error))
    if resources.get('mount') is not None and resources['mount'].is_mount():
        errors.append('Documents FUSE mount remains active')
    names = {}
    for name in ('org.freedesktop.portal.Desktop','org.freedesktop.impl.portal.desktop.gtk',
                 'org.freedesktop.portal.Documents','org.freedesktop.impl.portal.PermissionStore'):
        try:
            names[name] = bus_owner(name)
            errors.append('Private portal owner survived cleanup: ' + name)
        except GLib.Error as error:
            if Gio.DBusError.get_remote_error(error) != 'org.freedesktop.DBus.Error.NameHasNoOwner':
                errors.append('Private owner cleanup query: ' + repr(error))
            names[name] = None
    table = processes()
    protected = set()
    for identity in resources.get('desktop_owners', []):
        value = table.get(identity['pid'])
        if value and value['stamp'] == identity['stamp']:
            protected |= family(table, {identity['pid']})
    remaining = sorted(pid for pid in family(table, {os.getpid()}) - {os.getpid()} - protected
                       if table[pid]['state'] not in ('Z','X'))
    if remaining:
        errors.append('Owned arm descendants remain before next arm: ' + repr(remaining))
    return dict(errors=errors, documents_graceful=graceful, private_bus_names=names, owned_descendants_remaining=remaining)


def spawn(argv, *, env=None, cwd=None, log=None, stdin=None):
    user()
    process = register(subprocess.Popen(argv, env=env, cwd=cwd, stdin=stdin, stdout=log, stderr=subprocess.STDOUT,
                                       start_new_session=True))
    emit('process-started', argv=list(map(str, argv)), pid=process.pid, cwd=str(cwd or Path.cwd()))
    return process


def command(argv, *, env=None, cwd=None, timeout=20, stdout=None):
    user()
    started = time.time_ns()
    process = register(subprocess.Popen(argv, env=env, cwd=cwd, stdout=stdout or subprocess.PIPE,
                                       stderr=subprocess.PIPE, start_new_session=True))
    try:
        output, errors = process.communicate(timeout=timeout)
    except BaseException:
        stop(process)
        raise
    emit('command', argv=list(map(str, argv)), cwd=str(cwd or Path.cwd()), exit_code=process.returncode,
         started_unix_ns=started, stderr=(errors or b'').decode(errors='replace'))
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, argv, output=output, stderr=errors)
    return output or b''


class IncompleteTree(RuntimeError):
    pass


def wait(query, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = query()
            if result:
                return result
        except GLib.Error as error:
            if 'Unknown object' not in str(error) and not (error.domain == 'atspi_error' and error.code == 1
                    and re.fullmatch(r'/org/a11y/atspi/accessible/[0-9]+/[0-9]+', error.message)):
                raise
            last = error
        except IncompleteTree as error:
            last = error
        time.sleep(.15)
    raise RuntimeError('Observation did not settle; no input was repeated') from last


def observation(query):
    return wait(lambda: (query(),))[0]  # False is an immediate caller assertion failure.


def tree(pid):
    os.kill(pid, 0)
    desktop = pyatspi.Registry.getDesktop(0)
    if desktop is None:
        raise IncompleteTree('No desktop')
    apps = [desktop.getChildAtIndex(index) for index in range(desktop.childCount)]
    if any(node is None for node in apps):
        raise IncompleteTree('Desktop changed')
    apps = [node for node in apps if node.get_process_id() == pid]
    assert len(apps) <= 1
    result, pending = [], apps
    while pending:
        node = pending.pop()
        if node is None:
            raise IncompleteTree('Child changed')
        result.append(node)
        assert len(result) < 10000 and node.childCount < 1000
        pending.extend(node.getChildAtIndex(index) for index in range(node.childCount))
    return result


def shown(node):
    return node.getState().contains(pyatspi.STATE_SHOWING)


def bounds(node):
    value = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
    return [value.x, value.y, value.width, value.height]


def target(pid, name, role=pyatspi.ROLE_PUSH_BUTTON):
    values = [node for node in tree(pid) if node.name == name and node.getRole() == role and shown(node)]
    assert len(values) <= 1, ('Ambiguous actual native owner', name)
    return values[0] if values else None


def click(pid, name):
    # Retain only fresh scalar geometry across the native gesture.
    rectangle = wait(lambda: bounds(node) if (node := target(pid, name)) is not None
                     and node.getState().contains(pyatspi.STATE_ENABLED) else None)
    x, y, width, height = rectangle
    assert width > 0 and height > 0
    emit('native-click', target=name, pid=pid, bounds=rectangle)
    command(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])


def clipboard_image(root):
    stream = (root / 'offered.png').open('rb')
    process = spawn(['xclip', '-selection', 'clipboard', '-target', 'image/png', '-in', '-quiet'], stdin=stream,
                    log=(out / (arm + '-clipboard.log')).open('wb'))
    stream.close()
    emit('clipboard-offer', bytes=(root / 'offered.png').stat().st_size, sha256=sha(root / 'offered.png'), pid=process.pid)
    command(['xdotool', 'key', 'ctrl+v'])
    return process


def bus_owner(name):
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    def call(method, value, signature):
        return bus.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', method,
            GLib.Variant('(s)', (value,)), GLib.VariantType.new(signature), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]
    owner = call('GetNameOwner', name, '(s)')
    return dict(name=name, owner=owner, pid=call('GetConnectionUnixProcessID', owner, '(u)'))


def database(root):
    path = Path((root / 'database-path').read_text())
    assert path.resolve(strict=True) == path and path.is_relative_to(root / 'data')
    return path


def storage(root):
    path = database(root)
    workspace = (root / 'workspace-id').read_text()
    prefix = 'workspace:' + workspace + ':navigation:'
    try:
        with sqlite3.connect('file:' + str(path) + '?mode=ro', uri=True, timeout=.2) as db:
            db.execute('BEGIN')
            metadata = {key.removeprefix(prefix): json.loads(value) for key, value in db.execute(
                'SELECT key,value FROM metadata WHERE key LIKE ?', (prefix + '%',))}
            cards = {card: dict(source=json.loads(content), existence=json.loads(existence)) for card, content, existence in db.execute(
                'SELECT card_id,content_json,existence_json FROM cards ORDER BY ordinal')}
            return dict(cards=cards, metadata=metadata)
    except (sqlite3.Error, FileNotFoundError):
        return None


def source_witness(root):
    rows = [json.loads(line) for line in (root / 'fixture.jsonl').read_text().splitlines()]
    assert not any(row['event'].startswith('UNEXPECTED') for row in rows), rows[-5:]
    sources = [row for row in rows if row['event'] == 'source']
    assert len(sources) == 1
    return sources[0], rows


def check_storage(root, expected_recipe, expected_text=None, image=None):
    value = storage(root)
    if value is None:
        return None
    source, _ = source_witness(root)
    meta = value['metadata']
    if value['cards'].get(A, {}).get('source') != expected_recipe:
        return None
    assert value['cards'][B]['source'] == source['rejected']
    assert value['cards'][A]['existence'] == value['cards'][B]['existence'] == 'open'
    assert meta['quarantined_panes'] == {B: dict(kind='chat', **source['rejected'])}
    assert meta['chat_drafts'].get(B) == BAD_TEXT
    bad_images = meta['chat_draft_attachments'][B]
    assert len(bad_images) == 1 and bad_images[0]['path'] == str(root / 'rejected-saved.png')
    if expected_text is not None and meta['chat_drafts'].get(A) != expected_text:
        return None
    images = meta['chat_draft_attachments'].get(A, [])
    if image is not None and images != [image]:
        return None
    return value


class PortalObserver:
    """Read-only monitor on this private bus; never supplies a portal response."""
    def __init__(self, output):
        self.output = output
        self.rows = []
        self.lock = threading.Lock()
        flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
        address = os.environ['DBUS_SESSION_BUS_ADDRESS']
        self.monitor = Gio.DBusConnection.new_for_address_sync(address, flags, None, None)
        self.query = Gio.DBusConnection.new_for_address_sync(address, flags, None, None)
        self.frontend_owner = self.bus_call('GetNameOwner', 'org.freedesktop.portal.Desktop', '(s)')
        self.monitor.add_filter(self.receive, None)
        self.monitor.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus',
            'org.freedesktop.DBus.Monitoring', 'BecomeMonitor',
            GLib.Variant('(asu)', (["type='method_call',interface='org.freedesktop.portal.FileChooser',member='OpenFile'",
                "type='signal',interface='org.freedesktop.portal.Request',member='Response'",
                "type='method_return',sender='" + self.frontend_owner + "'"], 0)),
            None, Gio.DBusCallFlags.NONE, 5000, None)

    def bus_call(self, method, name, signature):
        return self.query.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', method,
            GLib.Variant('(s)', (name,)), GLib.VariantType.new(signature), Gio.DBusCallFlags.NONE, 5000, None).unpack()[0]

    def receive(self, connection, message, incoming, unused):
        if not incoming:
            return message
        body = message.get_body()
        value = dict(type=int(message.get_message_type()), sender=message.get_sender(), destination=message.get_destination(),
                     interface=message.get_interface(), member=message.get_member(), path=message.get_path(),
                     serial=message.get_serial(), reply_serial=message.get_reply_serial(),
                     body=body.unpack() if body else None, unix_ns=time.time_ns())
        # GLib variants nested in a{sv} are recursively unpacked by PyGObject.
        with self.lock:
            self.rows.append(value)
            with self.output.open('a') as stream:
                stream.write(json.dumps(value) + '\n')
        return message

    def opened(self, ui_pid, frontend_pid):
        with self.lock:
            rows = list(self.rows)
        calls = [row for row in rows if row['interface'] == 'org.freedesktop.portal.FileChooser' and row['member'] == 'OpenFile']
        assert len(calls) <= 1, 'No repeated or extra chooser input'
        if not calls:
            return None
        call = calls[0]
        assert self.bus_call('GetConnectionUnixProcessID', call['sender'], '(u)') == ui_pid
        owner = self.bus_call('GetNameOwner', 'org.freedesktop.portal.Desktop', '(s)')
        assert self.bus_call('GetConnectionUnixProcessID', owner, '(u)') == frontend_pid
        parent, title, options = call['body']
        assert parent == '' and title == 'Attach files to chat' and options['modal'] is True and options['multiple'] is True
        token = options['handle_token']
        assert re.fullmatch(r'[A-Za-z0-9_]+', token)
        returns = [row for row in rows if row['reply_serial'] == call['serial'] and row['destination'] == call['sender'] and row['sender'] == owner]
        assert len(returns) <= 1
        if not returns:
            return None
        path = returns[0]['body'][0]
        assert path == '/org/freedesktop/portal/desktop/request/' + call['sender'][1:].replace('.', '_') + '/' + token
        return dict(call=call, returned=returns[0], frontend_owner=owner, request_path=path, ui_pid=ui_pid, frontend_pid=frontend_pid)

    def cancelled(self, opened):
        with self.lock:
            values = [row for row in self.rows if row['interface'] == 'org.freedesktop.portal.Request'
                      and row['member'] == 'Response' and row['path'] == opened['request_path']]
        assert len(values) <= 1
        if not values:
            return None
        assert values[0]['sender'] == opened['frontend_owner'] and values[0]['body'][0] == 1
        return values[0]

    def close(self):
        self.monitor.close_sync(None)
        self.query.close_sync(None)


def compile_helper(control, checkout, label, target, base):
    artifact = out / (label + '-development-ui.jsonl')
    with artifact.open('xb') as stream:
        command(['cargo', 'build', '--locked', '--profile', 'dev-runtime', '--no-default-features',
                 '--features', 'development,ui', '--lib', '--message-format=json'], cwd=checkout, timeout=4200, stdout=stream)
    rows = [json.loads(line) for line in artifact.read_text().splitlines() if line.startswith('{')]
    libs = [name for row in rows if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'cagents'
            and set(row.get('features', [])) == {'development', 'ui'} for name in row['filenames'] if name.endswith('.rlib')]
    serde = [name for row in rows if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'serde_json'
             and 'lib' in row['target']['kind'] for name in row['filenames'] if name.endswith('.rlib')]
    assert len(libs) == len(serde) == 1
    helper = base / (label + '-fixture')
    argv = ['rustc', '--edition=2024', str(control / '.github/qualification/final-main-fixture.rs'),
            '--extern', 'cagents=' + libs[0], '--extern', 'serde_json=' + serde[0],
            '-L', 'dependency=' + str(target / 'dev-runtime/deps'), '-o', str(helper)]
    for row in rows:
        if row.get('reason') == 'build-script-executed':
            for path in row.get('linked_paths', []):
                argv += ['-L', path]
    command(argv, timeout=180)
    return helper


def exercise(control, checkout, label, helper, base, target, plan, resources):
    global arm
    arm = label
    root = base / label
    root.mkdir(mode=0o700)
    env = dict(os.environ)
    for key, suffix in [('HOME','home'),('XDG_DATA_HOME','data'),('XDG_CONFIG_HOME','config'),('XDG_CACHE_HOME','cache'),
                        ('XDG_STATE_HOME','state'),('TMPDIR','tmp'),('XDG_RUNTIME_DIR','runtime')]:
        path = root / suffix
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.update(RENDERER, ACCESSIBILITY_ENABLED='1', NO_AT_BRIDGE='0', WINIT_UNIX_BACKEND='x11',
               XDG_CURRENT_DESKTOP='GNOME')
    descriptors = root / 'portal-descriptors'
    descriptors.mkdir(mode=0o700)
    official_descriptor = Path('/usr/share/xdg-desktop-portal/portals/gtk.portal')
    descriptor = descriptors / 'gtk.portal'
    descriptor.write_bytes(official_descriptor.read_bytes())
    assert sha(descriptor) == sha(official_descriptor)
    env['XDG_DESKTOP_PORTAL_DIR'] = str(descriptors)
    (out / (label + '-gtk.portal')).write_bytes(descriptor.read_bytes())
    env['CARGO_HOME'] = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
    env['RUSTUP_HOME'] = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))
    icon = root / 'config/cosmic/com.system76.CosmicTk/v1/icon_theme'
    icon.parent.mkdir(parents=True)
    icon.write_text('"Adwaita"\n')
    permissions = spawn(['/usr/libexec/xdg-permission-store', '--verbose'], env=env,
                        log=(out / (label + '-permission-store.log')).open('wb'))
    resources['permissions'] = permissions
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.impl.portal.PermissionStore'], env=env, timeout=35)
    permission_owner = bus_owner('org.freedesktop.impl.portal.PermissionStore')
    assert permission_owner['pid'] == permissions.pid
    documents = spawn(['/usr/libexec/xdg-document-portal', '--verbose'], env=env,
                      log=(out / (label + '-documents.log')).open('wb'))
    resources['documents'] = documents
    resources['mount'] = root / 'runtime/doc'
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.portal.Documents'], env=env, timeout=35)
    document_owner = bus_owner('org.freedesktop.portal.Documents')
    assert document_owner['pid'] == documents.pid
    mount_reply = Gio.bus_get_sync(Gio.BusType.SESSION, None).call_sync('org.freedesktop.portal.Documents',
        '/org/freedesktop/portal/documents', 'org.freedesktop.portal.Documents', 'GetMountPoint', None,
        GLib.VariantType.new('(ay)'), Gio.DBusCallFlags.NONE, 30000, None).unpack()[0]
    mount = Path(bytes(mount_reply).rstrip(b'\0').decode())
    assert mount == root / 'runtime/doc' and mount.is_mount()
    backend = spawn(['/usr/libexec/xdg-desktop-portal-gtk', '--verbose'], env=env, log=(out / (label + '-portal-gtk.log')).open('wb'))
    resources['backend'] = backend
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.impl.portal.desktop.gtk'], env=env, timeout=35)
    frontend = spawn(['/usr/libexec/xdg-desktop-portal', '--verbose'], env=env, log=(out / (label + '-portal.log')).open('wb'))
    resources['frontend'] = frontend
    command(['gdbus', 'wait', '--session', '--timeout', '30', 'org.freedesktop.portal.Desktop'], env=env, timeout=35)
    monitor = PortalObserver(out / (label + '-portal.jsonl'))
    resources['monitor'] = monitor
    assert monitor.bus_call('GetConnectionUnixProcessID', monitor.bus_call('GetNameOwner', 'org.freedesktop.portal.Desktop', '(s)'), '(u)') == frontend.pid
    backend_owner = monitor.bus_call('GetNameOwner', 'org.freedesktop.impl.portal.desktop.gtk', '(s)')
    assert monitor.bus_call('GetConnectionUnixProcessID', backend_owner, '(u)') == backend.pid
    fixture = spawn([str(helper), str(root)], env=env, log=(out / (label + '-fixture.log')).open('wb'))
    resources['fixture'] = fixture
    wait(lambda: (root / 'fixture.jsonl').exists() and any(json.loads(line)['event'] == 'ready' for line in (root / 'fixture.jsonl').read_text().splitlines()))
    ui = spawn(['scripts/run-development', '--browserless', '--debug', '--log-file', str(out / (label + '-ui.log'))],
               env=env, cwd=checkout, log=(out / (label + '-runner.log')).open('wb'))
    resources['ui'] = ui
    def owned_window():
        assert ui.poll() is None, 'Development runner exited'
        result = subprocess.run(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'], text=True, stdout=subprocess.PIPE, timeout=5)
        values = result.stdout.split()
        assert len(values) <= 1
        return values[0] if values else None
    window = wait(owned_window, 1800)
    pid = int(command(['xdotool', 'getwindowpid', window]).strip())
    ui_identity = processes()[pid]
    assert pid in family(processes(), {ui.pid}), 'Native window must belong to this actual development runner'
    exe = Path('/proc') / str(pid) / 'exe'
    assert exe.resolve() == (target / 'dev-runtime/cagents-ui').resolve(strict=True)
    ui_sha = sha(exe)
    triplet = {name: dict(path=str(target / 'dev-runtime' / name), sha256=sha(target / 'dev-runtime' / name))
               for name in ('cagents', 'cagentsd', 'cagents-ui')}
    command(['xdotool', 'windowactivate', '--sync', window])  # Initial setup only.
    command(['xdotool', 'windowsize', '--sync', window, '1280', '800'])
    command(['xdotool', 'windowmove', '--sync', window, '20', '40'])
    def composer():
        fields = [node for node in tree(pid) if node.name == 'Message' and node.getRole() == pyatspi.ROLE_ENTRY
                  and shown(node) and node.getState().contains(pyatspi.STATE_EDITABLE)]
        assert len(fields) <= 1
        return fields[0] if fields else None
    def panels(present):
        nodes = tree(pid)
        names = ['Files panel tab', 'Details panel tab', 'Resize context inspector']
        if not all(sum(shown(node) and node.name == name for node in nodes) == int(present) for name in names):
            return None
        toggle = target(pid, 'Toggle inspector — Ctrl+Shift+I')
        return dict(present=present, toggle=bounds(toggle)) if toggle else None
    before_panel = wait(lambda: panels(True))
    command(['xdotool', 'key', 'ctrl+shift+i'])
    after_panel = wait(lambda: panels(False))
    wait(composer)
    def key_target(expected):
        field = composer()
        if field is None or field.queryText().getText(0, -1) != expected or not field.getState().contains(pyatspi.STATE_FOCUSED):
            return None
        native_focus = int(command(['xdotool', 'getwindowfocus']).strip())
        if native_focus != int(window):
            return None  # GTK can unmap before normal WM focus restoration.
        native_pid = int(command(['xdotool', 'getwindowpid', str(native_focus)]).strip())
        assert native_pid == pid
        assert processes()[pid]['stamp'] == ui_identity['stamp'], 'Native UI process generation changed'
        text = field.queryText()
        assert text.caretOffset == len(expected) and text.getNSelections() == 0
        return dict(window=native_focus, pid=native_pid, start_stamp=ui_identity['stamp'],
                    editor_bounds=bounds(field), text=expected, caret=text.caretOffset, selections=text.getNSelections())
    def caption(account):
        nodes = tree(pid)
        names = [node.name for node in nodes if shown(node)]
        return account if names.count(account) == 1 else None
    def snapshot(name, size):
        command(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
        wait(lambda: target(pid, 'Attach files') is not None)
        data = observation(lambda: [dict(name=node.name, role=node.getRoleName(), bounds=bounds(node),
            focused=node.getState().contains(pyatspi.STATE_FOCUSED)) for node in tree(pid) if shown(node)])
        if name in ('draft-image-wide', 'postcancel-wide'):
            assert any(node['name'] == 'Saved pane unavailable' for node in data), 'Rejected sibling must remain visibly Failed in the wide comparison'
        png = out / (label + '-' + name + '.png')
        command(['import', '-window', window, str(png)])
        with Image.open(png) as image:
            assert image.size == tuple(size)
            image.save(out / (label + '-' + name + '.webp'), lossless=True)
        (out / (label + '-' + name + '-a11y.json')).write_text(json.dumps(data, indent=2) + '\n')
        emit('capture', name=name, viewport=size, webp=label + '-' + name + '.webp')
        emit('client-geometry', phase=name, xwininfo=command(['xwininfo','-id',window,'-stats','-tree']).decode())
    source, rows = source_witness(root)
    wait(lambda: any(row['event'] == 'seed-A-only' for row in source_witness(root)[1]))
    channel_scope = source['full_model']['views'][0]['scope']
    wait(lambda: any(row['event'] == 'seed-ack' and row['scope'] == channel_scope for row in source_witness(root)[1]))
    wait(lambda: target(pid, 'Saved pane unavailable', pyatspi.ROLE_HEADING) or any(node.name == 'Saved pane unavailable' and shown(node) for node in tree(pid)))
    assert observation(lambda: composer().queryText().getText(0, -1)) == ''
    wait(lambda: caption('Synthetic initial account'))
    click(pid, 'Choose a model')
    wait(lambda: target(pid, 'Fixture model A') is not None and target(pid, 'Fixture model B') is not None)
    catalog_observed = observation(lambda: [dict(name=name, bounds=bounds(target(pid, name)))
                                           for name in ('Fixture model A', 'Fixture model B')])
    command(['xdotool', 'key', 'Escape'])  # Close only; model and effort remain constant.
    wait(lambda: target(pid, 'Fixture model A') is None and target(pid, 'Fixture model B') is None)
    rectangle = observation(lambda: bounds(composer()))
    x, y, width, height = rectangle
    command(['xdotool', 'mousemove', '--sync', str(x + width // 2), str(y + height // 2), 'click', '1'])
    wait(lambda: key_target(''))
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', TEXT])
    wait(lambda: key_target(TEXT))
    clipboard = clipboard_image(root)
    resources['clipboard'] = clipboard
    def pasted():
        value = storage(root)
        images = value and value['metadata'].get('chat_draft_attachments', {}).get(A, [])
        return (images[0] if len(images or []) == 1 and target(pid, 'Remove attachment') is not None
            and any(node.name == 'Image 1' and shown(node) for node in tree(pid))
            and not any(node.name == 'Pasting…' and shown(node) for node in tree(pid)) else None)
    image = wait(pasted)
    expected_text = observation(lambda: composer().queryText().getText(0, -1))
    assert TEXT in expected_text and 'image' in expected_text.lower()
    assert image['reference']['name'] == 'Pasted image' and image['reference']['number'] == 1
    asset = Path(image['path'])
    assert asset.resolve(strict=True) == asset and asset.is_relative_to(root / 'data') and sha(asset) == sha(root / 'offered.png')
    wait(lambda: check_storage(root, source['initial'], expected_text, image))
    (out / (label + '-payload-storage.json')).write_text(json.dumps(
        wait(lambda: check_storage(root, source['initial'], expected_text, image)), indent=2) + '\n')
    snapshot('draft-image-wide', (1280, 800))
    click(pid, 'Attach files')
    opened = wait(lambda: monitor.opened(pid, frontend.pid))
    def dialog():
        values = [node for node in tree(backend.pid) if node.getRole() in (pyatspi.ROLE_DIALOG, pyatspi.ROLE_FILE_CHOOSER)
                  and node.name == 'Attach files to chat' and shown(node)]
        assert len(values) <= 1
        return values[0] if values else None
    wait(dialog)
    def gtk_key_owner():
        focus = int(command(['xdotool', 'getwindowfocus']).strip())
        if focus in (0, 1):  # None/PointerRoot have no process owner.
            return None
        owner_property = command(['xprop', '-id', str(focus), '_NET_WM_PID']).decode().strip()
        owner_match = re.fullmatch(r'_NET_WM_PID\(CARDINAL\) = ([0-9]+)', owner_property)
        if owner_match is None or int(owner_match[1]) != backend.pid:
            return None  # Root/other-window focus can precede GTK activation.
        owner = int(command(['xdotool', 'getwindowpid', str(focus)]).strip())
        assert owner == backend.pid
        assert processes()[owner]['stamp'] == backend.owned_identity['stamp'], 'GTK process generation changed'
        return focus
    wait(gtk_key_owner)  # Observe normal WM activation before chooser-only keys.
    # libcosmic's portal path omits parent and ignores Dialog.directory. Use
    # the real GTK location entry to prevent captures of private Home listings.
    command(['xdotool', 'key', 'ctrl+l'])
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', str(root / 'chooser-data') + '/'])
    command(['xdotool', 'key', 'Return'])
    wait(lambda: any(node.name == 'synthetic-file.txt' and shown(node) for node in tree(backend.pid)))
    dialog_focus = wait(gtk_key_owner)
    emit('chooser-pending', portal=opened, gtk_pid=backend.pid, window=dialog_focus,
         chooser_directory=str(root / 'chooser-data'))
    (root / 'peer-update').touch(exist_ok=False)
    wait(lambda: any(row['event'] == 'peer-delivered' for row in source_witness(root)[1]))
    peer_storage = wait(lambda: check_storage(root, source['chosen'], expected_text, image))
    assert observation(lambda: caption('Synthetic initial account'))
    # Capture the actual owned dialog, after navigation and the peer update.
    command(['import', '-window', str(dialog_focus), str(out / (label + '-chooser-peer.png'))])
    with Image.open(out / (label + '-chooser-peer.png')) as image_file:
        image_file.save(out / (label + '-chooser-peer.webp'), lossless=True)
    emit('chooser-open-peer', portal=opened, gtk_pid=backend.pid, window=dialog_focus,
         chooser_directory=str(root / 'chooser-data'), canonical=peer_storage, image=image)
    click(backend.pid, 'Cancel')  # Genuine GTK Cancel once; no WM close or Request.Close.
    cancelled = wait(lambda: monitor.cancelled(opened))
    wait(lambda: not dialog())
    # No focus repair follows Cancel. A missing focus restoration is a real
    # observed failure of this private desktop, not a reason to click A again.
    focus_after = wait(lambda: key_target(expected_text))
    if label == 'after':
        wait(lambda: caption('Synthetic peer account'))
    # Before observes its actual result, without waiting for an expected bad
    # label or manufacturing a timeout. Two equal read snapshots settle it.
    def footer():
        return observation(lambda: [node.name for node in tree(pid) if shown(node)
            and (node.name in ('Synthetic initial account', 'Synthetic peer account', 'Claude · Default model', 'Ask before edits'))])
    actual_footer = footer()
    assert actual_footer and footer() == actual_footer
    if label == 'after':
        assert 'Synthetic peer account' in actual_footer and 'Synthetic initial account' not in actual_footer
    assert 'Claude · Default model' in actual_footer and 'Ask before edits' in actual_footer
    headers = observation(lambda: [node.name for node in tree(pid) if shown(node)
        and node.getRole() == pyatspi.ROLE_PUSH_BUTTON and node.name.startswith('Chat pane ')
        and not node.name.startswith('Chat pane Unavailable saved sibling, ')])
    if label == 'after':
        assert any(', ' + str(root / 'project/peer-directory') + ', focused' in header for header in headers)
    command(['xdotool', 'type', '--clearmodifiers', '--delay', '1', SUFFIX])
    final_text = expected_text + SUFFIX
    final_focus = wait(lambda: key_target(final_text))
    emit('postcancel-continuation', before=focus_after, after=final_focus, suffix=SUFFIX)
    wait(lambda: check_storage(root, source['chosen'], final_text, image))
    snapshot('postcancel-wide', (1280, 800))
    snapshot('postcancel-narrow', (900, 700))
    assert observation(lambda: key_target(final_text))
    stop(ui)
    stop(clipboard)
    stop(fixture)
    source, rows = source_witness(root)
    assert len([row for row in rows if row['event'] == 'seed-A-only']) == 1
    assert len([row for row in rows if row['event'] == 'peer-delivered']) == 1
    inspections = root / 'inspection'
    inspections.mkdir(mode=0o700)
    clone = inspections / 'final.sqlite3'
    with sqlite3.connect('file:' + str(database(root)) + '?mode=ro', uri=True) as original, sqlite3.connect(clone) as copied:
        original.backup(copied)
    os.chmod(clone, 0o600)
    typed = command([str(helper), str(root), '--inspect', str(clone)], env=env)
    (out / (label + '-typed-inspection.json')).write_bytes(typed)
    (out / (label + '-fixture.jsonl')).write_bytes((root / 'fixture.jsonl').read_bytes())
    final = wait(lambda: check_storage(root, source['chosen'], final_text, image))
    assert asset.read_bytes() == (root / 'offered.png').read_bytes()
    (out / (label + '-managed-image.png')).write_bytes(asset.read_bytes())
    asset_info = asset.lstat()
    assert stat.S_ISREG(asset_info.st_mode) and not asset.is_symlink() and asset_info.st_nlink == 1 and asset_info.st_uid == os.geteuid()
    (out / (label + '-storage.json')).write_text(json.dumps(final, indent=2) + '\n')
    (out / (label + '-source.json')).write_text(json.dumps(dict(revision=plan[label if label == 'before' else 'product'],
        ui_pid=pid, ui_start_stamp=ui_identity['stamp'], ui_sha256=ui_sha, window=int(window), matched_triplet=triplet,
        helper_sha256=sha(helper), fixture_sha256=sha(control / '.github/qualification/final-main-fixture.rs'),
        portal_backend=dict(pid=backend.pid, owner=backend_owner, executable='/usr/libexec/xdg-desktop-portal-gtk',
                            sha256=sha(Path('/usr/libexec/xdg-desktop-portal-gtk')), identity=backend.owned_identity),
        portal_frontend=dict(pid=frontend.pid, owner=monitor.frontend_owner, executable='/usr/libexec/xdg-desktop-portal',
                             sha256=sha(Path('/usr/libexec/xdg-desktop-portal')), identity=frontend.owned_identity),
        descriptor=dict(original=str(official_descriptor), private=str(descriptor), sha256=sha(descriptor),
                        original_sha256=sha(official_descriptor)), catalog_observed=catalog_observed,
        portal_auxiliaries=dict(permission_store={**permission_owner,'identity':permissions.owned_identity,
            'executable':'/usr/libexec/xdg-permission-store','sha256':sha(Path('/usr/libexec/xdg-permission-store'))},
            documents={**document_owner,'identity':documents.owned_identity,'executable':'/usr/libexec/xdg-document-portal',
                       'sha256':sha(Path('/usr/libexec/xdg-document-portal'))}, mount=str(mount)),
        inspector_before=before_panel, inspector_after=after_panel, portal_open=opened, portal_cancel=cancelled,
        postcancel_footer=actual_footer, postcancel_headers=headers, restored_focus=focus_after, continued_input=final_focus,
        actual_image=image, final_text=final_text,
        asset=dict(path=str(asset), sha256=sha(asset), bytes=asset_info.st_size, device=asset_info.st_dev,
                   inode=asset_info.st_ino, uid=asset_info.st_uid, links=asset_info.st_nlink),
        scope='Synthetic recipe and exact raw wrong-issuer sibling; actual UI, clipboard worker, portal and cancel. No provider/actor started.'), indent=2) + '\n')
    for name, value in triplet.items():
        assert sha(Path(value['path'])) == value['sha256'], 'Matched executable changed during its native arm'


def main():
    global out
    user()
    assert len(sys.argv) == 4
    control, before, after = [Path(value).resolve(strict=True) for value in sys.argv[1:]]
    plan = json.loads((control / '.github/qualification/final-main-plan.json').read_text())
    out = Path(os.environ['QUALIFICATION_EVIDENCE']) / 'native'
    out.mkdir(parents=True, exist_ok=True)
    (out / 'source.json').write_text(json.dumps(dict(product=plan['product'], before=plan['before'],
        control_sha=command(['git', '-C', str(control), 'rev-parse', 'HEAD']).decode().strip(),
        control_tree=command(['git', '-C', str(control), 'rev-parse', 'HEAD^{tree}']).decode().strip(),
        driver_sha256=sha(Path(__file__)), fixture_sha256=sha(control / '.github/qualification/final-main-fixture.rs'),
        workflow_sha256=sha(control / '.github/workflows/qualify-final-main-integration.yml'),
        run=os.environ['GITHUB_RUN_ID'], attempt=os.environ['GITHUB_RUN_ATTEMPT']), indent=2) + '\n')
    base = Path(tempfile.mkdtemp(prefix='h2018.', dir='/tmp')).resolve(strict=True)
    os.chmod(base, 0o700)
    target_dir = Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True)
    # Adopt escaped descendants for observed, exact owned cleanup.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER failed')
    errors = []
    arm_cleanups = {}
    def cancelled(signum, frame):
        raise RuntimeError('Native integration cancelled; owned cleanup is required')
    signal.signal(signal.SIGTERM, cancelled)
    try:
        launcher = next(path for path in [Path('/usr/libexec/at-spi-bus-launcher'), Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')] if path.exists())
        spawn([str(launcher), '--launch-immediately'], log=(out / 'atspi.log').open('wb'))
        for key in ('IsEnabled', 'ScreenReaderEnabled'):
            command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus',
                     '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', key, '<true>'])
        bus = command(['gdbus', 'call', '--session', '--dest', 'org.a11y.Bus', '--object-path', '/org/a11y/bus', '--method', 'org.a11y.Bus.GetAddress'])
        os.environ['AT_SPI_BUS_ADDRESS'] = ast.literal_eval(bus.decode())[0]
        registry = next(path for path in [Path('/usr/libexec/at-spi2-registryd'), Path('/usr/lib/at-spi2-core/at-spi2-registryd')] if path.exists())
        spawn([str(registry)], env=dict(os.environ, DBUS_SESSION_BUS_ADDRESS=os.environ['AT_SPI_BUS_ADDRESS']), log=(out / 'atspi-registry.log').open('wb'))
        rc = base / 'rc.xml'
        rc.write_text('<openbox_config xmlns="http://openbox.org/3.4/rc"><focus><focusNew>yes</focusNew><followMouse>no</followMouse></focus></openbox_config>')
        (out / 'openbox-rc.xml').write_bytes(rc.read_bytes())
        wm = spawn(['/usr/bin/openbox', '--config-file', str(rc)], log=(out / 'openbox.log').open('wb'))
        wait(lambda: b'_NET_SUPPORTING_WM_CHECK(WINDOW)' in command(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK']))
        (out / 'desktop.json').write_text(json.dumps(dict(openbox_pid=wm.pid, config_sha256=sha(rc),
            supporting_wm=command(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK']).decode(),
            versions=command(['dpkg-query', '-W', '-f=${Package} ${Version}\n', 'openbox', 'xdg-desktop-portal', 'xdg-desktop-portal-gtk']).decode(),
            renderer=RENDERER, virtual_screen=[1600,1000], dpi=96,
            system_bus_limit='Normal frontend startup may query or activate RealtimeKit; failure is nonfatal. No system service, policy or system bus is modified or owned by this fixture.',
            parent_identifier='omitted by the actual libcosmic portal implementation',
            directory='not supplied by libcosmic portal; normal GTK location entry is used'), indent=2) + '\n')
        for label, checkout in [('before', before), ('after', after)]:
            pin = plan['before' if label == 'before' else 'product']
            assert command(['git', '-C', str(checkout), 'rev-parse', 'HEAD']).decode().strip() == pin['sha']
            assert command(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}']).decode().strip() == pin['tree']
            assert not command(['git', '-C', str(checkout), 'status', '--porcelain=v1']).strip()
            helper = compile_helper(control, checkout, label, target_dir, base)
            resources = {'desktop_owners':[child.owned_identity for child in children
                         if child.owned_identity is not None and child.poll() is None]}
            try:
                exercise(control, checkout, label, helper, base, target_dir, plan, resources)
            finally:
                result = cleanup_arm(resources)
                arm_cleanups[label] = result
                (out / (label + '-cleanup.json')).write_text(json.dumps(result, indent=2) + '\n')
                errors.extend(label + ': ' + error for error in result['errors'])
                if result['errors']:
                    raise RuntimeError('Arm cleanup failed before another arm: ' + repr(result))
    except BaseException:
        (out / 'failure-traceback.txt').write_text(traceback.format_exc())
        raise
    finally:
        for child in reversed(children):
            try:
                stop(child)
            except BaseException as error:
                errors.append(repr(error))
        remaining = drain_adopted()
        cleanup = dict(errors=errors, owned_descendants_remaining=remaining)
        (out / 'cleanup.json').write_text(json.dumps(cleanup, indent=2) + '\n')
        if not errors and not remaining:
            (out / 'cleanup-complete').write_text('Owned application, fixture, portal, WM and accessibility descendants closed.\n')
        else:
            raise RuntimeError('Native cleanup incomplete: ' + repr(cleanup))


if __name__ == '__main__':
    main()
