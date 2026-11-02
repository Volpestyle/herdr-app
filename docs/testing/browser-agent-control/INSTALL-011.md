# Scheduled Browser-only swap

Prepared for the lead; **not executed** by the worker. Use only in the scheduled
owner-approved restart window. No herdr-core update/restart is needed.

The installable release bundle is:

`/Users/james/dev/herdr-app-browser/.local/wt-browser-agent-control/.local/agent-control-011/Herdr Browser.app`

It has the production bundle ID `dev.james.herdr-browser` and uses the existing
Apple Development signing identity. The separate `.local/preview/Herdr Browser.app`
has a preview bundle ID and is for scratch QA, not the install target.
Version remains **0.1.11**; version alone cannot distinguish these builds.
Read `candidate.json` and `verification.json` beside the installable bundle for
the source commit, exact executable hashes, browser patch hash and signing
requirement comparison.

## Why the updater stays paused

The installed 0.1.11 was an **uncommitted** merge of fork `572888c` with upstream
`8cbb9f3348ea802e9c06cca3b413a53d3ba9064e`, plus its recorded browser patch.
This branch now has a real merge commit preserving that upstream revision.
The official `v0.1.11` tag (`e12fd318`) predates both terminal fixes `b600cdb`
and `4e90274`; an automatically rebuilt stable 0.1.11 could regress them.

Keep the watcher paused after the swap until a reviewed updater candidate has
these upstream fixes and the browser controls. After PR #2 is integrated into
the canonical checkout without losing its local edits, refresh/review the saved
browser patch. Its file list now includes `src/control.rs`. Refreshing the
patch alone does not make the older stable tag contain the terminal fixes.
Do not blindly invoke `herdr-browser-update install` or `check --force`.

## Install — lead only, at the scheduled window

The prepared helper follows the previous 0.1.11 swap: it takes the existing
updater lock, refuses a running Browser, verifies the candidate and installed
executable hashes, verifies the signature, backs up updater state and preserves
any older rollback bundle before calling the existing installer. It never
quits or launches an app. If the lock is busy or a hash differs, stop and
reconcile; do not bypass the checks or replay a partial swap.

Run in a shell, not by typing into an agent's terminal:

```sh
set -e
control_swap='/Users/james/dev/herdr-app-browser/.local/wt-browser-agent-control/.local/agent-control-011'
control_launchd="gui/$(id -u)"
if launchctl print "$control_launchd/dev.james.herdr-browser-updater" >/dev/null 2>&1; then
  launchctl bootout "$control_launchd/dev.james.herdr-browser-updater"
fi
herdr-browser ls > "$control_swap/layout-before-install.json"
hc ls --json > "$control_swap/terminals-before-install.json"
herdr-browser do app.quit
clankie heavy -- env PYTHONDONTWRITEBYTECODE=1 /opt/homebrew/bin/python3 "$control_swap/install-after-approval.py" --owner-approved
codesign --verify --deep --strict "$HOME/Applications/Herdr Browser.app"
herdr-browser --version
open "$HOME/Applications/Herdr Browser.app"
```

Wait for the normal control socket to answer, then record `herdr-browser status`,
`herdr-browser ls` and `hc ls --json` in the same receipt folder. Compare terminal
addresses, identities and PIDs with the before record; do not close tabs or send
input to repair them. Page documents reload; transient page state/history may
be lost. Existing Chromium storage is retained, and matching signing requirements
are checked, but individual sign-in continuity is not proved by the fixture.
Leave the updater unloaded for the reason above. This worker has not changed
its live loaded/unloaded state.

The expected rollback bundle is:

`~/Applications/Herdr Browser.app.previous.noindex`

It will contain the currently installed, original 0.1.11. The helper preserves
the older previous bundle separately as
`Herdr Browser.app.previous-before-agent-control-011.noindex`; it refuses to
overwrite an existing preservation/receipt. `install-receipt.json` records the
actual promoted hash and backup paths.

## Rollback — a separately coordinated restart

Keep the updater paused. The prepared rollback helper requires both the current
candidate hash and original 0.1.11 backup hash to match the proof. It swaps only
those bundles under the updater lock, retains the candidate as previous, and
updates updater metadata/held key using the existing rollback convention.
It does not touch the core or launch the app.

```sh
set -e
control_swap='/Users/james/dev/herdr-app-browser/.local/wt-browser-agent-control/.local/agent-control-011'
herdr-browser do app.quit
clankie heavy -- env PYTHONDONTWRITEBYTECODE=1 /opt/homebrew/bin/python3 "$control_swap/rollback-after-approval.py" --owner-approved
codesign --verify --deep --strict "$HOME/Applications/Herdr Browser.app"
herdr-browser --version
open "$HOME/Applications/Herdr Browser.app"
```

Again compare the app layout and live terminal records. Both candidate and
rollback report 0.1.11, so verify the receipts/executable hashes rather than
version alone. Do not run a core stop/restart, `hc kill`, or pane/tab close as
part of this Browser swap. Do not automatically bootstrap the updater afterward.
