"""UNRUN immutable source capsule; no runtime authority or donor result.
Standard-library imports and definitions only. Explicit supervisor bindings
required by integration-contract.json; never connect to an inherited endpoint.
"""
import ast
import errno
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import stat
import struct
import subprocess
import time

DESKTOP = None
PIDFDS, PIDFD_FAILURES = {}, []
HEADLESS_CAPABILITIES = {}
WAYLAND_CAPABILITIES, WAYLAND_SURFACES = {}, {}
PRIVATE_SWAY_CONFIG = 'xwayland disable\nswaybg_command -\nswaynag_command -\ndefault_border none\ndefault_floating_border none\ngaps inner 0\ngaps outer 0\noutput * scale 1\ninput * xkb_layout us\nseat seat0 fallback true\n'
LOGIN_PATH_COMMAND = 'exec /bin/sh -c \'LC_ALL=C; printf "__cagents_path__%d:%s\\0" "${#PATH}" "$PATH"\''
ENDPOINT_NAMES = ('DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'DBUS_SESSION_BUS_PID',
                  'DBUS_STARTER_ADDRESS', 'DBUS_STARTER_BUS_TYPE', 'AT_SPI_BUS_ADDRESS',
                  'SESSION_MANAGER', 'ICEAUTHORITY', 'WAYLAND_DISPLAY', 'WAYLAND_SOCKET',
                  'DBUS_SYSTEM_BUS_ADDRESS', 'AT_SPI_DISPLAY', 'AT_SPI_IOR', 'GDK_DISPLAY',
                  'SWAYSOCK', 'I3SOCK', 'DBUS_SESSION_BUS_WINDOWID')


def refuse_inherited_desktop():
    present = sorted(name for name in ENDPOINT_NAMES if name in os.environ)
    assert not present, 'Inherited desktop/bus endpoints refused unchanged: ' + ', '.join(present)


def socket_inodes(address, optional=False):
    # Read kernel state only; never connect to an unproved socket.
    rows = []
    for line in Path('/proc/net/unix').read_text().splitlines()[1:]:
        fields = line.split(maxsplit=7)
        if len(fields) == 8 and fields[7] == address:
            assert fields[4] == '0001' and int(fields[3], 16) & 0x10000, 'Endpoint is not a listening Unix stream socket'
            rows.append(fields[6])
    if optional and not rows:
        return None
    assert len(rows) == 1, 'Missing/ambiguous kernel endpoint'
    return rows[0]


def server_record(process, executable, argv):
    row = processes().get(process.pid)
    assert row and row['start'] == process.native_identity['start'] and row['state'] not in ('Z', 'X')
    assert not pidfd_exited(dict(row, pid=process.pid)), 'Private endpoint lacks original retained live pidfd'
    proc = Path('/proc') / str(process.pid)
    expected = executable.resolve(strict=True)
    assert proc.stat().st_uid == os.getuid(), 'Private endpoint server UID differs'
    assert (proc / 'exe').resolve(strict=True) == expected
    ours, theirs = Path('/proc/self/ns/pid').stat(), (proc / 'ns/pid').stat()
    assert (ours.st_dev, ours.st_ino) == (theirs.st_dev, theirs.st_ino)
    assert (proc / 'cmdline').read_bytes().split(b'\0')[:-1] == [os.fsencode(arg) for arg in argv]
    return dict(pid=process.pid, start=row['start'], executable=str(expected), executable_sha256=digest(proc / 'exe'),
                argv=[str(arg) for arg in argv], uid=os.getuid(), pid_namespace=[ours.st_dev, ours.st_ino])


def verify_server(owner):
    assert not pidfd_exited(owner), 'Private endpoint original pidfd exited'
    current = processes().get(owner['pid'])
    assert current and current['start'] == owner['start'] and current['state'] not in ('Z', 'X'), 'Private endpoint server identity lost'
    assert any(row['pid'] == owner['pid'] and row['start'] == owner['start'] for row in OWNED.values()), 'Endpoint server is outside retained process custody'
    proc = Path('/proc') / str(owner['pid'])
    assert proc.stat().st_uid == owner['uid'] == os.getuid(), 'Private endpoint server UID changed'
    assert (proc / 'exe').resolve(strict=True) == Path(owner['executable'])
    assert digest(proc / 'exe') == owner['executable_sha256']
    assert (proc / 'cmdline').read_bytes().split(b'\0')[:-1] == [os.fsencode(arg) for arg in owner['argv']]
    namespace, ours = (proc / 'ns/pid').stat(), Path('/proc/self/ns/pid').stat()
    assert [namespace.st_dev, namespace.st_ino] == owner['pid_namespace'] == [ours.st_dev, ours.st_ino]


def socket_record(address, owner):
    verify_server(owner)
    inode = socket_inodes(address)
    fds = Path('/proc') / str(owner['pid']) / 'fd'
    assert any(os.readlink(fd) == 'socket:[' + inode + ']' for fd in fds.iterdir()), 'Listener is not held by the owned endpoint server'
    receipt = dict(address=address, kernel_inode=inode, owner=owner)
    if not address.startswith('@'):
        meta = Path(address).lstat()
        assert stat.S_ISSOCK(meta.st_mode) and meta.st_uid == os.getuid()
        receipt['filesystem_identity'] = [meta.st_dev, meta.st_ino, meta.st_uid]
    return receipt


def verify_socket(receipt):
    actual = socket_record(receipt['address'], receipt['owner'])
    assert actual == receipt, 'Private endpoint socket changed'


def verify_private_desktop():
    assert not PIDFD_FAILURES, 'Original pidfd admission/release failure remains HOLD'
    if DESKTOP is None:
        refuse_inherited_desktop()
        return
    root = Path(DESKTOP['root'])
    meta = root.lstat()
    assert stat.S_ISDIR(meta.st_mode) and [meta.st_dev, meta.st_ino, meta.st_uid] == DESKTOP['root_identity']
    assert stat.S_IMODE(meta.st_mode) == 0o700 and meta.st_uid == os.getuid()
    for key in ENDPOINT_NAMES:
        assert os.environ.get(key) == DESKTOP['environment'].get(key), 'Desktop/bus environment changed: ' + key
    auth = Path(DESKTOP['environment']['XAUTHORITY'])
    am = auth.lstat()
    assert stat.S_ISREG(am.st_mode) and stat.S_IMODE(am.st_mode) == 0o600
    assert [am.st_dev, am.st_ino, am.st_uid] == DESKTOP['auth_identity'] and am.st_uid == os.getuid()
    assert digest(auth) == DESKTOP['auth_sha256'], 'Private Xauthority changed'
    for endpoint in DESKTOP['sockets'].values():
        verify_socket(endpoint)
    assert Path(DESKTOP['x_lock']).read_text().strip() == str(DESKTOP['sockets']['x11']['owner']['pid'])
    if DESKTOP.get('launcher'):
        verify_server(DESKTOP['launcher'])


def require_private_desktop():
    guard()
    assert DESKTOP is not None and DESKTOP['ready'], 'Private desktop/session/accessibility custody is not established'


def child_desktop_environment(args, env, bootstrap=False, capability=None, cwd=None):
    assert env is not None, 'Implicit child environment refused'
    assert env.get('HOME') == original_home and env.get('CODEX_HOME') == original_codex_home
    assert {k: v for k, v in env.items() if k.startswith('CAGENTS_')} == {k: v for k, v in os.environ.items() if k.startswith('CAGENTS_')}
    headless, wayland, surface = None, None, None
    if capability is not None:
        require_private_desktop()
        assert not bootstrap
        if capability in HEADLESS_CAPABILITIES:
            headless = HEADLESS_CAPABILITIES[capability]
            assert not headless['used'] and args == headless['argv'] and env == headless['environment']
            assert str(cwd) == headless['cwd'] and digest(Path(args[0])) == headless['shell_sha256']
        elif capability in WAYLAND_CAPABILITIES:
            wayland = verify_wayland_capability(capability)
        else:
            assert capability in WAYLAND_SURFACES, 'Unknown desktop capability'
            surface = verify_wayland_surface(capability)
            wayland = WAYLAND_CAPABILITIES[WAYLAND_SURFACES[capability]['compositor']]
        name = Path(str(args[0])).name
        if wayland is not None:
            if name in ('wtype', 'grim'):
                assert surface is not None, 'Physical input/capture needs exact owned surface'
            if name == 'swaymsg':
                assert args[:4] == ['swaymsg', '-s', wayland['environment']['SWAYSOCK'], '-r']
                readonly = len(args) == 6 and args[4] == '-t' and args[5] in ('get_version', 'get_tree', 'get_outputs', 'get_seats')
                shutdown = len(args) == 5 and args[-1] == 'exit'
                output_mode = len(args) == 5 and re.fullmatch(r'output X11-[0-9]+ mode (?:1280x800|900x600)', args[-1]) is not None
                if surface is None:
                    assert readonly or shutdown or output_mode, 'Compositor-only capability cannot inject input or select a window'
                elif not (readonly or shutdown or output_mode):
                    assert len(args) == 5, 'Extra socket/options refused'
                    close_view = args[-1] == '[con_id=' + str(surface['view']['id']) + '] kill'
                    button = args[-1] in ('seat seat0 cursor press button1', 'seat seat0 cursor release button1')
                    motion = re.fullmatch(r'seat seat0 cursor set ([0-9]+) ([0-9]+)', args[-1])
                    inside = motion is not None and 0 <= int(motion[1]) < surface['view']['rect']['width'] and 0 <= int(motion[2]) < surface['view']['rect']['height']
                    assert close_view or button or inside, 'Unreviewed compositor mutation or other surface refused'
    if bootstrap:
        assert DESKTOP is None or not DESKTOP['ready'], 'Bootstrap exception cannot be used during collection'
        name = Path(str(args[0])).name
        assert name in ('Xvfb', 'dbus-daemon', 'at-spi-bus-launcher', 'gdbus'), 'Unknown private bootstrap executable'
        if DESKTOP is None:
            assert name in ('Xvfb', 'dbus-daemon') and not any(key in env for key in ENDPOINT_NAMES)
        else:
            assert name in ('at-spi-bus-launcher', 'gdbus')
            if name == 'gdbus':
                assert args[:3] == ['gdbus', 'call', '--address'] and args[3] == DESKTOP['environment']['DBUS_SESSION_BUS_ADDRESS']
                assert args[args.index('--method') + 1] in (
                    'org.freedesktop.DBus.ListNames', 'org.freedesktop.DBus.GetNameOwner', 'org.freedesktop.DBus.GetConnectionUnixProcessID', 'org.a11y.Bus.GetAddress')
    else:
        require_private_desktop()
    for key in ENDPOINT_NAMES:
        expected = DESKTOP['environment'].get(key) if DESKTOP else None
        if wayland is not None and key in ('WAYLAND_DISPLAY', 'SWAYSOCK'):
            assert env.get(key) == wayland['environment'][key]
        elif key == 'DISPLAY' and headless is not None:
            assert key not in env, 'Headless capability omits only the newly owned DISPLAY'
        elif key == 'DBUS_SESSION_BUS_ADDRESS' and DESKTOP and DESKTOP['ready']:
            allowed = (expected, DESKTOP['environment']['AT_SPI_BUS_ADDRESS']) if Path(str(args[0])).name == 'at-spi2-registryd' else (expected,)
            assert env.get(key) in allowed
        else:
            assert env.get(key) == expected, 'Child endpoint differs from owned private desktop: ' + key
    if headless is not None:
        headless['used'] = True


def private_desktop(base):
    """Create owned servers before any desktop client/library connection.

    Refuse inherited endpoints; never query, authenticate to or mutate them.
    Xvfb and session dbus are direct supervised children. The accessibility
    launcher and its bus must be retained descendants on that private session.
    """
    global DESKTOP
    guard()
    assert DESKTOP is None
    root = base / 'desktop'
    root.mkdir(mode=0o700)
    meta = root.stat()
    env_root = root / 'environment'
    env_root.mkdir(mode=0o700)
    env = private_env(env_root)
    # Binary Xauthority record, FamilyWild with explicit display number. The
    # server loads the same random cookie before choosing its unused display;
    # only the client's display-number field changes after displayfd reports it.
    auth = root / 'Xauthority'
    cookie = os.urandom(16)
    def authority(number):
        fields = (b'', number.encode('ascii'), b'MIT-MAGIC-COOKIE-1', cookie)
        return struct.pack('!H', 65535) + b''.join(struct.pack('!H', len(value)) + value for value in fields)
    fd = os.open(auth, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        assert os.write(fd, authority('0')) == len(authority('0'))
    finally:
        with native_cleanup_scope():
            check_native_budget()
            os.close(fd)
    display_file = root / 'display-number'
    display_fd = os.open(display_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    xargv = ['/usr/bin/Xvfb', '-displayfd', str(display_fd), '-screen', '0', '1600x1000x24',
             '-nolisten', 'tcp', '-noreset', '-auth', str(auth), '-dpi', '96']
    try:
        xserver = spawn(xargv, env=env, pass_fds=(display_fd,), bootstrap=True,
            stdout=(OUT / 'private-xvfb.log').open('wb'), stderr=subprocess.STDOUT)
    finally:
        with native_cleanup_scope():
            check_native_budget()
            os.close(display_fd)
    def display_ready():
        assert xserver.poll() is None, 'Private Xvfb exited'
        raw = display_file.read_text()
        if not raw:
            return None
        assert re.fullmatch(r'[0-9]+\n', raw)
        return raw.strip()
    number = int(wait(display_ready))
    assert 0 <= number < 65536
    auth_fd = os.open(auth, os.O_RDWR | os.O_NOFOLLOW)
    try:
        check_native_budget()
        record = authority(str(number))
        assert os.pwrite(auth_fd, record, 0) == len(record)
        check_native_budget()
        os.ftruncate(auth_fd, len(record))
    finally:
        with native_cleanup_scope():
            check_native_budget()
            os.close(auth_fd)
    xowner = server_record(xserver, Path('/usr/bin/Xvfb'), xargv)
    xsocket = socket_record('/tmp/.X11-unix/X' + str(number), xowner)
    # No service activation directories: only explicitly supervised services.
    session_socket = root / 'session.sock'
    config = root / 'session.conf'
    config.write_text('<busconfig><type>session</type><listen>unix:path=' + str(session_socket) +
        '</listen><auth>EXTERNAL</auth><policy context="default"><allow user="' + str(os.getuid()) +
        '"/><allow own="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy></busconfig>')
    busargv = ['/usr/bin/dbus-daemon', '--nofork', '--nopidfile', '--config-file=' + str(config)]
    session = spawn(busargv, env=env, bootstrap=True, stdout=(OUT / 'private-session-bus.log').open('wb'), stderr=subprocess.STDOUT)
    def session_ready():
        assert session.poll() is None, 'Private session bus exited'
        return session_socket.exists() and socket_inodes(str(session_socket), optional=True)
    wait(session_ready)
    sowner = server_record(session, Path('/usr/bin/dbus-daemon'), busargv)
    ssocket = socket_record(str(session_socket), sowner)
    am = auth.lstat()
    DESKTOP = dict(root=str(root), root_identity=[meta.st_dev, meta.st_ino, meta.st_uid],
        auth_identity=[am.st_dev, am.st_ino, am.st_uid], auth_sha256=digest(auth), ready=False,
        x_lock='/tmp/.X' + str(number) + '-lock',
        environment=dict(DISPLAY=':' + str(number), XAUTHORITY=str(auth),
            DBUS_SESSION_BUS_ADDRESS='unix:path=' + str(session_socket)), sockets=dict(x11=xsocket, session=ssocket))
    os.environ.update(DESKTOP['environment'])  # New owned task endpoints only.
    verify_private_desktop()
    put(OUT / 'private-desktop-before-clients.json', DESKTOP)


def private_accessibility(base):
    guard()
    assert DESKTOP and not DESKTOP['ready']
    root = base / 'bootstrap'
    root.mkdir(mode=0o700)
    env = private_env(root)
    launcher = next(p for p in [Path('/usr/libexec/at-spi-bus-launcher'), Path('/usr/lib/at-spi2-core/at-spi-bus-launcher')] if p.exists())
    log = (OUT / 'accessibility.log').open('wb')
    argv = [str(launcher), '--launch-immediately']
    process = spawn(argv, env=env, bootstrap=True, stdout=log, stderr=subprocess.STDOUT)
    DESKTOP['launcher'] = server_record(process, launcher, argv)
    def call(dest, object_path, method, *args):
        reply = cmd(['gdbus', 'call', '--address', DESKTOP['environment']['DBUS_SESSION_BUS_ADDRESS'],
            '--dest', dest, '--object-path', object_path, '--method', method, *args], env=env, bootstrap=True)
        if method == 'org.freedesktop.DBus.GetConnectionUnixProcessID':
            match = re.fullmatch(r'\(uint32 ([0-9]+),\)\s*', reply)
            assert match and 0 < int(match[1]) <= 0xffffffff, 'Unknown gdbus PID grammar'
            return (int(match[1]),)
        return ast.literal_eval(reply)
    wait(lambda: 'org.a11y.Bus' in call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus.ListNames')[0])
    name = call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus.GetNameOwner', 'org.a11y.Bus')
    assert isinstance(name, tuple) and len(name) == 1 and re.fullmatch(r':[0-9]+\.[0-9]+', name[0])
    owner = call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus.GetConnectionUnixProcessID', name[0])
    # gdbus formats uint32 values with an explicit type annotation.
    # The call helper parses only that exact method's original output.
    assert owner == (process.pid,), 'Accessibility launcher name belongs to a different process'
    DESKTOP['launcher_bus_name'] = name[0]
    reply = call(name[0], '/org/a11y/bus', 'org.a11y.Bus.GetAddress')
    assert isinstance(reply, tuple) and len(reply) == 1
    address = reply[0]
    match = re.fullmatch(r'unix:(path|abstract)=([/A-Za-z0-9_.-]+)(?:,guid=[0-9a-f]{32})?', address)
    assert match and match[2].startswith('/tmp/'), 'Non-local or unknown accessibility address refused'
    kernel = ('@' if match[1] == 'abstract' else '') + match[2]
    inode = socket_inodes(kernel)
    candidates = []
    for row in live(ACTIVE):
        proc = Path('/proc') / str(row['pid'])
        try:
            if (proc / 'exe').resolve() != Path('/usr/bin/dbus-daemon').resolve():
                continue
            if any(os.readlink(fd) == 'socket:[' + inode + ']' for fd in (proc / 'fd').iterdir()):
                candidates.append(row)
        except (FileNotFoundError, ProcessLookupError):
            continue
    assert len(candidates) == 1, 'Accessibility bus is not one owned descendant listener'
    row = candidates[0]
    proxy = next((child for child in CHILDREN if child.pid == row['pid']), None)
    if proxy is None:
        proxy = type('RetainedDescendant', (), dict(pid=row['pid'], native_identity=row))()
    argv = [os.fsdecode(value) for value in (Path('/proc') / str(row['pid']) / 'cmdline').read_bytes().split(b'\0')[:-1]]
    busowner = server_record(proxy, Path('/usr/bin/dbus-daemon'), argv)
    DESKTOP['sockets']['accessibility'] = socket_record(kernel, busowner)
    DESKTOP['environment']['AT_SPI_BUS_ADDRESS'] = address
    DESKTOP['ready'] = True
    os.environ.update(DESKTOP['environment'])
    require_private_desktop()
    put(OUT / 'private-desktop-ready.json', DESKTOP)
    for prop in ('IsEnabled', 'ScreenReaderEnabled'):
        cmd(['gdbus', 'call', '--address', DESKTOP['environment']['DBUS_SESSION_BUS_ADDRESS'],
            '--dest', DESKTOP['launcher_bus_name'], '--object-path', '/org/a11y/bus',
            '--method', 'org.freedesktop.DBus.Properties.Set', 'org.a11y.Status', prop, '<true>'], env=dict(env, AT_SPI_BUS_ADDRESS=address))
    registry = next(p for p in [Path('/usr/libexec/at-spi2-registryd'), Path('/usr/lib/at-spi2-core/at-spi2-registryd')] if p.exists())
    spawn([registry], env=dict(env, DBUS_SESSION_BUS_ADDRESS=address, AT_SPI_BUS_ADDRESS=address), stdout=log, stderr=subprocess.STDOUT)
    config = base / 'openbox.xml'
    config.write_text('<openbox_config xmlns="http://openbox.org/3.4/rc"><focus><focusNew>yes</focusNew></focus><applications><application class="*"><decor>no</decor><position force="yes"><x>0</x><y>0</y></position></application></applications></openbox_config>')
    spawn(['openbox', '--config-file', config], env=dict(env, AT_SPI_BUS_ADDRESS=address), stdout=(OUT / 'openbox.log').open('wb'), stderr=subprocess.STDOUT)


def bootstrap(base):
    private_desktop(base)
    private_accessibility(base)


def admit_owned(pid, row, arm):
    """Pin a newly observed owned identity before admitting it for signals."""
    key = str(pid) + ':' + row['start']
    if key in PIDFDS:
        assert PIDFDS[key]['validated'], 'Original pidfd admission remains unconfirmed'
        return OWNED[key]
    assert not any(item['identity'] == key for item in PIDFD_FAILURES), 'Prior pidfd failure remains HOLD'
    owned = dict(row, pid=pid, arm=arm)
    pin = dict(identity=key, pid=pid, start=row['start'], fd=None, validated=False,
               original_wait_settled=False, closed=False)
    PIDFDS[key] = pin
    OWNED[key] = owned  # Unknown custody remains visible if any following step fails.
    try:
        check_native_budget()
        before = processes().get(pid)
        assert before and all(before[name] == row[name] for name in ('start', 'parent', 'group'))
        pin['fd'] = os.pidfd_open(pid, 0)
        pin['admission_fd'] = pin['fd']
        check_native_budget()
        after = processes().get(pid)
        assert after and all(after[name] == before[name] for name in ('start', 'parent', 'group')), 'Identity changed during pidfd admission'
        fields = dict(line.split(':', 1) for line in (Path('/proc/self/fdinfo') / str(pin['fd'])).read_text().splitlines() if ':' in line)
        assert int(fields['Pid'].strip()) == pid, 'Opened pidfd does not name the observed owned process'
        pin['validated'] = True
        return owned
    except BaseException as error:
        PIDFD_FAILURES.append(dict(identity=key, error=type(error).__name__ + ': ' + str(error), hold=True))
        raise


def pidfd_exited(row):
    key = str(row['pid']) + ':' + row['start']
    pin = PIDFDS[key]
    assert pin['validated'], 'Unknown pidfd custody cannot be signalled'
    assert pin['identity'] == key and pin['pid'] == row['pid'] and pin['start'] == row['start'], 'Retained pidfd stamp differs'
    if pin['original_wait_settled']:
        return True
    if pin.get('descendant_exit_proof') and pin['closed']:
        proof = pin['descendant_exit_proof']
        assert proof['identity'] == key and proof['retained_fd'] == pin['admission_fd'] and proof['positive_exit_poll_before'] and proof['positive_exit_poll_after']
        assert proof['kind'] in ('PINNED_DESCENDANT_EXIT_ECHILD', 'PINNED_DESCENDANT_EXIT_CONTROL_WAIT')
        assert pin.get('close_attempted') and pin['fd'] is None
        assert all(hasattr(child, 'native_identity') for child in CHILDREN)
        assert not any(str(child.native_identity['pid']) + ':' + child.native_identity['start'] == key for child in CHILDREN)
        return True
    assert pin['fd'] is not None and pin['fd'] == pin['admission_fd'] and not pin['closed']
    check_native_budget()
    poller = select.poll()
    poller.register(pin['fd'], select.POLLIN | select.POLLHUP | select.POLLERR | select.POLLNVAL)
    events = poller.poll(0)
    check_native_budget()
    assert not any(flags & (select.POLLERR | select.POLLNVAL) for _, flags in events), 'Retained pidfd became invalid'
    return any(flags & (select.POLLIN | select.POLLHUP) for _, flags in events)


def signal_owned(row, number):
    key = str(row['pid']) + ':' + row['start']
    pin = PIDFDS[key]
    assert pin['validated'] and not pin['original_wait_settled'] and not pin['closed']
    if pidfd_exited(row):
        return
    check_native_budget()
    try:
        signal.pidfd_send_signal(pin['fd'], number, None, 0)
        EVENTS.append(dict(monotonic=time.monotonic(), cleanup_signal=number,
                           identity=row, retained_pidfd=key))
    except ProcessLookupError:
        pass


def settle_direct(child):
    # Only the original retained Popen's wait result permits its fd release.
    assert any(value is child for value in CHILDREN)
    assert child.returncode is not None
    row = child.native_identity
    key = str(row['pid']) + ':' + row['start']
    pin = PIDFDS.get(key)
    if pin is None:
        return
    pin['original_wait_settled'] = True
    pin['original_direct_exit'] = child.returncode
    release_settled_pidfd(pin)


def settle_descendant(row):
    key = str(row['pid']) + ':' + row['start']
    try:
        assert all(hasattr(child, 'native_identity') for child in CHILDREN), 'Original direct identity unknown'
        assert not any(str(child.native_identity['pid']) + ':' + child.native_identity['start'] == key for child in CHILDREN), 'Original direct Popen requires its original wait'
        assert not any(item['identity'] == key for item in PIDFD_FAILURES), 'Original descriptor custody remains HOLD'
        pin = PIDFDS[key]
        assert pin['validated'] and pin['identity'] == key and pin['pid'] == row['pid'] and pin['start'] == row['start'], 'Unvalidated/stale descendant pin'
        assert not pin['original_wait_settled'], 'Descendant cannot claim an original direct wait'
        if pin.get('descendant_exit_proof') and pin['closed']:
            assert pidfd_exited(row)
            return
        fd = pin['fd']
        assert fd is not None and fd == pin['admission_fd'] and not pin['closed'] and not pin.get('close_attempted'), 'Original descriptor not retained'
        assert pidfd_exited(row), 'A live/unknown descendant cannot use exit proof'
        check_native_budget()
        raw_error, receipt = None, None
        try:
            receipt = os.waitid(os.P_PIDFD, fd, os.WEXITED | os.WNOHANG)
        except ChildProcessError as error:
            check_native_budget()
            assert error.errno == errno.ECHILD, 'Only raw ECHILD may accompany positive pidfd exit proof'
            raw_error = dict(type=type(error).__name__, errno=error.errno, strerror=error.strerror,
                             text=str(error), representation=repr(error))
        check_native_budget()
        assert PIDFDS.get(key) is pin and pin['fd'] == fd == pin['admission_fd'] and pin['validated'] and pin['identity'] == key
        assert pin['pid'] == row['pid'] and pin['start'] == row['start']
        assert not pin['closed'] and not pin.get('close_attempted') and not pin['original_wait_settled']
        assert pidfd_exited(row), 'Original retained descriptor must still positively report exit'
        proof = dict(identity=key, retained_fd=fd, direct_original=False,
                     positive_exit_poll_before=True, positive_exit_poll_after=True,
                     original_parent_wait='UNOBSERVED')
        if raw_error is not None:
            proof.update(kind='PINNED_DESCENDANT_EXIT_ECHILD', raw_waitid_error=raw_error,
                         descendant_exit_status='UNKNOWN_UNOBSERVED')
        else:
            assert receipt is not None and receipt.si_pid == row['pid'], 'Descendant wait result unknown/wrong identity'
            proof.update(kind='PINNED_DESCENDANT_EXIT_CONTROL_WAIT',
                         control_wait_receipt=dict(pid=receipt.si_pid, code=receipt.si_code, status=receipt.si_status),
                         descendant_exit_status='OBSERVED_BY_CONTROL_WAITID')
        pin['descendant_exit_proof'] = proof
        # This qualifies only descriptor release. It never fabricates a parent
        # wait or promotes a descendant into the original CHILDREN ledger.
        release_settled_pidfd(pin)
    except Exception as error:
        PIDFD_FAILURES.append(dict(identity=key, hold=True, error='Descendant exit proof refused: ' + str(error)))
        raise


def release_settled_pidfd(pin):
    if not pin['original_wait_settled']:
        key = pin['identity']
        assert PIDFDS.get(key) is pin and pin['validated']
        proof = pin['descendant_exit_proof']
        assert proof['identity'] == key and proof['retained_fd'] == pin['fd'] == pin['admission_fd']
        assert proof['positive_exit_poll_before'] and proof['positive_exit_poll_after']
        assert proof['kind'] in ('PINNED_DESCENDANT_EXIT_ECHILD', 'PINNED_DESCENDANT_EXIT_CONTROL_WAIT')
        assert proof['direct_original'] is False
        assert all(hasattr(child, 'native_identity') for child in CHILDREN)
        assert not any(str(child.native_identity['pid']) + ':' + child.native_identity['start'] == key for child in CHILDREN)
    if pin.get('close_attempted') or pin['fd'] is None:
        return
    check_native_budget()
    fd = pin['fd']
    # Claim the one close attempt BEFORE its syscall; cancellation cannot cause
    # a later retry on a descriptor number that may already have been reused.
    pin.update(close_attempted=True, close_unconfirmed_fd=fd, fd=None)
    try:
        os.close(fd)
    except BaseException as error:
        PIDFD_FAILURES.append(dict(identity=pin['identity'], hold=True,
            error='Original settled pidfd close unconfirmed: ' + str(error)))
        raise
    else:
        pin['closed'] = True
        del pin['close_unconfirmed_fd']


def scan():
    table = processes()
    parents = {os.getpid(): ACTIVE}
    for key, row in OWNED.items():
        pin = PIDFDS.get(key)
        if pin and pin['validated'] and not pidfd_exited(row) and row['pid'] in table and table[row['pid']]['start'] == row['start']:
            parents[row['pid']] = row['arm']
    while True:
        new = [(pid, row) for pid, row in table.items() if pid not in parents and row['parent'] in parents]
        if not new:
            break
        for pid, row in new:
            arm = parents[row['parent']]
            admit_owned(pid, row, arm)
            parents[pid] = arm
    return table


def live(arm='all'):
    table = scan()
    return [row for row in OWNED.values() if (arm == 'all' or row['arm'] == arm)
            and row['pid'] in table and table[row['pid']]['start'] == row['start']
            and table[row['pid']]['state'] not in ('Z', 'X')]


def close_owned(arm='all'):
    check_native_budget()
    failures = []
    def remaining():
        return [row for key, row in OWNED.items() if (arm == 'all' or row['arm'] == arm)
                and PIDFDS.get(key, {}).get('validated') and not pidfd_exited(row)]
    for number, seconds in ((signal.SIGTERM, 5), (signal.SIGKILL, 10)):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            check_native_budget()
            try:
                scan()
            except Exception as error:
                failures.append(str(error))  # Unknown admissions stay HOLD; known pidfds still settle.
            pending = remaining()
            if not pending:
                break
            for row in pending:
                signal_owned(row, number)
            check_native_budget()
            time.sleep(min(.05, native_budget_seconds()))
            check_native_budget()
    check_native_budget()
    assert not remaining(), 'Original pinned owned identities remain live'
    direct = set()
    for child in CHILDREN:
        check_native_budget()
        if hasattr(child, 'native_identity'):
            row = child.native_identity
            direct.add(str(row['pid']) + ':' + row['start'])
        returned = child.poll()
        check_native_budget()
        if returned is not None:
            child.wait(timeout=native_budget_seconds())
            check_native_budget()
            settle_direct(child)
    for key, row in OWNED.items():
        if key not in direct:
            settle_descendant(row)
    assert not PIDFD_FAILURES and not failures, 'Unknown pidfd admission remains HOLD; no numeric fallback'


def prepare_login_path_probe(env):
    """One read-only source-exact login-shell PATH query; omit only owned DISPLAY."""
    require_private_desktop()
    assert not WAYLAND_CAPABILITIES, 'X11-only PATH probe capability'
    for name in ENDPOINT_NAMES:
        assert env.get(name) == DESKTOP['environment'].get(name)
    shell = selected_login_shell(env)
    assert Path(shell).is_absolute() and os.access(shell, os.X_OK)
    argv = [str(shell), '-i', '-c', LOGIN_PATH_COMMAND]
    child_env = env.copy()
    assert child_env['DISPLAY'] == DESKTOP['environment']['DISPLAY']
    del child_env['DISPLAY']  # Never remove an inherited endpoint; entry refused them.
    token = object()
    HEADLESS_CAPABILITIES[token] = dict(argv=argv.copy(), environment=child_env.copy(),
        cwd=str(Path(original_home)), shell_sha256=digest(Path(shell)), used=False)
    return dict(argv=argv, env=child_env, cwd=Path(original_home), capability=token)


def private_peer_credentials(endpoint):
    """Connect only after kernel-listener ownership proof; send no protocol data."""
    require_private_desktop()
    verify_socket(endpoint)
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        remaining = native_budget_seconds()
        assert remaining > 0
        connection.settimeout(min(1.0, remaining))
        check_native_budget()
        connection.connect(endpoint['address'])
        check_native_budget()
        raw = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize('3i'))
        pid, uid, gid = struct.unpack('3i', raw)
        check_native_budget()
        assert pid == endpoint['owner']['pid'] and uid == os.getuid(), 'Socket peer is not the pinned private compositor'
        verify_socket(endpoint)
        return dict(pid=pid, uid=uid, gid=gid, socket=endpoint['address'], kernel_inode=endpoint['kernel_inode'])
    finally:
        with native_cleanup_scope():
            check_native_budget()
            connection.close()


def admit_private_wayland(process, executable, argv, runtime, ipc_path, wayland_path):
    """Explicit capability for one Sway nested in the proved private desktop."""
    require_private_desktop()
    assert not WAYLAND_CAPABILITIES, 'Only one private compositor capability per control'
    assert Path(executable).resolve(strict=True) == Path('/usr/bin/sway').resolve(strict=True)
    owner = server_record(process, Path(executable), argv)
    runtime = Path(runtime).resolve(strict=True)
    rm = runtime.lstat()
    assert stat.S_ISDIR(rm.st_mode) and stat.S_IMODE(rm.st_mode) == 0o700 and rm.st_uid == os.getuid()
    assert runtime.is_relative_to(Path(DESKTOP['root']).parent)
    arguments = [str(arg) for arg in argv]
    assert arguments.count('--config') == 1
    config = Path(arguments[arguments.index('--config') + 1])
    cm = config.lstat()
    assert stat.S_ISREG(cm.st_mode) and cm.st_uid == os.getuid() and config.is_relative_to(runtime.parent)
    assert config.read_text() == PRIVATE_SWAY_CONFIG, 'Unreviewed compositor config refused'
    observed = {}
    entries = (Path('/proc') / str(process.pid) / 'environ').read_bytes().split(b'\0')
    for name in ('DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'AT_SPI_BUS_ADDRESS', 'XDG_RUNTIME_DIR', 'WLR_BACKENDS'):
        values = [value.split(b'=', 1)[1].decode() for value in entries if value.startswith(name.encode() + b'=')]
        assert len(values) == 1
        observed[name] = values[0]
    assert all(observed[key] == DESKTOP['environment'][key] for key in ('DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS', 'AT_SPI_BUS_ADDRESS'))
    assert observed['XDG_RUNTIME_DIR'] == str(runtime) and observed['WLR_BACKENDS'] == 'x11'
    if observed['WLR_BACKENDS'] == 'x11':
        values = [value.split(b'=', 1)[1].decode() for value in entries if value.startswith(b'WLR_X11_OUTPUTS=')]
        assert values == ['1'], 'Nested compositor must have exactly one private X11 output'
    endpoints = {}
    for name, value in (('SWAYSOCK', ipc_path), ('WAYLAND_DISPLAY', wayland_path)):
        path = Path(value)
        assert path.is_absolute() and path.is_relative_to(runtime) and not path.is_symlink()
        endpoints[name] = socket_record(str(path), owner)
        private_peer_credentials(endpoints[name])
    token = object()
    WAYLAND_CAPABILITIES[token] = dict(owner=owner, runtime=str(runtime),
        runtime_identity=[rm.st_dev, rm.st_ino, rm.st_uid], config=str(config), config_sha256=digest(config),
        environment={key: item['address'] for key, item in endpoints.items()}, endpoints=endpoints, parent_environment=dict(DESKTOP['environment']))
    verify_wayland_capability(token)
    put(OUT / 'private-wayland-capability.json', dict(owner=owner, runtime=str(runtime),
        endpoints=endpoints, observed_parent_environment=observed, config_sha256=digest(config),
        authority='Owned nested compositor only; physical input/capture additionally requires exact owned surface capability'))
    return token


def verify_wayland_capability(token):
    require_private_desktop()
    assert token in WAYLAND_CAPABILITIES
    capability = WAYLAND_CAPABILITIES[token]
    assert capability['parent_environment'] == DESKTOP['environment']
    verify_server(capability['owner'])
    meta = Path(capability['runtime']).lstat()
    assert stat.S_ISDIR(meta.st_mode) and stat.S_IMODE(meta.st_mode) == 0o700
    assert [meta.st_dev, meta.st_ino, meta.st_uid] == capability['runtime_identity']
    assert digest(Path(capability['config'])) == capability['config_sha256']
    for endpoint in capability['endpoints'].values():
        private_peer_credentials(endpoint)
    return capability


def wayland_environment(token, env):
    capability = verify_wayland_capability(token)
    for key in ENDPOINT_NAMES:
        assert env.get(key) == DESKTOP['environment'].get(key), 'Wayland context must derive from proved private parent'
    child = env.copy()
    child.update(capability['environment'])
    return child


def private_wayland_query(token, name):
    assert name in ('get_version', 'get_tree', 'get_outputs', 'get_seats')
    capability = verify_wayland_capability(token)
    env = wayland_environment(token, os.environ.copy())
    return json.loads(cmd(['swaymsg', '-s', capability['environment']['SWAYSOCK'], '-r', '-t', name],
                          env=env, desktop_capability=token))


def bind_wayland_surface(compositor, window):
    verify_wayland_capability(compositor)
    assert window['surface_id'] is not None
    token = object()
    WAYLAND_SURFACES[token] = dict(compositor=compositor, pid=window['pid'],
        identity=dict(window['identity']), source=dict(window['source']), surface_id=window['surface_id'],
        binary=dict(window['binaries']['cagents-ui']))
    try:
        receipt = verify_wayland_surface(token)
    except BaseException:
        del WAYLAND_SURFACES[token]
        raise
    put(OUT / ('private-wayland-surface-' + str(window['pid']) + '.json'), receipt)
    return token


def verify_wayland_surface(token):
    assert token in WAYLAND_SURFACES
    bound = WAYLAND_SURFACES[token]
    compositor = bound['compositor']
    capability = verify_wayland_capability(compositor)
    assert not pidfd_exited(bound['identity']), 'Original UI surface process exited'
    row = processes().get(bound['pid'])
    assert row and row['start'] == bound['identity']['start'] and row['state'] not in ('Z', 'X')
    proc = Path('/proc') / str(bound['pid'])
    assert (proc / 'exe').resolve(strict=True) == Path(bound['binary']['path']).resolve(strict=True)
    assert digest(proc / 'exe') == bound['binary']['sha256']
    assert 'local.' + bound['source']['head'][:7] in bound['binary']['version']
    root = private_wayland_query(compositor, 'get_tree')
    nodes, pending = [], [root]
    while pending:
        check_native_budget()
        node = pending.pop()
        nodes.append(node)
        assert len(nodes) <= 10000, 'Unbounded compositor tree'
        pending.extend(node.get('nodes', []) + node.get('floating_nodes', []))
    views = [node for node in nodes if node.get('app_id') is not None or node.get('window') is not None]
    assert len(views) == 1, 'Unexpected second/missing private application surface'
    view = views[0]
    assert view['id'] == bound['surface_id'] and view['pid'] == bound['pid']
    assert view['app_id'] == 'dev.cagents.Cagents.Development' and view.get('shell') == 'xdg_shell'
    assert view.get('visible') is True and view.get('focused') is True
    outputs = [item for item in private_wayland_query(compositor, 'get_outputs') if item.get('active')]
    assert len(outputs) == 1 and outputs[0]['scale'] == 1 and outputs[0]['transform'] == 'normal'
    assert outputs[0]['rect']['x'] == outputs[0]['rect']['y'] == 0
    assert outputs[0]['rect']['width'] > 0 and outputs[0]['rect']['height'] > 0
    assert view['rect'] == outputs[0]['rect'] and view['current_border_width'] == 0
    assert view['window_rect'] == dict(x=0, y=0, width=view['rect']['width'], height=view['rect']['height'])
    seats = private_wayland_query(compositor, 'get_seats')
    assert len(seats) == 1 and seats[0]['name'] == 'seat0' and seats[0]['focus'] == view['id']
    verify_server(capability['owner'])
    return dict(pid=bound['pid'], identity=bound['identity'], binary=bound['binary'], source=bound['source'],
                compositor=capability['owner'], view=view, output=outputs[0], seats=seats,
                scope='Exact owned native application surface; caller still owns renderer/pixel/interaction/UX oracles')
