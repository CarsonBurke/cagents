"""Read-only proof and request/reply adapter for an admitted private desktop.

The controller performs window PID verification, input and capture inside its
existing lock-owning holder. This module never dispatches XTest or desktop
commands itself. OCR reads only the screenshot returned by that holder.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def current(record):
    try:
        process = Path('/proc') / str(record['pid'])
        fields = (process / 'stat').read_text().rsplit(')', 1)[1].split()
        return process.stat().st_uid == os.getuid() and fields[0] != 'Z' and fields[19] == str(record['start'])
    except (FileNotFoundError, ProcessLookupError, KeyError, TypeError, ValueError):
        return False


def window_identity(value, pid, ticks):
    if not isinstance(value, dict) or not isinstance(value.get('ui'), dict):
        raise SystemExit('admitted controller returned no UI window identity')
    ui = value['ui']
    if ui.get('pid') != pid or str(ui.get('start')) != ticks or not current(ui):
        raise SystemExit('admitted controller window belongs to a different UI process')
    xid = value.get('xid')
    try:
        if not isinstance(xid, str) or int(xid, 16) <= 0:
            raise ValueError()
    except ValueError:
        raise SystemExit('admitted controller returned no real X11 window')
    return xid


class Adapter:
    def __init__(self, case, control, pid, ticks, output):
        self.case = case.resolve(strict=True)
        self.control = control.resolve(strict=True)
        self.pid, self.ticks, self.output = pid, ticks, output
        if self.control.parent != self.case or self.control.name != 'control-chat.py':
            raise SystemExit('supply the exact private CASE/control-chat.py request client')
        manifest = json.loads((self.case / 'domain-review-manifest.json').read_text())
        entry = manifest.get('files', {}).get(str(self.control), {})
        self.control_hash = entry.get('sha256')
        if self.control_hash != hashlib.sha256(self.control.read_bytes()).hexdigest():
            raise SystemExit('controller differs from its explicitly reviewed manifest')
        self.owner = json.loads((self.case / 'harness/owner.json').read_text())
        desktop = json.loads((self.case / 'harness/desktop.json').read_text())
        if not current(self.owner) or self.owner.get('runtime') != desktop.get('XDG_RUNTIME_DIR'):
            raise SystemExit('private desktop owner has ended or changed')
        process = Path('/proc') / str(pid)
        env = dict(item.split(b'=', 1) for item in (process / 'environ').read_bytes().split(b'\0') if b'=' in item)
        if not desktop.get('DISPLAY') or env.get(b'DISPLAY') != os.fsencode(desktop['DISPLAY']):
            raise SystemExit('private UI does not belong to this explicit desktop')
        self.xid = None

    def action(self, command, *arguments):
        if command == 'key':
            command, arguments = 'chord', ({'ctrl+w': 'Control_L+w', 'ctrl+PageDown': 'Control_L+Page_Down'}[arguments[0]],)
        if command not in ('window', 'screenshot', 'chord'):
            raise SystemExit('unsupported pane-close controller request')
        if not current(self.owner) or hashlib.sha256(self.control.read_bytes()).hexdigest() != self.control_hash:
            raise SystemExit('private desktop/controller changed during pane closure')
        result = subprocess.run([sys.executable, str(self.control), command, *arguments],
                                capture_output=True, text=True, timeout=90)
        value = json.loads(result.stdout) if result.returncode == 0 else None
        with self.output.open('a') as receipt:
            receipt.write(json.dumps({'controller_action': command, 'arguments': list(arguments),
                                      'monotonic_ns': time.monotonic_ns(), 'exit': result.returncode,
                                      'result': value, 'stderr': result.stderr}) + '\n')
        result.check_returncode()
        xid = window_identity(value, self.pid, self.ticks)
        if self.xid is not None and xid != self.xid:
            raise SystemExit('private UI window changed during pane closure')
        self.xid = xid
        return value

    def focused_ui(self, expected_actor=None):
        self.action('window')
        if expected_actor is None:
            return
        directory = self.case / 'pane-close-screenshots'
        directory.mkdir(mode=0o700, exist_ok=True)
        image = directory / (str(time.monotonic_ns()) + '.png')
        self.action('screenshot', str(image))
        if not image.is_file() or image.is_symlink():
            raise SystemExit('admitted holder did not produce its private screenshot')
        result = subprocess.run(['/usr/bin/tesseract', str(image), 'stdout'],
                                capture_output=True, text=True, timeout=30)
        result.check_returncode()
        compact = ''.join(result.stdout.split())
        matched = ('Exactactor' + expected_actor + ';') in compact
        with self.output.open('a') as receipt:
            receipt.write(json.dumps({'native_ocr': str(image), 'image_sha256': hashlib.sha256(image.read_bytes()).hexdigest(),
                                      'exact_actor_marker': expected_actor, 'matched': matched,
                                      'monotonic_ns': time.monotonic_ns()}) + '\n')
        if not matched:
            raise SystemExit('selected parent exact actor footer is not visible; refusing pane-close dispatch')
        # The controller validates _NET_WM_PID again after OCR and before the
        # caller's final durable-focus fence and native dispatch.
        self.action('window')
