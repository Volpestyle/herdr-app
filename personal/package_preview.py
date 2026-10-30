#!/usr/bin/env python3
"""Package the debug app with its sandboxed Chromium runtime for isolated QA."""
from pathlib import Path
import tomllib
import browser_update as updater

root = Path(__file__).resolve().parents[1]
bundle = root / '.local/preview/Herdr Browser.app'
if bundle.exists():
    if updater.app_running(bundle): raise SystemExit('The preview is running; close that preview before repackaging')
    import shutil
    shutil.rmtree(bundle)
version = tomllib.loads((root / 'Cargo.toml').read_text())['package']['version']
updater.package(root / 'target/debug/herdr-app', bundle, version, root, 'development', bundle_id='dev.james.herdr-browser.preview')
print(bundle)
