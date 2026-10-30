#!/usr/bin/env python3
"""Package the debug app with its sandboxed Chromium runtime for isolated QA."""
from pathlib import Path
import browser_update as updater

root = Path(__file__).resolve().parents[1]
bundle = root / '.local/preview/Herdr Browser.app'
if bundle.exists():
    if updater.app_running(bundle): raise SystemExit('The preview is running; close that preview before repackaging')
    import shutil
    shutil.rmtree(bundle)
updater.package(root / 'target/debug/herdr-app', bundle, '0.1.9', root, 'development', bundle_id='dev.james.herdr-browser.preview')
print(bundle)
