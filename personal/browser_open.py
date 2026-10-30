#!/usr/bin/env python3
"""Open webpages in Herdr Browser, starting the macOS app when needed."""
from pathlib import Path
import argparse
import subprocess
import sys
import time


def open_urls(urls, *, launcher=None, app=None, timeout=15):
    if not urls:
        raise ValueError('Usage: herdr-browser-open <url> [url ...]')
    launcher = str(launcher or Path.home() / '.local/bin/herdr-browser')
    app = str(app or Path.home() / 'Applications/Herdr Browser.app')

    def running():
        return subprocess.run([launcher, 'status'], stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL).returncode == 0

    if not running():
        updater = Path(launcher).with_name('herdr-browser-update')
        if updater.is_file():
            # Install an already validated update before opening a closed app.
            # The installer independently checks that no app is running.
            updated = subprocess.run([str(updater), 'install'], stdout=subprocess.DEVNULL)
            if updated.returncode:
                print('herdr-browser-open: update installation failed; opening the installed app.', file=sys.stderr)
        subprocess.run(['/usr/bin/open', app], check=True)
        deadline = time.monotonic() + timeout
        while not running():
            if time.monotonic() >= deadline:
                raise RuntimeError('Herdr Browser did not become ready; run herdr-browser to inspect it.')
            time.sleep(.1)
    for url in urls:
        subprocess.run([launcher, 'do', 'browser.open', '--url', url], check=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--launcher', type=Path)
    parser.add_argument('--app', type=Path)
    parser.add_argument('urls', nargs='+')
    args = parser.parse_args()
    try:
        open_urls(args.urls, launcher=args.launcher, app=args.app)
    except (ValueError, RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f'herdr-browser-open: {error}', file=sys.stderr)
        raise SystemExit(1)
