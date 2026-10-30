"""Supplemental actual native evidence using exact, unauthenticated local data."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import time


def exercise(api, checkout, helper, variant, revision, expected_tree):
    run, wait = api['run'], api['wait']
    capture, event, stop = api['capture'], api['event'], api['stop']
    snapshot, live_tree, visible = api['accessibility_snapshot'], api['live_tree'], api['visible']
    atspi, out, target = api['pyatspi'], api['out'], api['target']
    label = variant + '-review-tooltip'
    api['active_arm'] = label
    api['window'] = api['pid'] = None
    event({'event': 'diagnostic-arm-started', 'revision': revision,
           'scope': 'Normal saved PR utility with synthetic unauthenticated read data; no actor/proof fixture.'})
    root = api['base'] / label
    root.mkdir(mode=0o700)
    env = os.environ.copy()
    for key, name in [('HOME', 'home'), ('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'),
                      ('XDG_CACHE_HOME', 'cache'), ('XDG_STATE_HOME', 'state'),
                      ('TMPDIR', 'tmp'), ('XDG_RUNTIME_DIR', 'runtime')]:
        path = root / name
        path.mkdir(mode=0o700)
        env[key] = str(path)
    assert not any(key in os.environ for key in (
        'GIT_DIR', 'GIT_WORK_TREE', 'GIT_COMMON_DIR', 'GIT_INDEX_FILE', 'GIT_CONFIG_COUNT',
        'GIT_CONFIG_PARAMETERS')), 'Inherited Git routing refuses this fixture without environment changes'
    icon_config = root / 'config/cosmic/com.system76.CosmicTk/v1/icon_theme'
    icon_config.parent.mkdir(parents=True)
    icon_config.write_text('"Adwaita"\n')
    env['CARGO_HOME'] = os.environ.get('CARGO_HOME', str(Path.home() / '.cargo'))
    env['RUSTUP_HOME'] = os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup'))
    env.update(api['native_renderer'])
    env.update(ACCESSIBILITY_ENABLED='1', NO_AT_BRIDGE='0', WINIT_UNIX_BACKEND='x11',
               GIT_AUTHOR_DATE='2026-01-02T00:00:00Z', GIT_COMMITTER_DATE='2026-01-02T00:00:00Z')
    project, origin = root / 'project', root / 'origin.git'
    def git(*args):
        run(['git', '-C', str(project), *args], env=env)
    run(['git', 'init', '-b', 'main', str(project)], env=env)
    git('config', '--local', 'user.name', 'Synthetic native fixture')
    git('config', '--local', 'user.email', 'native-fixture@example.invalid')
    (project / 'src').mkdir()
    source = project / 'src/discussion.rs'
    source.write_text('pub fn value() -> u32 {\n    1\n}\n')
    git('add', '--', 'src/discussion.rs')
    git('-c', 'commit.gpgsign=false', 'commit', '-m', 'Synthetic native base')
    base_oid = subprocess.check_output(['git', '-C', str(project), 'rev-parse', 'HEAD'], env=env, text=True).strip()
    git('switch', '-c', 'fixture/native-review')
    source.write_text('pub fn value() -> u32 {\n    2\n}\n')
    git('add', '--', 'src/discussion.rs')
    git('-c', 'commit.gpgsign=false', 'commit', '-m', 'Synthetic native review head')
    head_oid = subprocess.check_output(['git', '-C', str(project), 'rev-parse', 'HEAD'], env=env, text=True).strip()
    assert subprocess.check_output(['git', '-C', str(project), 'rev-parse', 'HEAD^'], env=env, text=True).strip() == base_oid
    run(['git', 'clone', '--bare', '--no-hardlinks', str(project), str(origin)], env=env)
    run(['git', '--git-dir=' + str(origin), 'update-ref', 'refs/pull/42/head', head_oid], env=env)
    raw_origin = 'https://github.com/fixture-native/tooltip-data.git'
    git('remote', 'add', 'origin', raw_origin)
    git('config', '--local', 'url.' + origin.as_uri() + '.insteadOf', raw_origin)
    git('config', '--local', 'branch.fixture/native-review.remote', 'origin')
    git('config', '--local', 'branch.fixture/native-review.merge', 'refs/heads/fixture/native-review')
    git('update-ref', 'refs/remotes/origin/fixture/native-review', head_oid)
    raw_config = subprocess.check_output(['git', '-C', str(project), 'config', '--get', 'remote.origin.url'], env=env, text=True).strip()
    assert raw_config == raw_origin
    repository_probe = ['git', '-C', str(project), 'rev-parse', '--path-format=absolute', '--git-common-dir']
    discovered = subprocess.check_output(repository_probe, env=env, text=True, timeout=5)
    repository = Path(discovered.removesuffix('\n'))
    assert discovered == str(repository) + '\n' and repository == project / '.git'
    assert repository.is_absolute() and repository.resolve(strict=True) == repository
    current = repository
    while True:
        info = current.lstat()
        assert stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode) and info.st_uid == os.getuid()
        if current == root:
            break
        assert current.is_relative_to(root) and current != current.parent
        current = current.parent
    data_source = api['control'] / '.github/qualification/review-data.json'
    data = json.loads(data_source.read_text().replace('$BASE_OID', base_oid).replace('$HEAD_OID', head_oid))
    required = {'discover-repository', 'discover-unauthenticated', 'browser-list', 'review-context',
                'review-metadata', 'review-files', 'conversation-reviewThreads',
                'conversation-comments', 'conversation-reviews'}
    background = {'daemon-authored-open', 'daemon-authored-all'}
    contracts = {case['case']: case for case in data['allowlist']}
    assert len(contracts) == len(data['allowlist']) and set(contracts) == required | background
    cwd_bindings = {'$REPOSITORY': str(repository), '$WORKTREE': str(project), '$CHECKOUT': str(checkout)}
    for name, case in contracts.items():
        assert case['required'] is (name in required)
        assert case['consumer'] == ('daemon-background' if name in background else 'native-review')
        case['cwd'] = [cwd_bindings[value] for value in case['cwd']]
    directory, binary = root / 'gh-data', root / 'bin'
    directory.mkdir(mode=0o700)
    binary.mkdir(mode=0o700)
    def private_write(path, value, mode=0o600):
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(value)
    for name, value in data['responses'].items():
        payload = (json.dumps(value, indent=2) + '\n').encode()
        private_write(directory / name, payload)
        for case in data['allowlist']:
            if case['response'] == name:
                case['response_sha256'] = hashlib.sha256(payload).hexdigest()
    private_write(directory / 'manifest.json', (json.dumps({
        'allowlist': data['allowlist'], 'allowed_cwd': [str(checkout), str(project), str(repository)],
        'repository': str(repository), 'working_checkout': str(project),
        'ui_executable': str((target / 'dev-runtime/cagents-ui').resolve(strict=True)),
        'daemon_executable': str((target / 'dev-runtime/cagentsd').resolve(strict=True))}, indent=2) + '\n').encode())
    private_write(binary / 'gh', (api['control'] / '.github/qualification/private-review-gh.py').read_bytes(), 0o500)
    private_write(root / 'gh-calls.jsonl', b'')
    env['PATH'] = str(binary) + os.pathsep + os.environ['PATH']
    trace = root / 'git-trace.jsonl'
    env['GIT_TRACE2_EVENT'] = str(trace)
    run([str(helper), str(root)], env=env)
    repository_receipt = root / 'review-repository-path'
    repository_receipt_identity = api['private_inspection_file'](root, repository_receipt)
    assert repository_receipt.read_text() == str(repository)
    with (out / (label + '-runner.log')).open('w') as log:
        ui = subprocess.Popen(['scripts/run-development', '--browserless', '--debug', '--log-file',
                               str(out / (label + '-ui.log'))], cwd=checkout, env=env,
                              stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    api['children'].append(ui)
    def window_ready():
        assert ui.poll() is None, 'Private review runner exited before window'
        result = subprocess.run(['xdotool', 'search', '--onlyvisible', '--class', 'cagents'],
                                text=True, stdout=subprocess.PIPE, timeout=5)
        windows = result.stdout.split()
        return windows[0] if len(windows) == 1 else None
    window = wait(window_ready, timeout=1800)
    pid = int(subprocess.check_output(['xdotool', 'getwindowpid', window], text=True, timeout=5))
    api['window'], api['pid'] = window, pid
    executable = Path('/proc') / str(pid) / 'exe'
    assert executable.resolve() == (target / 'dev-runtime/cagents-ui').resolve()
    launch = {'arm': label, 'revision': revision, 'tree': expected_tree, 'pid': pid, 'window': window,
              'executable': str(executable.resolve()),
              'ui_sha256': api['digest'](executable), 'helper_sha256': api['digest'](helper),
              'fixture_source_sha256': api['digest'](api['control'] / '.github/qualification/review-fixture.rs'),
              'script_sha256': api['digest'](binary / 'gh'), 'data_source_sha256': api['digest'](data_source),
              'manifest_sha256': api['digest'](directory / 'manifest.json'),
              'base_oid': base_oid, 'head_oid': head_oid, 'raw_origin': raw_origin,
              'working_checkout': str(project), 'repository': str(repository),
              'repository_probe': repository_probe, 'repository_receipt': repository_receipt_identity,
              'daemon_executable': str((target / 'dev-runtime/cagentsd').resolve(strict=True)),
              'local_origin': str(origin), 'discovery': 'Unauthenticated; synthetic viewer is read DTO data only.'}
    (out / (label + '-launch.json')).write_text(json.dumps(launch, indent=2) + '\n')
    run(['xdotool', 'windowactivate', '--sync', window])
    run(['xdotool', 'windowsize', '--sync', window, '1280', '800'])
    run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
    api['viewport'] = (1280, 800)
    def inspector_setup(panel_visible):
        # The global inspector and this review's internal file panel are
        # separate. Observe their actual controls; do not infer toggle state.
        nodes = live_tree(pid)
        header_names = tuple('Pull requests pane Private review fixture, ' + str(repository) + ', ' + state
                             for state in ('focused', 'not focused'))
        controls = {}
        for key, names, role, present in [
            ('inspector_toggle', ('Toggle inspector — Ctrl+Shift+I',), atspi.ROLE_PUSH_BUTTON, True),
            ('global_files_tab', ('Files panel tab',), atspi.ROLE_PUSH_BUTTON, panel_visible),
            ('global_details_tab', ('Details panel tab',), atspi.ROLE_PUSH_BUTTON, panel_visible),
            ('global_resize', ('Resize context inspector',), atspi.ROLE_SEPARATOR, panel_visible),
            ('pane_header', header_names, atspi.ROLE_PUSH_BUTTON, True),
            ('review_list', ('Review pull request #42 Synthetic discussion-count review',), atspi.ROLE_PUSH_BUTTON, True),
            ('internal_file_panel', ('Hide review files',), atspi.ROLE_PUSH_BUTTON, True),
        ]:
            matches = [node for node in nodes if node.name in names and node.getRole() == role and visible(node)]
            assert len(matches) <= 1, ('Ambiguous native review setup control', key)
            if len(matches) != int(present):
                return None
            if matches:
                node = matches[0]
                bounds = node.queryComponent().getExtents(atspi.DESKTOP_COORDS)
                if not (bounds.width > 0 and bounds.height > 0 and bounds.x >= 0 and bounds.y >= 0
                        and bounds.x + bounds.width <= api['viewport'][0]
                        and bounds.y + bounds.height <= api['viewport'][1]):
                    return None
                enabled = node.getState().contains(atspi.STATE_ENABLED)
                if role == atspi.ROLE_PUSH_BUTTON and not enabled:
                    return None
                controls[key] = {'name': node.name, 'role': node.getRoleName(),
                                 'bounds': [bounds.x, bounds.y, bounds.width, bounds.height],
                                 'enabled': enabled}
        return {'panel_controls_visible': panel_visible, 'controls': controls,
                'repository': str(repository), 'internal_file_panel_open_control': 'Hide review files'}
    setup_before = wait(lambda: snapshot(lambda: inspector_setup(True)))
    setup_focus = int(subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5).strip())
    setup_pid = int(subprocess.check_output(['xdotool', 'getwindowpid', str(setup_focus)], text=True, timeout=5).strip())
    assert setup_focus == int(window) and setup_pid == pid, 'Inspector setup left the owned review window'
    event({'event': 'native-review-inspector-setup', 'phase': 'before-close',
           'window': setup_focus, 'pid': setup_pid, 'viewport': list(api['viewport']), 'binding': setup_before})
    run(['xdotool', 'key', 'ctrl+shift+i'])
    setup_after = wait(lambda: snapshot(lambda: inspector_setup(False)))
    assert setup_after['controls']['pane_header']['name'] == setup_before['controls']['pane_header']['name']
    event({'event': 'native-review-inspector-setup', 'phase': 'after-close',
           'input': 'ctrl+shift+i', 'viewport': list(api['viewport']), 'binding': setup_after,
           'state_evidence': 'Global panel tabs and resize handle removed; exact PR list and internal Hide review files control exposed.'})
    api['click'](pid, 'Review pull request #42 Synthetic discussion-count review')
    def owned_row():
        nodes = live_tree(pid)
        rows = [node for node in nodes if node.name == 'Review src/discussion.rs'
                and node.getRole() == atspi.ROLE_PUSH_BUTTON and visible(node)]
        counts = [node for node in nodes if node.name == '2 · 1 open'
                  and node.getRole() == atspi.ROLE_PARAGRAPH and visible(node)]
        if len(rows) != 1 or len(counts) != 1:
            return None
        row, count = rows[0], counts[0]
        rb, cb = (node.queryComponent().getExtents(atspi.DESKTOP_COORDS) for node in (row, count))
        if not (0 <= rb.x <= cb.x < cb.x + cb.width <= rb.x + rb.width <= api['viewport'][0]
                and 0 <= rb.y <= cb.y < cb.y + cb.height <= rb.y + rb.height <= api['viewport'][1]):
            return None
        assert row.getState().contains(atspi.STATE_ENABLED)
        assert not count.getState().contains(atspi.STATE_FOCUSABLE)
        return {'row': row, 'row_bounds': [rb.x, rb.y, rb.width, rb.height],
                'count_bounds': [cb.x, cb.y, cb.width, cb.height]}
    api['click'](pid, 'Review src/discussion.rs')
    database = Path((root / 'review-database-path').read_text())
    assert database.is_absolute() and database.resolve(strict=True) == database and database.is_relative_to(root / 'data')
    def selected_cache(after_order=-1):
        api['private_inspection_file'](root, database)
        with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True, timeout=.2) as db:
            rows = db.execute(
                'SELECT repository, head, viewer, record, write_order FROM review_cache WHERE number=42').fetchall()
        assert len(rows) <= 1
        if not rows:
            return None
        repository, head, viewer, raw, order = rows[0]
        assert repository == 'github.com/fixture-native/tooltip-data' and head == head_oid and viewer == 'fixture-viewer'
        record = json.loads(raw)
        if record['selected'] != list(b'src/discussion.rs') or order <= after_order:
            return None
        return {'repository': repository, 'head': head, 'viewer': viewer,
                'selected': record['selected'], 'write_order': order,
                'record_sha256': hashlib.sha256(raw.encode()).hexdigest()}
    pointer_selection = wait(selected_cache)
    frames = []
    for size, suffix in [((1280, 800), 'wide'), ((900, 700), 'narrow')]:
        run(['xdotool', 'windowsize', '--sync', window, *map(str, size)])
        run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
        api['viewport'] = size
        wait(lambda: snapshot(owned_row))
        capture(window, label + '-rest-' + suffix, size, neutral_pointer=True)
        value = wait(lambda: snapshot(owned_row))
        x, y, width, height = value['count_bounds']
        point = (x + width // 2, y + height // 2)
        run(['xdotool', 'mousemove', '--sync', *map(str, point)])
        entered = time.monotonic()
        time.sleep(.5)
        name = label + '-count-dwell-' + suffix
        capture(window, name, size)
        observed = wait(lambda: snapshot(owned_row))
        actual = dict(line.split('=', 1) for line in subprocess.check_output(
            ['xdotool', 'getmouselocation', '--shell'], text=True, timeout=5).splitlines())
        assert (int(actual['X']), int(actual['Y'])) == point
        assert observed['count_bounds'] == value['count_bounds'] and observed['row_bounds'] == value['row_bounds']
        assert int(subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5)) == int(window)
        frame = {'viewport': list(size), 'row_bounds': value['row_bounds'],
                 'count_bounds': value['count_bounds'], 'actual_pointer': actual,
                 'dwell_seconds': time.monotonic() - entered, 'capture': name,
                 'capture_sha256': api['digest'](out / (name + '.webp')),
                 'caption': '2 · 1 open', 'expected_tooltip': '2 comments, 1 unresolved threads'}
        frames.append(frame)
        event({'event': 'native-review-tooltip-dwell', **frame})
        capture(window, label + '-exit-' + suffix, size, neutral_pointer=True)
        wait(lambda: snapshot(owned_row))
    # Exercise the actual row with keyboard input after observing its focus;
    # never substitute AT-SPI activation or a reducer call for native Enter.
    run(['xdotool', 'key', '--clearmodifiers', 'Tab'])
    for _ in range(100):
        value = wait(lambda: snapshot(owned_row))
        if value['row'].getState().contains(atspi.STATE_FOCUSED):
            break
        run(['xdotool', 'key', '--clearmodifiers', 'Tab'])
        time.sleep(.06)
    else:
        raise RuntimeError('The actual review file row is unreachable by native Tab')
    assert int(subprocess.check_output(['xdotool', 'getwindowfocus'], text=True, timeout=5)) == int(window)
    before_keyboard = wait(selected_cache)
    keyboard_target = wait(lambda: snapshot(owned_row))
    keyboard_target['row'].clearCache()
    assert keyboard_target['row'].getState().contains(atspi.STATE_FOCUSED)
    keyboard_started = time.clock_gettime_ns(time.CLOCK_BOOTTIME)
    event({'event': 'native-review-keyboard-target', 'window': int(window), 'pid': pid,
           'name': keyboard_target['row'].name, 'role': keyboard_target['row'].getRoleName(),
           'focused': True, 'enabled': True, 'row_bounds': keyboard_target['row_bounds'],
           'boottime_ns': keyboard_started, 'before_write_order': before_keyboard['write_order']})
    run(['xdotool', 'key', '--clearmodifiers', 'Return'])
    # Cache writes capture CLOCK_BOOTTIME on the UI thread before dispatch.
    # An older queued write cannot qualify this newly observed native Enter.
    keyboard_selection = wait(lambda: selected_cache(max(before_keyboard['write_order'], keyboard_started)))
    event({'event': 'native-review-keyboard-processed', 'keyboard_started_boottime_ns': keyboard_started,
           'selection': keyboard_selection})
    capture(window, label + '-keyboard-selected-narrow', (900, 700), neutral_pointer=True)
    stop(ui)
    calls = [json.loads(line) for line in (root / 'gh-calls.jsonl').read_text().splitlines()]
    assert len(calls) % 2 == 0 and calls
    for request, delivered in zip(calls[::2], calls[1::2]):
        assert request['event'] == 'request' and delivered['event'] == 'delivered'
        assert {k: v for k, v in request.items() if k != 'event'} == {k: v for k, v in delivered.items() if k != 'event'}
        assert request['case'] != 'UNEXPECTED' and request['exit_code'] == 0
        case = contracts[request['case']]
        assert request['argv'] == case['argv'] and request['cwd'] in case['cwd']
        assert request['response'] == str(directory / case['response'])
        assert request['response_sha256'] == case['response_sha256']
        assert request['script_sha256'] == launch['script_sha256']
        assert any(parent['pid'] == pid and parent.get('executable') == launch['executable'] for parent in request['ancestry'])
        if request['case'] in background:
            assert any(parent['pid'] == request['ppid'] and parent.get('parent') == pid
                       and parent.get('executable') == launch['daemon_executable'] for parent in request['ancestry'])
    assert required <= {call['case'] for call in calls}, 'Native flow never received all exact fixture DTOs'
    (out / (label + '-gh-calls.jsonl')).write_bytes((root / 'gh-calls.jsonl').read_bytes())
    traces = [json.loads(line) for line in trace.read_text().splitlines()]
    for row in traces:
        if row.get('event') == 'child_start':
            argv = row.get('argv', [])
            assert not any('remote-https' in arg or 'remote-http' in arg or arg == 'ssh' for arg in argv)
            if row.get('child_class') == 'transport/file':
                assert any(str(origin) in arg and 'upload-pack' in arg for arg in argv)
    assert any(row.get('child_class') == 'transport/file' for row in traces), 'No real private Git fetch was observed'
    (out / (label + '-git-trace.jsonl')).write_bytes(trace.read_bytes())
    assert icon_config.read_text() == '"Adwaita"\n'
    assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip() == revision
    assert subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD^{tree}'], text=True).strip() == expected_tree
    assert not subprocess.check_output(['git', '-C', str(checkout), 'status', '--porcelain=v1'], text=True).strip()
    result = {'launch': launch, 'frames': frames, 'delivered_cases': sorted(required),
              'optional_background_cases': sorted(background),
              'delivered_background_cases': sorted(background & {call['case'] for call in calls}),
              'pointer_selection': pointer_selection, 'before_keyboard': before_keyboard,
              'keyboard_selection': keyboard_selection, 'keyboard_started_boottime_ns': keyboard_started,
              'scope': 'Actual native saved-card/list/review/conversation/count/hover/row interaction with synthetic unauthenticated data. Independent pixel UX review required; no real GitHub auth/tracking or mutations claimed.'}
    (out / (label + '-receipt.json')).write_text(json.dumps(result, indent=2) + '\n')
    event({'event': 'diagnostic-arm-completed', 'revision': revision})
    api['active_arm'] = None
