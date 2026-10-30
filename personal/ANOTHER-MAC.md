# Herdr Browser on another Mac

This build targets macOS on Apple silicon. The source is maintained in
`Volpestyle/herdr-app` on `james/native-browser`. The optional appearance profile
is maintained separately in the private `Volpestyle/dotfiles` repository under
`config/herdr-app-browser`.

## Copy the tested app

Install ordinary [Herdr](https://herdr.dev/download/) on the new Mac first so
herdr-core is available. Keep it alongside the custom app.

On the source Mac, the transfer archive is at
`~/dev/herdr-app-browser/.local/transfer/Herdr-Browser-apple-silicon.zip`, with
`SHA256SUMS` beside it. It contains the signed application only. Browser profiles,
cookies, Keychain data, running sessions and personal/work agent credentials
remain on their original Mac. AirDrop the archive and checksum file to the
other Mac. No public binary release is published.

On a Mac without Herdr Browser installed, extract the archive into
`~/Applications`; if a copy is already installed, close that custom app and
back it up before replacing it. Finder can extract it, or use:

```sh
mkdir -p ~/Applications
ditto -x -k ~/Downloads/Herdr-Browser-apple-silicon.zip ~/Applications
codesign --verify --deep --strict "$HOME/Applications/Herdr Browser.app"
```

This build is development-signed and is not notarized for public distribution.
macOS or the work Mac's management policy may require approval to open it.
The portable bundle selects the current account's `~/.config/herdr-app-browser`;
it has no baked-in `/Users/james` path. Sign in to websites normally on this Mac.

## Restore the appearance and keyboard shortcuts

Use your personal GitHub account to access the private dotfiles repository.
These steps install only Herdr's appearance; they do not install or switch
personal agent accounts on the work Mac. Reuse existing clones when present.

```sh
git clone git@github.com:Volpestyle/dotfiles.git ~/dotfiles
git clone --branch james/native-browser --recurse-submodules \
  https://github.com/Volpestyle/herdr-app.git ~/dev/herdr-app-browser
python3 ~/dotfiles/scripts/herdr-browser-setup.py
python3 ~/dotfiles/scripts/herdr-browser-setup.py --apply
python3 ~/dev/herdr-app-browser/personal/browser_update.py install
open "$HOME/Applications/Herdr Browser.app"
```

Use Python 3.11 or newer. The setup previews by default, backs up replaced
files, links authored settings to dotfiles, and merges just the saved appearance
settings and selected layout. It restores the full-app wallpaper, transparent
Ghostty layout, Browser menu, tab bar and Cmd+Shift+[ / ] shortcuts. It leaves
machine connection settings and local session/browser state in place. Generated
mod typings live under ignored `.local/herdr-app-browser` in dotfiles.

## Build and update from source

Copying the signed app does not require transferring a signing private key.
For local builds and automatic updates, install Rust (tested with 1.96), Bun,
Python 3.11+, and cargo-about 0.9.3. Configure a stable personal signing identity
available in this Mac's Keychain; list identities with:

```sh
security find-identity -v -p codesigning
```

Put the selected identity's fingerprint in the ignored
`~/dev/herdr-app-browser/.local/browser-updater/signing_identity` file. Each
machine owns its signing selection, cache, updater state and LaunchAgent.
Then, from the fork:

```sh
python3 personal/browser_update.py snapshot
python3 personal/browser_update.py build --tag v0.1.9
python3 personal/browser_update.py install
python3 personal/browser_update.py enable
```

The updater checks official Herdr and stable CEF releases, builds the fork,
and installs validated updates after the custom app closes. Zig and the CEF
SDK are downloaded into local caches during the build. Keep both Git clones
in place for the updater and configuration links. Closing the app keeps core
terminals running. Fork source and configuration are backed up on GitHub;
build caches, signing credentials and live app data are deliberately local.
