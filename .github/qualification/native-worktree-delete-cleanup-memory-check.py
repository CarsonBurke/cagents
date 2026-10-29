"""Memory-only causal check of extracted probe cleanup functions.

Never imports the probe or its helpers. No signals, processes, native libraries,
CLI, fixture or desktop are used. Only source reads and a new receipt write touch
the filesystem; every function dependency below is a memory fake.
"""
import argparse
import ast
import hashlib
import json
from pathlib import Path
from types import SimpleNamespace


class MemoryCancelled(BaseException):
    pass


FUNCTIONS = {'cleanup', 'close_probe_inputs', 'close_probe_screen',
             'cleanup_handback', 'probe_status'}
AFTER = '24d0474e250bcc4acc0958cf3abbf53ee1125581'


def extract(path, names):
    raw = path.read_bytes()
    tree = ast.parse(raw, filename=str(path))
    selected = [node for node in tree.body
                if isinstance(node, ast.FunctionDef) and node.name in names]
    assert {node.name for node in selected} == names
    # Compile function definitions only. Imports, top-level assignments, main,
    # gates and all real helper bodies are excluded from the executable module.
    module = ast.Module(body=selected, type_ignores=[])
    return compile(module, str(path) + ':memory-only-extraction', 'exec'), {
        'source_sha256': hashlib.sha256(raw).hexdigest(),
        'extracted_function_ast_sha256': {
            node.name: hashlib.sha256(ast.dump(node, include_attributes=False).encode()).hexdigest()
            for node in selected}}


class Memory:
    def __init__(self, code, cancel_at=None, ordinary_at=None):
        self.trace, self.counts, self.receipts = [], {}, {}
        self.cancel_at, self.ordinary_at = cancel_at, ordinary_at
        self.remaining = [dict(pid=900, start='memory-observer')]
        self.exited = False
        self.context = dict(
            terminal_ids={role: 'memory-' + role for role in ('primary', 'outer', 'foreign', 'own')},
            terminal_scopes={role: Path('/memory/scope') / role for role in ('primary', 'outer', 'foreign', 'own')},
            binary=Path('/memory/cagents'), pid=100, xid=123,
            daemon_owner=dict(pid=200, start='memory-daemon'), env={},
            final_conservation=dict(processes={role: dict(pid=300 + n)
                for n, role in enumerate(('primary', 'outer', 'foreign', 'own'))}),
            after=Path('/memory/after'), control=Path('/memory/control'),
            manifest=dict(product_source_sha256={'src/memory.rs': 'memory-product-hash'}),
            head='memory-control-head', hashes={'memory-control.py': 'memory-control-hash'},
            fixture=Path('/memory/retained-fixture'))
        self.context['runner'] = SimpleNamespace(poll=self.poll, wait=self.runner_wait)
        self.inputs = SimpleNamespace(keys={'Shift_L', 'Escape'}, buttons={1}, display='input-display',
            event=self.event, request_close=self.request_close, close=self.legacy_inputs_close,
            x=SimpleNamespace(XCloseDisplay=self.inputs_display_close))
        screen = SimpleNamespace(display='screen-display', previous='memory-previous-handler',
            checked=self.screen_checked, close=self.legacy_screen_close,
            x=SimpleNamespace(XCloseDisplay=self.screen_display_close,
                              XSetErrorHandler=self.screen_handler_restore))
        self.native = SimpleNamespace(CRITICAL=True, CANCEL_SIGNAL=None, ACTIVE='after-probe',
            TARGET=Path('/memory/target'), OUT=Path('/memory/out'), SCREEN=screen,
            wait=self.wait, identity=self.identity, live=self.live, close_owned=self.close_owned,
            put=self.put)
        self.namespace = dict(NATIVE=self.native, INPUTS=self.inputs, cli=self.cli,
                              owner=self.owner, digest=self.digest, git=self.git, AFTER=AFTER)
        exec(code, self.namespace)

    def hit(self, name):
        self.counts[name] = self.counts.get(name, 0) + 1
        event = name + '#' + str(self.counts[name])
        self.trace.append(event)
        if event == self.cancel_at:
            self.native.CANCEL_SIGNAL = 'MEMORY_ONLY_DEADLINE'
            raise MemoryCancelled('memory-only deadline at ' + event)
        if event == self.ordinary_at:
            raise RuntimeError('memory-only ordinary failure at ' + event)

    def cli(self, context, args, label):
        self.hit('cli.' + label)
        if args[:2] == ['terminal', 'show']:
            role = next(role for role, value in context['terminal_ids'].items() if value == args[2])
            return dict(terminal=dict(session=dict(id=args[2], launch=dict(
                working_directory=str(context['terminal_scopes'][role])))))
        if args[:2] == ['terminal', 'list']:
            return dict(terminals=[])
        return {}

    def poll(self):
        self.hit('runner.poll')
        return 0 if self.exited else None

    def runner_wait(self):
        self.hit('runner.wait')
        return 0

    def owner(self, context):
        self.hit('owned.owner')

    def request_close(self, xid):
        assert xid == 123
        self.hit('input.request_close')

    def wait(self, predicate, timeout):
        assert timeout == 15
        self.hit('owned.wait')
        self.exited = True
        assert predicate()

    def identity(self, pid, binary, env):
        assert pid == 200 and binary == Path('/memory/target/dev-runtime/cagentsd')
        self.hit('daemon.identity')
        return self.context['daemon_owner']

    def event(self, kind, value, down, purpose):
        assert not down and purpose == 'cleanup' and not self.native.CRITICAL
        values = self.inputs.keys if kind == 'key' else self.inputs.buttons
        assert value in values
        self.hit('input.release.' + kind + '.' + str(value))
        values.discard(value)

    def inputs_display_close(self, display):
        assert display == 'input-display'
        self.hit('input.XCloseDisplay')

    def screen_checked(self, status):
        assert status is True
        self.hit('screen.checked')

    def screen_display_close(self, display):
        assert display == 'screen-display'
        self.hit('screen.XCloseDisplay')

    def screen_handler_restore(self, previous):
        assert previous == 'memory-previous-handler'
        self.hit('screen.XSetErrorHandler')

    def legacy_inputs_close(self):
        self.hit('legacy.inputs.close')

    def legacy_screen_close(self):
        self.hit('legacy.screen.close')

    def live(self, arm='all'):
        assert arm in ('all', 'after-probe')
        self.hit('owned.live.' + arm)
        return list(self.remaining)

    def close_owned(self):
        self.hit('owned.close')
        self.remaining.clear()

    def digest(self, path):
        self.hit('source.digest.' + str(path))
        return {'/memory/after/src/memory.rs': 'memory-product-hash',
                '/memory/control/memory-control.py': 'memory-control-hash'}[str(path)]

    def git(self, root, *args):
        self.hit('source.git.' + root.name + '.' + '.'.join(args))
        if args == ('rev-parse', 'HEAD'):
            return AFTER if root == self.context['after'] else self.context['head']
        assert args == ('status', '--porcelain=v1', '--untracked-files=all')
        return ''

    def put(self, path, value):
        self.hit('receipt.' + path.name)
        self.receipts[path.name] = value

    def run(self):
        errors = self.namespace['cleanup_handback'](self.context)
        assert errors is self.context['cleanup_errors']
        status = self.namespace['probe_status'](False, errors, self.context['cleanup_physical_status'])
        return errors, status


def verify(source, predecessor=None):
    code, provenance = extract(source, FUNCTIONS)
    baseline = Memory(code)
    errors, status = baseline.run()
    assert errors == [] and status == 'AFTER_ONLY_CANCEL_PROBE_OBSERVED_REVIEW_PENDING'
    assert baseline.context['cleanup_physical_status'] == 'OBSERVED_DISPOSED'
    assert not baseline.inputs.keys and not baseline.inputs.buttons and not baseline.remaining
    # Each actual call site in the successful cleanup path becomes a separate
    # interruption point. The complete event prefix must end at the exception;
    # there can be no later native input, CLI, wait, forced close or source read.
    cancellations = []
    for n, event in enumerate(baseline.trace):
        memory = Memory(code, cancel_at=event)
        prior = dict(stage='earlier-cleanup-error', error='sticky-memory-failure')
        memory.context['cleanup_errors'] = [prior]
        errors, status = memory.run()
        assert memory.trace == baseline.trace[:n + 1], (event, memory.trace)
        assert errors[0] is prior and len(errors) == 2
        assert errors[-1]['stage'] == 'cleanup-aborted'
        assert errors[-1]['type'] == 'MemoryCancelled'
        assert status == 'FAILED_UNQUALIFIED'
        assert memory.context['cleanup_physical_status'] == 'UNCONFIRMED'
        cancellations.append(dict(injected_at=event, effect_trace=memory.trace,
                                  later_effects=[], original_error_retained=True))
    ordinary = []
    for event in ['cli.cleanup-show-primary#1', 'input.release.key.Escape#1']:
        memory = Memory(code, ordinary_at=event)
        errors, status = memory.run()
        assert errors and status == 'FAILED_UNQUALIFIED'
        assert 'owned.close#1' in memory.trace and memory.remaining == []
        if event.startswith('input.'):
            assert memory.context['cleanup_observer_status'] == 'UNCONFIRMED'
            assert memory.context['cleanup_physical_status'] == 'UNCONFIRMED'
            assert 'screen.checked#1' not in memory.trace
        ordinary.append(dict(injected_at=event, errors=errors, effect_trace=memory.trace,
                             physical_status=memory.context['cleanup_physical_status'], status=status))
    sticky = Memory(code, ordinary_at='cli.cleanup-show-primary#1', cancel_at='owned.owner#1')
    sticky_errors, status = sticky.run()
    assert [e['stage'] for e in sticky_errors] == ['terminal-primary', 'cleanup-aborted']
    assert sticky.trace == ['cli.cleanup-show-primary#1', 'runner.poll#1', 'owned.owner#1']
    assert status == 'FAILED_UNQUALIFIED'
    residue = Memory(code)
    residue.remaining.append(dict(pid=100, start='memory-unclosed-ui'))
    residue_errors, residue_status = residue.run()
    assert [error['stage'] for error in residue_errors] == ['application-residue']
    assert residue.context['cleanup_physical_status'] == 'OBSERVED_DISPOSED'
    assert residue.remaining == [] and residue_status == 'FAILED_UNQUALIFIED'
    failed_collection = baseline.namespace['probe_status'](True, [], 'OBSERVED_DISPOSED')
    assert failed_collection == 'FAILED_UNQUALIFIED'
    # Successful physical disposal never erases the original collection result.
    assert baseline.namespace['probe_status'](False, [], 'UNCONFIRMED') == 'FAILED_UNQUALIFIED'
    negative = None
    if predecessor is not None:
        old_code, old_provenance = extract(predecessor, {'cleanup'})
        old = Memory(old_code, cancel_at='cli.cleanup-show-primary#1')
        errors = old.namespace['cleanup'](old.context)
        assert errors and errors[0]['stage'] == 'terminal-primary'
        assert all(event in old.trace for event in
                   ['owned.owner#1', 'input.request_close#1', 'owned.wait#1', 'owned.close#1'])
        negative = dict(provenance=old_provenance,
                        verdict='PREDECESSOR_SWALLOWS_CANCELLATION_AND_CONTINUES', effect_trace=old.trace)
    return dict(verdict='MEMORY_ONLY_CLEANUP_CANCELLATION_CLEAR', provenance=provenance,
        executed='Only AST-extracted actual cleanup functions with pure memory dependencies',
        native_helpers_imported=False, signals_used=False, processes_started=False,
        baseline_effect_trace=baseline.trace, cancellation_cases=cancellations,
        ordinary_error_cases=ordinary,
        ordinary_then_cancel=dict(errors=sticky_errors, effect_trace=sticky.trace),
        forced_disposal_after_application_residue=dict(errors=residue_errors,
            physical_status=residue.context['cleanup_physical_status'], status=residue_status),
        collection_failure_after_disposal=failed_collection,
        predecessor_negative=negative, runtime_qualification='UNRUN_UNGRANTED')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path(__file__).with_name(
        'native-worktree-delete-after-probe.py'))
    parser.add_argument('--predecessor-source', type=Path)
    parser.add_argument('--receipt', type=Path, required=True)
    args = parser.parse_args()
    result = verify(args.source, args.predecessor_source)
    result['checker_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    with args.receipt.open('x') as stream:
        stream.write(json.dumps(result, indent=2) + '\n')
    print(json.dumps(dict(verdict=result['verdict'],
        cancellation_cases=len(result['cancellation_cases']), receipt=str(args.receipt))))


if __name__ == '__main__':
    main()
