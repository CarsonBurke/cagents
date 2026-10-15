#!/usr/bin/env python3
"""Offline native UI fixture. No network or credential reads; durable Claude JSONL."""
import json, os, pathlib, re, sys, time, uuid
args = sys.argv[1:]
config = pathlib.Path(os.environ.get('CLAUDE_CONFIG_DIR', str(pathlib.Path.home() / '.claude')))
account_label = {'11111111-1111-4111-8111-111111111111': 'a', '22222222-2222-4222-8222-222222222222': 'b'}.get(config.name, 'system')
if '--version' in args:
    print('2.1.286 (native validation fixture)')
    sys.exit()
if args and args[0] == 'auth':
    config.mkdir(parents=True, exist_ok=True)
    marker = config / '.fixture-signed-out'
    if len(args) > 1 and args[1] == 'logout':
        marker.touch()
    elif len(args) > 1 and args[1] == 'login':
        marker.unlink(missing_ok=True)
    print(json.dumps({'loggedIn': not marker.exists(), 'email': account_label + '@fixture.invalid', 'subscriptionType': 'pro'}))
    sys.exit()
session = str(uuid.uuid4())
for i, arg in enumerate(args):
    if arg in ('--session-id', '--resume') and i + 1 < len(args):
        session = args[i + 1]
    elif arg.startswith(('--session-id=', '--resume=')):
        session = arg.split('=', 1)[1]
project = re.sub('[^a-zA-Z0-9]', '-', os.getcwd())
history = pathlib.Path(os.environ.get('CLAUDE_CONFIG_DIR', str(pathlib.Path.home() / '.claude'))) / 'projects' / project / (session + '.jsonl')
history.parent.mkdir(parents=True, exist_ok=True)
records = [json.loads(line) for line in history.read_text().splitlines()] if history.exists() else []
parent = records[-1].get('uuid') if records else None
turn = sum((r.get('type') == 'assistant' for r in records))
pending = None
pending_tool = None

def emit(frame, persist=False):
    global parent
    if persist:
        frame.update(sessionId=session, parentUuid=parent)
        with history.open('a') as out:
            out.write(json.dumps(frame) + '\n')
        parent = frame.get('uuid')
    print(json.dumps(frame), flush=True)

def assistant(content, message_id=None):
    emit({'type': 'assistant', 'uuid': str(uuid.uuid4()), 'message': {'id': message_id or str(uuid.uuid4()), 'role': 'assistant', 'content': content, 'usage': {'input_tokens': 4200, 'output_tokens': 180}}}, True)

def done():
    emit({'type': 'result', 'uuid': str(uuid.uuid4()), 'subtype': 'success', 'is_error': False, 'session_id': session})
for line in sys.stdin:
    try:
        frame = json.loads(line)
    except ValueError:
        continue
    kind = frame.get('type')
    with (pathlib.Path.home().parent / 'provider-input.jsonl').open('a') as log:
        log.write(json.dumps({'session': session, 'frame': frame}) + '\n')
    if kind == 'control_request':
        subtype = frame.get('request', {}).get('subtype')
        metadata = {'claude_code_version': '2.1.286', 'current_permission_mode': 'default', 'models': [{'value': 'haiku', 'displayName': 'Claude Haiku'}]} if subtype == 'initialize' else {}
        if subtype == 'get_usage':
            metadata = {'subscription_type': 'pro', 'rate_limits': {'limits': [{'kind': 'session', 'group': 'session', 'percent': 100 if (config / '.fixture-limited').exists() else 25, 'resets_at': int(time.time() + 3600), 'is_active': True}]}}
        emit({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': frame['request_id'], 'response': metadata}})
        if subtype == 'interrupt':
            if pending:
                emit({'type': 'control_cancel_request', 'request_id': pending})
            pending = None
            done()
    elif kind == 'control_response' and pending:
        denied = frame.get('response', {}).get('response', {}).get('behavior') == 'deny'
        emit({'type': 'user', 'uuid': str(uuid.uuid4()), 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': pending_tool, 'content': 'User denied the request' if denied else 'Allowed fixture command completed', 'is_error': denied}]}}, True)
        pending = None
        assistant([{'type': 'text', 'text': 'The approval response was received. The fixture is now idle.'}])
        done()
    elif kind == 'user':
        turn += 1
        emit(frame, True)
        emit({'type': 'command_lifecycle', 'command_uuid': frame.get('uuid'), 'state': 'started'})
        emit({'type': 'system', 'subtype': 'init', 'session_id': session, 'claude_code_version': '2.1.286', 'model': 'haiku'})
        content = frame.get('message', {}).get('content', '')
        if isinstance(content, list):
            content = ' '.join((v.get('text', '') for v in content))
        parts = content.strip().split()
        command = parts[0] if parts else 'normal'
        if command == 'perm':
            pending = 'native-approval-' + str(turn)
            pending_tool = 'tool-approval-' + str(turn)
            assistant([{'type': 'text', 'text': 'I will inspect the local project after this approval.'}])
            emit({'type': 'control_request', 'request_id': pending, 'request': {'subtype': 'can_use_tool', 'tool_name': 'Bash', 'tool_use_id': pending_tool, 'input': {'command': 'cargo test --locked --no-default-features', 'description': 'Run the narrow project checks'}}})
        elif command in ('tools', 'slow20'):
            count = int(parts[1]) if len(parts) > 1 else 20
            for n in range(count):
                identifier = 'tool-' + str(turn) + '-' + str(n)
                variant = n % 5
                path = str(pathlib.Path.cwd() / 'src/fixture_component.rs')
                if variant in (0, 4):
                    name = 'Bash'
                    tool_input = {'command': 'cargo test --lib module_' + str(n), 'description': 'Run tests for module ' + str(n)}
                elif variant == 1:
                    name = 'Read'
                    tool_input = {'file_path': path}
                elif variant == 2:
                    name = 'Grep'
                    tool_input = {'pattern': 'fixture_owner', 'path': str(pathlib.Path.cwd() / 'src')}
                else:
                    name = 'Edit'
                    tool_input = {'file_path': path, 'old_string': 'fn fixture() {\n    old();\n}\n', 'new_string': 'fn fixture() {\n    new();\n    validate();\n}\n'}
                assistant([{'type': 'tool_use', 'id': identifier, 'name': name, 'input': tool_input}])
                emit({'type': 'user', 'uuid': str(uuid.uuid4()), 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': identifier, 'content': 'test fixture_case_' + str(n) + ' ... ' + ('FAILED' if variant == 4 else 'ok'), 'is_error': variant == 4}]}}, True)
                if command == 'slow20':
                    time.sleep(0.2)
            assistant([{'type': 'text', 'text': 'Completed the deterministic tool workload.'}])
            done()
        elif command == 'slow':
            seconds = int(parts[1]) if len(parts) > 1 else 20
            text = ''
            emit({'type': 'stream_event', 'event': {'type': 'message_start', 'message': {'id': 'slow-' + str(turn), 'role': 'assistant', 'content': [], 'usage': {'input_tokens': 4200, 'output_tokens': 0}}}})
            emit({'type': 'stream_event', 'event': {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}})
            for n in range(seconds):
                delta = 'Iteration ' + str(n) + ': streaming a prose-only busy turn.\n\n'
                text += delta
                emit({'type': 'stream_event', 'event': {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': delta}}})
                time.sleep(1)
            emit({'type': 'stream_event', 'event': {'type': 'content_block_stop', 'index': 0}})
            assistant([{'type': 'text', 'text': text}], message_id='slow-' + str(turn))
            done()
        elif command == 'error' and turn == 1:
            emit({'type': 'system', 'subtype': 'api_retry', 'attempt': 2, 'retry_delay_ms': 8000, 'error': '529 Overloaded'})
            time.sleep(2)
            emit({'type': 'result', 'uuid': str(uuid.uuid4()), 'subtype': 'error_during_execution', 'is_error': True, 'session_id': session, 'errors': ['API Error: 529 Overloaded. Please try again.']})
        elif command == 'memory':
            kb = int(parts[1]) if len(parts) > 1 else 512
            children = int(parts[2]) if len(parts) > 2 else 32
            unit = 'A long-lived memory fixture checks presentation ownership and hidden residency.\n\n```rust\nfn fixture_owner() -> usize { 42 }\n```\n\n'
            text = (unit * (kb * 1024 // len(unit) + 1))[:kb * 1024]
            assistant([{'type': 'text', 'text': text}])
            for n in range(children):
                identifier = 'child-' + str(turn) + '-' + str(n)
                assistant([{'type': 'tool_use', 'id': identifier, 'name': 'Agent', 'input': {'description': 'Memory child ' + str(n), 'prompt': 'Inspect fixture data', 'subagent_type': 'Explore'}}])
                child_text = (unit * (64 * 1024 // len(unit) + 1))[:64 * 1024]
                child = {'type': 'assistant', 'uuid': str(uuid.uuid4()), 'parent_tool_use_id': identifier, 'subagent_type': 'Explore', 'task_description': 'Unselected child fixture ' + str(n), 'message': {'id': str(uuid.uuid4()), 'role': 'assistant', 'content': [{'type': 'text', 'text': child_text}], 'usage': {'input_tokens': 4200, 'output_tokens': 180}}}
                emit(child, True)
                emit({'type': 'user', 'uuid': str(uuid.uuid4()), 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': identifier, 'content': 'Child fixture completed.'}]}}, True)
            for n in range(30):
                identifier = 'memory-tool-' + str(turn) + '-' + str(n)
                assistant([{'type': 'tool_use', 'id': identifier, 'name': 'Bash', 'input': {'command': 'cargo test --locked --no-default-features', 'description': 'Memory tool ' + str(n)}}])
                emit({'type': 'user', 'uuid': str(uuid.uuid4()), 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': identifier, 'content': 'fixture diagnostic line\n' * 128, 'is_error': False}]}}, True)
            assistant([{'type': 'text', 'text': 'Memory fixture ready: parent markdown/code, 32 unselected child histories, 30 tools. Exact actor ' + (parts[3] if len(parts) > 3 else 'memory') + '; exact session ' + session + '.'}])
            done()
        elif command in ('huge', 'stream'):
            kb = int(parts[1]) if len(parts) > 1 else 300
            blocks = []
            size = 0
            n = 0
            while size < kb * 1024:
                block = ('## Section ' + str(n // 25) + '\n\n' if n % 25 == 0 else '') + 'Measured iteration ' + str(n) + ': native chat layout, selection, rendering, and residency stay observable.\n\n'
                blocks.append(block)
                size += len(block)
                n += 1
            text = ''.join(blocks)[:kb * 1024]
            if command == 'stream':
                emit({'type': 'stream_event', 'event': {'type': 'message_start', 'message': {'id': 'stream-' + str(turn), 'role': 'assistant', 'content': [], 'usage': {'input_tokens': 4200, 'output_tokens': 0}}}})
                emit({'type': 'stream_event', 'event': {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}})
                rate = int(parts[2]) if len(parts) > 2 else 300
                frequency = int(parts[3]) if len(parts) > 3 else 80
                chunk = max(128, rate * 1024 // frequency)
                for offset in range(0, len(text), chunk):
                    emit({'type': 'stream_event', 'event': {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': text[offset:offset + chunk]}}})
                    time.sleep(1 / frequency)
                emit({'type': 'stream_event', 'event': {'type': 'content_block_stop', 'index': 0}})
            assistant([{'type': 'text', 'text': text}], message_id='stream-' + str(turn) if command == 'stream' else None)
            done()
        else:
            text = 'The native fixture validates the stable reading column with a normal transcript and composer. Account ' + account_label + '; exact session ' + session + '.'
            if command == 'wide':
                text += '\n\n```rust\n' + 'long_token_' * 300 + '\n```\n\n' + 'X' * 3000
            assistant([{'type': 'text', 'text': text}])
            done()
