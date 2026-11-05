import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import urllib.error
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('browser_update', Path(__file__).with_name('browser_update.py'))
updater = importlib.util.module_from_spec(spec)
spec.loader.exec_module(updater)


class PatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Update test')
        self.git('config', 'user.email', 'update-test@localhost')
        (self.root / 'host.txt').write_text('before\n')
        self.git('add', 'host.txt'); self.git('commit', '-qm', 'base')
        self.base = self.git('rev-parse', 'HEAD').strip()

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root).decode()

    def patch(self):
        (self.root / 'host.txt').write_text('native browser\n')
        (self.root / 'browser.rs').write_text('new native source\n')
        data = updater.export_patch(self.root, self.base, ['host.txt', 'browser.rs'])
        path = self.root / 'feature.patch'; path.write_bytes(data)
        self.git('restore', 'host.txt'); (self.root / 'browser.rs').unlink()
        return path

    def test_carries_new_source_and_preserves_independent_upstream_changes(self):
        patch = self.patch()
        (self.root / 'upstream.txt').write_text('new upstream feature\n')
        self.git('add', 'upstream.txt'); self.git('commit', '-qm', 'upstream release')
        self.git('apply', '--check', str(patch)); self.git('apply', str(patch))
        self.assertEqual((self.root / 'host.txt').read_text(), 'native browser\n')
        self.assertEqual((self.root / 'browser.rs').read_text(), 'new native source\n')
        self.assertEqual((self.root / 'upstream.txt').read_text(), 'new upstream feature\n')

    def test_conflict_refuses_to_modify_candidate(self):
        patch = self.patch()
        (self.root / 'host.txt').write_text('upstream redesigned host\n')
        result = subprocess.run(['git', 'apply', '--check', str(patch)], cwd=self.root, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.root / 'host.txt').read_text(), 'upstream redesigned host\n')
        self.assertFalse((self.root / 'browser.rs').exists())

    def test_browser_manifest_patch_survives_an_upstream_version_bump(self):
        current = (updater.ROOT / 'Cargo.toml').read_text()
        upstream = current.replace('default-run = "herdr-app"\n', '')
        cargo = self.root / 'Cargo.toml'; cargo.write_text(upstream)
        self.git('add', 'Cargo.toml'); self.git('commit', '-qm', 'upstream manifest')
        base = self.git('rev-parse', 'HEAD').strip()
        cargo.write_text(current)
        patch = self.root / 'manifest.patch'
        patch.write_bytes(updater.export_patch(self.root, base, ['Cargo.toml']))
        cargo.write_text(re.sub(r'(?m)^version = "[^"]+"', 'version = "99.0.0"', upstream))
        self.git('add', 'Cargo.toml'); self.git('commit', '-qm', 'next upstream release')
        self.git('apply', '--check', str(patch)); self.git('apply', str(patch))
        self.assertIn('default-run = "herdr-app"', cargo.read_text())
        self.assertIn('version = "99.0.0"', cargo.read_text())

    def test_fresh_clone_snapshots_committed_browser_changes_from_tracked_base(self):
        personal = self.root / 'personal'; personal.mkdir()
        shutil.copy2(updater.__file__, personal / 'browser_update.py')
        (personal / 'upstream-base').write_text(self.base + '\n')
        (self.root / 'src').mkdir()
        (self.root / 'src/browser.rs').write_text('committed browser implementation\n')
        self.git('add', '.'); self.git('commit', '-qm', 'browser fork')
        subprocess.run([sys.executable, personal / 'browser_update.py', 'snapshot'],
                       cwd=self.root, check=True, capture_output=True)
        patch = (self.root / '.local/browser-updater/feature.patch').read_text()
        self.assertIn('committed browser implementation', patch)
        self.assertEqual((self.root / '.local/browser-updater/base').read_text().strip(), self.base)
        # Pulling a newer tracked baseline must not leave snapshot pinned to a
        # machine's stale cache from a previous release.
        (self.root / '.local/browser-updater/base').write_text(self.git('rev-parse', 'HEAD'))
        subprocess.run([sys.executable, personal / 'browser_update.py', 'snapshot'],
                       cwd=self.root, check=True, capture_output=True)
        self.assertEqual((self.root / '.local/browser-updater/base').read_text().strip(), self.base)


class PromotionTests(unittest.TestCase):
    def test_automatic_install_writes_a_working_browser_opener(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); home = root / 'home with $literal'; app = home / 'Applications/Herdr Browser.app'
            candidate = root / 'Candidate.app'
            binary = candidate / 'Contents/MacOS/HerdrBrowser'; binary.parent.mkdir(parents=True)
            binary.write_text('#!' + sys.executable + '\nimport json, os, sys\nfrom pathlib import Path\nPath(os.environ["LINK_CAPTURE"]).write_text(json.dumps({"argv": sys.argv[1:], "home": os.environ["HERDR_APP_HOME"]}))\n')
            binary.chmod(0o755)
            app_home = home / '.config/herdr-app-browser'; app_home.mkdir(parents=True)
            local = root / 'updater'
            state = {'pending': {'bundle': str(candidate), 'tag': 'v0.1.10'}}
            fake = subprocess.CompletedProcess([], 0, stdout=b'')
            with patch.object(updater.Path, 'home', return_value=home), \
                 patch.object(updater, 'APP', app), patch.object(updater, 'APP_HOME', app_home), \
                 patch.object(updater, 'LOCAL', local), patch.object(updater, 'run', return_value=fake):
                updater.install_pending(state)
            self.assertEqual(state['status'], 'installed')
            opener = home / '.local/bin/herdr-browser-open'
            self.assertIn('--launcher', opener.read_text())
            self.assertIn(str(home / '.local/bin/herdr-browser'), opener.read_text())
            url = 'https://example.test/search?q=hello world&mode=inline'
            capture = root / 'link.json'
            import os
            env = dict(os.environ, LINK_CAPTURE=str(capture))
            subprocess.run([str(opener), url], env=env, check=True)
            actual = json.loads(capture.read_text())
            self.assertEqual(actual['argv'], ['do', 'browser.open', '--url', url])
            self.assertEqual(actual['home'], str(app_home))

    def test_config_migration_preserves_source_and_runs_only_once(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); source = root / 'stock'; target = root / 'browser'
            menu = source / 'mods/new-menu-browser/main.js'; menu.parent.mkdir(parents=True)
            original = 'run("tab.new", { name: "Browser", space: space?.id, argv: ["/Users/james/.local/bin/terminal-browser"] })'
            menu.write_text(original)
            updater.migrate_config(source, target)
            self.assertEqual(menu.read_text(), original)
            copied = target / 'mods/new-menu-browser/main.js'
            self.assertIn('browser.open', copied.read_text())
            copied.write_text('personal edit')
            updater.migrate_config(source, target)
            self.assertEqual(copied.read_text(), 'personal edit')

    def test_running_app_is_never_replaced(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); old = root / 'App.app'; new = root / 'Candidate.app'
            old.mkdir(); new.mkdir()
            (old / 'version').write_text('working'); (new / 'version').write_text('new')
            self.assertFalse(updater.promote(new, old, running=lambda app: True))
            self.assertEqual((old / 'version').read_text(), 'working')

    def test_closed_app_is_replaced_and_previous_build_is_retained(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); old = root / 'App.app'; new = root / 'Candidate.app'
            old.mkdir(); new.mkdir()
            (old / 'version').write_text('working'); (new / 'version').write_text('new')
            self.assertTrue(updater.promote(new, old, running=lambda app: False))
            self.assertEqual((old / 'version').read_text(), 'new')
            self.assertEqual((root / 'App.app.previous.noindex/version').read_text(), 'working')

    def test_app_opened_during_copy_is_not_replaced(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); old = root / 'App.app'; new = root / 'Candidate.app'
            old.mkdir(); new.mkdir()
            (old / 'version').write_text('working'); (new / 'version').write_text('new')
            checks = iter([False, True])
            self.assertFalse(updater.promote(new, old, running=lambda app: next(checks)))
            self.assertEqual((old / 'version').read_text(), 'working')
            self.assertFalse((root / 'App.app.incoming.noindex').exists())

    def test_only_stable_version_tags_are_accepted(self):
        for tag in ['master', 'v0.2.0-preview', '../bad', 'v1.2.3;whoami']:
            with self.assertRaises(ValueError): updater.stable_tag(tag)
        self.assertEqual(updater.stable_tag('v1.2.3'), (1, 2, 3))


class FeedTests(unittest.TestCase):
    def response(self, version):
        response = io.BytesIO(json.dumps({'version': version, 'targets': {'macos-aarch64': {}}}).encode())
        response.headers = {'ETag': 'stable-release'}
        return response

    def test_official_feed_maps_to_github_tag_and_throttles_checks(self):
        state = {}
        with patch.object(updater.urllib.request, 'urlopen', return_value=self.response('0.1.9')) as fetch:
            self.assertEqual(updater.latest_release(state), 'v0.1.9')
            self.assertEqual(fetch.call_args.args[0].full_url, updater.RELEASE_FEED)
            self.assertEqual(updater.latest_release(state), None)
            self.assertEqual(fetch.call_count, 1)
        self.assertEqual(state['etag'], 'stable-release')

    def test_preview_version_is_not_treated_as_stable(self):
        with patch.object(updater.urllib.request, 'urlopen', return_value=self.response('0.2.0-preview')):
            with self.assertRaises(ValueError): updater.latest_release({})


class EngineTests(unittest.TestCase):
    def test_zig_version_accepts_literal_and_upstream_environment_setting(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); ci = root / '.github/workflows/ci.yml'
            ci.parent.mkdir(parents=True)
            ci.write_text('steps:\n  - uses: mlugg/setup-zig@hash\n    with:\n      version: 0.16.0\n')
            self.assertEqual(updater.zig_version(root), '0.16.0')
            ci.write_text('env:\n  ZIG_VERSION: 0.16.0\nsteps:\n  - uses: mlugg/setup-zig@hash\n    with:\n      version: ${{ env.ZIG_VERSION }}\n')
            self.assertEqual(updater.zig_version(root), '0.16.0')
            ci.write_text(ci.read_text().replace('ZIG_VERSION: 0.16.0', 'ZIG_VERSION: unknown'))
            with self.assertRaisesRegex(RuntimeError, 'toolchain needs review'):
                updater.zig_version(root)

    def test_engine_pin_is_scoped_to_cef_dependency(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            cargo = root / 'Cargo.toml'
            cargo.write_text('version = "0.1.9"\ncef = { version = "=156.2.0", features = ["sandbox"] }\n')
            updater.pin_engine(root, '156.3.0+156.0.15')
            self.assertEqual(cargo.read_text(), 'version = "0.1.9"\ncef = { version = "=156.3.0+156.0.15", features = ["sandbox"] }\n')

    def test_engine_feed_selects_only_a_stable_version(self):
        response = io.BytesIO(b'{"crate":{"max_stable_version":"156.2.0+156.0.14"}}')
        state = {}
        with patch.object(updater.urllib.request, 'urlopen', return_value=response) as fetch:
            self.assertEqual(updater.latest_engine(state), '156.2.0+156.0.14')
            self.assertEqual(updater.latest_engine(state), '156.2.0+156.0.14')
            self.assertEqual(fetch.call_count, 1)
        for invalid in ['156.2.0-beta', '../156.2.0', 'latest']:
            with self.assertRaises(ValueError): updater.validate_engine(invalid)


class BuildTimeoutTests(unittest.TestCase):
    def test_owned_build_step_times_out_without_stopping_its_parent(self):
        with tempfile.TemporaryDirectory() as temp, open('/dev/null', 'w') as output:
            with self.assertRaisesRegex(RuntimeError, 'timed out'):
                updater.build_step([sys.executable, '-c', 'import time; time.sleep(30)'],
                                   cwd=temp, env=None, output=output, timeout=.1)


class BrowserBundleTests(unittest.TestCase):
    # Platform requirements are spelled out independently of the packager's
    # mapping so removing a declaration cannot silently weaken this regression.
    required = {'NSBluetoothAlwaysUsageDescription',
                'NSWebBrowserPublicKeyCredentialUsageDescription',
                'NSCameraUsageDescription', 'NSMicrophoneUsageDescription'}

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bundle = self.root / 'Herdr Browser.app'
        self.signed = []

    def framework(self, parent, version, binary):
        framework = parent / updater.CEF_FRAMEWORK
        (framework / 'Resources').mkdir(parents=True)
        with (framework / 'Resources/Info.plist').open('wb') as plist:
            plistlib.dump({'CFBundleShortVersionString': version + '.0'}, plist)
        (framework / 'Chromium Embedded Framework').write_bytes(binary)

    def pin_archive(self, version='156.0.15'):
        """A codec archive shaped like the real one, pinned for this test."""
        build = self.root / 'codec-build'; build.mkdir()
        self.framework(build, version, b'codec')
        (build / updater.CEF_FRAMEWORK / 'Resources/._Info.plist').write_bytes(b'appledouble')  # as macOS tar writes it
        archive = self.root / 'codecs.tar.bz2'
        with tarfile.open(archive, 'w:bz2') as tar:
            tar.add(build / updater.CEF_FRAMEWORK, arcname=updater.CEF_FRAMEWORK)
        for name, value in [('CEF_CODEC_ARCHIVE', archive), ('CEF_CODEC_VERSION', '156.0.15'),
                            ('CEF_CODEC_SHA256', hashlib.sha256(archive.read_bytes()).hexdigest())]:
            pinned = patch.object(updater, name, value); pinned.start(); self.addCleanup(pinned.stop)
        return archive

    def package(self, cef='156.0.15'):
        root = self.root; source = root / 'source'; binaries = root / 'bin'; sdk = root / 'sdk'
        (source / 'assets/icons').mkdir(parents=True)
        (source / 'assets/icons/Herdr.icns').write_bytes(b'icon')
        (source / 'LICENSE').write_text('app license')
        (source / 'THIRD-PARTY-NOTICES').write_text('dependency notices')
        binaries.mkdir(); (binaries / 'herdr-app').write_bytes(b'app')
        (binaries / 'herdr-browser-helper').write_bytes(b'helper')
        self.framework(sdk, cef, b'stock')
        local = root / 'state'; local.mkdir(); (local / 'signing_identity').write_text('test-identity')
        def fake_run(args, **kwargs):
            output = (str(sdk) + '\n').encode() if '--print-cef-path' in args else b'[]'
            if 'commands' in args:
                output = json.dumps([{'name': name} for name in ['browser.open','browser.navigate','browser.inspect']]).encode()
            if '--sign' in args: self.signed.append(args)
            return subprocess.CompletedProcess(args, 0, stdout=output)
        with patch.object(updater, 'LOCAL', local), patch.object(updater, 'run', side_effect=fake_run):
            return updater.package(binaries / 'herdr-app', self.bundle, '0.1.9', source, 'tested-patch')

    def packaged_binary(self):
        return (self.bundle / 'Contents/Frameworks' / updater.CEF_FRAMEWORK / 'Chromium Embedded Framework').read_bytes()

    def test_the_pinned_cef_gets_the_codec_framework(self):
        self.pin_archive()
        self.assertEqual(self.package(), {'h264': True, 'cef': '156.0.15', 'sha256': updater.CEF_CODEC_SHA256})
        self.assertEqual(self.packaged_binary(), b'codec')
        self.assertEqual(list((self.bundle / 'Contents/Frameworks').rglob('._*')), [])
        self.assertEqual(len(self.signed), 1)

    def test_a_changed_codec_archive_stops_the_build_before_signing(self):
        archive = self.pin_archive()
        archive.write_bytes(archive.read_bytes() + b'tampered')
        with self.assertRaisesRegex(RuntimeError, 'is not the pinned'): self.package()
        self.assertEqual(self.signed, [])

    def test_a_missing_codec_archive_stops_the_build_before_signing(self):
        self.pin_archive().unlink()
        with self.assertRaisesRegex(RuntimeError, 'Missing the CEF 156.0.15 codec framework'): self.package()
        self.assertEqual(self.signed, [])

    def test_an_archive_of_another_cef_is_refused(self):
        self.pin_archive(version='156.0.14')
        with self.assertRaisesRegex(RuntimeError, 'does not hold CEF 156.0.15'): self.package()
        self.assertEqual(self.signed, [])

    def test_a_moved_cef_falls_back_to_stock_and_warns(self):
        self.pin_archive().unlink()  # the fallback must not need the archive at all
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors): codecs = self.package(cef='157.0.2')
        self.assertEqual(self.packaged_binary(), b'stock')
        self.assertEqual((codecs['h264'], codecs['cef']), (False, '157.0.2'))
        self.assertIn('H.264/AAC video will not play', codecs['warning'])
        self.assertIn('WARNING: CEF 157.0.2 has no codec build', errors.getvalue())
        self.assertEqual(len(self.signed), 1)

    def build_candidate(self, codecs):
        """The watcher's path: build output is redirected to a log, so the warning has to escape it."""
        local = self.root / 'local'; local.mkdir()
        (local / 'feature.patch').write_text('patch')
        state, announced = {}, []
        fake = subprocess.CompletedProcess([], 0, stdout=b'/nonexistent\n')
        with patch.object(updater, 'LOCAL', local), patch.object(updater, 'run', return_value=fake), \
                patch.object(updater, 'git', return_value='commit'), patch.object(updater, 'pin_engine'), \
                patch.object(updater, 'ensure_zig', return_value='zig'), patch.object(updater, 'build_step'), \
                patch.object(updater, 'package', return_value=codecs), \
                patch.object(updater, 'announce_codec_loss', side_effect=announced.append):
            updater.build_candidate('v0.1.12', state, engine='157.0.0+157.0.2')
        return state, announced

    def test_an_unattended_stock_build_is_announced_and_stays_in_status(self):
        state, announced = self.build_candidate({'h264': False, 'cef': '157.0.2', 'warning': 'no H.264'})
        self.assertEqual(state['status'], 'staged')
        self.assertEqual(announced, ['no H.264'])
        self.assertEqual(updater.codec_warning(state), 'no H.264')
        state['installed'], state['pending'] = state['pending'], None
        self.assertEqual(updater.codec_warning(state), 'no H.264')

    def test_a_codec_build_is_not_announced(self):
        state, announced = self.build_candidate({'h264': True, 'cef': '156.0.15', 'sha256': 'pinned'})
        self.assertEqual(announced, [])
        self.assertIsNone(updater.codec_warning(state))
        self.assertIsNone(updater.codec_warning({'installed': {'tag': 'v0.1.12'}}))  # built before codecs were recorded

    def test_packaged_app_and_helpers_declare_browser_privacy_purposes(self):
        self.pin_archive()
        self.package()
        bundle = self.bundle
        bundle = self.bundle
        plists = [bundle / 'Contents/Info.plist', *sorted((bundle / 'Contents/Frameworks').glob('*.app/Contents/Info.plist'))]
        self.assertEqual(len(plists), 6)
        self.assertEqual((bundle / 'Contents/Resources/LICENSE').read_text(), 'app license')
        self.assertEqual((bundle / 'Contents/Resources/THIRD-PARTY-NOTICES').read_text(), 'dependency notices')
        for path in plists:
            with path.open('rb') as file: info = plistlib.load(file)
            self.assertTrue(self.required <= info.keys(), path)
            self.assertTrue(all(isinstance(info[key], str) and info[key].strip() for key in self.required), path)
        main = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
        self.assertEqual(main['LSEnvironment'], {'HERDR_APP_HOME': '~/.config/herdr-app-browser'})
        # A damaged artifact must be refused before it reaches installation.
        with plists[1].open('rb') as file: info = plistlib.load(file)
        del info['NSBluetoothAlwaysUsageDescription']
        with plists[1].open('wb') as file: plistlib.dump(info, file)
        with self.assertRaisesRegex(RuntimeError, 'NSBluetoothAlwaysUsageDescription'):
            updater.validate_browser_bundle(bundle)



def chrome_post(title, text, published='2026-11-03T00:00:00Z'):
    return {'title': {'$t': title}, 'published': {'$t': published}, 'content': {'$t': text}}


# Shapes copied from the live feeds on 2026-11-04; tests never reach the network.
STABLE = {'versions': [{'name': 'chrome/platforms/mac/channels/stable/versions/157.0.8103.39',
                        'version': '157.0.8103.39'}]}
RELEASES = {'feed': {'entry': [
    chrome_post('Chrome for Android Update', '<p>Chrome 157.0.8103.40 for Android. This update includes 9 security fixes.</p>'),
    chrome_post('Stable Channel Update for Desktop',
                '<p>Chrome 157.0.8103.38 (Linux) 157.0.8103.38/.39 Windows/Mac contains a number of fixes.</p>'
                '<p>This update includes 213 security fixes.</p>'),
    chrome_post('Stable Channel Update for Desktop',
                '<p>The Stable channel has been updated to 156.0.8078.79/.80 for Windows and Mac.</p>'
                '<p>This update includes 21 security fixes.</p>', '2026-10-29T00:00:00Z'),
    chrome_post('Stable Channel Update for Desktop',
                '<p>The Stable channel has been updated to 156.0.8078.60/.61 for Windows and Mac.</p>'
                '<p>This update includes 4 security fixes.</p>', '2026-10-20T00:00:00Z'),
]}}


class ChromiumFreshnessTests(unittest.TestCase):
    """#705: how far Herdr Browser's own Chromium is behind stable Chrome, read-only."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # Any live request fails the test rather than reaching the network.
        offline = patch.object(updater.urllib.request, 'urlopen', side_effect=AssertionError('network in a test'))
        offline.start(); self.addCleanup(offline.stop)

    def app(self, version, name='Herdr Browser.app'):
        """A bundle shaped like the installed one: CEF's plist names the branch, the binary the version."""
        framework = self.root / name / 'Contents/Frameworks/Chromium Embedded Framework.framework'
        (framework / 'Resources').mkdir(parents=True)
        branch = version.split('.')[2]
        with (framework / 'Resources/Info.plist').open('wb') as plist:
            plistlib.dump({'CFBundleShortVersionString': '156.0.15.0',
                           'SCMRevision': f'fb096c00-refs/branch-heads/{branch}@{{#1705}}'}, plist)
        noise = b'\x00' * 4096 + b'1.0.' + branch.encode() + b'.2.3 ' + b'\x00' * 4096
        (framework / 'Chromium Embedded Framework').write_bytes(
            noise + f'Mozilla/5.0 Chrome/{version} Safari'.encode() + noise + version.encode() + noise)
        return self.root / name

    def feeds(self, url):
        return STABLE if url == updater.CHROME_STABLE_FEED else RELEASES

    def test_reads_the_embedded_chromium_from_the_bundle(self):
        self.assertEqual(updater.embedded_chromium(self.app('156.0.8078.80')), '156.0.8078.80')
        self.assertIsNone(updater.embedded_chromium(self.root / 'Missing.app'))

    def test_an_older_build_counts_the_newer_stable_releases_with_security_fixes(self):
        status = updater.chromium_status(self.app('156.0.8078.60'), self.feeds)
        self.assertEqual({key: status[key] for key in (
            'embedded', 'stable', 'upToDate', 'securityReleasesBehind', 'securityFixesBehind', 'atLeast', 'update')}, {
            'embedded': '156.0.8078.60', 'stable': '157.0.8103.39', 'upToDate': False,
            'securityReleasesBehind': 2, 'securityFixesBehind': 234, 'atLeast': False,
            'update': 'herdr-browser-update check --force'})

    def test_the_build_of_the_latest_desktop_post_is_not_counted_against_itself(self):
        status = updater.chromium_status(self.app('156.0.8078.80'), self.feeds)
        self.assertEqual((status['securityReleasesBehind'], status['securityFixesBehind']), (1, 213))

    def test_current_stable_is_up_to_date(self):
        status = updater.chromium_status(self.app('157.0.8103.39'), self.feeds)
        self.assertEqual((status['upToDate'], status['securityReleasesBehind']), (True, 0))

    def test_older_than_the_feed_reaches_is_a_lower_bound(self):
        self.assertTrue(updater.chromium_status(self.app('150.0.7000.10'), self.feeds)['atLeast'])

    def test_unreadable_feeds_are_reported_not_raised(self):
        def down(url): raise urllib.error.URLError('offline')
        status = updater.chromium_status(self.app('156.0.8078.80'), down)
        self.assertEqual(status['embedded'], '156.0.8078.80')
        self.assertIn('Stable Chrome release data unavailable', status['error'])
        self.assertNotIn('securityReleasesBehind', status)
        self.assertEqual(updater.chromium_status(self.root / 'Missing.app', down), {'installed': False})

    def test_status_reports_freshness_without_the_update_lock_or_any_change(self):
        app = self.app('156.0.8078.60')
        local = self.root / 'local'; local.mkdir()
        (local / 'state.json').write_text(json.dumps({'status': 'installed'}))
        (local / 'lock').write_text('')
        before = {path: path.stat().st_mtime_ns for path in [*local.iterdir(), *app.rglob('*')]}
        output = io.StringIO()
        with patch.object(updater, 'APP', app), patch.object(updater, 'LOCAL', local), \
                patch.object(updater, 'fetch_json', self.feeds), patch.object(sys, 'argv', ['herdr-browser-update', 'status']), \
                patch('sys.stdout', output), open(local / 'lock') as held:
            # An update in progress holds the lock; status still answers.
            updater.fcntl.flock(held, updater.fcntl.LOCK_EX | updater.fcntl.LOCK_NB)
            self.assertEqual(updater.main(), 0)
        printed = json.loads(output.getvalue())
        self.assertEqual(printed['status'], 'installed')
        self.assertEqual(printed['chromium']['securityReleasesBehind'], 2)
        self.assertEqual({path: path.stat().st_mtime_ns for path in before}, before)


if __name__ == '__main__': unittest.main()
