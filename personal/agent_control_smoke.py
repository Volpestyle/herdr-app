#!/usr/bin/env python3
"""Real in-process CEF control proof; scratch app home/core, non-activating window.

Run through clankie heavy -- python3 personal/agent_control_smoke.py.
No debugging port or OS pointer/key automation. Never launches the installed app.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import subprocess
import struct
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
PAGE = b'''<!doctype html><title>Agent control fixture</title>
<style>body{font:20px system-ui;padding:25px}input,button{font:inherit}</style>
<main><h1>Agent control fixture</h1>
<button onclick="document.querySelector('#result').textContent='Clicked safely'">Click me</button>
<form onsubmit="event.preventDefault();document.querySelector('#result').textContent='Submitted: '+document.querySelector('#name').value">
<label>Name <input id="name" autocomplete="off"></label>
<label>Secret <input type="password" id="secret" autocomplete="off"></label><button>Submit</button></form>
<p id="result">Waiting for input</p><p id="key"></p><p id="scroll"></p><p id="cookie"></p><script>document.querySelector('#cookie').textContent='Cookie: '+document.cookie;document.addEventListener('keydown',e=>{if(e.key==='Tab')document.querySelector('#key').textContent='Tab received'});window.addEventListener('scroll',()=>{if(scrollY>0)document.querySelector('#scroll').textContent='Scroll received'})</script><p><a href="/next">Next page</a></p></main>
<div style="height:1500px"></div><p>Bottom of fixture</p>'''


def asn(value):
    # `find` includes the display name, while `front` prints only the ASN.
    match = re.search(r'ASN:0x[0-9a-f]+-0x[0-9a-f]+', value, re.IGNORECASE)
    return match[0] if match else value.strip()


def front():
    return asn(subprocess.run(['/usr/bin/lsappinfo', 'front'], text=True, capture_output=True).stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(ROOT / '.local/preview/Herdr Browser.app/Contents/MacOS/HerdrBrowser'))
    args = parser.parse_args()
    work = ROOT / '.local/agent-control-smoke' / str(time.time_ns())
    # macOS sockaddr_un paths are limited to 104 bytes; task worktrees are longer.
    home = Path('/tmp') / f'hb-agent-{os.getpid()}-{work.name}'
    work.mkdir(parents=True)
    home.mkdir()
    home = home.resolve()
    (home / 'shared.json').write_text('{"frame.welcomed":"finished"}')
    (home / 'settings.json').write_text(json.dumps({'sounds': {'enabled': False}, 'telemetry': False, 'core': str(Path.home() / '.local/bin/hc')}))

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def do_GET(self):
            body = PAGE if self.path != '/next' else b'<!doctype html><title>Next fixture</title><h1>Navigation complete</h1>'
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            if self.path.startswith('/?set=agent'): self.send_header('Set-Cookie', 'profile_marker=agent-only; Path=/; SameSite=Lax')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update(HERDR_APP_HOME=str(home), HC_SESSION=f'hb-agent-{os.getpid()}', HERDR_APP_BACKGROUND='1', HERDR_BROWSER_TEST_KEYCHAIN=f'Herdr Browser Agent QA {work.name}', HERDR_TELEMETRY='0')
    for name in ['HERDR_BROWSER_DEBUG_PORT', 'HERDR_BROWSER_TEST_WINDOW_FRONT', 'HC_TERMINAL_ID', 'HERDR_PANE_ID']:
        env.pop(name, None)
    binary = str(Path(args.binary).resolve())
    assert '/.local/' in binary and not binary.startswith(str(Path.home() / 'Applications/'))
    before = front()
    log = (work / 'app.log').open('w')
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    transcript = []
    foreground_audit = []
    space = None

    def command(command_name, expect_error=None, expect_timeout=False, caller='qa-terminal-a', caller_key='HC_TERMINAL_ID', **kwargs):
        cli_env = env.copy()
        if caller is not None: cli_env[caller_key] = caller
        result = subprocess.run([binary, 'do', command_name, json.dumps(kwargs)], env=cli_env, text=True, capture_output=True, timeout=45)
        try: value = json.loads(result.stdout)
        except Exception: raise RuntimeError(f'{command_name}: {result.stderr}')
        transcript.append({'command': command_name, 'args': kwargs, 'caller': caller, 'result': value})
        if expect_error:
            assert expect_error in value.get('error', ''), value
        else:
            assert result.returncode == 0 and 'error' not in value, value
            if expect_timeout: assert value.get('timeout') is True, value
            else: assert not value.get('timeout'), value
        current_front = front()
        own_asn = asn(subprocess.run(['/usr/bin/lsappinfo', 'find', f'pid={app.pid}'], text=True, capture_output=True).stdout)
        foreground_audit.append({'command': command_name, 'before': before, 'current': current_front, 'preview': own_asn})
        # The owner may switch apps while the finite test runs. Detect our own
        # preview becoming frontmost without attributing their activity to us.
        assert current_front == before or (own_asn and current_front != own_asn), foreground_audit[-1]
        assert not own_asn or current_front != own_asn, foreground_audit[-1]
        return value

    def ref(snapshot, role, name):
        for line in snapshot['tree'].splitlines():
            match = re.match(r'(e\d+) (\S+) (.*)', line)
            if match and match[2] == role and json.loads(match[3]).strip() == name.strip():
                return match[1]
        raise AssertionError(f'No {role} {name}: {snapshot}')

    try:
        deadline = time.monotonic() + 60
        while not (home / 'herdr-app.sock').exists():
            assert app.poll() is None, f'App exited; {work}/app.log'
            assert time.monotonic() < deadline, f'No socket; {work}/app.log'
            time.sleep(.1)
        space = command('space.new', name='Agent QA', folder=str(work), tab='Scratch terminal')
        url = f'http://127.0.0.1:{server.server_port}/'
        # Unknown callers must never target the person's focused page.
        command('browser.read', caller='unmapped', expect_error='Pass pane explicitly')
        browser = command('browser.open', profile='agent', url=url + '?set=agent')
        pane = browser['pane']
        # Browser entity appears on the next frame; do not front the window to force it.
        deadline = time.monotonic() + 30
        while True:
            time.sleep(.1)
            try:
                state = command('browser.inspect')
                if state.get('page_error'): raise RuntimeError(state['page_error'])
                if state['title'] == 'Agent control fixture' and not state['loading']: break
            except AssertionError:
                if time.monotonic() >= deadline: raise
            assert time.monotonic() < deadline, f'Page did not load: {work}'
        assert state['native']['window_key'] is False, state
        listeners = subprocess.run(['/usr/sbin/lsof', '-nP', '-a', '-p', str(app.pid), '-iTCP', '-sTCP:LISTEN'], capture_output=True, text=True).stdout
        assert not listeners.strip(), listeners
        assert command('browser.read', caller_key='HERDR_PANE_ID')['pane'] == pane
        assert state['profile'] == 'agent'
        assert 'profile_marker=agent-only' in command('browser.read')['text']
        command('browser.wait', load=True)
        command('browser.wait', text='Agent control fixture')
        command('browser.wait', url='127.0.0.1')
        assert command('browser.wait', text='missing text', timeout=100, expect_timeout=True)['timeout']
        snap = command('browser.snapshot')
        click = ref(snap, 'button', 'Click me')
        command('browser.click', ref=click)
        assert 'Clicked safely' in command('browser.read')['text']
        # Refs survive unrelated DOM changes; navigation or node removal expires them.
        command('browser.click', ref=click)
        command('browser.click', name='Click me')
        command('browser.click', name='No such control', expect_error='No interactive element')
        command('browser.type', name='Name', text='Typed by name', submit=True)
        command('browser.wait', text='Submitted: Typed by name')
        command('browser.type', name='Secret', text='must never be inserted', expect_error='Password fields')
        snap = command('browser.snapshot')
        field = ref(snap, 'textbox', 'Name ')
        command('browser.type', ref=field, text='discard this value')
        snap = command('browser.snapshot')
        command('browser.type', ref=ref(snap, 'textbox', 'Name'), text='Background CEF input', submit=True)
        command('browser.wait', text='Submitted: Background CEF input')
        assert 'Submitted: Background CEF input' in command('browser.read', format='markdown')['text']
        snap = command('browser.snapshot')
        password = ref(snap, 'textbox', 'Secret ')
        command('browser.type', ref=password, text='must never be inserted', expect_error='Password fields')
        command('browser.click', ref=password)
        command('browser.press', key='x', expect_error='Password fields')
        # A newer snapshot replaces the previous one's refs.
        old = ref(snap, 'button', 'Click me')
        scoped = command('browser.snapshot', scope='main')
        assert 'Bottom of fixture' not in scoped['tree'] and 'Agent control fixture' in scoped['tree']
        command('browser.click', ref=old, expect_error='snapshot again')
        shot = command('browser.screenshot')
        path = Path(shot['path'])
        assert path.is_relative_to(home / 'screenshots') and path.read_bytes().startswith(b'\x89PNG\r\n\x1a\n')
        assert min(struct.unpack('>II', path.read_bytes()[16:24])) > 100, 'Capture has no usable viewport'
        (work / 'page.png').write_bytes(path.read_bytes())
        snap = command('browser.snapshot')
        command('browser.click', ref=ref(snap, 'textbox', 'Name'))
        command('browser.press', key='Tab')
        command('browser.wait', text='Tab received')
        command('browser.scroll', dy=700)
        command('browser.wait', text='Scroll received')
        person = command('browser.open', profile='person', url=url, caller='qa-terminal-b')
        command('browser.read', pane=person['pane'], expect_error='person-profile')
        command('browser.navigate', pane=person['pane'], url=url, expect_error='person-profile')
        # Explicit agent target continues to work even while person pane is selected.
        assert command('browser.read')['pane'] == pane
        command('browser.inspect', pane=person['pane'], confirmed=True)
        command('browser.wait', pane=person['pane'], confirmed=True, load=True)
        assert 'profile_marker=agent-only' not in command('browser.read', pane=person['pane'], confirmed=True)['text']
        command('browser.read', caller='unknown', expect_error='Pass pane explicitly')
        # Navigate invalidates refs even when URL returns to the same page.
        command('browser.navigate', url=url + 'next')
        command('browser.wait', text='Navigation complete')
        command('browser.click', ref=old, expect_error='snapshot again')
        # A browser split into a tab nobody is viewing still starts and takes commands.
        listing = json.loads(subprocess.run([binary, 'ls'], env=env, text=True, capture_output=True).stdout)
        scratch = next(t['pane'] for s in listing['spaces'] if s['id'] == space['space'] for tab in s['tabs'] if tab['name'] == 'Scratch terminal' for t in tab['terminals'])
        command('tab.new', name='Elsewhere', space=space['space'])
        hidden = command('browser.split', pane=scratch, profile='agent', url=url, caller='qa-terminal-c')['pane']
        deadline = time.monotonic() + 30
        while True:
            time.sleep(.1)
            try:
                if command('browser.inspect', pane=hidden)['title'] == 'Agent control fixture': break
            except AssertionError:
                pass
            assert time.monotonic() < deadline, f'Hidden browser did not start: {work}'
        command('browser.click', pane=hidden, name='Click me')
        assert 'Clicked safely' in command('browser.read', pane=hidden)['text']
        command('pane.close', pane=hidden)
        command('pane.close', pane=pane)
        time.sleep(.2)
        command('browser.read', expect_error='No native browser')
        result = {'ok': True, 'tcp_listeners': listeners, 'binary': binary, 'work': str(work), 'home': str(home), 'session': env['HC_SESSION'], 'front_before': before, 'front_after': front(), 'foreground_audit': foreground_audit.copy(), 'inspection': state, 'screenshot': str(work / 'page.png'), 'commands': transcript}
        (work / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps({k: v for k, v in result.items() if k != 'commands'}, indent=2), flush=True)
    finally:
        (work / 'foreground-audit.json').write_text(json.dumps(foreground_audit, indent=2))
        (work / 'transcript.json').write_text(json.dumps(transcript, indent=2))
        if app.poll() is None:
            try:
                if space: command('space.close', id=space['space'], confirmed=True)
                command('app.quit')
                app.wait(timeout=10)
            except Exception:
                app.terminate(); app.wait(timeout=10)
        # Stopping the core retains kept terminal records. End only this test's
        # terminal first, even if the preview failed before it could close space.
        cleanup = {}
        if space:
            terminal = subprocess.run([str(Path.home() / '.local/bin/hc'), 'kill', space['terminal'], '--json'], env=env, capture_output=True, text=True, timeout=10)
            cleanup['terminal'] = {'code': terminal.returncode, 'stdout': terminal.stdout, 'stderr': terminal.stderr}
        stopped = subprocess.run([str(Path.home() / '.local/bin/hc'), 'server', 'stop', '--json'], env=env, capture_output=True, text=True, timeout=10)
        cleanup['core'] = {'code': stopped.returncode, 'stdout': stopped.stdout, 'stderr': stopped.stderr}
        (work / 'cleanup.json').write_text(json.dumps(cleanup, indent=2))
        assert stopped.returncode == 0, cleanup
        server.shutdown()
        log.close()
        print(f'Evidence: {work}', flush=True)


if __name__ == '__main__': main()
