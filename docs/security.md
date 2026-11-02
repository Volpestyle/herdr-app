# Security and privacy

## Mods have your access

A mod is trusted JavaScript running inside Herdr. It can run programs with your environment, fetch URLs and read files.
The built-in file API limits writes to the app home, but a mod can run a program to write elsewhere. That is not a security boundary.
Mods can also run programs on connected machines.

There is no permission prompt for each program or network request. Read a mod before installing it, and treat it like a shell script you would run yourself.
Layouts can load mods. Check those references too.

## What stays on disk

The default app home is `~/.config/herdr-app`, including on macOS and Windows (under your user home).
`XDG_CONFIG_HOME` changes the config root. `HERDR_APP_HOME` selects an app home directly and takes precedence.

This folder holds your settings, keys, layouts, mods, generated agent guide and API types.
It also holds mod data, shared state, window positions, this screen's focus and zoom, update notes and cached media, and telemetry while enabled.
Mods may store sensitive data here. Herdr does not encrypt this folder.
The [installed guide's file map](../defaults/AGENTS.md#the-files) explains the files.

Spaces, tabs and splits are stored in each machine's core document, not in the app home.
Terminals and their output belong to [herdr-core](https://github.com/herdrdev/herdr-core).
Removing the app home does not stop the core or erase its data.

The app exposes a local control socket in its home so agents can inspect and drive the window.
Treat access to that home as access to the window and its commands.
Remote connections use SSH through the core.

## Native browser panes (personal fork, macOS)

The `browser.snapshot`, `browser.read`, `browser.click`, `browser.type`,
`browser.press`, `browser.scroll`, `browser.wait` and `browser.screenshot`
commands use that same local socket trust line. They reach only the named
pane's CEF browser through its in-process DevTools message observer. No network
debugger is opened. `HERDR_BROWSER_DEBUG_PORT` remains an explicit development
switch; do not enable it for ordinary agent control. Page input uses Chromium
`Input` events, never the desktop pointer, OS keys or window activation.
Screenshots capture the page, not the screen, and stay under the app home's
`screenshots/` directory. They and returned page text can contain private data.

`browser.open` defaults to `profile: "person"` for compatibility. Agent work
uses `profile: "agent"`, a separate CEF request context with cookies/storage
under `chromium/Agent`, beside the person's `chromium/Default`. Agent panes
share that agent profile, not the person's sign-ins. Popup URLs retain the
source pane's profile. Programmatic reading/input/navigation on person panes
requires `confirmed: true`, which callers should set only with the owner's
specific authorization. It is a guard against mistakes, not a security
boundary: any process with this socket can send that boolean or run terminals.
`browser.type` refuses password fields, and `browser.press` refuses input while
a password field is active. Sign-ins belong in the sign-in vault flow; these
commands do not implement that vault or expose its secrets.

Without an explicit pane, a terminal caller targets only the last browser it
opened, using `HC_TERMINAL_ID` or `HERDR_PANE_ID`, never UI focus. This caller field is routing,
not proof of identity. Unknown or closed targets are errors. Snapshot refs are
local to a browser and its generation and expire on DOM mutations or navigation;
an isolated JavaScript world observes the document without sharing the page's
JavaScript globals. Page content remains untrusted. Ref expiry and person
confirmation do not defend against another process running as the same user.

For isolated native QA, `HERDR_APP_HOME` must select a scratch home and
`HC_SESSION` a scratch core. `HERDR_APP_BACKGROUND=1` creates a non-key window
and skips app activation; it is not a headless mode. Never use the test-only
`HERDR_BROWSER_TEST_WINDOW_FRONT` flag on the owner's screen.

## Updates and signing

Installed releases check the Herdr release service, download an update, and check its SHA-256 checksum against the release manifest before unpacking it.
The app switches to the downloaded version on quit. Source builds do not update themselves.
Core updates are handled separately by the core's updater.

A checksum from the same service detects a damaged download; it is not an independent signature on the manifest.
The app does not verify a separate cryptographic signature on update manifests.

The release pipeline supports Apple Developer ID signing on macOS and Azure artifact signing on Windows when their credentials are configured.
Without them, packages can be unsigned or ad hoc signed.
On macOS the installer and updater verify certificate-signed bundles and preserve their signatures.
Apple notarization is tested separately; it is not required for publishing a release.
Do not assume every download is notarized or signed.

## Telemetry

Usage telemetry is **on by default**. Release builds send a small daily report to `https://telemetry.herdr.dev/v1/report` with a random install ID.
Reports include app and core versions, OS and architecture, bucketed usage time and counts, known agent names, built-in layout names, mod counts, app command names, and fixed event counters.
Custom layout names become `custom`; custom mod names are not sent.
Terminal text, prompts, file contents, folder paths and free-text names are not included.

Turn it off in **Settings → Privacy**. That page also lets you inspect the report.
Or set this field in your existing `settings.json`:

```json
{
  "telemetry": false
}
```

Turning it off deletes the local install ID and unsent reports. It cannot recall reports already sent.
For one run, set `DO_NOT_TRACK=1` or `HERDR_TELEMETRY=0`. CI environments disable collection too.
Ordinary source builds collect locally while enabled but do not send, unless pointed at a test endpoint with `HERDR_TELEMETRY_URL`.

Telemetry being off does not stop update checks, mod network requests or feedback you choose to send.
Feedback is drafted for you to review and send, not sent silently.

[Back to Herdr](../README.md)
