import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('browser_open', Path(__file__).with_name('browser_open.py'))
opener = importlib.util.module_from_spec(spec)
spec.loader.exec_module(opener)


class BrowserOpenTests(unittest.TestCase):
    def test_starts_the_custom_app_and_routes_urls_without_shell_parsing(self):
        calls = []; states = iter([1, 1, 0])
        def run(args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, next(states) if args[-1] == 'status' else 0)
        urls = ['https://example.test/?q=hello world&mode=inline', 'http://localhost:3000/']
        with patch.object(opener.subprocess, 'run', side_effect=run), patch.object(opener.time, 'sleep'):
            opener.open_urls(urls, launcher='/test/bin/herdr-browser', app='/test/Herdr Browser.app')
        self.assertIn(['/usr/bin/open', '/test/Herdr Browser.app'], calls)
        self.assertEqual(calls[-2:], [['/test/bin/herdr-browser', 'do', 'browser.open', '--url', url] for url in urls])

    def test_unavailable_app_reports_failure_without_opening_an_external_browser(self):
        calls = []
        def run(args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(args, 1 if args[-1] == 'status' else 0)
        with patch.object(opener.subprocess, 'run', side_effect=run):
            with self.assertRaisesRegex(RuntimeError, 'did not become ready'):
                opener.open_urls(['https://example.test/'], launcher='/test/bin/herdr-browser',
                                 app='/test/Herdr Browser.app', timeout=0)
        self.assertFalse(any('browser.open' in call for call in calls))

    def test_pending_update_is_applied_before_launching_the_app(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); launcher = root / 'herdr-browser'
            updater = root / 'herdr-browser-update'; updater.touch()
            states = iter([1, 0]); calls = []
            def run(args, **kwargs):
                calls.append(args)
                return subprocess.CompletedProcess(args, next(states) if args[-1] == 'status' else 0)
            with patch.object(opener.subprocess, 'run', side_effect=run):
                opener.open_urls(['https://example.test/'], launcher=launcher, app=root/'App.app')
            self.assertLess(calls.index([str(updater), 'install']), calls.index(['/usr/bin/open', str(root/'App.app')]))


if __name__ == '__main__':
    unittest.main()
