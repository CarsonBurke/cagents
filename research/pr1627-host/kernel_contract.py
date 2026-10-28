"""Kernel closure validators copied from the reviewed R4 supervisor.

Only the six receipt predicates are reused; no local B mutex/admission paths,
runner configuration or producer execution is imported. HEAD/TREE are bound
by the serial research harness before each owned observer phase. The inherited
B-mutex wording in valid_closed describes its origin; hosted research retains
its private target and starts no further phase when closure is unproved.
"""
import json
import math
import re
import signal
HEAD = TREE = None

def _integer(value, minimum=0):
    return type(value) is int and value >= minimum


def _identity(row):
    return (isinstance(row, dict)
            and all(_integer(row.get(key), 1) for key in ('pid', 'group', 'session'))
            and _integer(row.get('parent')) and _integer(row.get('start_ticks'))
            and isinstance(row.get('state'), str) and len(row['state']) == 1)


def _membership(proof, start_ticks=None):
    return (isinstance(proof, dict)
            and proof.get('kind') in ('reserved-leader-session', 'kernel-direct-child')
            and _integer(proof.get('start_ticks'))
            and (start_ticks is None or proof['start_ticks'] == start_ticks))


def _family_closed(receipt):
    cleanup, leader = receipt.get('cleanup'), receipt.get('leader')
    if not (isinstance(cleanup, dict) and _identity(leader)
            and leader['group'] == leader['session'] == leader['pid']
            and cleanup.get('complete') is True and cleanup.get('leader_reaped') is True
            and cleanup.get('remaining') == []
            and cleanup.get('membership_uncertain') == []
            and isinstance(cleanup.get('membership_proofs'), dict)
            and isinstance(cleanup.get('initial_census'), list) and cleanup['initial_census']
            and isinstance(cleanup.get('signals'), list) and cleanup['signals']
            and type(receipt.get('command_exit')) is int):
        return False
    # Proofs are independent epoch records. The initial census and final map
    # need not have identical contents after reaping and legitimate PID reuse.
    for pid, proof in cleanup['membership_proofs'].items():
        if not (isinstance(pid, str) and pid.isascii() and pid.isdecimal()
                and str(int(pid)) == pid and int(pid) > 0 and int(pid) != leader['pid']
                and _membership(proof)):
            return False
    leader_rows = 0
    observed = set()
    for row in cleanup['initial_census']:
        if not _identity(row) or row['pid'] in observed:
            return False
        observed.add(row['pid'])
        proof = row.get('membership')
        if row['pid'] == leader['pid']:
            leader_rows += 1
            if not (proof == {'kind': 'retained-leader'}
                    and row['start_ticks'] == leader['start_ticks']
                    and row['group'] == row['session'] == leader['pid']):
                return False
        elif not _membership(proof, row['start_ticks']):
            return False
    for event in cleanup['signals']:
        if not (isinstance(event, dict) and type(event.get('signal')) is int
                and event['signal'] in (signal.SIGTERM, signal.SIGKILL)
                and type(event.get('monotonic')) in (int, float)
                and math.isfinite(event['monotonic']) and event['monotonic'] >= 0):
            return False
    return leader_rows == 1


def _actual_exit(receipt):
    reason = receipt.get('exit_reason')
    if reason not in ('exited', 'timeout', 'interrupted', 'exception'):
        return None
    if type(receipt.get('timed_out')) is not bool or receipt['timed_out'] != (reason == 'timeout'):
        return None
    if 'interrupted_signal' in receipt:
        sig = receipt['interrupted_signal']
        return (128 + sig if type(sig) is int and sig in (signal.SIGTERM, signal.SIGINT)
                else None)
    if reason == 'timeout':
        return 124
    if reason == 'exception':
        return 1
    if reason == 'exited' and type(receipt.get('command_exit')) is int:
        code = receipt['command_exit']
        return code if code >= 0 else 128 - code
    return None


def valid_closed(output, child, command):
    """Validate producer closure, never infer custody from subset resource metrics.

    Stable R3 API: output is the phase directory, child is the owned observer
    Popen after wait(), and command is its exact argv. Missing/uncertain proofs
    fail closed. The driver must retain its mutex through hold_unproved().
    """
    try:
        receipt = json.loads((output / 'COMMAND.json').read_text())
    except (OSError, ValueError):
        return False, None
    if not isinstance(receipt, dict):
        return False, receipt
    closed_before_start = ('cleanup' not in receipt and 'leader' not in receipt
                           and 'command_exit' not in receipt
                           and receipt.get('exit_reason') == 'interrupted'
                           and type(receipt.get('interrupted_signal')) is int
                           and receipt['interrupted_signal'] in (signal.SIGTERM, signal.SIGINT))
    good = (type(child.returncode) is int and type(receipt.get('actual_exit')) is int
            and receipt.get('status') == 'closed'
            and isinstance(HEAD, str) and re.fullmatch(r'[0-9a-f]{40}', HEAD) is not None
            and isinstance(TREE, str) and re.fullmatch(r'[0-9a-f]{40}', TREE) is not None
            and receipt.get('source_head') == HEAD and receipt.get('source_tree') == TREE
            and isinstance(command, list) and bool(command)
            and all(isinstance(argument, str) for argument in command)
            and receipt.get('command') == command
            and type(receipt.get('owned_metrics_schema')) is int
            and receipt['owned_metrics_schema'] == 1
            and (_family_closed(receipt) or closed_before_start)
            and receipt.get('sampler_closed') is True
            and receipt['actual_exit'] == child.returncode == _actual_exit(receipt))
    return bool(good), receipt
