#!/usr/bin/env bash
# Native evidence steps: fresh empty fixture, integrated build, 1600x1000,
# scale 1. The toolbar point comes from that actual native fixture viewport;
# the menu and page interactions use visible text. No provider is started.
set -euo pipefail

evidence env sh -c '[ "$(tail -n 1 "$CARGO_TARGET_DIR/evidence-source")" = integrated ]'

document=$(cat <<'HTML'
<!doctype html><meta charset="utf-8"><title>Native browser regression</title>
<style>body{font:24px sans-serif;color:#111;background:#fff;margin:24px}input,button{font:24px sans-serif;padding:12px}#spinner{width:32px;height:32px;border:6px solid #ddd;border-top-color:#1677bd;border-radius:50%;animation:spin 1s linear infinite}@keyframes spin{to{transform:rotate(360deg)}}</style>
<h1>Native browser ready</h1><div id="spinner"></div>
<p><input aria-label="Type value" placeholder="Type value" oninput="document.getElementById('echo').textContent='Typed '+this.value"></p>
<button onclick="window.count=(window.count||0)+1;document.getElementById('count').textContent='Count '+window.count">Increment</button>
<h2 id="echo">Waiting for input</h2><h2 id="count">Count 0</h2>
<p style="margin-top:1200px">Bottom marker</p>
HTML
)
url=$(evidence env /usr/bin/python3 -c "$(cat <<'PY'
from pathlib import Path
import os, subprocess, sys, time
fixture = Path(os.environ['CARGO_TARGET_DIR']).parent / 'fixture'
page = fixture / 'browser-page'
page.mkdir(parents=True, exist_ok=True)
(page / 'index.html').write_text(sys.argv[1])
port_file = fixture / 'browser-page.port'
port_file.unlink(missing_ok=True)
server = r'''
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import os, sys
server = ThreadingHTTPServer(('127.0.0.1', 0), partial(SimpleHTTPRequestHandler, directory=sys.argv[1]))
port = Path(sys.argv[2])
temporary = port.with_suffix('.tmp')
temporary.write_text(str(server.server_port))
os.replace(temporary, port)
server.serve_forever()
'''
with (fixture / 'browser-page-server.log').open('ab') as log:
    process = subprocess.Popen([sys.executable, '-u', '-c', server, str(page), str(port_file)],
        stdin=subprocess.DEVNULL, stdout=log, stderr=log, close_fds=True, start_new_session=True)
    for _ in range(100):
        if process.poll() is not None:
            raise RuntimeError('local browser fixture server exited; see browser-page-server.log')
        if port_file.exists():
            print('http://127.0.0.1:' + str(int(port_file.read_text())) + '/index.html')
            break
        time.sleep(0.05)
    else:
        process.terminate()
        process.wait()
        raise RuntimeError('local browser fixture server did not bind a port')
PY
)" "$document")

evidence drive click 291,22
evidence drive click Browser
evidence drive wait 'Import browser logins' --timeout 15
evidence drive click Skip
evidence drive key ctrl+l
evidence drive type "$url"
evidence drive key Return
evidence drive wait 'Native browser ready' --timeout 30
if [ "${BROWSER_SMOKE_OPEN_ONLY:-0}" = 1 ]; then
    exit 0
fi
evidence drive click 'Type value'
evidence drive type native42
evidence drive wait 'Typed native42' --timeout 15
evidence drive click Increment
evidence drive wait 'Count 1' --timeout 15
evidence drive key space
evidence drive wait 'Count 2' --timeout 15
evidence drive scroll v 80 --at 'Native browser ready'
evidence drive wait 'Bottom marker' --timeout 15
evidence drive scroll v -80 --at 'Bottom marker'
evidence drive wait 'Native browser ready' --timeout 15
evidence shot browser-wide
evidence nested niri msg action set-column-width '75%'
evidence drive wait 'Native browser ready' --timeout 15
evidence shot browser-narrow
