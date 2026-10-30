#!/usr/bin/env python3
"""Carry the personal native-browser patch across stable Herdr releases."""
from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
LOCAL = ROOT / '.local' / 'browser-updater'
APP = Path.home() / 'Applications' / 'Herdr Browser.app'
APP_HOME = Path.home() / '.config' / 'herdr-app-browser'
RELEASE_FEED = 'https://releases.herdr.dev/app/latest.json'
UPSTREAM = 'https://github.com/herdrdev/herdr-app.git'
ENGINE_FEED = 'https://crates.io/api/v1/crates/cef'
LABEL = 'dev.james.herdr-browser-updater'
FEATURE_PATHS = ['Cargo.toml', 'src/main.rs', 'src/window.rs', 'src/commands.rs', 'src/mods.rs',
                 'src/browser.rs', 'src/browser', 'src/terminal.rs', 'src/hc.rs', 'src/SKILL.md', 'defaults/AGENTS.md',
                 'defaults/mods/new-menu', 'defaults/mods/native-browser',
                 'defaults/keys.macos.json',
                 'licenses/CEF.txt', 'licenses/inventory.json']


def run(args, *, cwd=ROOT, env=None, capture=False, check=True):
    return subprocess.run([str(a) for a in args], cwd=cwd, env=env, check=check,
                          stdout=subprocess.PIPE if capture else sys.stdout,
                          stderr=subprocess.PIPE if capture else sys.stderr)


def build_step(args, *, cwd, env, output, timeout):
    """Bound an owned build process tree, including cargo's compiler/test children."""
    process = subprocess.Popen(args, cwd=cwd, env=env, stdout=output,
                               stderr=subprocess.STDOUT, start_new_session=True)
    try:
        code = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try: process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL); process.wait()
        raise RuntimeError(f'Build step timed out after {timeout}s: {args}')
    if code: raise subprocess.CalledProcessError(code, args)


def git(*args, cwd=ROOT):
    return run(['git', *args], cwd=cwd, capture=True).stdout.decode().strip()


def read_json(path, default=None):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return {} if default is None else default


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(value, indent=2) + '\n')
    tmp.replace(path)


def stamp():
    return dt.datetime.now(dt.timezone.utc).isoformat()


def export_patch(root, base, paths):
    patch = run(['git', 'diff', '--binary', base, '--', *paths], cwd=root, capture=True).stdout
    files = git('ls-files', '--others', '--exclude-standard', '--', *paths, cwd=root).splitlines()
    for name in sorted(files):
        diff = run(['git', 'diff', '--no-index', '--binary', '--', '/dev/null', name], cwd=root, capture=True, check=False)
        if diff.returncode != 1:
            raise RuntimeError(f'Cannot export new source file {name}')
        patch += diff.stdout
    if not patch:
        raise RuntimeError('No browser changes to export')
    return patch


def snapshot(base=None):
    LOCAL.mkdir(parents=True, exist_ok=True)
    base_file = LOCAL / 'base'
    tracked_base = ROOT / 'personal/upstream-base'
    base = base or (tracked_base.read_text().strip() if tracked_base.exists() else
                    base_file.read_text().strip() if base_file.exists() else git('rev-parse', 'HEAD'))
    base = git('rev-parse', f'{base}^{{commit}}')
    patch = export_patch(ROOT, base, FEATURE_PATHS)
    (LOCAL / 'feature.patch').write_bytes(patch)
    base_file.write_text(base + '\n')
    print(f'Captured browser patch: {len(patch)} bytes, base {base[:12]}')


def stable_tag(tag):
    if not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError(f'Not a stable Herdr release tag: {tag!r}')
    return tuple(map(int, tag[1:].split('.')))


def latest_release(state, force=False):
    if not force and time.time() < state.get('next_check', 0):
        return None
    headers = {'Accept': 'application/json', 'User-Agent': 'herdr-browser-updater'}
    if state.get('etag') and not force:
        headers['If-None-Match'] = state['etag']
    request = urllib.request.Request(RELEASE_FEED, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            release = json.load(response)
            state['etag'] = response.headers.get('ETag')
    except urllib.error.HTTPError as error:
        if error.code != 304:
            raise
        state['next_check'] = time.time() + 6 * 60 * 60
        return None
    tag = 'v' + release['version']
    stable_tag(tag)
    if 'macos-aarch64' not in release.get('targets', {}):
        raise RuntimeError('The stable release does not support this Mac')
    state['next_check'] = time.time() + 6 * 60 * 60
    state['release_feed'] = RELEASE_FEED
    return tag


def zig_version(tree):
    ci = (tree / '.github/workflows/ci.yml').read_text()
    match = re.search(r'setup-zig@[^\n]+\n\s+with:\n\s+version:[ \t]*([^\n]+)', ci)
    if not match:
        raise RuntimeError('Cannot determine the upstream Zig version; toolchain needs review')
    value = match[1].strip()
    reference = re.fullmatch(r'\$\{\{\s*env\.([A-Z_]+)\s*\}\}', value)
    if reference:
        setting = re.search(r'(?m)^\s+' + re.escape(reference[1]) + r':[ \t]*([\d.]+)[ \t]*$', ci)
        value = setting[1] if setting else ''
    if not re.fullmatch(r'\d+\.\d+\.\d+', value):
        raise RuntimeError('Cannot determine the upstream Zig version; toolchain needs review')
    return value


def ensure_zig(tree):
    version = zig_version(tree)
    tools = Path.home() / '.local/share/herdr-browser/tools'
    name = f'zig-aarch64-macos-{version}'
    binary = tools / name / 'zig'
    if binary.exists():
        return binary
    tools.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen('https://ziglang.org/download/index.json', timeout=30) as response:
        info = json.load(response)[version]['aarch64-macos']
    if not info['tarball'].startswith('https://ziglang.org/'):
        raise RuntimeError('Unexpected Zig download origin')
    archive = tools / f'zig-{version}.tar.xz'
    with urllib.request.urlopen(info['tarball'], timeout=60) as response, archive.open('wb') as output:
        shutil.copyfileobj(response, output)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != info['shasum']:
        raise RuntimeError('Zig download checksum mismatch')
    with tarfile.open(archive) as tar:
        tar.extractall(tools, filter='data')
    if not binary.exists():
        raise RuntimeError('Zig archive did not contain the expected executable')
    return binary


def source_engine():
    lock = (ROOT / 'Cargo.lock').read_text()
    return re.search(r'\[\[package\]\]\nname = "cef"\nversion = "([^"]+)"', lock)[1]


def latest_engine(state, force=False):
    if not force and time.time() < state.get('next_engine_check', 0):
        return state.get('latest_engine', source_engine())
    request = urllib.request.Request(ENGINE_FEED, headers={'User-Agent': 'herdr-browser-updater'})
    with urllib.request.urlopen(request, timeout=30) as response:
        version = json.load(response)['crate']['max_stable_version']
    validate_engine(version)
    state.update(latest_engine=version, next_engine_check=time.time() + 6 * 60 * 60)
    return version


def validate_engine(version):
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:\+\d+\.\d+\.\d+)?', version):
        raise ValueError(f'Not a stable CEF crate version: {version!r}')
    return tuple(map(int, re.split(r'[.+]', version)))


def pin_engine(tree, version):
    validate_engine(version)
    cargo = tree / 'Cargo.toml'
    text, count = re.subn(r'(?m)^(cef\s*=\s*\{\s*version\s*=\s*")[^"]*(")',
                         lambda m: m[1] + '=' + version + m[2], cargo.read_text())
    if count != 1: raise RuntimeError('Cannot locate the native Chromium dependency')
    cargo.write_text(text)


# Chromium's macOS permission and WebAuthn code can run even before a site
# completes sign-in. Keep these declarations on the app AND its helper bundles,
# matching cef-rs's macOS bundle contract. These strings do not grant access.
BROWSER_USAGE_DESCRIPTIONS = {
    'NSBluetoothAlwaysUsageDescription': 'Use Bluetooth to sign in to websites with a passkey on a nearby phone or security key.',
    'NSWebBrowserPublicKeyCredentialUsageDescription': 'Use passkeys to sign in to websites when you choose a passkey.',
    'NSCameraUsageDescription': 'Allow a website you approve to use your camera for calls or capture.',
    'NSMicrophoneUsageDescription': 'Allow a website you approve to use your microphone for calls or recording.',
}


def validate_browser_bundle(bundle):
    plists = [bundle / 'Contents/Info.plist']
    plists += sorted((bundle / 'Contents/Frameworks').glob('HerdrBrowser Helper*.app/Contents/Info.plist'))
    for path in plists:
        with path.open('rb') as source: info = plistlib.load(source)
        for key in BROWSER_USAGE_DESCRIPTIONS:
            if not isinstance(info.get(key), str) or not info[key].strip():
                raise RuntimeError(f'{path}: missing browser privacy declaration {key}')


def package(binary, destination, version, source, patch_hash, *, bundle_id="dev.james.herdr-browser"):
    macos = destination / 'Contents/MacOS'
    resources = destination / 'Contents/Resources'
    macos.mkdir(parents=True)
    resources.mkdir(parents=True)
    shutil.copy2(binary, macos / 'HerdrBrowser')
    shutil.copy2(source / 'assets/icons/Herdr.icns', resources / 'Herdr.icns')
    for name in ['LICENSE', 'THIRD-PARTY-NOTICES']:
        shutil.copy2(source / name, resources / name)
    helper = binary.parent / 'herdr-browser-helper'
    cef_environment = os.environ.copy()
    cef_environment['CEF_PATH'] = str(Path.home() / '.local/share/herdr-browser/cef')
    cef_dir = Path(run([helper, '--print-cef-path'], env=cef_environment, capture=True).stdout.decode().strip())
    credits = cef_dir / 'CREDITS.html'
    if credits.exists(): shutil.copy2(credits, resources / 'Chromium-Credits.html')
    frameworks = destination / 'Contents/Frameworks'
    frameworks.mkdir()
    framework = cef_dir / 'Chromium Embedded Framework.framework'
    if not framework.is_dir(): framework = cef_dir / 'Release/Chromium Embedded Framework.framework'
    shutil.copytree(framework, frameworks / framework.name, symlinks=True)
    for role in ['', ' (GPU)', ' (Renderer)', ' (Plugin)', ' (Alerts)']:
        name = 'HerdrBrowser Helper' + role
        contents = frameworks / f'{name}.app/Contents'
        (contents / 'MacOS').mkdir(parents=True)
        shutil.copy2(helper, contents / 'MacOS' / name)
        helper_info = {'CFBundleExecutable': name, 'CFBundleName': name,
                       'CFBundleIdentifier': bundle_id + '.helper' + role.replace(' (', '.').replace(')', '').lower(),
                       'CFBundleVersion': version, 'CFBundlePackageType': 'APPL', 'LSUIElement': True}
        helper_info.update(BROWSER_USAGE_DESCRIPTIONS)
        with (contents / 'Info.plist').open('wb') as output: plistlib.dump(helper_info, output)
    info = {
        'CFBundleExecutable': 'HerdrBrowser', 'CFBundleIdentifier': bundle_id,
        'CFBundleName': 'Herdr Browser', 'CFBundleDisplayName': 'Herdr Browser',
        'CFBundleIconFile': 'Herdr', 'CFBundlePackageType': 'APPL',
        'CFBundleShortVersionString': version, 'CFBundleVersion': version,
        'NSPrincipalClass': 'NSApplication', 'NSHighResolutionCapable': True,
        'NSSupportsAutomaticGraphicsSwitching': True,
        'LSEnvironment': {'HERDR_APP_HOME': '~/.config/herdr-app-browser'},
        'HerdrBrowserPatch': patch_hash,
    }
    info.update(BROWSER_USAGE_DESCRIPTIONS)
    with (destination / 'Contents/Info.plist').open('wb') as output:
        plistlib.dump(info, output)
    validate_browser_bundle(destination)
    entitlements = destination.parent / 'chromium-entitlements.plist'
    with entitlements.open('wb') as output:
        plistlib.dump({'com.apple.security.cs.allow-jit': True}, output)
    identity_file = LOCAL / 'signing_identity'
    if bundle_id == 'dev.james.herdr-browser' and not identity_file.exists():
        raise RuntimeError('Configure a stable personal signing identity before packaging automatic updates')
    identity = identity_file.read_text().strip() if identity_file.exists() else '-'
    run(['/usr/bin/codesign', '--force', '--deep', '--sign', identity, '--timestamp=none', '--entitlements', entitlements, destination])
    run(['/usr/bin/codesign', '--verify', '--deep', '--strict', destination])
    listing = run([macos / 'HerdrBrowser', 'commands'], capture=True).stdout
    commands = {item['name'] for item in json.loads(listing)}
    if not {'browser.open', 'browser.navigate', 'browser.inspect'} <= commands:
        raise RuntimeError('Candidate is missing the native browser commands')


def build_candidate(tag, state, *, force=False, engine=None):
    stable_tag(tag)
    engine = engine or source_engine()
    validate_engine(engine)
    patch = LOCAL / 'feature.patch'
    if not patch.exists():
        raise RuntimeError('Run snapshot to capture the reviewed native-browser patch first')
    patch_hash = hashlib.sha256(patch.read_bytes()).hexdigest()
    key = f'{tag}-{patch_hash[:12]}-cef{engine}'
    if not force and state.get('held_key') == key:
        print('Keeping the rollback version until a different update is available.')
        return
    installed = state.get('installed', {})
    if not force and installed.get('tag') == tag and installed.get('patch') == patch_hash and installed.get('engine') == engine:
        return
    if not force and state.get('failed_key') == key:
        print(f'{key} already failed; keeping the installed version (see status/log).')
        return
    if not force and installed.get('tag') and stable_tag(tag) < stable_tag(installed['tag']):
        raise RuntimeError('Refusing an automatic downgrade')
    if not force and installed.get('engine') and validate_engine(engine) < validate_engine(installed['engine']):
        raise RuntimeError('Refusing an automatic Chromium downgrade')
    build_root = LOCAL / 'builds' / f'{key}-{time.time_ns()}'
    tree = build_root / 'source'
    build_root.mkdir(parents=True)
    logfile = build_root / 'build.log'
    state.update(status='building', target=tag, log=str(logfile), checked=stamp(), error=None)
    write_json(LOCAL / 'state.json', state)
    try:
        with logfile.open('a', buffering=1) as log, contextlib.redirect_stdout(log), contextlib.redirect_stderr(log):
            # origin is James's fork on a fresh clone; release tags always come
            # from the official project, independently of local remote names.
            run(['git', 'fetch', '--no-tags', UPSTREAM, f'refs/tags/{tag}:refs/tags/{tag}'])
            commit = git('rev-parse', f'refs/tags/{tag}^{{commit}}')
            run(['git', 'worktree', 'add', '--detach', tree, commit])
            run(['git', 'apply', '--check', patch], cwd=tree)
            run(['git', 'apply', patch], cwd=tree)
            pin_engine(tree, engine)
            run(['git', 'submodule', 'update', '--init', '--depth', '1'], cwd=tree)
            environment = os.environ.copy()
            environment.pop('HERDR_APP_RELEASE', None)
            environment['ZIG'] = str(ensure_zig(tree))
            environment['CARGO_TARGET_DIR'] = str(ROOT / 'target')
            environment['CEF_PATH'] = str(Path.home() / '.local/share/herdr-browser/cef')
            # xcrun can pick a Command Line Tools SDK newer than the selected Xcode's linker.
            developer = Path(run(['xcode-select', '-p'], capture=True).stdout.decode().strip())
            sdk = developer / 'Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk'
            if sdk.exists(): environment.setdefault('SDKROOT', str(sdk))
            # Cargo preserves upstream's lockfile and resolves only what the patch adds.
            with logfile.open('a', buffering=1) as child_log:
                build_step(['cargo', 'build', '--release', '--bins'], cwd=tree, env=environment,
                           output=child_log, timeout=1800)
                build_step(['cargo', 'test', '--locked', '--bin', 'herdr-app', '--no-run'], cwd=tree, env=environment,
                           output=child_log, timeout=1800)
                build_step(['cargo', 'test', '--locked', '--bin', 'herdr-app', '--', '--test-threads=1'], cwd=tree, env=environment,
                           output=child_log, timeout=120)
                about = ROOT / '.local/tools/cargo-about/cargo-about'
                if about.exists(): environment['CARGO_ABOUT'] = str(about)
                build_step(['bun', 'scripts/notices.ts'], cwd=tree, env=environment,
                           output=child_log, timeout=120)
            candidate = build_root / 'Herdr Browser.app'
            package(ROOT / 'target/release/herdr-app', candidate, tag[1:], tree, patch_hash)
        state.update(status='staged', pending={'bundle': str(candidate), 'tag': tag,
                     'commit': commit, 'patch': patch_hash, 'engine': engine, 'built': stamp()}, failed_key=None, error=None)
    except Exception as error:
        state.update(status='failed', failed_key=key, error=str(error))
        write_json(LOCAL / 'state.json', state)
        raise
    write_json(LOCAL / 'state.json', state)


def app_running(app=APP):
    executable = str(app / 'Contents/MacOS/HerdrBrowser')
    output = run(['/bin/ps', '-axo', 'comm='], capture=True).stdout.decode()
    return any(line.strip() == executable for line in output.splitlines())


def promote(candidate, app=APP, *, running=app_running):
    if running(app):
        return False
    app.parent.mkdir(parents=True, exist_ok=True)
    incoming = app.with_name(app.name + '.incoming.noindex')
    previous = app.with_name(app.name + '.previous.noindex')
    if incoming.exists():
        shutil.rmtree(incoming)
    shutil.copytree(candidate, incoming)
    # The bundle copy can take time; check again at the promotion boundary.
    if running(app):
        shutil.rmtree(incoming)
        return False
    if previous.exists():
        shutil.rmtree(previous)
    had_app = app.exists()
    if had_app:
        app.rename(previous)
    try:
        incoming.rename(app)
    except Exception:
        if had_app:
            previous.rename(app)
        raise
    return True


def install_pending(state):
    pending = state.get('pending')
    if not pending:
        return
    candidate = Path(pending['bundle'])
    run(['/usr/bin/codesign', '--verify', '--deep', '--strict', candidate])
    migrate_config()
    if not promote(candidate, APP):
        state['status'] = 'staged'
        print('Validated update is staged; the working app stays in place until it closes.')
        write_json(LOCAL / 'state.json', state)
        return
    state['previous'] = state.get('installed')
    state['installed'] = pending
    state['pending'] = None
    state['status'] = 'installed'
    write_json(LOCAL / 'state.json', state)
    write_launchers()
    print(f'Installed Herdr Browser {pending["tag"]}')


def migrate_config(source=None, destination=None):
    """Copy the existing appearance once; future updates preserve personal edits."""
    source = source or Path.home() / '.config/herdr-app'
    destination = destination or APP_HOME
    if destination.exists():
        return
    destination.mkdir(parents=True)
    for name in ['settings.json', 'keys.json', 'shared.json', 'window.json', 'view.json',
                 'installed.json', 'seen.json', 'layouts', 'mods', 'themes', 'icons', 'data']:
        item = source / name
        if item.is_dir(): shutil.copytree(item, destination / name)
        elif item.is_file(): shutil.copy2(item, destination / name)
    # James's existing Ghostty layout points at this personal menu clone.
    menu = destination / 'mods/new-menu-browser/main.js'
    if menu.exists():
        text = menu.read_text()
        text = text.replace('run("tab.new", { name: "Browser", space: space?.id, argv: ["/Users/james/.local/bin/terminal-browser"] })',
                            'run("browser.open", { space: space?.id })')
        text = text.replace('space?.machine ? "local only" : "", !space?.machine)', '"", true)')
        menu.write_text(text)


def write_launchers():
    folder = Path.home() / '.local/bin'
    folder.mkdir(parents=True, exist_ok=True)
    launcher = folder / 'herdr-browser'
    launcher.write_text('#!/bin/sh\nexport HERDR_APP_HOME=' + shlex.quote(str(APP_HOME)) + '\nexec ' +
                        shlex.quote(str(APP / 'Contents/MacOS/HerdrBrowser')) + ' "$@"\n')
    launcher.chmod(0o755)
    opener = folder / 'herdr-browser-open'
    opener.write_text('#!/bin/sh\nexec ' + shlex.join([sys.executable, str(ROOT / 'personal/browser_open.py'),
                      '--launcher', str(launcher), '--app', str(APP), '--']) + ' "$@"\n')
    opener.chmod(0o755)
    updater = folder / 'herdr-browser-update'
    updater.write_text('#!/bin/sh\nexec ' + shlex.join([sys.executable, str(Path(__file__).resolve())]) + ' "$@"\n')
    updater.chmod(0o755)


def enable_watcher():
    write_launchers()
    agent = Path.home() / 'Library/LaunchAgents' / f'{LABEL}.plist'
    agent.parent.mkdir(parents=True, exist_ok=True)
    info = {'Label': LABEL, 'ProgramArguments': [sys.executable, str(Path(__file__).resolve()), 'check'],
            'StartInterval': 900, 'RunAtLoad': True, 'ProcessType': 'Background',
            'LowPriorityIO': True, 'Nice': 10,
            'EnvironmentVariables': {'PATH': f'{Path.home()}/.cargo/bin:{Path.home()}/.local/bin:/opt/homebrew/bin:/usr/bin:/bin'},
            'StandardOutPath': str(LOCAL / 'watcher.log'), 'StandardErrorPath': str(LOCAL / 'watcher.log')}
    with agent.open('wb') as output:
        plistlib.dump(info, output)
    loaded = run(['launchctl', 'print', f'gui/{os.getuid()}/{LABEL}'], capture=True, check=False).returncode == 0
    if not loaded:
        run(['launchctl', 'bootstrap', f'gui/{os.getuid()}', agent])
    print(f'Watching stable releases every six hours; staged updates are checked every 15 minutes.\n{agent}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['snapshot', 'build', 'check', 'install', 'status', 'enable', 'rollback'])
    parser.add_argument('--tag')
    parser.add_argument('--base')
    parser.add_argument('--engine', help='Pinned stable CEF crate version for a manual build')
    parser.add_argument('--force', action='store_true')
    args = parser.parse_args()
    LOCAL.mkdir(parents=True, exist_ok=True)
    with (LOCAL / 'lock').open('w') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print('An update is already in progress.')
            return 0
        state = read_json(LOCAL / 'state.json')
        if args.command == 'snapshot': snapshot(args.base)
        elif args.command == 'status': print(json.dumps(state, indent=2))
        elif args.command == 'enable': enable_watcher()
        elif args.command == 'rollback':
            previous = APP.with_name(APP.name + '.previous.noindex')
            if not previous.exists(): raise RuntimeError('No previous app is available')
            if app_running(): raise RuntimeError('Close Herdr Browser before rolling back')
            spare = APP.with_name(APP.name + '.rollback.noindex')
            if spare.exists(): shutil.rmtree(spare)
            current = state.get('installed', {})
            state['held_key'] = f'{current.get("tag")}-{current.get("patch", "")[:12]}-cef{current.get("engine")}'
            APP.rename(spare); previous.rename(APP); spare.rename(previous)
            state['installed'], state['previous'] = state.get('previous'), state.get('installed')
            state['status'] = 'rolled_back'; write_json(LOCAL / 'state.json', state)
        elif args.command == 'install': install_pending(state); write_launchers()
        else:
            install_pending(state)
            tag = args.tag or latest_release(state, force=args.force)
            engine = args.engine or (latest_engine(state, force=args.force) if args.command == 'check' else source_engine())
            if not tag and state.get('installed', {}).get('engine') != engine:
                tag = state.get('installed', {}).get('tag')
            write_json(LOCAL / 'state.json', state)
            if tag: build_candidate(tag, state, force=args.force, engine=engine)
            if args.command == 'check': install_pending(state)
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f'herdr-browser-update: {error}', file=sys.stderr)
        raise SystemExit(1)
