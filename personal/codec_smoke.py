#!/usr/bin/env python3
"""Prove H.264/AAC playback in a scratch preview (own home/core, non-activating).

Run through clankie heavy -- python3 personal/codec_smoke.py --playwright-core <dir>
[--binary <app binary>] [--public-url <mp4>]... [--youtube <url>] [--expect-stock].

Serves a generated MP4 with Range support, opens it in an agent pane and drives
it through the guarded Playwright pipe: canPlayType, decoded frames, advancing
currentTime, then a screenshot. --expect-stock inverts the assertion, so the same
run on an unpatched framework shows what the codec build changes. Never launches
the installed app.
"""
import argparse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
PAGE = b'''<!doctype html><title>Codec fixture</title>
<style>body{margin:0;background:#111;color:#eee;font:16px system-ui}video{width:640px;display:block}</style>
<h1>H.264/AAC fixture</h1><video id="v" src="/fixture.mp4" muted playsinline loop></video><pre id="o"></pre>'''


class Handler(SimpleHTTPRequestHandler):
    """Static files plus the single-range responses media elements ask for."""
    def log_message(self, *_): pass

    def do_GET(self):
        match = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers.get('Range', ''))
        path = Path(self.translate_path(self.path))
        if not match or not path.is_file(): return super().do_GET()
        data = path.read_bytes()
        start, end = int(match[1]), int(match[2] or len(data) - 1)
        end = min(end, len(data) - 1)
        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(str(path)))
        self.send_header('Accept-Ranges', 'bytes')
        self.send_header('Content-Range', f'bytes {start}-{end}/{len(data)}')
        self.send_header('Content-Length', str(end - start + 1))
        self.end_headers()
        self.wfile.write(data[start:end + 1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default=str(ROOT / '.local/preview/Herdr Browser.app/Contents/MacOS/HerdrBrowser'))
    parser.add_argument('--playwright-core', required=True)
    parser.add_argument('--public-url', action='append', default=[])
    parser.add_argument('--youtube')
    parser.add_argument('--expect-stock', action='store_true')
    args = parser.parse_args()
    work = ROOT / '.local/codec-smoke' / str(time.time_ns())
    pages = work / 'pages'
    pages.mkdir(parents=True)
    home = Path('/tmp') / f'hb-codec-{os.getpid()}-{work.name[-6:]}'
    home.mkdir()
    home = home.resolve()
    (home / 'shared.json').write_text('{"frame.welcomed":"finished"}')
    (home / 'settings.json').write_text(json.dumps({'sounds': {'enabled': False}, 'telemetry': False, 'core': str(Path.home() / '.local/bin/hc')}))
    (pages / 'index.html').write_bytes(PAGE)
    fixture = pages / 'fixture.mp4'
    subprocess.run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=640x360:rate=30:duration=20',
                    '-f', 'lavfi', '-i', 'sine=frequency=440:duration=20', '-c:v', 'libx264', '-profile:v', 'baseline',
                    '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-movflags', '+faststart', str(fixture)], check=True)
    probe = subprocess.run(['ffprobe', '-v', 'error', '-show_entries', 'stream=codec_name,profile', '-of', 'json', str(fixture)],
                           check=True, capture_output=True, text=True).stdout
    handler = lambda *a, **k: Handler(*a, directory=str(pages), **k)
    server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f'http://127.0.0.1:{server.server_port}/'
    env = os.environ.copy()
    env.update(HERDR_APP_HOME=str(home), HC_SESSION=f'hb-codec-{os.getpid()}', HERDR_APP_BACKGROUND='1',
               HERDR_BROWSER_TEST_KEYCHAIN=f'Herdr Browser Codec QA {work.name}', HERDR_TELEMETRY='0')
    for name in ['HERDR_BROWSER_DEBUG_PORT', 'HERDR_BROWSER_TEST_WINDOW_FRONT', 'HC_TERMINAL_ID', 'HERDR_PANE_ID', 'HERDR_BROWSER_CDP']:
        env.pop(name, None)
    binary = str(Path(args.binary).resolve())
    assert '/.local/' in binary and not binary.startswith(str(Path.home() / 'Applications/'))
    log = (work / 'app.log').open('w')
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)

    def command(command_name, **kwargs):
        cli_env = env.copy()
        cli_env['HC_TERMINAL_ID'] = 'codec-qa-terminal'
        out = subprocess.run([binary, 'do', command_name, json.dumps(kwargs)], env=cli_env, text=True, capture_output=True, timeout=45)
        value = json.loads(out.stdout)
        assert out.returncode == 0 and 'error' not in value, value
        return value

    space = None
    try:
        deadline = time.monotonic() + 60
        while not (home / 'herdr-app.sock').exists():
            assert app.poll() is None, f'App exited; {work}/app.log'
            assert time.monotonic() < deadline, f'No socket; {work}/app.log'
            time.sleep(.1)
        while True:
            try:
                space = command('space.new', name='Codec QA', folder=str(work), tab='Scratch terminal')
                break
            except AssertionError as error:
                if 'unavailable' not in str(error) or time.monotonic() > deadline: raise
                time.sleep(.5)
        agent = command('browser.open', profile='agent', url=url, place='tab')['pane']
        deadline = time.monotonic() + 30
        while (state := command('browser.inspect', pane=agent))['title'] != 'Codec fixture' or state['loading']:
            assert time.monotonic() < deadline, state
            time.sleep(.1)
        cdp = command('browser.cdp', pane=agent)
        options = {'fixture': url, 'shots': str(work), 'public': args.public_url, 'youtube': args.youtube}
        run = subprocess.run(['node', str(ROOT / 'personal/codec_playwright.mjs'), args.playwright_core, cdp['endpoint'], json.dumps(options)],
                             capture_output=True, text=True, timeout=240)
        (work / 'playwright.log').write_text(run.stdout + run.stderr)
        assert run.returncode == 0, run.stdout + run.stderr
        report = json.loads(run.stdout.strip().splitlines()[-1])
        local = report['local']
        if args.expect_stock:
            assert local['canPlayType'] == '' and local['advanced'] == 0, local
        else:
            assert local['canPlayType'] == 'probably', local
            assert local['advanced'] > 1 and local['frames'] > 0 and local['videoWidth'] == 640 and local['audioBytes'] > 0, local
            for item in report['public']:
                assert item['advanced'] > 1 and item['frames'] > 0, item
        result = {'ok': True, 'expect_stock': args.expect_stock, 'binary': binary, 'streams': json.loads(probe)['streams'], **report, 'work': str(work)}
        (work / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result, indent=2), flush=True)
    finally:
        if app.poll() is None:
            try:
                if space: command('space.close', id=space['space'], confirmed=True)
                command('app.quit')
                app.wait(timeout=10)
            except Exception:
                app.terminate(); app.wait(timeout=10)
        hc = str(Path.home() / '.local/bin/hc')
        if space: subprocess.run([hc, 'kill', space['terminal'], '--json'], env=env, capture_output=True, text=True, timeout=10)
        subprocess.run([hc, 'server', 'stop', '--json'], env=env, capture_output=True, text=True, timeout=10)
        server.shutdown()
        log.close()
        print(f'Evidence: {work}', flush=True)


if __name__ == '__main__':
    main()
