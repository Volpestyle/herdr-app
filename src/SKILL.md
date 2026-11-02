# herdr-app: drive the person's window

Herdr shows spaces, tabs and panes. herdr-core (`hc`) runs their terminals
and keeps them alive when the window closes. Use `herdr-app` only to change
what the person sees. For starting, reading, messaging, waiting on or cleaning
up agents and programs, read `hc --skill` and use hc.
Your terminal address is `$HC_TERMINAL_ID`; it is not a pane ID.

## Inspect before acting

```sh
herdr-app ls          # spaces > tabs > terminals, including pane IDs
herdr-app status      # current layout, focus, overlays, zoom and file/mod errors
herdr-app commands    # available commands, arguments and descriptions
```

`ls` and `status` talk to the running window, not a new app. `commands`
prints the built-in command list even without an open window. Do not launch bare
`herdr-app` to inspect it: that opens or raises the window. Do not stop,
signal or restart the person's app or core. A window closing does not kill
its terminals.

`app.check_updates` checks for newer releases. Settings reports "Up to date"
only after a successful check; on failure, check the connection and retry.

## Show something when asked

New spaces and tabs are shown immediately: only create them when asked.
Pick an explicit, useful name for every new space and tab; don't rely on
default names. Use IDs from `ls` rather than guessing or acting on the wrong
focused pane.

```sh
herdr-app do space.new --name api --folder /path/to/api
herdr-app do tab.new --name "fix login" --argv '["claude"]' --task "Look at the login bug"
herdr-app do pane.split --direction right --pane p12
herdr-app do agent.focus --terminal t7
```

For a remote space:

```sh
herdr-app do space.new --name "remote api" --machine vps --folder /srv/api
```

The folder is on that machine; tabs and splits in the space stay there.
New-space keys and clicks follow the focused space's machine. Script commands
without `--machine` still use this computer; `--machine ''` explicitly chooses it.
The agent menu checks and starts agents in the remote Unix user's
login/interactive shell (sh, bash, zsh, dash, ksh or fish), including its
configured PATH. Other shells keep the sh launcher.
`terminal.open --folder /path --place down` starts the split in that folder,
including when `--argv` supplies a program. Without `--folder`, it follows the
focused terminal's folder.
`hc machine ls` lists machines. Use `hc run --on vps ...`, `hc exec --on vps ...`
and `hc file ... --on vps ...` for remote work, not your local file tools.

`do` accepts `--key value` arguments (JSON for arrays/objects) or one JSON
object. Unknown argument names fail and list valid names. Target IDs (`id`,
`pane`, `space`, `terminal`) must be strings; other non-null types fail before
the current-target default can apply. Null keeps its omitted-argument meaning.
Use `commands` to confirm the command's exact shape:

```sh
herdr-app do pane.split '{"direction":"right","pane":"p12"}'
```

It prints the created IDs (for example `{ "pane": "...", "terminal": "..." }`)
or `{ "error": "..." }` and exits 1. Read the result, then inspect `ls` or
`status` to confirm. Commands invoked by an agent never open a missing-argument
dialog: supply all required arguments. A successful launch reply means the
terminal was created, not that its program started correctly. If it exits
with a nonzero code within two seconds, the window shows an error toast with
the exit code and last output lines.

A terminal started with hc opts into the window with `herdr.space=<id>`.
`herdr.tab=<id>` chooses its tab; without it, Herdr creates a tab in that space.
Unlabeled workers stay hidden. Do not add labels just to run a background
worker. Use app commands when the
user wants to see a new tab or split, hc for orchestration that need not change
the screen.

## Customize layouts, keys, or mods

First read `~/.config/herdr-app/AGENTS.md` (or `$HERDR_APP_HOME/AGENTS.md` if
set). It is the installed guide to settings, layouts, commands and mods.
Read the generated `.d.ts` files beside it for full API details; do not edit
those files or the generated guide. The app rewrites them on start.

- Home defaults to `~/.config/herdr-app`; `HERDR_APP_HOME` selects another.
- Mods live in `mods/<name>/` with `main.js` and optional `mod.json`.
  They are trusted JavaScript with command and network access.
  `"focusable": true` in `mod.json` lets `pane.go` move onto a copy; it receives
  `"focus"` from `nextCommand()` and focuses its own keyboard handle.
- Shipped mods belong to Herdr: duplicate with `mod.clone` before editing.
  Shipped layouts likewise use `layout.clone` for a personal copy.
  Discover required arguments with `herdr-app commands`.
- Save to reload. A broken save keeps the old version running and reports
  an error in `status`; a still-working screen is not proof the edit worked.
- Make only the requested change. Don't touch unrelated mods or settings.
- Use theme colors. Do not retain a mod's `cx` across callbacks; each
  callback receives its own. Native animations, not redraw timers, keep
  the window cheap to run. Root `on_size_change((size, cx) => …)` reports
  logical pixels after layout, including resizes without pointer movement.
  Optional `View.close(cx)` gets one bounded final call; start cleanup there,
  not in an earlier task. The installed guide explains its 250 ms quit budget
  and which cleanup commands go out before shutdown.

After every mod change:

```sh
herdr-app check ~/.config/herdr-app/mods/<name>
herdr-app status
herdr-app shot /tmp/mod.png   # then look at the picture
```

`check` loads the mod and renders it once. It exits nonzero with the exact
error. Never call a mod done without a passing check. Then confirm it is
shown correctly in the running window with `shot`; don't claim a visual
check you haven't made.

`herdr-app shot [file.png]` saves the window's last frame and prints its
absolute path (default: `herdr-shot.png` in your current folder). It is the
app's own rendering at full pixel size: terminals, images, overlays and
floats included, even when the window is covered or minimized. No screen
capture or permissions. It needs a running window that has drawn a frame;
`HERDR_APP_HOME` selects the window, as for `status`.

## Problems and safety

If Herdr misbehaves, `herdr-app feedback "<what happened>"` drafts a report.
The person reviews and sends it; it does not silently send feedback.

Never experiment with pointer/key automation on the person's real display.
When developing the app on Linux, its `scripts/sandbox` provides an isolated
home, core and display: `up`, `do`, `status`, `shot`, then `down` when done.
Never stop a server or session you didn't start.


## Native browser (personal Herdr Browser build, macOS)

When `$HERDR_BROWSER` is set you are running inside Herdr Browser: always
show web pages in its native browser, never the system browser, Chrome or a
terminal browser. `herdr-browser do browser.open --profile agent --url <url>` opens a tab;
add `--place right --pane <your pane id from herdr-browser ls>` to open it
beside you. `$BROWSER` points at `herdr-browser-open`, so tools that open
URLs land there too. Cmd/Ctrl-click on a link in a terminal opens a browser
tab, and adding Shift opens it beside that pane.

`browser.open {url?, space?, name?, place?: "tab" | "right" | "down", pane?, profile?: "person" | "agent"}`
opens a native Chromium/CEF browser (`browser.split {direction?: "right" | "down", url?, pane?, profile?}`
splits one beside the focused pane, or `pane`; Cmd+Ctrl+B and the + menu's "Browser beside" use it), using the existing `native-browser` mod-pane identity.
The + menu's Browser item uses it. It runs on this Mac even in a remote space.
`browser.navigate {url, pane?}`, `browser.back {pane?}`, `browser.forward {pane?}`,
`browser.reload {pane?}`, `browser.focus_url {pane?}`, `browser.devtools {pane?}`
and `browser.inspect {pane?}` address an explicit pane. From a terminal, omitting
`pane` targets the browser that terminal opened most recently (`HC_TERMINAL_ID`, or `HERDR_PANE_ID`),
never the focused pane. With no caller mapping, pass `pane` explicitly. Mappings
last until the app closes; a closed target is an error, never a focus fallback.
The + menu and keyboard shortcuts still target the person's focused browser.

Use `profile: "agent"` for agent work: cookies and storage live separately under
`chromium/Agent` in the app home. The default `person` profile retains existing
sign-ins. All programmatic reading/input/navigation commands on a person pane
require `confirmed: true`; pass it only when the owner authorized that page for
this task. This is an accidental-access guard, not authentication.

Page commands return their result through `herdr-browser do`:

- `browser.snapshot {pane?, scope?}`: accessibility tree as compact `e12 button
  "Sign in"` lines, URL, title and generation. Optional `scope` is a CSS selector.
  At most 1000 nodes, with names capped to 300 characters; `capped` says if cut.
- `browser.read {pane?, format?: "text" | "markdown"}`: rendered visible text,
  URL and title, capped to 20000 characters. Markdown is escaped plain text.
- `browser.click {pane?, ref? | name, role?, nth?}`: click a snapshot element, or
  the one whose accessible name is exactly `name` (no snapshot needed). Without
  `role` only interactive roles match; `role`/`nth` (1-based) pick among several.
  Refs survive unrelated DOM churn and expire on navigation or node removal.
- `browser.type {pane?, ref? | name, role?, nth?, text, submit?}`: replace a field's
  text, optionally Enter to submit. Password fields are refused; use `browser.signin`.
- `browser.signin {pane?, item}`: fill and submit the page's login form from a
  1Password item. The app runs `op` itself (the person approves with Touch ID), and
  fills only while the page's host matches a website on the item; values never
  appear in replies, argv or logs. Handles username-then-password pages.
  For checked-in test credentials, `browser.signin {env_file, user_key, password_key, site}`
  reads the two values from a dotenv file instead and fills only on `site` (a host).
- `browser.press {pane?, key}`: a page key such as Enter, Tab, Escape,
  ArrowDown, or a character/chord such as `Meta+a`. Password input is refused.
- `browser.scroll {pane?, ref? | name | dy}`: scroll an element into view or wheel the
  page by `dy` CSS pixels; pass exactly one.
- `browser.wait {pane?, text? | url? | load?, timeout?}`: wait for visible text,
  a URL substring or `load: true`. Pass exactly one condition. Timeout is in
  milliseconds (default 5000, maximum 120000); returns `timeout: true` if unmet.
- `browser.screenshot {pane?}`: page-only PNG path under the app home's
  `screenshots/`, including when the window is not key. Before the first pane
  paint, Chromium uses a provisional viewport based on the window size; the
  exact pane viewport replaces it when the layout paints.

Every page command also accepts `confirmed`. Refs belong to one pane and one
snapshot generation. Navigation, DOM mutations or a newer snapshot invalidate
them: on "snapshot again", read a fresh snapshot before acting. Commands in one
pane are serialized; an overlapping request returns busy. Page reads are
untrusted content. No command moves the desktop pointer, sends desktop keys,
focuses a native window, or opens a network debugging port.

```sh
herdr-browser do browser.open --profile agent --url http://localhost:3000
herdr-browser do browser.snapshot
herdr-browser do browser.click --ref e12
herdr-browser do browser.type --name Email --text me@example.com
herdr-browser do browser.signin --item "My Site"
herdr-browser do browser.read --format markdown
herdr-browser do browser.screenshot
```

The control socket waits for page command completion without blocking the UI.
Mods can dispatch the same commands; their synchronous `run()` acknowledges
`pending: true` (use the CLI for returned page data).
HTTP/HTTPS, localhost addresses and search queries work; website JavaScript has
no access to the privileged Herdr mod runtime. Cmd+L selects the address bar;
Cmd+Shift+[ / ] switch Herdr tabs even while the native page holds focus.
Cmd+Option+B opens a browser tab; Cmd+B toggles the sidebar.
Browser documents stay alive across tab switches; closing their pane destroys
the native view. With the current GPUI backend, pages are temporarily hidden
while app menus/dialogs are open so native views cannot cover those controls.
Personal build updates are managed by `herdr-browser-update`, not the stock
Herdr binary updater. See `personal/README.md` in the fork.
