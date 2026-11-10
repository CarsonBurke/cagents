#!/usr/bin/python3
"""Create a permanent startup refusal in an isolated native-evidence fixture.

The caller must first retire and reap its synthetic Claude actor normally.
This helper changes only that retired actor's recorded recipe, not live state.
Version 4 recognizes schema 22 shared views and schema 23 normalized panes
with descriptor format 1. --restore undoes only this helper's recorded CAS.
--self-test uses invented in-memory SQLite, never an app or provider home.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import uuid

VERSION = 4
NATIVE_TABLES = {'native_actor_ownerships', 'native_actor_retirements'}


def columns(database, table):
    return {row[1] for row in database.execute(f'PRAGMA table_info({table})')}


def require_columns(database, table, names):
    if not set(names) <= columns(database, table):
        raise ValueError(f'Unrecognized {table} columns')


def selected_card(database, thread):
    """One current committed pane, never a newest-row or stale-projection guess."""
    version = database.execute('PRAGMA user_version').fetchone()[0]
    tables = {row[0] for row in database.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    if not NATIVE_TABLES <= tables:
        return None
    cards = []
    if version == 23:
        if not {'view_scopes', 'panes'} <= tables or 'shared_views' in tables:
            raise ValueError('Unrecognized schema 23 normalized view layout')
        require_columns(database, 'view_scopes', ['id', 'revision'])
        require_columns(database, 'panes', ['id', 'scope', 'kind', 'resource', 'provider', 'provider_session', 'descriptor_format', 'descriptor'])
        query = '''SELECT p.id,p.scope,p.kind,p.resource,p.provider,p.provider_session,
                          p.descriptor_format,p.descriptor,s.id,s.revision
                   FROM panes p LEFT JOIN view_scopes s ON s.id=p.scope ORDER BY p.id'''
        for pane, scope, kind, resource, provider, session, format_, encoded, joined_scope, revision in database.execute(query):
            uuid.UUID(pane)
            if joined_scope is None or format_ != 1:
                raise ValueError('Unknown pane scope or descriptor format')
            descriptor = json.loads(encoded)
            if descriptor.get('kind') != kind:
                raise ValueError('Pane kind disagrees with its descriptor')
            if kind in {'chat', 'terminal', 'browser'}:
                uuid.UUID(descriptor['id'])
                if resource != descriptor['id']:
                    raise ValueError('Pane resource disagrees with its descriptor')
            elif kind in {'pull_requests', 'repository'}:
                if resource is not None:
                    raise ValueError('Non-resource pane has an indexed resource')
            else:
                raise ValueError('Unknown pane kind')
            if kind == 'chat':
                launch = descriptor['launch']
                if provider != launch['provider'] or session != launch['session_id']:
                    raise ValueError('Pane provider indexes disagree with its descriptor')
                if provider == 'Claude' and session == thread:
                    cards.append({'schema': 23, 'pane': pane, 'scope': scope, 'revision': revision, 'descriptor': descriptor})
    elif version == 22:
        if not {'shared_views', 'shared_view_scopes', 'shared_view_panes'} <= tables or 'panes' in tables:
            raise ValueError('Unrecognized schema 22 shared view layout')
        require_columns(database, 'shared_views', ['scope', 'revision', 'panes'])
        require_columns(database, 'shared_view_scopes', ['scope'])
        require_columns(database, 'shared_view_panes', ['id', 'scope'])
        query = '''SELECT v.scope,v.revision,v.panes,s.scope FROM shared_views v
                   LEFT JOIN shared_view_scopes s ON s.scope=v.scope ORDER BY v.scope'''
        for scope, revision, encoded, joined_scope in database.execute(query):
            if joined_scope is None:
                raise ValueError('Saved shared view lacks its registered scope')
            panes = json.loads(encoded)
            if not isinstance(panes, list):
                raise ValueError('Shared view panes are not an array')
            for pane in panes:
                descriptor = pane['kind']
                launch = descriptor.get('launch', {})
                if descriptor.get('kind') != 'chat' or launch.get('provider') != 'Claude' or launch.get('session_id') != thread:
                    continue
                uuid.UUID(pane['id'])
                uuid.UUID(descriptor['id'])
                owner = database.execute('SELECT scope FROM shared_view_panes WHERE id=?', (pane['id'],)).fetchone()
                if owner is None:
                    raise ValueError('Selected legacy pane lacks its committed ownership index')
                if owner[0] != scope:
                    continue  # Its former scope's retained projection is not the committed card.
                cards.append({'schema': 22, 'pane': pane['id'], 'scope': scope, 'revision': revision, 'descriptor': descriptor})
    else:
        raise ValueError(f'Unrecognized model schema {version}; refusing to infer a card')
    if len(cards) > 1:
        raise ValueError(f'Ambiguous committed Claude cards for the exact thread: {len(cards)}')
    return cards[0] if cards else None


def owned_regular(path, private=False):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError('Expected an owned regular file: ' + str(path))
    if stat.S_IMODE(info.st_mode) & (0o077 if private else 0o022):
        raise ValueError('Unexpected file permissions: ' + str(path))
    return {'device': info.st_dev, 'inode': info.st_ino,
            'owner': info.st_uid, 'mode': stat.S_IMODE(info.st_mode)}


def fixture_root(root):
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        raise ValueError('A genuine native-evidence User environment is required')
    if root.is_symlink():
        raise ValueError('Refusing a symlink fixture')
    root = root.resolve()
    info = root.stat()
    if root.name != 'fixture' or info.st_uid != os.getuid() or not stat.S_ISDIR(info.st_mode):
        raise ValueError('Refusing a non-owned fixture path')
    owned_regular(root.parent / '.native-evidence')
    return root


def write_private(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        json.dump(value, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def restore_record(database, record):
    """Pure SQLite boundary, also exercised against invented rows below."""
    if record['helper_version'] != VERSION:
        raise ValueError('Only a fault recorded by this helper version can be restored')
    selected = record['selected']
    thread = record['thread']
    original = record['original']
    faulted = record['faulted']
    ownership = json.loads(original)
    body = ownership.get('pending', ownership)
    source = body['source']
    card = selected['descriptor']
    if source['id'] != card['id'] or source['launch'] != card['launch'] or source['proof'] != card['proof']:
        raise ValueError('Recorded original ownership does not match the exact committed card')
    receipt = json.loads(record['retirement'])
    if 'pending' in receipt or receipt['confirmed_session'] != thread:
        raise ValueError('Recorded retirement does not confirm the exact thread')
    body['source']['launch']['recipe']['arguments'].append('--fixture-recipe-mismatch')
    if json.dumps(ownership, separators=(',', ':')) != faulted:
        raise ValueError("Recorded fault is not this helper's exact argument append")
    database.execute('BEGIN IMMEDIATE')
    try:
        if selected_card(database, thread) != selected:
            raise ValueError('The exact committed pane changed since fault injection')
        retirement = database.execute('SELECT receipt FROM native_actor_retirements WHERE actor=?', (record['actor'],)).fetchall()
        if retirement != [(record['retirement'],)]:
            raise ValueError('The exact confirmed retirement changed since fault injection')
        current = database.execute('SELECT ownership FROM native_actor_ownerships WHERE actor=?', (record['actor'],)).fetchall()
        if current != [(faulted,)]:
            raise ValueError('The exact faulted ownership changed; refusing a stale or foreign restore')
        cursor = database.execute('UPDATE native_actor_ownerships SET ownership=? WHERE actor=? AND ownership=?',
                                  (original, record['actor'], faulted))
        if cursor.rowcount != 1:
            raise ValueError('The exact faulted ownership changed concurrently')
        database.commit()
    except BaseException:
        database.rollback()
        raise


def restore(root, thread):
    root = fixture_root(root)
    backup = root / 'original-retired-ownership.json'
    owned_regular(backup, private=True)
    flags = os.O_RDONLY | os.O_NOFOLLOW
    descriptor = os.open(backup, flags)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise ValueError('The recorded fault backup has an unexpected owner or mode')
        record = json.load(stream)
    if record['helper_sha256'] != hashlib.sha256(Path(__file__).read_bytes()).hexdigest():
        raise ValueError('The fault was recorded by another helper revision')
    if record['thread'] != thread or record['root'] != str(root):
        raise ValueError('The recorded fault belongs to another fixture or thread')
    path = Path(record['database'])
    if path.is_symlink() or not path.is_relative_to(root / 'data') or path.resolve() != path:
        raise ValueError('The recorded database is outside this fixture data directory')
    if owned_regular(path) != record['database_identity']:
        raise ValueError('The recorded database inode, owner or mode changed')
    report_path = root / 'restored-retired-ownership.json'
    if report_path.exists() or report_path.is_symlink():
        raise ValueError('This fixture already has a restore receipt')
    with closing(sqlite3.connect(path, timeout=2)) as database, database:
        restore_record(database, record)
    report = {'helper_version': VERSION, 'helper_sha256': record['helper_sha256'],
              'database': str(path), 'actor': record['actor'], 'thread': thread,
              'schema': record['selected']['schema'], 'pane': record['selected']['pane'],
              'restored_sha256': hashlib.sha256(record['original'].encode()).hexdigest(),
              'scope': 'Restored only the exact original ownership row recorded by this helper; no provider request'}
    write_private(report_path, report)
    return report


def inject(root, thread):
    root = fixture_root(root)
    found = []
    for path in sorted((root / 'data').rglob('*')):
        if not path.is_file() or path.is_symlink():
            continue
        with path.open('rb') as stream:
            if stream.read(16) != b'SQLite format 3\x00':
                continue
        with closing(sqlite3.connect(path, timeout=2)) as database, database:
            selected = selected_card(database, thread)
            if selected is None:
                continue
            card = selected['descriptor']
            for actor, encoded in database.execute('SELECT actor,ownership FROM native_actor_ownerships'):
                ownership = json.loads(encoded)
                body = ownership.get('pending', ownership)
                source = body['source']
                if source['id'] != card['id']:
                    continue
                if source['launch'] != card['launch'] or source['proof'] != card['proof']:
                    raise ValueError('The selected card and its retired ownership already disagree')
                receipt_row = database.execute('SELECT receipt FROM native_actor_retirements WHERE actor=?', (actor,)).fetchone()
                if receipt_row is None:
                    continue
                receipt = json.loads(receipt_row[0])
                if 'pending' in receipt or receipt['confirmed_session'] != thread:
                    continue
                found.append((path, actor, encoded, ownership, source['id'], selected, receipt_row[0]))
    if len(found) != 1:
        raise ValueError(f'Expected one retired actor for the invented thread; found {len(found)}')
    path, actor, original, ownership, source_id, selected, retirement = found[0]
    body = ownership.get('pending', ownership)
    arguments = body['source']['launch']['recipe']['arguments']
    before = list(arguments)
    arguments.append('--fixture-recipe-mismatch')
    changed = json.dumps(ownership, separators=(',', ':'))
    backup = root / 'original-retired-ownership.json'
    record = {'helper_version': VERSION, 'helper_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'root': str(root), 'database': str(path), 'database_identity': owned_regular(path),
              'actor': actor, 'thread': thread, 'selected': selected, 'retirement': retirement,
              'original': original, 'faulted': changed}
    write_private(backup, record)
    with closing(sqlite3.connect(path, timeout=2)) as database, database:
        database.execute('BEGIN IMMEDIATE')
        if selected_card(database, thread) != selected:
            raise ValueError('The exact committed pane changed before fault injection')
        if database.execute('SELECT receipt FROM native_actor_retirements WHERE actor=?', (actor,)).fetchall() != [(retirement,)]:
            raise ValueError('The exact retirement changed before fault injection')
        cursor = database.execute('UPDATE native_actor_ownerships SET ownership=? WHERE actor=? AND ownership=?', (changed, actor, original))
        if cursor.rowcount != 1:
            raise ValueError('The exact retired ownership changed concurrently; no second row was touched')
    report = {'helper_version': VERSION, 'helper_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'schema_version': selected['schema'], 'pane_id': selected['pane'],
              'scope': selected['scope'], 'view_revision': selected['revision'],
              'database': str(path), 'actor': actor, 'source_id': source_id, 'thread_id': thread,
              'field': 'ownership.source.launch.recipe.arguments', 'before': before, 'after': arguments,
              'before_sha256': hashlib.sha256(original.encode()).hexdigest(),
              'after_sha256': hashlib.sha256(changed.encode()).hexdigest(),
              'intended_guard': 'validate_retired_identity: saved actor has no matching completed retirement; no provider start'}
    (root / 'permanent-mismatch.json').write_text(json.dumps(report, indent=2) + '\n')
    return report


def self_test():
    thread = '33333333-4444-4aaa-8bbb-cccccccccccc'
    pane = '11111111-1111-4111-8111-111111111111'
    source = '22222222-2222-4222-8222-222222222222'
    descriptor = {'kind': 'chat', 'id': source, 'launch': {'provider': 'Claude', 'session_id': thread, 'recipe': {'arguments': []}}, 'proof': {'fixture': True}}
    count = 0
    def database(version):
        db = sqlite3.connect(':memory:')
        db.executescript('CREATE TABLE native_actor_ownerships(actor TEXT,ownership TEXT); CREATE TABLE native_actor_retirements(actor TEXT,receipt TEXT);')
        db.execute(f'PRAGMA user_version={version}')
        if version == 23:
            db.executescript('CREATE TABLE view_scopes(id INTEGER,revision INTEGER); CREATE TABLE panes(id TEXT,scope INTEGER,kind TEXT,resource TEXT,provider TEXT,provider_session TEXT,descriptor_format INTEGER,descriptor TEXT); INSERT INTO view_scopes VALUES(7,11);')
            db.execute('INSERT INTO panes VALUES(?,?,?,?,?,?,?,?)', (pane, 7, 'chat', source, 'Claude', thread, 1, json.dumps(descriptor)))
        else:
            db.executescript('CREATE TABLE shared_view_scopes(scope TEXT); CREATE TABLE shared_views(scope TEXT,revision INTEGER,panes TEXT); CREATE TABLE shared_view_panes(id TEXT,scope TEXT); INSERT INTO shared_view_scopes VALUES(\'current\');')
            db.execute('INSERT INTO shared_view_panes VALUES(?,?)', (pane, 'current'))
            db.execute('INSERT INTO shared_views VALUES(?,?,?)', ('current', 11, json.dumps([{'id': pane, 'kind': descriptor}])))
        return db
    for version in [22, 23]:
        with closing(database(version)) as db:
            chosen = selected_card(db, thread)
            assert chosen['pane'] == pane and chosen['descriptor'] == descriptor
            assert selected_card(db, str(uuid.uuid4())) is None
            count += 2
    for column, value in [('resource', 'wrong'), ('kind', 'terminal'), ('provider', 'Codex'), ('provider_session', 'wrong'), ('descriptor_format', 2), ('scope', 99)]:
        with closing(database(23)) as db:
            db.execute(f'UPDATE panes SET {column}=?', (value,))
            try:
                selected_card(db, thread)
            except ValueError:
                count += 1
            else:
                raise AssertionError(f'Accepted mismatched normalized {column}')
    with closing(database(23)) as db:
        other = dict(descriptor, id=str(uuid.uuid4()))
        db.execute('INSERT INTO panes VALUES(?,?,?,?,?,?,?,?)', (str(uuid.uuid4()), 7, 'chat', other['id'], 'Claude', thread, 1, json.dumps(other)))
        try:
            selected_card(db, thread)
        except ValueError:
            count += 1
        else:
            raise AssertionError('Accepted ambiguous normalized target')
    with closing(database(22)) as db:
        db.execute("INSERT INTO shared_view_scopes VALUES('former')")
        stale = dict(descriptor, proof={'fixture': 'stale'})
        db.execute('INSERT INTO shared_views VALUES(?,?,?)', ('former', 1, json.dumps([{'id': pane, 'kind': stale}])))
        assert selected_card(db, thread)['descriptor'] == descriptor
        count += 1
    with closing(database(22)) as db:
        other_pane = str(uuid.uuid4())
        other = dict(descriptor, id=str(uuid.uuid4()))
        db.execute('INSERT INTO shared_view_panes VALUES(?,?)', (other_pane, 'current'))
        db.execute('UPDATE shared_views SET panes=?', (json.dumps([{'id': pane, 'kind': descriptor}, {'id': other_pane, 'kind': other}]),))
        try:
            selected_card(db, thread)
        except ValueError:
            count += 1
        else:
            raise AssertionError('Accepted ambiguous legacy target')
    with closing(database(22)) as db:
        db.execute('DELETE FROM shared_view_panes')
        try:
            selected_card(db, thread)
        except ValueError:
            count += 1
        else:
            raise AssertionError('Accepted unindexed legacy card')
    for version in [0, 24]:
        with closing(database(version)) as db:
            try:
                selected_card(db, thread)
            except ValueError:
                count += 1
            else:
                raise AssertionError(f'Accepted unknown schema {version}')
    restore_count = 0
    def recorded_fault(db):
        selected = selected_card(db, thread)
        card = selected['descriptor']
        original = json.dumps({'source': {key: card[key] for key in ['id', 'launch', 'proof']}})
        changed = json.loads(original)
        changed['source']['launch']['recipe']['arguments'].append('--fixture-recipe-mismatch')
        faulted = json.dumps(changed, separators=(',', ':'))
        retirement = json.dumps({'confirmed_session': thread})
        actor = 'fixture-actor'
        db.execute('INSERT INTO native_actor_ownerships VALUES(?,?)', (actor, faulted))
        db.execute('INSERT INTO native_actor_ownerships VALUES(?,?)', ('unrelated', 'untouched'))
        db.execute('INSERT INTO native_actor_retirements VALUES(?,?)', (actor, retirement))
        db.commit()
        return {'helper_version': VERSION, 'selected': selected, 'thread': thread,
                'actor': actor, 'original': original, 'faulted': faulted, 'retirement': retirement}
    for version in [22, 23]:
        with closing(database(version)) as db:
            record = recorded_fault(db)
            restore_record(db, record)
            assert db.execute('SELECT actor,ownership FROM native_actor_ownerships ORDER BY actor').fetchall() == [('fixture-actor', record['original']), ('unrelated', 'untouched')]
            restore_count += 1
            try:
                restore_record(db, record)
            except ValueError:
                restore_count += 1
            else:
                raise AssertionError('Accepted a second restore of the same fault')
        for mismatch in ['ownership', 'retirement', 'card', 'thread', 'actor', 'version', 'recorded_fault']:
            with closing(database(version)) as db:
                record = recorded_fault(db)
                if mismatch == 'ownership':
                    db.execute('UPDATE native_actor_ownerships SET ownership=? WHERE actor=?', ('concurrent-owned-value', record['actor']))
                elif mismatch == 'retirement':
                    db.execute('UPDATE native_actor_retirements SET receipt=?', (json.dumps({'confirmed_session': 'another-thread'}),))
                elif mismatch == 'card':
                    if version == 23:
                        db.execute('UPDATE view_scopes SET revision=revision+1')
                    else:
                        db.execute('UPDATE shared_views SET revision=revision+1')
                elif mismatch == 'thread':
                    record['thread'] = 'another-thread'
                elif mismatch == 'actor':
                    record['actor'] = 'foreign-actor'
                elif mismatch == 'version':
                    record['helper_version'] = 3
                else:
                    record['faulted'] += ' '
                db.commit()
                before_owners = db.execute('SELECT actor,ownership FROM native_actor_ownerships ORDER BY actor').fetchall()
                before_receipts = db.execute('SELECT actor,receipt FROM native_actor_retirements ORDER BY actor').fetchall()
                try:
                    restore_record(db, record)
                except ValueError:
                    assert db.execute('SELECT actor,ownership FROM native_actor_ownerships ORDER BY actor').fetchall() == before_owners
                    assert db.execute('SELECT actor,receipt FROM native_actor_retirements ORDER BY actor').fetchall() == before_receipts
                    restore_count += 1
                else:
                    raise AssertionError(f'Accepted a {version} {mismatch} restore mismatch')
    return {'helper_version': VERSION, 'selection_checks_passed': count,
            'restore_checks_passed': restore_count, 'schemas': [22, 23],
            'scope': 'invented in-memory SQLite only; no app fixture or provider execution'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path, nargs='?')
    parser.add_argument('thread', nargs='?')
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--restore', action='store_true', help="Restore only this helper's exact recorded ownership fault")
    args = parser.parse_args()
    if args.self_test:
        print(json.dumps(self_test()))
        return
    if 'CAGENTS_AGENT_AUTHORITY' in os.environ:
        parser.exit(77, 'A genuine native-evidence User environment is required.\n')
    if args.root is None or args.thread is None:
        parser.error('root and exact thread are required')
    try:
        print(json.dumps(restore(args.root, args.thread) if args.restore else inject(args.root, args.thread)))
    except (OSError, ValueError, KeyError, TypeError, sqlite3.Error) as error:
        parser.exit(1, f'Cannot change retired fixture ownership fault: {error}\n')


if __name__ == '__main__':
    main()
