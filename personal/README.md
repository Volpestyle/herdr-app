# Herdr Browser

Open `~/Applications/Herdr Browser.app`, then choose **+ → Browser**. The
`herdr-browser` CLI addresses this custom app; `herdr-app` still addresses the
stock installation. Native Chromium is integrated through CEF in Herdr's
Rust/GPUI shell.

Source: [Volpestyle/herdr-app](https://github.com/Volpestyle/herdr-app),
branch `james/native-browser`. Optional appearance and keyboard customizations
are maintained in the private `Volpestyle/dotfiles` repository under
`config/herdr-app-browser`. See [moving to another Mac](ANOTHER-MAC.md).

The browser has an address/search bar, back/forward, reload, persistent website
storage, new-window links opening as Herdr tabs, downloads, and Chromium's
inspector. Native panes use CEF's Chrome runtime style for Web Store extension
support. CEF owns the native child view, compositor, input and Retina scaling;
there is no screenshot stream or CPU image-upload loop. Browser profiles live
under `~/.config/herdr-app-browser/chromium` and downloads under
`~/.config/herdr-app-browser/downloads`. Website JavaScript has no bridge to
Herdr's privileged mod runtime. Chromium's renderer/GPU helpers are sandboxed.
Remote debugging is disabled unless `HERDR_BROWSER_DEBUG_PORT` explicitly
selects a local development port.

Cmd+L selects the address bar, Cmd+R reloads, and Cmd+Shift+[ / ] cycle all Herdr
tabs while a webpage owns keyboard focus. Native copy/paste/select-all and
undo/redo stay in the webpage. Cmd+Shift+I opens Chromium's inspector.

The first install copies the existing appearance, personal mods and keys into
`~/.config/herdr-app-browser`. Subsequent updates preserve that configuration.
Both applications use the existing
herdr-core session unless `HC_SESSION` selects another one. Browser engine
replacement uses a new Chromium profile; existing Safari/WebKit sign-ins are
not imported.

## Automatic updates

`~/Library/LaunchAgents/dev.james.herdr-browser-updater.plist` invokes the local
updater every 15 minutes. At most every six hours it checks:

- Herdr's official stable feed, `https://releases.herdr.dev/app/latest.json`,
  then fetches the matching GitHub version tag. Herdr does not publish GitHub
  Releases objects.
- The stable `cef` Rust package on crates.io, which selects its matching
  Chromium/CEF binary distribution. An engine update can trigger a build even
  when the Herdr version stays the same.

It creates an isolated detached worktree, checks and applies the saved browser
patch, pins the selected stable CEF version, builds the optimized app/helpers,
and runs app unit tests locally. It regenerates dependency notices, packages
Chromium with the app, signs and verifies the complete bundle, and checks its
browser commands before staging.
The main app and every Chromium helper carry the macOS Bluetooth, passkey,
camera and microphone usage declarations; packaging fails if any is missing.
Compiles have a 30-minute limit; app tests run serially with a two-minute
limit. A timeout stops only that updater-owned build process tree. This keeps
an occasional filesystem-watcher test stall from hanging the watcher.
Update checks use public release feeds; builds run locally.

The staged build installs once Herdr Browser closes. Updates retain the previous
bundle for rollback and never quit the working app themselves. Patch conflicts,
build/test/signing failures and automatic downgrades are refused. An identical
failed candidate is not rebuilt repeatedly. Compatibility changes can still
require a manual patch repair; the installed working version is retained.
The stock binary updater is disabled so it cannot overwrite the custom build.

The updater uses a stable Apple Development signing identity selected by the
ignored `.local/browser-updater/signing_identity` file. Configure a valid
identity available on the build machine and update the selection when its
certificate is renewed. Signing failures preserve the working app. Do not
replace stable signing with an ad-hoc signature for unattended updates: Keychain may require
new authorization when the trusted code identity changes.

Chromium uses a dedicated **Herdr Browser Personal Safe Storage** Keychain
service/account, separate from other Chromium applications. No existing
browser Keychain entries or credentials are read or migrated by the setup tools.
Chromium itself encrypts its profile using that key. The continuity test used
two different signed builds with an identical certificate-based requirement;
an encrypted persistent test cookie survived the update without approving any
OS prompts or inspecting Keychain contents.

```sh
herdr-browser-update status
herdr-browser-update check --force       # Check/build now
herdr-browser-update install             # Promote a staged build after closing
herdr-browser-update rollback            # Restore the previous bundle after closing
```

The implementation is `personal/browser_update.py`. State, patch snapshot,
candidate worktrees, logs and signing selection are in ignored
`.local/browser-updater/`. Keep this checkout in place for the watcher; the
installed app includes its runtime and runs independently. Old candidates are
retained for diagnosis. Remove obsolete build directories when needed, keeping
any `pending.bundle` named in `state.json`.

To disable the watcher:

```sh
launchctl bootout "gui/$(id -u)/dev.james.herdr-browser-updater"
```

## Development and verification

```sh
export CEF_PATH="$HOME/.local/share/herdr-browser/cef"
export ZIG="$HOME/.local/share/herdr-browser/tools/zig-aarch64-macos-0.16.0/zig"
cargo build --bins
PYTHONDONTWRITEBYTECODE=1 /opt/homebrew/bin/python3 personal/package_preview.py
PYTHONDONTWRITEBYTECODE=1 /opt/homebrew/bin/python3 personal/native_smoke.py \
  --binary '.local/preview/Herdr Browser.app/Contents/MacOS/HerdrBrowser'
PYTHONDONTWRITEBYTECODE=1 /opt/homebrew/bin/python3 -m unittest discover -s personal -p 'test_*.py'
herdr-browser-update snapshot
herdr-browser-update build --tag v0.1.10 --force
herdr-browser-update install
```

The CEF build script downloads the matching SDK into the configured versioned
cache. The packager obtains that exact SDK path from the compiled helper,
copies its framework/helpers/credits, and signs the result. Cargo preserves
the upstream lockfile and resolves the dependencies added by the browser patch.
The saved patch excludes the lockfile and personal updater; it includes new browser
source files. `snapshot` does not stage, commit, reset, or rewrite this working
checkout.

`snapshot` uses the versioned `personal/upstream-base`, including after a
release merge when a machine still has an older cached baseline. Committed
browser code is included. Official release tags are fetched directly
from `herdrdev/herdr-app`, independently of the fork's `origin`. The Zig version is read from upstream CI, including its `ZIG_VERSION`
setting. Native builds also need Bun and cargo-about 0.9.3 for the repository's notice generator.
Fork pushes run only the small Python updater checks; inherited native CI and
release builds run on this fork only through explicit manual dispatch.

`native_smoke.py` owns an isolated app home/core and a local fixture server. It
checks real Chromium loading/JavaScript, navigation, tab next/prev, nonoverlapping
split bounds, modal visibility, downloads and address focus. Test-only env flags
bring its owned window forward (GPUI correctly skips fully occluded windows)
and select a fresh test Keychain identity. It does not synthesize desktop
pointer/key input or close another app. The preview has a separate bundle ID.

Linear's public login page renders the normal Google/email/SSO/passkey choices
in Chromium with a 993×658 CSS-pixel viewport at devicePixelRatio=2. The former
WebKit build showed an incompatible-browser warning. Manual testing confirmed
Google sign-in to Linear on 2026-10-30. That flow initially exposed
a packaging omission: passkey discovery requested Bluetooth without an
`NSBluetoothAlwaysUsageDescription`, so macOS TCC terminated the app. The
packager now includes and validates the browser usage descriptions on the app
and all helpers. These declarations permit normal macOS permission prompts;
they do not grant a website access. Other authentication paths have not all
been exercised. Physical keyboard
shortcuts and comparative CPU/GPU performance have not been benchmarked.
The [extension runtime PR](https://github.com/Volpestyle/herdr-app/pull/1)
reports successful 1Password installation and its inline menu in browser panes.

## Current limits

Native views sit above GPUI's drawing surface. Pages temporarily hide while
Herdr menus/dialogs are open and return afterward. A pane retains its document
across tab switches, but recreating it starts at the original URL; history is
not restored after restarting the app. Remote-space browsing runs locally.
External protocol navigation is blocked. Popup URLs currently open in a new
Herdr tab; flows that require a preserved `window.opener` may need further work.
Extension-created tabs can open in separate Chrome windows, and extension
toolbar-popup placement is not yet integrated into the Herdr pane controls.
Broader extension compatibility has not been verified. Keep a local profile
backup before switching an existing Alloy-runtime profile to Chrome style.
GPUI's `shot` capture omits native child views; Chromium DevTools screenshots
were used for native rendering evidence.

Orca references:
[desktop webview host](https://github.com/stablyai/orca/blob/main/src/renderer/src/components/browser-pane/host-guest/browser-page-webview.ts),
[browser identity handling](https://github.com/stablyai/orca/blob/main/src/main/browser/browser-process-user-agent.ts).
The CEF integration uses [cef-rs](https://github.com/tauri-apps/cef-rs).
