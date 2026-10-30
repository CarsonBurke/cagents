"""Granted After-only PR1984 native target probe. Never merge.

No worktree removal or confirmation activation exists in this controller.
Review/EC receipt checks precede fixture, desktop, build and launch.
An optional external UI-check receipt is accepted; the actual matched runner
also establishes compilation and ELF custody before opening the UI.
Receipts are externally issued evidence; this controller cannot grant a lease.
"""
import argparse
import ctypes
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

CONTROL_BASE = '77647db730fd0d5b6ec7f2fc984d5edc94d42a3c'
PARENT = '935d94f2ce7b8c228c5f3fe4422a5de031ef3b20'
AFTER = '24d0474e250bcc4acc0958cf3abbf53ee1125581'
AFTER_TREE = '7d84da268933045b1add2ad310ce45c84be751db'
NATIVE, CONTEXT, INPUTS = None, None, None


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(root, *args):
    return subprocess.check_output(['git', '--no-optional-locks', '-C', str(root), *args], text=True, timeout=10).strip()


def receipt(path, expected):
    path = Path(path)
    assert path.is_absolute() and not path.is_symlink() and path == path.resolve(strict=True)
    raw = path.read_bytes()
    assert re.fullmatch('[0-9a-f]{64}', expected) and hashlib.sha256(raw).hexdigest() == expected
    return json.loads(raw), raw


def gates(args):
    """Read-only checks. No import of native helpers or effect before success."""
    control, after = args.control.resolve(strict=True), args.after.resolve(strict=True)
    assert control != after
    head, tree = git(control, 'rev-parse', 'HEAD'), git(control, 'rev-parse', 'HEAD^{tree}')
    assert git(control, 'rev-parse', 'HEAD^') == PARENT
    assert git(control, 'status', '--porcelain=v1', '--untracked-files=all') == ''
    assert git(after, 'rev-parse', 'HEAD') == AFTER and git(after, 'rev-parse', 'HEAD^{tree}') == AFTER_TREE
    assert git(after, 'status', '--porcelain=v1', '--untracked-files=all') == ''
    manifest_path = control / 'research/worktree-delete-native/after-probe.json'
    manifest = json.loads(manifest_path.read_text())
    assert manifest['parent_control_head'] == PARENT
    assert manifest['control_base_head'] == CONTROL_BASE
    assert manifest['admission'] is None
    isolation = manifest['private_desktop_correction']
    assert isolation['status'] == 'FROZEN_SHARED_CAPSULE_FRESH_REVIEW_REQUIRED'
    assert digest(control / isolation['capsule_path']) == isolation['capsule_sha256']
    assert isolation['endpoint_names'] and len(set(isolation['endpoint_names'])) == len(isolation['endpoint_names'])
    assert not any(name in os.environ for name in isolation['endpoint_names']), 'Inherited desktop/bus endpoints refused unchanged'
    assert manifest['after'] == dict(head=AFTER, tree=AFTER_TREE)
    assert manifest['mode'] == 'AFTER_ONLY_CANCEL_NO_REMOVE'
    assert set(git(control, 'diff', '--name-only', CONTROL_BASE, 'HEAD').splitlines()) == set(manifest['control_paths_from_base'])
    # Every original base control/product entry, including Before pins, is
    # retained. The direct predecessor is the separately preserved 935 candidate;
    # historical source clearance does not establish this exact composition.
    removed_or_modified = git(control, 'diff', '--name-only', '--diff-filter=DMRTUXB', CONTROL_BASE, 'HEAD')
    assert removed_or_modified == ''
    hashes = {path: digest(control / path) for path in manifest['review_paths']}
    for path, expected in manifest['product_source_sha256'].items():
        assert digest(after / path) == expected, 'Changed product source: ' + path
    review, review_raw = receipt(args.review, args.review_sha256)
    lease, lease_raw = receipt(args.ec_lease, args.ec_lease_sha256)
    for value in [review, lease]:
        assert value['product_head'] == AFTER and value['product_tree'] == AFTER_TREE
    assert review['verdict'] == 'AFTER_ONLY_PROBE_SOURCE_CLEAR'
    assert review['independent'] is True and review['reviewer']
    assert review['control_head'] == head and review['control_tree'] == tree
    assert review['source_sha256'] == hashes
    assert review['composition_review'] == 'SOURCE_CLEAR'
    assert review['private_desktop_custody'] == 'SOURCE_CLEAR'
    originals = {'source-review.json': review_raw, 'ec-lease.json': lease_raw}
    assert (args.ui_checks is None) == (args.ui_checks_sha256 is None)
    if args.ui_checks is not None:
        checks, checks_raw = receipt(args.ui_checks, args.ui_checks_sha256)
        assert checks['product_head'] == AFTER and checks['product_tree'] == AFTER_TREE
        assert checks['verdict'] == 'MATCHED_BOUNDARIES_PASS'
        assert checks['source_only'] is False and re.fullmatch('[0-9a-f]{64}', checks['raw_evidence_sha256'])
        assert set(checks['boundaries']) == {'cli', 'host', 'ui'}
        for boundary, feature in [('cli', []), ('host', ['host']), ('ui', ['ui'])]:
            assert checks['boundaries'][boundary]['exit'] == 0
            assert checks['boundaries'][boundary]['features'] == feature
            assert re.fullmatch('[0-9a-f]{64}', checks['boundaries'][boundary]['raw_receipt_sha256'])
        originals['ui-checks.json'] = checks_raw
    assert lease['verdict'] == 'GRANTED' and lease['issuer'] == 'EC'
    assert lease['control_head'] == head and lease['control_tree'] == tree
    assert lease['scope'] == 'PR1984_AFTER_ONLY_CANCEL_NATIVE_PROBE'
    assert lease['grant_id'] and lease['actual_grant_reference']
    assert lease['max_parallel_builds'] == 1 and lease['cargo_jobs'] in (1, 2)
    assert lease['max_fixture_terminals'] == 4 and lease['provider_allowed'] is False
    assert lease['rust_toolchain'] == '1.93.0'
    now = datetime.now(timezone.utc)
    issued = datetime.fromisoformat(lease['issued_at'].replace('Z', '+00:00'))
    expires = datetime.fromisoformat(lease['expires_at'].replace('Z', '+00:00'))
    assert issued.tzinfo and expires.tzinfo and issued <= now < expires
    assert 0 < lease['collection_seconds'] <= (expires - issued).total_seconds()
    assert 0 < lease['cleanup_seconds'] <= 60
    remaining = min(lease['collection_seconds'], (expires - now).total_seconds() - lease['cleanup_seconds'])
    assert remaining > 0, 'No granted collection budget remains'
    assert os.environ.get('RUNNER_ENVIRONMENT') == 'github-hosted'
    assert os.environ.get('GITHUB_ACTIONS') == 'true'
    assert not any(name.startswith('CAGENTS_') for name in os.environ), 'Inherited authority is refused unchanged'
    assert not any(name in os.environ for name in ('WAYLAND_DISPLAY', 'WAYLAND_SOCKET'))
    assert os.environ.get('RUSTUP_TOOLCHAIN') == '1.93.0'
    runner_temp = Path(os.environ['RUNNER_TEMP']).resolve(strict=True)
    target = Path(os.environ['CARGO_TARGET_DIR']).resolve()
    out = args.evidence.resolve()
    assert target == Path(lease['target']).resolve() and target.is_relative_to(runner_temp)
    assert out == Path(lease['evidence_root']).resolve() and out.is_relative_to(runner_temp)
    assert target != out and not out.exists() and not out.is_symlink()
    assert not target.is_relative_to(out) and not out.is_relative_to(target)
    assert not out.is_relative_to(control) and not out.is_relative_to(after)
    remaining = min(lease['collection_seconds'],
        (expires - datetime.now(timezone.utc)).total_seconds() - lease['cleanup_seconds'])
    assert remaining > 0, 'No granted collection budget remains after preflight'
    work_deadline = time.monotonic() + remaining
    return dict(control=control, after=after, head=head, tree=tree, manifest=manifest,
                hashes=hashes, out=out, target=target, lease=lease, seconds=remaining,
                work_deadline=work_deadline,
                final_deadline=work_deadline + lease['cleanup_seconds'],
                original_receipts=originals)


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cli(context, args, label):
    p = NATIVE
    context['last_command'] = dict(label=label, argv=list(map(str, args)))
    raw = p.cmd([context['binary'], '--json', '--no-start', *args], env=context['env'], timeout=90)
    value = json.loads(raw)
    p.put(p.OUT / (label + '-cli.json'), value)
    assert value['ok'] is True
    return value['data']


def owner(context):
    p = NATIVE
    assert int(p.query(['getwindowpid', context['xid']])) == context['pid']
    return p.identity(context['pid'], context['ui_binary'], context['env'])


def checkpoint(context, label):
    p, prep = NATIVE, context['prep']
    pixels = prep.native_checkpoint(p, context['pid'], context['xid'], context['ui_binary'],
                                    context['env'], context['size'], label)
    assert pixels['drawable_exit'] == 0, 'Actual direct drawable capture failed'
    assert tuple(pixels['drawable_size']) == tuple(context['size'])
    assert p.nonblank(pixels['drawable_metrics']), 'Direct drawable is blank'
    context['completed_captures'].append(label)
    return pixels


def visible_words(context, label):
    """Read the already retained native frame; no input and no capture repair."""
    p = NATIVE
    png = (p.OUT / (label + '.png')).resolve(strict=True)
    capture = p.CAPTURES.get(png)
    assert capture and capture['arm'] == p.ACTIVE and p.digest(png) == capture['png_sha256']
    raw = p.cmd(['tesseract', png, 'stdout', '-l', 'eng', '--psm', '11', '--oem', '1', 'tsv'])
    (p.OUT / (label + '.tsv')).write_text(raw)
    assert p.digest(png) == capture['png_sha256']
    rows = p.tsv_lines(raw, tuple(context['size']))
    p.put(p.OUT / (label + '-ocr.json'), dict(capture_sha256=capture['png_sha256'], rows=rows,
        purpose='Read-only actual native frame; no OCR coordinates are used for probe input'))
    return rows


def literal_visible(context, label, literal, rows):
    p = NATIVE
    if any(literal in n.name for n in p.tree(context['pid']) if p.visible(n)):
        return 'actual-owned-AX'
    # Tokenize only punctuation/whitespace, retaining word order and the exact
    # source words. This accepts wrapping, never guessed input coordinates.
    wanted = re.findall(r'[A-Za-z0-9]+', literal.casefold())
    words = [word for row in rows for word in row['words']]
    tokens = [(token, word['confidence']) for word in words
              for token in re.findall(r'[A-Za-z0-9]+', word['text'].casefold())]
    found = [tokens[i:i + len(wanted)] for i in range(len(tokens) - len(wanted) + 1)
             if [token for token, confidence in tokens[i:i + len(wanted)]] == wanted]
    assert len(found) == 1, 'Actual readable literal missing/ambiguous: ' + literal
    assert min(confidence for token, confidence in found[0]) >= p.M['ocr']['minimum_word_confidence']
    return 'confidence-qualified-actual-native-OCR'


def node(context, label, prefix=False, enabled=True, role=None):
    p = NATIVE
    found = [n for n in p.tree(context['pid'])
             if (n.name.startswith(label) if prefix else n.name == label)
             and p.visible(n) and (role is None or n.getRole() == role)
             and (not enabled or n.getState().contains(p.pyatspi.STATE_ENABLED))]
    assert len(found) <= 1, 'Ambiguous actual native target: ' + label
    return found[0] if found else None


def button(context, label, enabled=True):
    return node(context, label, enabled=enabled, role=NATIVE.pyatspi.ROLE_PUSH_BUTTON)


def read_node(context, query):
    # Only a complete read-only lookup is retried for the recognized stale AX error.
    def checked():
        NATIVE.require_private_desktop()
        value = query()
        NATIVE.require_private_desktop()
        return value
    return NATIVE.a11y_read(checked)()


def click(context, target, label):
    p = NATIVE
    # All activations in this probe are literal approved controls. Menu entries
    # that would open Delete are deliberately not clicked; row Delete opens it.
    assert label in {'Search projects', 'Search projects, branches, channels',
                     'Cancel', 'Application notices', 'Application notices: success', 'Application errors'}
    assert target is not None and target.name == label
    before = owner(context)
    bounds = p.box(target)
    assert p.visible(target) and target.getState().contains(p.pyatspi.STATE_ENABLED)
    assert p.on_screen(bounds, tuple(context['size']))
    assert not p.SCREEN.state(context['xid'])['occluders']
    p.EVENTS.append(dict(arm=p.ACTIVE, monotonic=time.monotonic(), purpose='probe-safe-control',
                         exact_target=p.describe(target)))
    p.click_node(INPUTS, target)
    assert before == owner(context)


def dialog_absent(context):
    return read_node(context, lambda: button(context, 'Cancel', enabled=False)) is None


def focus_row(context, branch):
    p = NATIVE
    assert dialog_absent(context)
    field = read_node(context, lambda: node(context, 'Search projects, branches, channels',
                                            role=p.pyatspi.ROLE_ENTRY))
    if field is None:
        target = p.wait(lambda: read_node(context, lambda: button(context, 'Search projects')))
        click(context, target, 'Search projects')
        field = p.wait(lambda: read_node(context, lambda: node(
            context, 'Search projects, branches, channels', role=p.pyatspi.ROLE_ENTRY)))
    click(context, field, 'Search projects, branches, channels')
    p.wait(lambda: read_node(context, lambda: (
        (current := node(context, 'Search projects, branches, channels', role=p.pyatspi.ROLE_ENTRY))
        and current.getState().contains(p.pyatspi.STATE_FOCUSED))))
    INPUTS.shortcut(['Control_L', 'a'])
    INPUTS.type_text(branch)
    INPUTS.key('Down')
    prefix = 'Open worktree ' + branch + ','
    def focused():
        target = read_node(context, lambda: node(context, prefix, prefix=True, role=p.pyatspi.ROLE_PUSH_BUTTON))
        return target if target and target.getState().contains(p.pyatspi.STATE_FOCUSED) else None
    target = p.wait(focused)
    assert p.on_screen(p.box(target), tuple(context['size']))
    p.put(p.OUT / (context['view'] + '-' + branch + '-focused-row.json'), p.describe(target))
    return target


def menu_probe(context, branch, primary=False):
    p = NATIVE
    target = focus_row(context, branch)
    before = owner(context)
    x, y, width, height = p.box(target)
    assert not p.SCREEN.state(context['xid'])['occluders']
    INPUTS.move(x + width // 2, y + height // 2)
    INPUTS.click(3)
    NATIVE.bounded_sleep(.35)  # One bounded settle; no second click or focus repair.
    label = context['view'] + '-' + branch + '-context-menu'
    checkpoint(context, label)
    names = read_node(context, lambda: [n.name for n in p.tree(context['pid']) if p.visible(n)])
    rows = visible_words(context, label)
    anchors = {name: literal_visible(context, label, name, rows)
               for name in ['New terminal', 'Copy path', 'Copy branch']}
    delete_lines = [row for row in rows if re.match(r'^Delete\s+worktree\b', row['text'])]
    delete_exported = 'Delete worktree…' in names
    p.put(p.OUT / (label + '-menu.json'), dict(actual_visible_names=names, primary=primary,
        anchors=anchors, delete_entry_exported=delete_exported, actual_delete_ocr_lines=delete_lines,
        assertion='Native AX export plus actual pixels; absence/readability requires independent image review'))
    if primary:
        assert not delete_exported and not delete_lines
    else:
        assert delete_exported or (len(delete_lines) == 1 and
            delete_lines[0]['min_confidence'] >= p.M['ocr']['minimum_word_confidence'])
    INPUTS.key('Escape')
    NATIVE.bounded_sleep(.2)
    dismissed = label + '-dismissed'
    checkpoint(context, dismissed)
    after_names = read_node(context, lambda: [n.name for n in p.tree(context['pid']) if p.visible(n)])
    after_rows = visible_words(context, dismissed)
    assert not any(name in after_names for name in ['Copy path', 'Copy branch'])
    assert not any(row['text'] in ['Copy path', 'Copy branch'] for row in after_rows)
    assert before == owner(context)


def cancel_dialog(context, branch, blocked):
    p = NATIVE
    focus_row(context, branch)
    before = owner(context)
    INPUTS.key('Delete')  # Only exact focused real row. Never Return/space/confirm.
    cancel = p.wait(lambda: read_node(context, lambda: button(context, 'Cancel')))
    confirm = p.wait(lambda: read_node(context, lambda: button(context, 'Delete worktree', enabled=False)))
    enabled = confirm.getState().contains(p.pyatspi.STATE_ENABLED)
    label = context['view'] + '-' + branch + ('-refusal' if blocked else '-confirmation')
    checkpoint(context, label)
    rows = visible_words(context, label)
    literal_visible(context, label, 'Delete worktree', rows)
    literal_visible(context, label, 'Cancel', rows)
    p.put(p.OUT / (label + '-dialog.json'), dict(branch=branch,
        cancel=p.describe(cancel), confirm=p.describe(confirm), expected_blocked=blocked,
        observed_confirm_enabled=enabled, action='Cancel only; confirmation never activated'))
    assert enabled is (not blocked), 'Unexpected real confirmation/refusal state'
    click(context, cancel, 'Cancel')
    p.wait(lambda: dialog_absent(context))
    checkpoint(context, label + '-cancelled')
    assert before == owner(context)


def primary_delete(context):
    p = NATIVE
    focus_row(context, 'fixture-primary-a')
    INPUTS.key('Delete')
    assert dialog_absent(context), 'Primary unexpectedly opened a deletion dialog'
    def notice():
        candidates = [read_node(context, lambda label=label: button(context, label))
                      for label in ['Application notices', 'Application notices: success', 'Application errors']]
        candidates = [n for n in candidates if n is not None]
        assert len(candidates) <= 1
        return candidates[0] if candidates else None
    target = p.wait(notice)
    click(context, target, target.name)
    label = context['view'] + '-primary-delete-notice'
    checkpoint(context, label)
    rows = visible_words(context, label)
    literal_visible(context, label, 'The primary worktree cannot be deleted.', rows)
    names = [n.name for n in p.tree(context['pid']) if p.visible(n)]
    p.put(p.OUT / (context['view'] + '-primary-notice-names.json'), dict(actual_visible_names=names,
        expected_message='The primary worktree cannot be deleted.',
        limits='Inspect actual notice pixels; name export alone is not readability/UX approval'))
    # Toggle the exact same actual control closed, without changing focus by key.
    target = p.wait(notice)
    click(context, target, target.name)
    assert dialog_absent(context)


def terminal_processes(context):
    p = NATIVE
    result = {}
    expected_exe = Path('/bin/sleep').resolve(strict=True)
    for row in p.live(p.ACTIVE):
        proc = Path('/proc') / str(row['pid'])
        try:
            if (proc / 'exe').resolve(strict=True) != expected_exe:
                continue
            cwd = (proc / 'cwd').resolve(strict=True)
        except (FileNotFoundError, ProcessLookupError):
            continue
        roles = [role for role, path in context['terminal_scopes'].items() if cwd == path]
        assert len(roles) <= 1
        if roles:
            assert roles[0] not in result, 'Two actual terminal programs in one scope'
            result[roles[0]] = p.identity(row['pid'], expected_exe)
    return result


def conservation(context, label):
    p = NATIVE
    records = context['prep'].retain_fixture_and_databases(
        p, context['fixture'], context['app_root'], label)
    listed = cli(context, ['terminal', 'list', '--all'], label + '-terminals')
    actual = {entry['id']: entry for entry in listed['terminals']}
    assert set(actual) == set(context['terminal_ids'].values())
    assert all(entry['state'] == 'running' for entry in actual.values())
    shows = {role: cli(context, ['terminal', 'show', terminal_id], label + '-show-' + role)
             for role, terminal_id in context['terminal_ids'].items()}
    for role, shown in shows.items():
        session = shown['terminal']['session']
        assert session['id'] == context['terminal_ids'][role]
        assert session['launch']['working_directory'] == str(context['terminal_scopes'][role])
        assert shown['proof'] == listed['proof']
    processes = p.wait(lambda: (value if len(value := terminal_processes(context)) == 4 else None))
    identities = {}
    for relative in ['A', 'B', 'outer', 'outer/.worktrees/foreign', 'own', 'same-parent',
                     'same-parent/.worktrees/same-child', 'absent']:
        info = (context['fixture'] / relative).stat()
        identities[relative] = [info.st_dev, info.st_ino]
    value = dict(records=records, terminal_list=listed, terminal_shows=shows,
                 processes=processes, directory_identities=identities,
                 projects=cli(context, ['project', 'list'], label + '-projects'),
                 channels=cli(context, ['channel', 'list'], label + '-channels'))
    p.put(p.OUT / (label + '-scope-conservation.json'), value)
    return value


def unchanged(context, before, after):
    context['prep'].assert_foreign_bytes_and_registration_kept(before['records'], after['records'])
    assert before['records']['git'] == after['records']['git']
    assert before['directory_identities'] == after['directory_identities']
    assert before['processes'] == after['processes']
    assert before['terminal_list']['proof'] == after['terminal_list']['proof']
    assert {r['id'] for r in before['terminal_list']['terminals']} == {
        r['id'] for r in after['terminal_list']['terminals']}
    for role in before['terminal_shows']:
        for key in ['id', 'launch', 'continuity']:
            assert before['terminal_shows'][role]['terminal']['session'][key] == after[
                'terminal_shows'][role]['terminal']['session'][key]


def launch(context):
    p = NATIVE
    log = p.OUT / 'after-ui.log'
    version_log = p.OUT / 'build-ui-version.log'
    # Supported UI argument forwarding, no invented build-only flag or source
    # hook. The unchanged set-e runner builds CLI/HOST/UI separately, then the
    # actual UI --version exits without starting a native window.
    p.cmd([context['after'] / 'scripts/run-development', '--browserless', '--debug',
           '--fresh-daemon', '--log-file', version_log, '--', '--version'],
          cwd=context['after'], env=context['env'], timeout=context['seconds'])
    assert not p.owned_window(context['ui_binary']), 'Version-only build unexpectedly opened a window'
    products = {}
    for name in ['cagents', 'cagentsd', 'cagents-ui']:
        path = p.TARGET / 'dev-runtime' / name
        with path.open('rb') as stream:
            assert stream.read(4) == b'\x7fELF'
        version = p.cmd([path, '--version'], env=context['env']).strip()
        assert 'local.' + AFTER[:7] in version
        products[name] = dict(path=str(path), sha256=p.digest(path), version=version)
    p.put(p.OUT / 'matched-products-before-ui.json', dict(products=products,
        runner_source_sha256=p.digest(context['after'] / 'scripts/run-development'),
        evidence='Actual unchanged set-e runner exit0 after three isolated development builds; original command/logs retained.',
        source_head=AFTER, source_tree=AFTER_TREE, native_window_started=False,
        graphs=['development CLI', 'development,host cagentsd', 'development,ui cagents-ui'],
        tests='Not run or inferred by matched compilation'))
    runner = p.spawn([context['after'] / 'scripts/run-development', '--browserless', '--debug',
                      '--fresh-daemon', '--log-file', log], cwd=context['after'], env=context['env'],
                     stdout=(p.OUT / 'runner.log').open('wb'), stderr=subprocess.STDOUT)
    context['runner'] = runner
    def ready():
        context['last_runner_exit'] = p.poll_owned(runner)
        assert context['last_runner_exit'] is None, 'Runner exited; original output retained, no retry'
        return p.owned_window(context['ui_binary'])
    context['pid'], context['xid'] = p.wait(ready, timeout=context['seconds'], interval=2)
    context['owner'] = owner(context)
    after_start = {}
    for name in ['cagents', 'cagentsd', 'cagents-ui']:
        path = p.TARGET / 'dev-runtime' / name
        version = p.cmd([path, '--version'], env=context['env']).strip()
        after_start[name] = dict(path=str(path), sha256=p.digest(path), version=version)
    assert after_start == products, 'Matched ELF bytes changed before native capture'
    p.put(p.OUT / 'matched-products.json', dict(before_ui=products, after_start=after_start,
        second_runner='Ordinary supported runner, serial incremental use of the same granted target; no second queue/job'))
    p.viewport(context['xid'], context['manifest']['viewports'][0]['size'])
    context.update(size=context['manifest']['viewports'][0]['size'], view='wide')
    checkpoint(context, 'after-development-runner-bootstrap')
    observed = p.renderer(log, context['owner'])
    p.put(p.OUT / 'after-renderer.json', observed)
    assert observed['observed_renderer']['status'] == 'OBSERVED_owned_log', 'Renderer remains UNMEASURED'
    assert observed['observed_renderer']['backend'] == 'Gl', 'Actual renderer is not requested GL'
    # Starting the same private matched daemon is a supported real User action,
    # needed when an otherwise empty UI has not yet started its host.
    raw = p.cmd([context['binary'], '--json', 'daemon', 'start'], env=context['env'], timeout=90)
    started = json.loads(raw)
    p.put(p.OUT / 'daemon-start.json', started)
    assert started['ok'] is True
    status = cli(context, ['daemon', 'status'], 'daemon-status')
    assert status['running'] and status['instance'] == 'development'
    assert Path(status['socket']).is_relative_to(Path(context['env']['XDG_RUNTIME_DIR']))
    matches = []
    for row in p.live(p.ACTIVE):
        try:
            if (Path('/proc') / str(row['pid']) / 'exe').resolve(strict=True) == (p.TARGET / 'dev-runtime/cagentsd').resolve():
                matches.append(p.identity(row['pid'], p.TARGET / 'dev-runtime/cagentsd', context['env']))
        except (FileNotFoundError, ProcessLookupError):
            continue
    assert len(matches) == 1
    context['daemon_owner'] = matches[0]
    p.put(p.OUT / 'daemon-owner.json', matches[0])


def collect(context):
    p = NATIVE
    launch(context)
    root = context['fixture']
    cli(context, ['project', 'open', root / 'A'], 'open-A')
    context['terminal_scopes'] = {'primary': root / 'A', 'outer': root / 'outer',
                                  'foreign': root / 'outer/.worktrees/foreign', 'own': root / 'own'}
    context['terminal_ids'] = {}
    for role, path in context['terminal_scopes'].items():
        value = cli(context, ['terminal', 'spawn', '--cwd', path, '--', '/bin/sleep', '86400'], 'spawn-' + role)
        context['terminal_ids'][role] = value['terminal']['session']['id']
    p.put(p.OUT / 'fixture-terminal-ids.json', context['terminal_ids'])
    assert len(set(context['terminal_ids'].values())) == 4
    initial = conservation(context, 'seeded-unopened-B')
    assert len(initial['projects']['projects']) == 1
    for view in context['manifest']['viewports']:
        context.update(view=view['name'], size=view['size'])
        p.viewport(context['xid'], view['size'])
        menu_probe(context, 'fixture-primary-a', primary=True)
        primary_delete(context)
        menu_probe(context, 'outer')
        cancel_dialog(context, 'outer', blocked=False)
        cancel_dialog(context, 'own', blocked=False)
        cancel_dialog(context, 'same-parent', blocked=True)
        state = conservation(context, view['name'] + '-unopened-B-kept')
        unchanged(context, initial, state)
    cli(context, ['project', 'open', root / 'B'], 'open-B')
    known = conservation(context, 'seeded-loaded-B')
    unchanged(context, initial, known)
    assert len(known['projects']['projects']) == 2
    for view in context['manifest']['viewports']:
        context.update(view=view['name'], size=view['size'])
        p.viewport(context['xid'], view['size'])
        menu_probe(context, 'foreign')
        cancel_dialog(context, 'foreign', blocked=False)
        cancel_dialog(context, 'outer', blocked=True)
        # Existing target is observed without removing/parking it. G1 remains
        # explicitly missing, not "resolved" through a fabricated stale row.
        cancel_dialog(context, 'absent', blocked=False)
        state = conservation(context, view['name'] + '-loaded-B-kept')
        unchanged(context, known, state)
    context['final_conservation'] = conservation(context, 'final-before-cleanup')
    unchanged(context, initial, context['final_conservation'])
    p.put(p.OUT / 'G1-unexercised-recovery.json', dict(status='UNQUALIFIED',
        reason='No directory removal, parking, replacement, refresh race or confirmation in this bounded probe.',
        observed='Valid existing absent-named target dialog opened and cancelled at both viewports.',
        source_boundary='UI requires exact current sidebar row; backend exact-prunable absence contract alone does not establish native reachability.',
        full_suite='G3 destructive/buffer/absent-replacement cases remain held until probe mapping is independently reviewed.'))


def close_probe_inputs(inputs):
    """Release exact held inputs without the parent's broad catch/finally.

    Any cancellation escapes immediately, before a later release or Xlib close.
    The original reusable primitive bodies remain immutable.
    """
    NATIVE.CRITICAL = False
    for kind, values in [('key', inputs.keys), ('button', inputs.buttons)]:
        for value in list(values):
            NATIVE.require_private_desktop()
            inputs.event(kind, value, False, 'cleanup')
    assert not inputs.keys and not inputs.buttons, 'Held input cleanup incomplete'
    NATIVE.require_private_desktop()
    inputs.x.XCloseDisplay(inputs.display)


def close_probe_screen(screen):
    """Sequential observer shutdown; no native operation in an abort finally."""
    screen.checked(True)
    NATIVE.require_private_desktop()
    screen.x.XCloseDisplay(screen.display)
    NATIVE.require_private_desktop()
    screen.x.XSetErrorHandler(screen.previous)


def cleanup(context):
    p = NATIVE
    # Keep this same list at outer handback even when cancellation prevents a
    # normal return. Earlier ordinary cleanup failures must not be lost.
    errors = context.setdefault('cleanup_errors', [])
    # Explicit disposal occurs after final conservation, only for four actually
    # issued synthetic IDs. No project/worktree close/remove command exists.
    if context.get('terminal_ids') and context.get('binary'):
        for role, terminal_id in context['terminal_ids'].items():
            try:
                shown = cli(context, ['terminal', 'show', terminal_id], 'cleanup-show-' + role)
                assert shown['terminal']['session']['id'] == terminal_id
                assert shown['terminal']['session']['launch']['working_directory'] == str(context['terminal_scopes'][role])
                cli(context, ['terminal', 'close', terminal_id], 'cleanup-close-' + role)
            except Exception as error:
                errors.append(dict(stage='terminal-' + role, error=str(error)))
                break
    try:
        if context.get('pid') and context.get('runner') and p.poll_owned(context['runner']) is None:
            owner(context)
            INPUTS.request_close(context['xid'])
            p.wait(lambda: p.poll_owned(context['runner']) is not None, timeout=15)
            assert p.reap_owned(context['runner']) == 0
        if context.get('daemon_owner'):
            current = p.identity(context['daemon_owner']['pid'], p.TARGET / 'dev-runtime/cagentsd', context['env'])
            assert current == context['daemon_owner']
            remaining = cli(context, ['terminal', 'list', '--all'], 'cleanup-inventory')
            assert remaining['terminals'] == []
            cli(context, ['daemon', 'stop', '--require-empty'], 'cleanup-daemon-stop')
    except Exception as error:
        errors.append(dict(stage='normal-app-host-close', error=str(error)))
    try:
        if INPUTS:
            close_probe_inputs(INPUTS)
        if p.SCREEN:
            close_probe_screen(p.SCREEN)
        context['cleanup_observer_status'] = 'CLOSED'
    except Exception as error:
        errors.append(dict(stage='native-observer-close', error=str(error)))
    try:
        # Inspect immutable source while owned endpoint servers still exist;
        # all Git children use the same supervised command/deadline contract.
        source = {path: p.digest(context['after'] / path)
                  for path in context['manifest']['product_source_sha256']}
        assert source == context['manifest']['product_source_sha256']
        assert p.git(context['after'], 'rev-parse', 'HEAD') == AFTER
        assert p.git(context['after'], 'status', '--porcelain=v1', '--untracked-files=all') == ''
        assert p.git(context['control'], 'rev-parse', 'HEAD') == context['head']
        assert p.git(context['control'], 'status', '--porcelain=v1', '--untracked-files=all') == ''
        assert {path: p.digest(context['control'] / path) for path in context['hashes']} == context['hashes']
        p.put(p.OUT / 'final-source-conservation.json', dict(status='UNCHANGED', product=source,
            control_head=context['head'], source_sha256=context['hashes']))
    except Exception as error:
        errors.append(dict(stage='final-source-conservation', error=str(error)))
    try:
        # Only descendant PID/start identities retained by the original table.
        # Close endpoint servers only after input/screen handles and all source
        # commands. Later custody probes are budget-only, never desktop clients.
        application_residue = [r for r in p.live(p.ACTIVE) if r['pid'] in {
            context.get('pid'), context.get('daemon_owner', {}).get('pid'),
            *[v['pid'] for v in context.get('final_conservation', {}).get('processes', {}).values()]}]
        if application_residue:
            errors.append(dict(stage='application-residue', identities=application_residue))
        p.close_owned()
    except Exception as error:
        errors.append(dict(stage='owned-cleanup', error=str(error)))
    remaining = p.live()
    if remaining:
        errors.append(dict(stage='owned-processes-remain', identities=remaining))
    # The shared core alone validates direct-original waits or the separately
    # named positive descendant-exit proof. A descendant never claims its
    # natural parent's wait/status; descriptor closure is still mandatory.
    unsettled_pins = {key: value for key, value in p.PIDFDS.items()
                     if not value['closed'] or not p.PRIVATE_DESKTOP.pidfd_exited(p.OWNED[key])}
    if unsettled_pins or p.PIDFD_FAILURES:
        errors.append(dict(stage='original-pidfd-custody-retained', pins=unsettled_pins,
            admission_or_release_failures=list(p.PIDFD_FAILURES)))
    context['cleanup_process_status'] = (
        'OBSERVED_NO_OWNED_PROCESSES' if not remaining else 'OWNED_PROCESSES_REMAIN')
    context['cleanup_physical_status'] = ('OBSERVED_DISPOSED' if not remaining and
        context.get('cleanup_observer_status') == 'CLOSED' and
        not p.CALLER_STATE['unconfirmed'] and not unsettled_pins and
        not p.PIDFD_FAILURES else 'UNCONFIRMED')
    p.put(p.OUT / 'cleanup.json', dict(errors=errors, live_owned=remaining,
        physical_status=context['cleanup_physical_status'],
        process_status=context['cleanup_process_status'],
        observer_status=context.get('cleanup_observer_status', 'UNCONFIRMED'),
        retained_pidfds=unsettled_pins, pidfd_failures=p.PIDFD_FAILURES,
        retained_fixture=str(context.get('fixture')), directories_removed=False))
    return errors


def cleanup_handback(context):
    """One outer abort handler; after cancellation only memory bookkeeping.

    QualificationCancelled inherits BaseException. Each cleanup stage catches
    only Exception, so the single-shot alarm cannot be swallowed and followed
    by more input, CLI, waits, forced cleanup or source inspection.
    """
    errors = context.setdefault('cleanup_errors', [])
    context['cleanup_physical_status'] = 'UNCONFIRMED'
    context['cleanup_process_status'] = 'UNCONFIRMED'
    context['cleanup_observer_status'] = 'UNCONFIRMED'
    try:
        cleanup(context)
    except BaseException as error:
        context['cleanup_physical_status'] = 'UNCONFIRMED'
        errors.append(dict(stage='cleanup-aborted', type=type(error).__name__,
            error=str(error), signal=NATIVE.CANCEL_SIGNAL))
    return errors


def probe_status(collection_failed, cleanup_errors, physical_status):
    if collection_failed or cleanup_errors or physical_status != 'OBSERVED_DISPOSED':
        return 'FAILED_UNQUALIFIED'
    return 'AFTER_ONLY_CANCEL_PROBE_OBSERVED_REVIEW_PENDING'


def main():
    global NATIVE, CONTEXT, INPUTS
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['control', 'after', 'evidence', 'review', 'ec-lease']:
        parser.add_argument('--' + name, type=Path, required=True)
    for name in ['review-sha256', 'ec-lease-sha256']:
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--ui-checks', type=Path)
    parser.add_argument('--ui-checks-sha256')
    context = gates(parser.parse_args())
    context.update(completed_captures=[], terminal_ids={})
    CONTEXT = context
    out = context['out']
    out.mkdir(mode=0o700)
    for name, raw in context['original_receipts'].items():
        (out / name).write_bytes(raw)
    native = load_module('worktree_delete_native_primitives', context['control'] /
                         '.github/qualification/native-worktree-delete-primitives.py')
    prep = load_module('worktree_delete_fixture_source', context['control'] /
                       '.github/qualification/native-worktree-delete-driver.py')
    parent_manifest = json.loads((context['control'] / 'research/worktree-delete-native/manifest.json').read_text())
    native.configure(parent_manifest, out, context['target'])
    NATIVE, context['prep'], native.ACTIVE = native, prep, 'after-probe'
    desktop = load_module('worktree_delete_private_desktop', context['control'] /
                          context['manifest']['private_desktop_correction']['capsule_path'])
    runtime = load_module('worktree_delete_runtime', context['control'] /
                         '.github/qualification/native-worktree-delete-runtime.py')
    runtime.bind(native, desktop, context)
    native.guard()
    native.put(out / 'gate-bindings.json', dict(control_head=context['head'], control_tree=context['tree'],
        product=dict(head=AFTER, tree=AFTER_TREE), source_sha256=context['hashes'],
        grant_id=context['lease']['grant_id'], collection_seconds=context['seconds'],
        requested_renderer=native.REQUESTED_RENDERER,
        policy='Natural hosted User; HOME/CODEX_HOME/inherited opaque authority retained; no provider'))
    assert ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0
    signal.signal(signal.SIGTERM, native.cancelled)
    signal.signal(signal.SIGINT, native.cancelled)
    signal.signal(signal.SIGALRM, native.cancelled)
    signal.setitimer(signal.ITIMER_REAL, context['seconds'])
    failed, error_record, cleanup_errors = True, None, []
    try:
        for tool in ['xdotool', 'xdpyinfo', 'import', 'openbox', 'gdbus', 'tesseract']:
            assert shutil.which(tool), 'Required observation tool absent: ' + tool
        desktop_root = out / 'owned-desktop'
        desktop_root.mkdir(mode=0o700)
        native.bootstrap(desktop_root)
        native.require_private_desktop()
        display = native.cmd(['xdpyinfo'])
        assert re.search(r'dimensions:\s+1600x1000 pixels', display)
        assert re.search(r'resolution:\s+96x96 dots per inch', display)
        context['fixture'] = prep.create_fixture(native)
        native.require_private_desktop()
        import pyatspi
        from gi.repository import GLib
        from PIL import Image
        native.pyatspi, native.GLib, native.Image = pyatspi, GLib, Image
        native.SCREEN, INPUTS = native.NativeScreen(), native.Inputs()
        app_root = context['fixture'] / 'application'
        app_root.mkdir(mode=0o700)
        context['app_root'] = app_root
        env = native.private_env(app_root)
        env['CARGO_BUILD_JOBS'] = str(context['lease']['cargo_jobs'])
        settings = Path(env['XDG_CONFIG_HOME']) / 'cagents-development/settings.toml'
        settings.parent.mkdir(mode=0o700)
        settings.write_text(parent_manifest['settings'] + 'auto_delete_merged_worktrees = false\n')
        context.update(env=env, binary=native.TARGET / 'dev-runtime/cagents',
                       ui_binary=native.TARGET / 'dev-runtime/cagents-ui')
        collect(context)
        failed = False
    except BaseException as error:
        error_record = dict(type=type(error).__name__, error=str(error),
                            last_command=context.get('last_command'),
                            original_native_command=native.COMMANDS[-1] if native.COMMANDS else None,
                            runner_exit=context.get('last_runner_exit'),
                            runner_log=str(out / 'runner.log'), ui_log=str(out / 'after-ui.log'),
                            monotonic=time.monotonic())
        native.put(out / 'first-failure.json', error_record)
        last_exit = native.COMMANDS[-1].get('exit') if native.COMMANDS else 0
        if (context.get('pid') and context.get('size') and native.SCREEN and
                native.CANCEL_SIGNAL is None and last_exit == 0 and
                context.get('runner') and time.monotonic() < context['work_deadline']):
            # One read-only failure frame, within the original still-active
            # budget. No action replay, and no follow-on command after an
            # actual nonzero/unfinished command, cancellation or runner exit.
            try:
                checkpoint(context, 'after-probe-first-failure')
            except BaseException as capture_error:
                native.put(out / 'failure-capture-error.json', dict(
                    type=type(capture_error).__name__, error=str(capture_error)))
    finally:
        # Collection never gets more time. Only bounded cleanup uses the
        # separately pre-granted allowance, including after the first failure.
        signal.setitimer(signal.ITIMER_REAL, 0)
        try:
            cleanup_remaining = native.begin_probe_cleanup()
            signal.setitimer(signal.ITIMER_REAL, cleanup_remaining)
            cleanup_errors = cleanup_handback(context)
        except BaseException as cleanup_error:
            context['cleanup_physical_status'] = 'UNCONFIRMED'
            cleanup_errors = context.setdefault('cleanup_errors', [])
            cleanup_errors.append(dict(stage='cleanup-entry-aborted', type=type(cleanup_error).__name__,
                error=str(cleanup_error)))
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
        native.put(out / 'inputs.json', native.EVENTS)
        native.put(out / 'commands.json', native.COMMANDS)
        native.put(out / 'owned-processes.json', native.OWNED)
        native.put(out / 'pidfds.json', dict(pins=native.PIDFDS, failures=native.PIDFD_FAILURES))
        native.put(out / 'result.json', dict(
            status=probe_status(failed, cleanup_errors, context.get('cleanup_physical_status', 'UNCONFIRMED')),
            product_head=AFTER, control_head=context['head'], captures=context['completed_captures'],
            first_failure=error_record, cleanup_errors=cleanup_errors,
            cleanup_physical_status=context.get('cleanup_physical_status', 'UNCONFIRMED'),
            cleanup_process_status=context.get('cleanup_process_status', 'UNCONFIRMED'),
            cleanup_observer_status=context.get('cleanup_observer_status', 'UNCONFIRMED'),
            caller_custody=dict(native.CALLER_STATE),
            G1='UNQUALIFIED_NO_ABSENCE_OR_REPLACEMENT_PROBE',
            G3='Full eight-case Before/After suite remains held; no causal/destructive/buffer/recovery evidence',
            native_scale_factor='UNMEASURED; physical viewport and configured scale retained separately',
            screenshots='Artifacts only, no screenshot branch/publication or UX approval'))
        (out / 'sha256.txt').write_text(''.join(
            digest(path) + '  ' + str(path.relative_to(out)) + '\n'
            for path in sorted(out.rglob('*')) if path.is_file() and path.name != 'sha256.txt'))
    if probe_status(failed, cleanup_errors, context.get('cleanup_physical_status', 'UNCONFIRMED')) == 'FAILED_UNQUALIFIED':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
