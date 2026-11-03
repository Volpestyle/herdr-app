#!/usr/bin/env python3
"""Real Playwright-over-CDP proof against a scratch preview (own home/core, non-activating).

Run through clankie heavy -- python3 personal/cdp_smoke.py --playwright-core <dir>.
Proves: no listener before `browser.cdp`; the endpoint is loopback-only and
token-gated (wrong token, browser Origin and foreign Host are refused); person
panes are invisible to Playwright and refuse `browser.cdp`; Playwright drives
the agent pane (navigate, locator click, fill, evaluate, console, network,
full-page screenshot, new page as a visible pane); Browser.close only
disconnects. Never launches the installed app.
"""
import argparse
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import threading
import time
from urllib.parse import urlparse

from agent_control_smoke import PAGE

ROOT = Path(__file__).resolve().parents[1]


def handshake(port, path, host=None, origin=None):
    """Raw WebSocket upgrade; returns the HTTP status line."""
    key = base64.b64encode(os.urandom(16)).decode()
    lines = [f'GET {path} HTTP/1.1', f'Host: {host or f"127.0.0.1:{port}"}', 'Upgrade: websocket',
             'Connection: Upgrade', f'Sec-WebSocket-Key: {key}', 'Sec-WebSocket-Version: 13']
    if origin: lines.append(f'Origin: {origin}')
    with socket.create_connection(('127.0.0.1', port), timeout=5) as s:
        s.sendall(('\r\n'.join(lines) + '\r\n\r\n').encode())
        return s.recv(4096).decode(errors='replace').split('\r\n', 1)[0]


def pipe_holders(app_pid):
    """Processes other than the app holding the app's DevTools pipe ends (fds 3/4)."""
    out = subprocess.run(['/usr/sbin/lsof', '-nP', '-a', '-u', str(os.getuid()), '-d', '3,4'], capture_output=True, text=True).stdout
    rows = [line.split() for line in out.splitlines()[1:]]
    ends = {r[5] for r in rows if len(r) > 5 and r[1] == str(app_pid) and r[4] == 'PIPE'}
    assert len(ends) == 2, f'app fds 3/4 are not the DevTools pipe: {out}'
    return [' '.join(r) for r in rows if len(r) > 5 and r[1] != str(app_pid) and r[5] in ends]


def listeners(pid):
    out = subprocess.run(['/usr/sbin/lsof', '-nP', '-a', '-p', str(pid), '-iTCP', '-sTCP:LISTEN'], capture_output=True, text=True).stdout
    return [line for line in out.splitlines()[1:] if line.strip()]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(ROOT / '.local/preview/Herdr Browser.app/Contents/MacOS/HerdrBrowser'))
    parser.add_argument('--playwright-core', required=True)
    args = parser.parse_args()
    work = ROOT / '.local/cdp-smoke' / str(time.time_ns())
    home = Path('/tmp') / f'hb-cdp-{os.getpid()}-{work.name[-6:]}'
    work.mkdir(parents=True)
    home.mkdir()
    home = home.resolve()
    (home / 'shared.json').write_text('{"frame.welcomed":"finished"}')
    (home / 'settings.json').write_text(json.dumps({'sounds': {'enabled': False}, 'telemetry': False, 'core': str(Path.home() / '.local/bin/hc')}))

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def do_GET(self):
            body = PAGE if not self.path.startswith('/next') else b'<!doctype html><title>Next fixture</title><h1>Navigation complete</h1>'
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            if self.path.startswith('/?set=agent'): self.send_header('Set-Cookie', 'profile_marker=agent-only; Path=/')
            if self.path.startswith('/?set=person'): self.send_header('Set-Cookie', 'person_secret=owner-session; Path=/')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    # The person page lives on another origin (localhost vs 127.0.0.1) so its
    # cookie could only leak through the profile, never through the URL.
    agent_url = f'http://127.0.0.1:{server.server_port}/'
    person_url = f'http://localhost:{server.server_port}/'
    env = os.environ.copy()
    env.update(HERDR_APP_HOME=str(home), HC_SESSION=f'hb-cdp-{os.getpid()}', HERDR_APP_BACKGROUND='1', HERDR_BROWSER_TEST_KEYCHAIN=f'Herdr Browser CDP QA {work.name}', HERDR_TELEMETRY='0')
    for name in ['HERDR_BROWSER_DEBUG_PORT', 'HERDR_BROWSER_TEST_WINDOW_FRONT', 'HC_TERMINAL_ID', 'HERDR_PANE_ID', 'HERDR_BROWSER_CDP']:
        env.pop(name, None)
    binary = str(Path(args.binary).resolve())
    assert '/.local/' in binary and not binary.startswith(str(Path.home() / 'Applications/'))
    log = (work / 'app.log').open('w')
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    transcript, space, result = [], None, {}

    def command(command_name, expect_error=None, **kwargs):
        cli_env = env.copy()
        cli_env['HC_TERMINAL_ID'] = 'cdp-qa-terminal'
        out = subprocess.run([binary, 'do', command_name, json.dumps(kwargs)], env=cli_env, text=True, capture_output=True, timeout=45)
        try: value = json.loads(out.stdout)
        except Exception: raise RuntimeError(f'{command_name}: {out.stderr}')
        transcript.append({'command': command_name, 'args': kwargs, 'result': value})
        if expect_error: assert expect_error in value.get('error', ''), value
        else: assert out.returncode == 0 and 'error' not in value, value
        return value

    def loaded(pane, title, **extra):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                state = command('browser.inspect', pane=pane, **extra)
                if state['title'] == title and not state['loading']: return state
            except AssertionError: pass
            time.sleep(.1)
        raise AssertionError(f'{pane} did not load {title}')

    try:
        deadline = time.monotonic() + 60
        while not (home / 'herdr-app.sock').exists():
            assert app.poll() is None, f'App exited; {work}/app.log'
            assert time.monotonic() < deadline, f'No socket; {work}/app.log'
            time.sleep(.1)
        while True:
            try:
                space = command('space.new', name='CDP QA', folder=str(work), tab='Scratch terminal')
                break
            except AssertionError as e:
                # The scratch core may still be connecting.
                if 'unavailable' not in str(e) or time.monotonic() > deadline: raise
                time.sleep(.5)
        person = command('browser.open', profile='person', url=person_url + '?set=person', place='tab')['pane']
        # The test harness stands in for the owner here; agents never pass this.
        loaded(person, 'Agent control fixture', confirmed=True)
        agent = command('browser.open', profile='agent', url=agent_url + '?set=agent', place='tab')['pane']
        loaded(agent, 'Agent control fixture')
        # The control socket is owner-only, and nothing the app spawned (core,
        # pane shell) inherited the DevTools pipe.
        result['socket_mode'] = oct((home / 'herdr-app.sock').stat().st_mode & 0o777)
        assert result['socket_mode'] == '0o600', result['socket_mode']
        result['pipe_holders'] = pipe_holders(app.pid)
        assert not result['pipe_holders'], result['pipe_holders']
        result['listeners_before'] = listeners(app.pid)
        assert not result['listeners_before'], result['listeners_before']

        command('browser.cdp', pane=person, expect_error='limited to agent-profile panes')
        command('browser.cdp', pane=person, confirmed=True, expect_error='unknown argument(s): confirmed')
        cdp = command('browser.cdp', pane=agent)
        endpoint = urlparse(cdp['endpoint'])
        assert endpoint.scheme == 'ws' and endpoint.hostname == '127.0.0.1', cdp
        token = endpoint.path.rsplit('/', 1)[1]
        assert len(token) == 64, cdp
        assert command('browser.cdp', pane=agent)['endpoint'] == cdp['endpoint']
        result['listeners_after'] = listeners(app.pid)
        assert result['listeners_after'] and all('127.0.0.1:' in l for l in result['listeners_after']), result['listeners_after']

        port = endpoint.port
        result['gate'] = {
            'wrong_token': handshake(port, '/devtools/browser/' + '0' * 64),
            'no_token': handshake(port, '/json/version'),
            'browser_origin': handshake(port, endpoint.path, origin='https://evil.example'),
            'rebound_host': handshake(port, endpoint.path, host=f'evil.example:{port}'),
        }
        assert all(' 403 ' in line for line in result['gate'].values()), result['gate']

        shot = work / 'full-page.png'
        pw = subprocess.run(['node', str(ROOT / 'personal/cdp_playwright.mjs'), args.playwright_core, cdp['endpoint'], agent_url, str(shot)],
                            capture_output=True, text=True, timeout=120)
        (work / 'playwright.log').write_text(pw.stdout + pw.stderr)
        assert pw.returncode == 0, pw.stdout + pw.stderr
        report = json.loads(pw.stdout.strip().splitlines()[-1])
        result['playwright'] = report
        assert all(not u.startswith(person_url) for u in report['pages']), report
        assert report['clicked'] == 'Clicked safely', report
        assert report['submitted'] == 'Submitted: Playwright', report
        assert report['console'] == 'hello from the page' and report['response'] == 200, report
        assert 'profile_marker=agent-only' in report['cookies'], report
        assert 'pw_added=agent-write' in report['cookies'], report
        assert 'Herdr blocks' in report['clearCookies'], report
        assert not any(c.startswith('person_secret') for c in report['cookies']), report
        assert shot.exists() and shot.stat().st_size > 10_000, shot
        assert report['newPage'].startswith(agent_url), report
        # Browser.close only disconnected Playwright; both panes are still there.
        assert app.poll() is None, 'Browser.close quit the app'
        assert command('browser.inspect', pane=agent)['profile'] == 'agent'
        assert command('browser.inspect', pane=person, confirmed=True)['profile'] == 'person'
        assert 'person_secret=owner-session' in command('browser.read', pane=person, confirmed=True)['text']
        # Playwright's cookie writes stayed in the agent profile: the person profile
        # loading the agent's origin sees none of them.
        command('browser.navigate', pane=person, url=agent_url + '?step=person-check', confirmed=True)
        deadline = time.monotonic() + 15
        while 'step=person-check' not in command('browser.inspect', pane=person, confirmed=True)['url'] or command('browser.inspect', pane=person, confirmed=True)['loading']:
            assert time.monotonic() < deadline
            time.sleep(.1)
        person_view = command('browser.read', pane=person, confirmed=True)['text']
        result['person_cookie_view'] = [l for l in person_view.splitlines() if l.startswith('Cookie:')]
        assert 'pw_added' not in person_view and 'profile_marker' not in person_view, person_view
        result.update(ok=True, endpoint_shape='ws://127.0.0.1:<port>/devtools/browser/<64 hex>', screenshot=str(shot), work=str(work))
        (work / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result, indent=2), flush=True)
    finally:
        (work / 'transcript.json').write_text(json.dumps(transcript, indent=2))
        if app.poll() is None:
            try:
                if space: command('space.close', id=space['space'], confirmed=True)
                command('app.quit')
                app.wait(timeout=10)
            except Exception:
                app.terminate(); app.wait(timeout=10)
        cleanup = {}
        if space:
            terminal = subprocess.run([str(Path.home() / '.local/bin/hc'), 'kill', space['terminal'], '--json'], env=env, capture_output=True, text=True, timeout=10)
            cleanup['terminal'] = {'code': terminal.returncode, 'stdout': terminal.stdout, 'stderr': terminal.stderr}
        stopped = subprocess.run([str(Path.home() / '.local/bin/hc'), 'server', 'stop', '--json'], env=env, capture_output=True, text=True, timeout=10)
        cleanup['core'] = {'code': stopped.returncode, 'stdout': stopped.stdout, 'stderr': stopped.stderr}
        (work / 'cleanup.json').write_text(json.dumps(cleanup, indent=2))
        server.shutdown()
        log.close()
        print(f'Evidence: {work}', flush=True)


if __name__ == '__main__': main()
