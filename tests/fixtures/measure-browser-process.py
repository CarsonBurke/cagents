#!/usr/bin/env python3
"""Sample only the native browser fixture's UI processes on the build host."""

import argparse
import json
import os
from pathlib import Path
import re
import statistics
import time


def stat(pid):
    fields = (Path('/proc') / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[19]), int(fields[11]) + int(fields[12]), int(fields[21]) * os.sysconf('SC_PAGE_SIZE')


def presentation_intervals(path, begin, end):
    children, timestamps = set(), []
    content = Path(path).read_bytes()
    for line in content[:begin].decode(errors="replace").splitlines():
        child = re.search(r'get_subsurface\(.*?wl_surface[@#](\d+)', line)
        if child:
            children.add(child.group(1))
    for line in content[begin:end].decode(errors="replace").splitlines():
        child = re.search(r'get_subsurface\(.*?wl_surface[@#](\d+)', line)
        if child:
            children.add(child.group(1))
        commit = re.search(r'^\[\s*([\d.]+)\].*-> wl_surface[@#](\d+)\.commit\(', line)
        if commit and commit.group(2) in children:
            timestamps.append(float(commit.group(1)))
    if len(timestamps) < 3:
        raise RuntimeError('the measured interval contains too few browser surface commits')
    gaps = sorted(b - a for a, b in zip(timestamps, timestamps[1:]))
    if gaps[0] < 0:
        raise RuntimeError('the protocol clock wrapped or commits were reordered')
    return {'commits': len(timestamps), 'median_ms': statistics.median(gaps),
            'p95_ms': gaps[max(0, (95 * len(gaps) + 99) // 100 - 1)], 'max_ms': gaps[-1]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture-home', required=True)
    parser.add_argument('--seconds', type=float, default=30)
    parser.add_argument('--output', required=True)
    parser.add_argument('--protocol-log')
    args = parser.parse_args()
    if not 0 < args.seconds <= 60:
        parser.error('--seconds must be in (0, 60]')
    marker = ('HOME=' + args.fixture_home).encode()
    initial = {}
    for process in Path('/proc').iterdir():
        if not process.name.isdigit():
            continue
        try:
            executable = process.joinpath('exe').readlink().name.removesuffix(' (deleted)')
            if executable != 'cagents-ui':
                continue
            if marker not in process.joinpath('environ').read_bytes().split(b'\0'):
                continue
            initial[int(process.name)] = stat(process.name)
        except (OSError, ProcessLookupError):
            continue
    if not initial:
        raise RuntimeError('no UI process belongs to this fixture HOME')
    protocol_begin = Path(args.protocol_log).stat().st_size if args.protocol_log else 0
    peak_rss = 0
    started = time.monotonic()
    while True:
        current = {pid: stat(pid) for pid in initial}
        if any(current[pid][0] != values[0] for pid, values in initial.items()):
            raise RuntimeError('a fixture UI process was replaced during measurement')
        peak_rss = max(peak_rss, sum(values[2] for values in current.values()))
        elapsed = time.monotonic() - started
        if elapsed >= args.seconds:
            break
        time.sleep(min(0.1, args.seconds - elapsed))
    cpu_seconds = sum(current[pid][1] - values[1] for pid, values in initial.items()) / os.sysconf('SC_CLK_TCK')
    result = {'seconds': elapsed, 'cpu_seconds': cpu_seconds,
              'cpu_percent': 100 * cpu_seconds / elapsed, 'peak_sum_rss_bytes': peak_rss,
              'processes': len(initial)}
    if args.protocol_log:
        protocol_end = Path(args.protocol_log).stat().st_size
        result['protocol_bytes'] = [protocol_begin, protocol_end]
        result['presentation'] = presentation_intervals(args.protocol_log, protocol_begin, protocol_end)
    Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
