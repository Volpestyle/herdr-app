# Native browser agent control proof

The proof driver is [`personal/agent_control_smoke.py`](../../../personal/agent_control_smoke.py).
It starts the packaged preview in a fresh `HERDR_APP_HOME` and `HC_SESSION`,
serves a loopback fixture, and uses the public CLI/control socket for every
browser action. It does not enable a debugging port or synthesize OS input.
`HERDR_APP_BACKGROUND=1` skips app activation and creates a non-key window.
The installed app, its profile, and its core session are never restarted.
Chromium starts with a usable window-sized viewport if the occluded GPUI window
has not painted exact pane bounds yet; its native view stays hidden until then.

Run from an isolated task worktree, through the machine resource governor:

```sh
CEF_PATH="$HOME/.local/share/herdr-browser/cef" \
ZIG="$HOME/.local/share/herdr-browser/tools/zig-aarch64-macos-0.16.0/zig" \
clankie heavy -- cargo build --locked --bins
clankie heavy -- env PYTHONDONTWRITEBYTECODE=1 python3 personal/package_preview.py
clankie heavy -- python3 personal/agent_control_smoke.py
```

The finite test owns its HTTP fixture and preview process for the lifetime of
the governor permit, then closes its owned test space/terminal, quits its
preview and stops only its named scratch core. Core stop alone preserves
terminal programs. `cleanup.json` records terminal termination and the core's
stop receipt; do not use `hc server status` after stopping, since that CLI
starts a stopped core. It prints its evidence folder under `.local/agent-control-smoke/`.
That folder contains the CLI transcript, app log, result JSON and page PNG.
The app home itself uses a short canonical `/private/tmp/hb-agent-*` path:
macOS Unix sockets cannot use the long task-worktree path. CEF cache paths
also need the same canonical prefix as its root cache path.

## Results

Passed on macOS / Apple Silicon against native source commit `99af4f4`.
The [machine-readable summary](summary.json) records the PNG and binary hashes,
observed results and the relative local artifact path.

- Native `cargo build --locked --bins`: passed.
- Existing command/guide checks: 3 passed. Existing Python checks: 20 passed.
- Real CEF proof: 46 CLI calls, all eight new page commands exercised.
- Snapshot refs, click-driven text changes, replacing existing input and Enter
  submission, plain text/Markdown reads, scoped snapshots, key/scroll input,
  load/text/URL waits and timeout reporting, and page PNG capture passed.
- Password typing and password-focused key input were refused. DOM mutations,
  a new snapshot and navigation invalidated refs with "snapshot again".
- Both terminal caller ID forms resolved their last opened agent pane while a
  person pane was selected. Unknown and closed targets did not fall back to focus.
- Agent cookies were present in its page and absent from a fresh person pane.
  Person reads and navigation were refused until explicitly confirmed.
- The window was non-key (`window_key: false`). The native view remained hidden
  (`shown: false`, `bounds: null`), so this proves control before the occluded
  window's first pane paint. The frontmost app stayed unchanged at every CLI
  reply. `lsof` reported no app TCP listeners.
- The inspected 2560×1552 page PNG shows the submitted replacement text and an
  empty password field. Its SHA-256 is
  `d501b7e3c5a7e724a38b13114079f2e33b67a8e0ff0a9401a427089689f6be9f`.

`cleanup.json` confirms the owned terminal finished with reason `killed`
and the named core returned `stopped: true`. The preview was stopped too. The starting
checkout's uncommitted changes and installed application were left untouched.

## Limits and evidence access

This proves the in-process control path on a loopback fixture. Real sign-ins,
external sites, extension workflows and cross-frame interaction were not
exercised. Markdown is escaped plain visible text.

The raw capture and full transcript remain in
`.local/agent-control-smoke/1793590575619935000/` in the task worktree.
The worker bridge refused fleet admission (`local_process_membership_required`),
so the raw PNG could not be published to the evidence store from this seat.
The committed summary contains no credentials or private page data.
