import importlib.util
import io
import json
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
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

    def test_packaged_app_and_helpers_declare_browser_privacy_purposes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); source = root / 'source'; binaries = root / 'bin'; sdk = root / 'sdk'
            (source / 'assets/icons').mkdir(parents=True)
            (source / 'assets/icons/Herdr.icns').write_bytes(b'icon')
            (source / 'LICENSE').write_text('app license')
            (source / 'THIRD-PARTY-NOTICES').write_text('dependency notices')
            binaries.mkdir(); (binaries / 'herdr-app').write_bytes(b'app')
            (binaries / 'herdr-browser-helper').write_bytes(b'helper')
            (sdk / 'Chromium Embedded Framework.framework').mkdir(parents=True)
            local = root / 'state'; local.mkdir(); (local / 'signing_identity').write_text('test-identity')
            def fake_run(args, **kwargs):
                output = (str(sdk) + '\n').encode() if '--print-cef-path' in args else b'[]'
                if 'commands' in args:
                    output = json.dumps([{'name': name} for name in ['browser.open','browser.navigate','browser.inspect']]).encode()
                return subprocess.CompletedProcess(args, 0, stdout=output)
            bundle = root / 'Herdr Browser.app'
            with patch.object(updater, 'LOCAL', local), patch.object(updater, 'run', side_effect=fake_run):
                updater.package(binaries / 'herdr-app', bundle, '0.1.9', source, 'tested-patch')
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


if __name__ == '__main__': unittest.main()
