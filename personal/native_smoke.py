#!/usr/bin/env python3
"""Exercise the real Chromium/CEF in an isolated app home and herdr-core session."""
import argparse
import functools
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
import json
import os
from pathlib import Path
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(ROOT / '.local/preview/Herdr Browser.app/Contents/MacOS/HerdrBrowser'))
    parser.add_argument('--keep', action='store_true')
    args = parser.parse_args()
    work = ROOT / '.local/native-smoke' / str(time.time_ns())
    home = work / 'home'; pages = work / 'pages'
    home.mkdir(parents=True); pages.mkdir()
    (home / 'shared.json').write_text('{"frame.welcomed":"finished"}')
    (home / 'settings.json').write_text('{"sounds":{"enabled":false},"core":"' + str(Path.home() / '.local/bin/hc') + '"}')
    for page in ['one.html', 'two.html']:
        (pages / page).write_text(f'<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><title>{page}</title><style>body{{font:20px system-ui;padding:24px}}button{{font:inherit}}</style><h1>Native Herdr browser</h1><p>Page {page}. Rendered by native Chromium.</p><button onclick="this.textContent=\'Clicked\'">Click test</button><a href="/two.html">Next page</a><script>document.title="{page} / JavaScript ready"</script>')
    (pages / 'download.bin').write_bytes(b'Herdr native download check\n')
    class Handler(SimpleHTTPRequestHandler):
        def do_GET(self):
            self.server.user_agent = self.headers.get('User-Agent')
            super().do_GET()
    handler = functools.partial(Handler, directory=str(pages))
    server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    env = os.environ.copy(); env['HERDR_APP_HOME'] = str(home); env['HC_SESSION'] = f'hb-{os.getpid()}'
    # Exercise the portable bundle's literal ~/ home without changing HOME or
    # touching the person's real app profile.
    try: env['HERDR_APP_HOME'] = '~/' + str(home.relative_to(Path.home()))
    except ValueError: pass
    env.pop('HC_TERMINAL_ID', None)
    env['HERDR_BROWSER_TEST_WINDOW_FRONT'] = '1'
    env['HERDR_BROWSER_TEST_KEYCHAIN'] = 'Herdr Browser Test ' + work.name
    binary = str(Path(args.binary).resolve())
    log = (work / 'app.log').open('w')
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    def command(command_name, **kwargs):
        result = subprocess.run([binary, 'do', command_name, json.dumps(kwargs)], env=env, text=True, capture_output=True, timeout=5)
        value = json.loads(result.stdout)
        if result.returncode or value.get('error'): raise RuntimeError(f'{command_name}: {value}')
        return value
    def until(test, description, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if app.poll() is not None: raise RuntimeError(f'App exited: see {work}/app.log')
            try:
                value = test()
                if value: return value
            except (subprocess.SubprocessError, json.JSONDecodeError, RuntimeError): pass
            time.sleep(.1)
        raise RuntimeError(f'Timed out: {description}; see {work}/app.log')
    try:
        until(lambda: subprocess.run([binary, 'status'], env=env, capture_output=True, timeout=5).returncode == 0, 'app control socket')
        shell = command('space.new', name='Browser smoke', folder=str(work), tab='Terminal')
        url = f'http://127.0.0.1:{server.server_port}'
        browser = command('browser.open', url=url + '/one.html')
        pane = browser['pane']
        subprocess.run([binary], env=env, timeout=5)
        def inspected():
            try:
                value = command('browser.inspect', pane=pane)
            except Exception as error:
                (work / 'inspection.json').write_text(json.dumps({'error': str(error)}))
                raise
            (work / 'inspection.json').write_text(json.dumps(value, indent=2))
            return value
        first = until(lambda: (x if (x := inspected()).get('title') == 'one.html / JavaScript ready' and x.get('native', {}).get('shown') and not x.get('loading') else None), 'native page and JavaScript')
        bounds = first['native']['bounds']; assert bounds[2] > 100 and bounds[3] > 100, first
        command('browser.navigate', pane=pane, url=url + '/two.html')
        until(lambda: inspected().get('title') == 'two.html / JavaScript ready', 'second page')
        command('browser.back', pane=pane)
        until(lambda: inspected().get('title') == 'one.html / JavaScript ready', 'back navigation')
        command('browser.forward', pane=pane)
        until(lambda: inspected().get('title') == 'two.html / JavaScript ready', 'forward navigation')
        command('tab.prev')
        until(lambda: not inspected().get('native', {}).get('shown'), 'hidden browser tab')
        command('tab.next')
        until(lambda: inspected().get('native', {}).get('shown'), 'restored browser tab')
        split = command('browser.open', pane=pane, place='right', url=url + '/one.html')
        def split_bounds():
            left = inspected()['native']['bounds']
            right = command('browser.inspect', pane=split['pane']).get('native', {})
            bounds = right.get('bounds')
            return bounds and right.get('shown') and left[0] + left[2] <= bounds[0] + 1
        until(split_bounds, 'two native browsers within split bounds')
        command('pane.close', pane=split['pane'])
        command('app.settings')
        until(lambda: not inspected().get('native', {}).get('shown'), 'native view behind app dialog')
        command('overlay.close')
        until(lambda: inspected().get('native', {}).get('shown'), 'native view after dialog closes')
        until(lambda: inspected().get('native', {}).get('focused'), 'native page focus after dialog closes')
        command('browser.navigate', pane=pane, url=url + '/download.bin')
        until(lambda: (home / 'downloads/download.bin').exists(), 'native download')
        assert (home / 'downloads/download.bin').read_bytes() == b'Herdr native download check\n'
        command('browser.navigate', pane=pane, url=url + '/one.html')
        until(lambda: inspected().get('title') == 'one.html / JavaScript ready', 'page after download')
        command('browser.focus_url', pane=pane)
        report = {'ok': True, 'home': str(home), 'session': env['HC_SESSION'], 'browser': browser,
                  'inspection': inspected(), 'user_agent': getattr(server, 'user_agent', None), 'checks': ['page load', 'JavaScript', 'back/forward', 'tab next/prev', 'split bounds', 'dialog hide/show', 'native download', 'address focus']}
        (work / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), flush=True)
        if args.keep:
            print('Keeping the isolated preview open. Press Ctrl+C here to close it.', flush=True)
            app.wait()
    except Exception as error:
        print(f'SMOKE FAILED: {error}', flush=True)
        if args.keep and app.poll() is None:
            print(f'Keeping failed preview for inspection: {home}', flush=True)
            app.wait()
        raise
    finally:
        if app.poll() is None:
            try: command('app.quit'); app.wait(timeout=10)
            except Exception: app.terminate(); app.wait(timeout=10)
        subprocess.run([str(Path.home() / '.local/bin/hc'), 'server', 'stop'], env=env, capture_output=True, timeout=10)
        server.shutdown(); log.close()


if __name__ == '__main__': main()
