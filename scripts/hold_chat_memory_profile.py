#!/usr/bin/env python3
"""Finite capture+analysis holder, called INSIDE the initial r5 build admission.

Reuse the reviewed case control-chat/action-driver/control-worker helpers. This
holder owns all action/profiler descendants through run_owned_command; it never
starts another application, desktop or build. Native acceptance is separate.
"""
import json
import os
from pathlib import Path
import signal
import sys
import time
import uuid

# A prepared case copies this file and chat_memory_heap.py into its helpers.
from common import (atomic_json, current, identity, load_config, monotonic_stamp,
                    private_environment, require_build_admission, run_owned_command,
                    verify_manifest, verify_source)
import chat_memory_heap as profile


def action(cfg, request):
    return run_owned_command(cfg, [sys.executable, str(cfg['_case'] / 'action-driver.py'), str(request)])


def finish(cfg, ui, request):
    case = cfg['_case']
    manifest = verify_manifest(cfg['_bundle'] / 'manifest.json')
    profile.observe_children(cfg, profile.owned_children(cfg, manifest, ui))
    # Existing reviewed native action path validates private XID/PID immediately
    # before Alt+F4. No direct external XTest or signal-to-UI path is introduced.
    quit_request = case / 'geometry-requests' / ('quit-' + str(uuid.uuid4()) + '.json')
    atomic_json(quit_request, {'action': 'chord', 'arguments': ['Alt_L+F4']})
    try:
        if action(cfg, quit_request):
            raise RuntimeError('owned native window-close action failed')
    finally:
        quit_request.unlink(missing_ok=True)
        quit_request.with_suffix('.response').unlink(missing_ok=True)
    deadline = min(time.monotonic() + 10, profile_deadline(cfg))
    while not (case / 'snapshot-ui-exit.json').exists():
        if time.monotonic() >= deadline:
            raise RuntimeError('UI did not exit gracefully before the profile deadline')
        if not current(cfg['native_owner']):
            raise RuntimeError('native owner exited during profile close')
        time.sleep(.05)
    ended = json.loads((case / 'snapshot-ui-exit.json').read_text())
    if ended.get('ui') != ui or ended.get('returncode') != 0:
        raise RuntimeError('profile requires the exact UI graceful exit')
    # Capture has stopped; bounded interpretation continues under the SAME
    # initial broker/native leases. The holder's alarm covers all descendants.
    signal.setitimer(signal.ITIMER_REAL, 100)
    result = run_owned_command(cfg, [sys.executable, str(Path(__file__).with_name('chat_memory_heap.py')),
                                  'analyze', str(cfg['_config'])])
    if result:
        raise RuntimeError('owned profile interpretation failed')
    atomic_json(case / 'geometry-finished.json', {'at': monotonic_stamp(), 'profile_interpreted': True})
    atomic_json(request.with_suffix('.response'), {'exit_status': 0, 'stdout': 'private heap profile interpreted\n', 'stderr': ''})


def profile_deadline(cfg):
    prepared = json.loads((cfg['_case'] / 'heap-profile/prepared.json').read_text())
    return prepared['deadline_monotonic_ns'] / 1_000_000_000


def main():
    cfg = load_config(sys.argv[1])
    require_build_admission(cfg)
    private_environment(cfg)
    verify_manifest(cfg['_bundle'] / 'manifest.json')
    case = cfg['_case']
    ui = json.loads((case / 'runner-handoff.json').read_text())['ui']
    profile.monitor(cfg, ui)
    if (case / 'geometry-admitted.json').exists() or (case / 'geometry-finished.json').exists():
        raise RuntimeError('use a fresh case for the profile interval')
    job = json.loads(Path('/home/marvin/.cache/cagents-issue-owner/build-queue', cfg['worker'] + '.json').read_text())
    broker = identity(job['pid'])
    if broker['start'] != job['process_start_ticks']:
        raise RuntimeError('profile broker identity changed')
    requests = case / 'geometry-requests'
    requests.mkdir(mode=0o700)
    def stop(_number, _frame):
        raise InterruptedError('private profile interval cancelled')
    def expired(_number, _frame):
        raise TimeoutError('private profile interval deadline expired')
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGALRM, expired)
    remaining = profile_deadline(cfg) - time.monotonic()
    if remaining <= 0:
        raise RuntimeError('profile expired before its holder started')
    signal.setitimer(signal.ITIMER_REAL, remaining)
    atomic_json(case / 'geometry-admitted.json', {'holder': identity(os.getpid()), 'broker': broker,
        'ui': ui, 'source_sha': cfg['source_sha'], 'at': monotonic_stamp(),
        'deadline_monotonic_ns': int((profile_deadline(cfg) + 100) * 1_000_000_000),
        'capture_deadline_monotonic_ns': int(profile_deadline(cfg) * 1_000_000_000),
        'native_validation': False, 'profile_only_not_normal_performance': True})
    try:
        done = False
        while not done:
            profile.monitor(cfg, ui)
            for request in sorted(requests.glob('*.json')):
                if request.stat().st_size > 8192:
                    raise RuntimeError('oversized profile action request')
                value = json.loads(request.read_text())
                arguments = value.get('arguments')
                if (not isinstance(arguments, list) or len(arguments) > 2
                    or any(not isinstance(argument, str) for argument in arguments)
                    or sum(len(argument.encode()) for argument in arguments) > 4096):
                    raise RuntimeError('invalid bounded profile action request')
                if value['action'] == 'finish':
                    finish(cfg, ui, request)
                    request.unlink(missing_ok=True)
                    done = True
                    break
                result = action(cfg, request)
                if result and not request.with_suffix('.response').exists():
                    atomic_json(request.with_suffix('.response'), {'exit_status': result,
                        'stdout': '', 'stderr': 'owned profile action failed'})
                request.unlink(missing_ok=True)
            time.sleep(.05)
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
    verify_source(cfg)
    require_build_admission(cfg)
    atomic_json(case / 'heap-profile/holder-result.json', {'status': 'capture_and_analysis_completed',
        'source_sha': cfg['source_sha'], 'ui': ui, 'native_validation': False,
        'normal_rss_latency_comparison': False, 'at': monotonic_stamp()})


if __name__ == '__main__':
    main()
