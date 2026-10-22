# AGENT HANDOFF — read this first, append before you finish

This file is the shared memory between agent sessions working on the mouser
setup — the Mac host (sections below) and the Windows PC client (its section
is further down). Read the whole file, do your task, then append an entry to
the **Handoff log** at the bottom before reporting done. Do not delete
history; append only. Each machine's section carries its own values.

## This machine's role

This Mac is the **HOST** in a two-machine software-KVM setup:

| Fact | Value |
|---|---|
| This Mac (host) | `192.168.68.68`, sits LEFT of the Windows PC |
| Windows PC (client) | `192.168.68.67`, auto-reconnects every ~2 s |
| Shared edge | this Mac shares its **RIGHT** screen edge (`peer_edge: Right`) |
| Listen port | `47583` (bind `0.0.0.0`) |
| Repo | `/Users/ed/Documents/GitHub/mouser` (workspace: `mouser-core/-net/-input`, `src-tauri`) |
| Binary in use | `target/debug/mouser` (**dev profile** — no release build exists) |
| App config | `~/Library/Application Support/dev.mouser.mouser/config.json` (never stores the secret) |
| Pairing secret | NOT in this file. `cat ~/.config/mouser/host-secret` (file exists, mode 600) |

## How the host runs (canonical form)

```sh
cd /Users/ed/Documents/GitHub/mouser
nohup env MOUSER_SECRET="$(cat ~/.config/mouser/host-secret)" \
  ./target/debug/mouser --wait --edge right \
  </dev/null >"$TMPDIR/mouser-host.log" 2>&1 &
```

- Detached (nohup, PPID 1). No `--verbose` currently.
- Role/edge/port come from the config file; the secret comes only from the env.
- The host does **not** auto-start at boot. If nothing listens on 47583 after a
  reboot or crash, relaunch it with the command above.

## Update procedure (pull → build → restart only)

1. `git fetch origin && git status`. Clean tree expected. If dirty: **stash,
   never discard** — it may be another agent's work in progress. Then
   `git pull --ff-only origin main`. No rebases, no force, no merges.
2. `cargo test --workspace` — expect **66 passed on macOS, 0 failed**.
   (67 in the full suite cross-platform; 1 test, `keymap::arrows_and_delete_need_the_extended_flag`,
   is `#[cfg(windows)]` and never runs here. The `mouser-net` suite takes ~75 s —
   `public_address_is_refused` is slow by design. That is not a hang.)
3. Stop the old host: `kill $(pgrep -f 'target/debug/mouser')`, wait for the
   port to free.
4. Rebuild the same profile it was running: `cargo build` (dev). Do NOT switch
   to `--release` — match the running binary's path.
5. Relaunch with the canonical command above, detached, secret in env.
6. Run the done-checks below, then append to the handoff log.

## Objective done-checks (verify, don't trust prose)

```sh
lsof -nP -iTCP:47583 -sTCP:LISTEN          # mouser PID listening
lsof -nP -iTCP:47583 | grep ESTABLISHED    # line to 192.168.68.67:xxxxx
tail "$TMPDIR/mouser-host.log"            # "paired with windows-pc at 192.168.68.67:..."
```

The "paired with" log line is the real proof; a LISTEN socket alone is not.
Do **not** use `ping 192.168.68.67` as a health check — that Windows box drops
ICMP (100% loss while paired and working).

## Rules of engagement

- No code changes unless the user explicitly asks. Tasks here are usually
  pull/build/restart only.
- Never commit or push unless explicitly asked. Fast-forward pulls only.
- If `git pull` refuses because an untracked `AGENTS.md` would be overwritten,
  the upstream tracked version supersedes the local copy: delete the local
  file and pull again.
- Never put the secret value in the repo, this file, logs, or command args
  (`--secret` is visible in `ps`; use the env var).
- Dropping the link while working is fine — the PC client re-pairs by itself
  within ~2 s of the listener coming back.
- Mixed versions across the two machines are safe; the wire protocol is stable.
- If the host dies with "missing accessibility permission", that is macOS
  Privacy & Security (Accessibility/Input Monitoring) — not a code bug.

## The other machine — Windows PC (client)

Written from the PC side (handoff log, 2026-10-02 ~13:35). The rules of
engagement apply on both machines; only the facts differ.

| Fact | Value |
|---|---|
| Windows PC (client) | `192.168.68.67`, connects out to the host |
| Host (the Mac) | `192.168.68.68:47583`, sits to the PC's LEFT |
| Shared edge | the PC shares its **LEFT** edge (`--edge left`) |
| Repo | `C:\Users\eduar\mouser` (same workspace layout) |
| Binary in use | `target\debug\mouser.exe` (**dev profile** — no release build) |
| App config | `%APPDATA%\mouser\mouser\config\config.json` (never stores the secret) |
| Pairing secret | NOT in this file. `%USERPROFILE%\.config\mouser\client-secret` |
| Live console | `%TEMP%\opencode\mouser-live.err.log` (all app events with `--verbose`) |

Canonical launch (PowerShell, detached):

```powershell
$env:MOUSER_SECRET = (Get-Content "$env:USERPROFILE\.config\mouser\client-secret" -Raw).Trim()
Start-Process -FilePath "C:\Users\eduar\mouser\target\debug\mouser.exe" `
  -ArgumentList "--connect","eds-MacBook-Pro.local:47583","--edge","left","--name","windows-pc","--verbose" `
  -RedirectStandardError "$env:TEMP\opencode\mouser-live.err.log"
```

The client dials the Mac **by its Bonjour name** and re-resolves it on every
attempt, so DHCP moving the Mac no longer breaks pairing; the IPs in the
tables above are informational only. The first few attempts after launch
may log `No such host is known` while mDNS warms up (the network profile is
Public); it pairs within ~10 s. If the name never resolves
(`Resolve-DnsName eds-MacBook-Pro.local`), fall back to a router DHCP
reservation for the Mac's MAC `bc:d0:74:59:ac:87`. The desktop shortcut's
`%USERPROFILE%\.config\mouser\launch.vbs` uses the same hostname.

Never pass `-WindowStyle Minimized` here: creating the window hidden lets
WebView2 suspend the renderer, and the UI freezes on a stale frame while
the backend keeps working (see the 14:08 handoff entry). A suspended
renderer shows 0 CPU across the whole `msedgewebview2.exe` tree; a live
one idles around 30 ms per 3 s while polling.

The client does not auto-start at boot; if `Get-Process mouser` comes up
empty after a reboot, relaunch with the command above.

Update procedure (pull → build → restart, same order as the host):

1. `git fetch origin && git status` — clean tree expected; **stash, never
   discard**. Then `git pull --ff-only origin main`. No rebases, no force.
2. **Kill the exe before any build**: `Stop-Process -Name mouser -Force`,
   then wait ~1 s — the running exe locks `target\debug\mouser.exe` and the
   link step fails while it lives. Dropping the link is fine; the client
   re-pairs by itself within ~2 s of the host listener coming back.
3. `cargo test --workspace` — expect **74 passed on Windows, 0 failed**
   (the one extra vs macOS is `#[cfg(windows)]`).
4. `cargo build` (dev). Agent shells must call it by full path —
   `& "$env:USERPROFILE\.cargo\bin\cargo.exe" build` — `cargo` is not on PATH.
5. Relaunch with the canonical command above.
6. Run the done-checks below, then append to the handoff log.

Objective done-checks:

```powershell
Get-Process mouser                                   # alive
Get-NetTCPConnection -RemoteAddress 192.168.68.68    # State Established
Get-Content "$env:TEMP\opencode\mouser-live.err.log" -Tail 5
# expect: paired with mac at 192.168.68.68:47583 [0788FEF2]
```

The window's header clock must tick (~10 Hz): a stopped clock means the UI
froze while the backend may still be working — read the `5ff492b` handoff
log entry before diagnosing the link.

Windows quirks:

- `cargo test` does NOT refresh the exe — always `cargo build` before
  running the binary against anything. A stale exe produced false failures
  once.
- The low-level hooks ignore injected input (`LLMHF_INJECTED`): no script
  can simulate the physical push across the edge. Handoff feel can only be
  tested by the user's hand; the log lines ("pushed the cursor off the
  shared edge" / "-> local control") prove the state machine without it.
- The seam-parked pointer stays visible on Windows by design: `ShowCursor`
  is a per-thread count mouser's windowless threads cannot move — every
  software KVM on Windows shows the parked cursor.
- The UI's liveness depends on `additionalBrowserArgs` in
  `src-tauri/tauri.conf.json` (backgrounding disabled + native occlusion
  calculation off). If a rebuild ever drops them, a hidden or minimized
  window can suspend the WebView2 renderer to 0 CPU and freeze the
  display while the backend keeps working.
- Desktop facts: 100% scale, single 3440x1440 monitor, seam at x=0.

## Handoff log (append-only)

- **2026-10-02 ~13:20** — opencode (system.ai.glm-5-3): pulled `b6b6517`
  "Pump input at full rate..." then `efa8453` "Bound the client's connect...".
  Tests 66/66 on macOS. Host rebuilt (dev) and relaunched detached —
  PID 97894, `--wait --edge right`, port 47583, secret via env. PC paired
  immediately from 192.168.68.67:64717. Tree clean. Created this file,
  `~/.config/mouser/host-secret`, and the `.git/info/exclude` entry.
- **2026-10-02 ~13:35** — opencode on the Windows PC: the "reconnecting
  shortly" report was a frozen UI, not a dead link — one failed `snapshot`
  invoke permanently stopped the poller while the backend stayed paired
  and handoffs kept working underneath. Fixed in `5ff492b` "Keep the UI
  polling through transient snapshot failures" (poller retries through
  failures; header clock ticks per poll so a stale window is obvious).
  Already on `origin/main`; client rebuilt and relaunched 13:21, paired
  from `:60414`. **Host agent: your UI has the same one-strike freeze — at
  your next update, pull `main` (this file and `5ff492b` arrive together),
  then restart via the canonical command, which also moves your log to
  `$TMPDIR/mouser-host.log` as this file says.** Added the Windows PC
  section above and created `%USERPROFILE%\.config\mouser\client-secret`
  (value not in this file) so the PC's canonical command carries no secret.
  Open item: the user has not yet given the subjective verdict on the
  original complaints (forwarded-motion smoothness on the Mac, PC cursor
  staying parked at the seam) — clean handoff/return cycles show in the
  client console through 13:17; only feel remains unknown.
- **2026-10-02 ~14:02** — opencode on the Mac host (system.ai.glm-5-3):
  followed this file's update procedure. Pulled `main` → `8933fc0` (PRs #1 and
  #2 merged — this doc upstreamed, Windows PC section added — plus `5ff492b`
  "Keep the UI polling through transient snapshot failures", the one-strike
  poller freeze the PC agent flagged). Tests 66/0 on macOS, as this file
  predicts. Host stopped, rebuilt (dev, UI is embedded via `frontendDist`,
  so the poller fix and header clock are now live on the Mac too), relaunched
  via the canonical command — PID 6052, log moved to `$TMPDIR/mouser-host.log`
  as instructed. PC re-paired within ~1 s from `:49475`. Tree clean; this
  entry upstreamed via PR. Open item from the PC entry stands: user's
  subjective verdict on forwarded-motion feel and the seam-parked cursor.
- **2026-10-02 ~14:10** — opencode on the Windows PC: at 14:01 the host
  agent restarted the Mac host for its update; this client re-paired in
  6 s (expected, per procedure). The window then sat on a stale
  "reconnecting shortly" for minutes with the link up: the entire
  WebView2 tree (browser + 5 renderer/gpu processes) was suspended at
  0 CPU while the window was visible and un-minimized — a different
  class from the `5ff492b` poller fix; a suspended renderer runs no JS
  at all. Trigger: the window had been created minimized
  (`-WindowStyle Minimized` in the canonical command — removed above,
  do not re-add it). Relaunched 14:08 with the window shown normally;
  UI verified live by renderer CPU (31 ms/3 s vs 0.0 suspended). Field
  check: the header clock stops when the renderer is suspended. Open
  proposal awaiting the user, not done unilaterally (code change): set
  `additionalBrowserArgs` to `--disable-background-timer-throttling
  --disable-renderer-backgrounding --disable-backgrounding-occluded-windows`
  in tauri.conf.json so the renderer can never suspend.
- **2026-10-02 ~14:16** — opencode on the Windows PC, user approved: set
  `additionalBrowserArgs` on the main window in `src-tauri/tauri.conf.json`
  to `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion
  --disable-background-timer-throttling --disable-renderer-backgrounding
  --disable-backgrounding-occluded-windows` (wry's own defaults preserved
  verbatim — setting this key replaces them wholesale). This closes the
  14:10 renderer-suspension class at the engine level. Verified live:
  client relaunched 14:14 (PID 28384, paired in 39 ms), renderer CPU
  **31 ms/3 s while the window was held minimized** — the exact state
  that previously dropped to 0 — and ~16 ms/4 s steady state after
  restore, clock ticking. `cargo test --workspace` green (exit 0) before
  launch, build dev. The key is WebView2-only; the Mac ignores it — no
  host action needed beyond staying synced (its WKWebView suspension is
  a separate mechanism if it ever bites).

- **2026-10-11** — Codex inspected the checkout and host state for a minimal-fix
  assessment; the exact user-visible symptom is awaiting clarification. No
  mouser process or TCP 47583 listener was found, and the canonical host log
  was absent. Source review found a blocking std receiver inside a Tokio task
  (`src-tauri/src/link.rs:46–48`); moving that bridge to `spawn_blocking` with
  `blocking_send` would avoid occupying an async worker. macOS movement also
  reads the OS cursor before each posted move, a possible burst-motion issue
  requiring validation. No application code changed, no tests run, and no host
  restart, commit, or push performed.

- **2026-10-11 ~16:41 EDT** — Codex, user authorized a code review/fix and
  simplification pass. Local changes on the existing checkout (no fetch/pull,
  commit, or push): replaced the two outbound queues and blocking forwarding
  task with one per-pairing Tokio queue; overflow invalidates the connection,
  stale input cannot survive reconnect, writes/quiet peers time out, and Ping
  gets Pong. The current UI pairing secret is used on subsequent connections;
  changing it ends an active pairing. Cursor replay now posts the accumulated
  absolute position, clamps non-seam edges, and retains seam hysteresis. Fixed
  the right/bottom last-pixel crossing check and UI edge enum casing. Forwarded
  typing/clicks/scrolling are suppressed locally; pointer motion stays local
  for capture. Held injected keys/buttons are released on disconnect, return,
  and normal app exit. macOS injection retains modifier state and click position.
  Windows scan/HID mappings, Alt handling, wheel units, hook-ready reporting,
  and WM_QUIT shutdown were corrected. Removed ineffective Windows ShowCursor
  counting, redundant bridge locks, and unused RTT UI/state. UI polls cannot
  overlap and recover from failures. Non-test source is 38 lines smaller
  (including comments/whitespace); regression tests are additional.
  Validation: 72 workspace tests passed, final affected-crate tests passed,
  macOS Clippy with -D warnings and formatting/diff checks passed, and the Node
  UI regression passed. Windows input crate including tests cross-compiles for
  x86_64-pc-windows-gnu using the rustup stable compiler, with isolated output
  in target/windows-check (the default cargo/rustc are separate Homebrew tools).
  Windows runtime was NOT tested or updated. Built cargo build in dev profile;
  host launched detached with secret only in the environment, PID 64224,
  PPID 1, log at $TMPDIR/mouser-host.log. Log confirms input capture running and
  listener on 0.0.0.0:47583. No established PC connection or paired-with log
  arrived, so live two-machine behavior remains unverified. The usual shell
  nohup launch did not survive the tool shell; Python Popen with
  start_new_session=True launched the equivalent host successfully.

- **2026-10-11** — Codex measured the current checkout: 6,519 source lines
  across 22 Rust/JS/CJS/HTML/CSS files, including tests, comments, and blank
  lines (5,915 nonblank). Tracked/nonignored project files total about 0.64 MB;
  the dev executable is 41.14 MB. The entire checkout uses about 5.34 GB on
  disk, almost all of it generated target/ build output, including the Windows
  cross-check cache. No application code or running-process changes.
- **2026-10-16 ~15:45 EDT** — opencode on the Windows PC: pulled `main`
  `899e1bb` → `1f48dfa` ("Simplify the link queue...") `--ff-only`, tree was
  clean. Prior client had been down since 10/4 00:44 (last log line a tao
  `cannot move state from Destroyed` panic after normal handoffs).
  `cargo test --workspace` green: 72 passed (10+34+17+11), 0 failed — the
  file's "67" predates this commit's new tests. `cargo build` dev OK, exe
  refreshed 15:45. Relaunched via the canonical command (PID 20568, window
  shown, not minimized). Done-checks partial: process alive, but no TCP to
  192.168.68.68 and no `paired with mac` line — log shows `connect timed
  out; retrying` on every attempt, `Test-NetConnection` TCP 47583 failed.
  Host side looks down or pre-restart; it likely needs the same `1f48dfa`
  update. Client will re-pair by itself once the listener is back.
- **2026-10-16 ~15:46 EDT** — Claude Code on the Mac host: `main` at `1f48dfa`
  (the 2026-10-11 Codex changes, committed and pushed on user request).
  No host was running. Built dev, launched detached via Python Popen
  (start_new_session) with the secret only in env — PID 66365, PPID 1,
  `--wait --edge right`, log at `$TMPDIR/mouser-host.log`. Log shows input
  capture running and listening on 0.0.0.0:47583. No PC connection
  established at check time; the PC still needs to pull `1f48dfa`, rebuild
  and relaunch. Windows runtime of the new input code remains untested.
- **2026-10-16 ~16:25 EDT** — Antigravity on the Windows PC: diagnosed and
  fixed the remote mouse stutter / failure to move smoothly. Root cause:
  `RECENTER_JUMP = 8.0` with `await_recenter` in `src-tauri/src/state.rs`
  dropped every single move event once a fast swipe clamped against the
  edge (`x <= 1.0`), permanently stalling deltas while moving towards
  the peer. Furthermore, `recenter_anchor` only checked the shared edge,
  pinning other directions, and `mouse_hook` reported `dx: 0, dy: 0`
  causing warp jumps. Fixed by:
  (1) tracking `LAST_POS` in `mouse_hook` (`windows.rs`) to update on
  injected warps and emit true relative `(dx, dy)`;
  (2) centering the local cursor on handoff and recentering back toward
  center when drifting beyond the central region (`recenter_offset`),
  giving infinite travel in all directions;
  (3) removing `await_recenter` so no hardware move events are ever dropped;
  (4) landing the remote cursor 2-3 px inland in `take_control` to prevent
  instant ping-pong returns, and warping to the return seam on `release_control`.
  Workspace tests: 72 passed, 0 failed. Dev binary rebuilt and relaunched
  (PID 15724, paired to Mac at 192.168.68.68:47583 [0788FEF2], Established).
- **2026-10-16 ~16:41 EDT** — Antigravity on the Mac host: resolved merge
  conflict in `AGENTS.md`, preserving both host and PC handoff records. Synced
  with `origin/main` (at `f2d35c3`). Ran workspace test suite (72 passed, 0
  failed across all crates). Stopped previous host (PID 66365), rebuilt dev
  profile, and relaunched detached via Python Popen (PID 80545, PPID 1, log at
  `$TMPDIR/mouser-host.log`). Verified listen on `0.0.0.0:47583`, established
  connection to windows-pc (`192.168.68.60:52024` `[0788FEF2]`), and peer
  control transfer confirmed in log.
- **2026-10-16 ~17:45 EDT** — Antigravity on the Mac host: diagnosed and
  fixed mouse failing to move on Windows PC when handed off from Mac.
  Root cause: when Mac handed off control or recentered, `warp_cursor`
  teleported the cursor to screen center `(cx, cy)`. On macOS, Quartz's
  WindowServer computes mouse deltas (`MOUSE_EVENT_DELTA_X/Y`) based on
  the difference between the pre-warp and post-warp cursor positions,
  creating a massive artificial delta spike (hundreds of pixels directly
  pointing back towards the return seam). This huge delta was immediately
  forwarded to Windows, where `returns_home` detected an overshoot past
  the return margin and instantly sent `ReleaseControl` within ~25ms.
  Fixed by:
  (1) tracking `WARPED_TARGET` in `crates/mouser-input/src/macos.rs` so that
  the synthetic warp transition delta reported by Quartz upon reaching the
  target position is recognized and zeroed out (`dx = 0, dy = 0`), matching
  Windows's `LAST_POS` delta isolation;
  (2) guarding `(x - prev_x, y - prev_y)` fallback in `src-tauri/src/state.rs`
  so that when forwarding is active, a zero delta event never synthesizes
  a synthetic jump from position differences.
  Workspace tests: 72 passed, 0 failed. Dev binary rebuilt and relaunched
  detached via Python Popen (PID 96879, PPID 1, log at `$TMPDIR/mouser-host.log`).
- **2026-10-16 ~18:25 EDT** — Antigravity on the Mac host: diagnosed and fixed
  Mac cursor not disappearing / not staying parked at the screen edge when handing
  off control to Windows.
  Root causes:
  (1) `CGDisplay::main().hide_cursor()` on macOS does not hide the cursor system-wide
  when called from a background process or non-focused app unless the WindowServer
  connection property `SetsCursorInBackground` is enabled via CoreGraphics
  (`_CGSDefaultConnection` / `CGSSetConnectionProperty`);
  (2) `handoff` and `recenter` in `src-tauri/src/state.rs` unconditionally called
  `warp_cursor(cx, cy)`, teleporting the Mac cursor to the exact center of the screen
  and wandering around center whenever the user moved the mouse;
  (3) `suppress_local` in `crates/mouser-input/src/lib.rs` explicitly exempted `Move`
  events (`!matches!(event, CapturedEvent::Move)`), causing local macOS WindowServer
  to process every forwarded mouse move, moving the Mac cursor across the screen
  and unhiding it.
  Fixed by:
  (1) invoking `CGSSetConnectionProperty(_CGSDefaultConnection(), ...)` with
  `SetsCursorInBackground = true` and hiding/showing across active displays in
  `crates/mouser-input/src/macos.rs`;
  (2) gating screen center warp in `handoff` and `recenter` in `src-tauri/src/state.rs`
  to `#[cfg(windows)]` so macOS keeps the pointer parked at the seam;
  (3) updating `suppress_local` to suppress `Move` events on macOS while forwarding,
  so WindowServer never moves or unhides the local cursor while driving the peer.
  Workspace tests: 72 passed, 0 failed. Dev binary rebuilt (`cargo build`).
  Host relaunched detached via Python Popen (PID 7732, PPID 1, log at `$TMPDIR/mouser-host.log`).
  Verified listening on `0.0.0.0:47583`, paired and ESTABLISHED with `windows-pc` at
  `192.168.68.60:49348 [0788FEF2]`.
- **2026-10-16 ~16:45 EDT** — Antigravity on the Windows PC: implemented global
  cursor hiding on Windows and parked the forwarded pointer along the seam edge.
  (1) implemented `hide_cursor` in `windows.rs` using `SetSystemCursor` with a
  transparent cursor across standard system cursor IDs and `show_cursor` with
  `SystemParametersInfoW(SPI_SETCURSORS, ...)` plus a `Drop` guard on
  `WindowsBackend`, making the local cursor disappear entirely while controlling
  the peer;
  (2) replaced screen-center warping with `recenter_seam` (`EDGE_ANCHOR = 8.0`,
  `EDGE_MAX_DRIFT = 16.0`), parking the cursor directly along the seam edge
  instead of in the middle of the screen, while giving sufficient headroom
  on the seam and perpendicular boundaries so hardware motion deltas remain
  uninterrupted in all directions.
  Workspace tests: 73 passed, 0 failed. Dev binary rebuilt and relaunched
  (PID 22812, paired to Mac at 192.168.68.68:47583 [0788FEF2], Established).

- **2026-10-16 ~16:52 EDT** — opencode on the Windows PC, user asked to "open
  the updated mouser app". Found one already running: PID 18032, started
  16:47:38, launched by Antigravity's language server via a blocking
  `powershell -Command ... & mouser.exe` (background tool call), stderr not
  redirected, which is why the live log sat stale at 16:43:56. Sources were
  older than the 16:42:21 exe and `cargo build` was a 0.77 s no-op (exit 0),
  so the binary included the current tree (stutter fix `f2d35c3` plus the
  uncommitted `windows.rs`/`state.rs` cursor-hiding edits); no fetch/pull
  needed, tree still dirty as left by prior agents. That instance had **zero
  top-level windows** (EnumWindows, helper validated against explorer's 53
  windows) while its WebView2 tree, created at the same second as the
  process, was still alive and one renderer ticking — i.e. the window was
  destroyed while the process survived, same family as the 10/4 tao
  `Destroyed` panic but without the panic. Restarted per procedure:
  `Stop-Process -Name mouser -Force`, waited, built (no-op), then the
  canonical `Start-Process` launch with log redirect, window shown,
  16:51:15. This also ended Antigravity's blocking tool shell (its "Exit
  code:" line fires on the kill). Done-checks green: PID 3896, session 1,
  visible window hwnd 1116754 title `mouser` rect 1472,370–1968,1029,
  TCP Established to 192.168.68.68 (local :58154), log shows
  `paired with mac at 192.168.68.68:47583 [0788FEF2]`, WebView2 tree
  46.9 ms/3 s (live, not suspended). **Open item for the Mac side**: the
  `edge settings disagree` WARN repeats on every pairing since 16:43 —
  this client is `--edge left` (Mac on the PC's left, per this file), but
  the Mac peer reports the PC on ITS top, so control hands over but crosses
  in the wrong direction. Fix the Mac's edge config (`--edge right` per
  this file, or whatever the user actually wants) on the host side. No
  commits, no push; AGENTS.md edits remain uncommitted with the others.

- **2026-10-16 ~18:10 EDT** — Antigravity on the Windows PC: created desktop
  shortcut for the app per user request.
  (1) Created `%USERPROFILE%\.config\mouser\launch.vbs` to manage clean launches
  without a console flash: activates existing mouser window if one is open,
  terminates stale headless instances (e.g. lingering background process after
  window close), reads pairing secret from `%USERPROFILE%\.config\mouser\client-secret`
  into `MOUSER_SECRET`, and launches `mouser.exe` detached with stderr redirected
  to `%TEMP%\opencode\mouser-live.err.log`.
  (2) Set `MOUSER_SECRET` in user environment variables (`HKCU\Environment`).
  (3) Created desktop shortcut `Mouser.lnk` at `C:\Users\eduar\Desktop\Mouser.lnk`
  pointing to `launch.vbs` with official icon `src-tauri\icons\icon.ico`.
  (4) Verified live: PID 9584 running, TCP Established to 192.168.68.68:47583,
  paired log confirmed, and user actively using cursor handoff across machines.

- **2026-10-16 ~19:00 EDT** — Codex inspected read-only Windows crash evidence
  after the user reported a black screen while playing Minecraft. Event 6008
  records the unexpected shutdown at 18:15:40; Kernel-Power 41 at 18:46:06
  has BugcheckCode 0 and a power-button timestamp at 18:45:27, consistent with
  a hard hang followed by a manual restart rather than a recorded BSOD. No new
  Oct 16 dump was present. WER replayed older AMD LiveKernelEvent watchdog
  reports (141/117/193/1b0), newest dump dated 2026-09-30; prior dumps also
  exist from Apr-Jun and 2026-09-28, indicating recurring AMD GPU/driver hangs.
  GPU is an AMD Radeon RX 5700 XT, driver 32.0.21045.5002 dated 2026-08-16.
  Minecraft launcher logging stopped at 18:45:40; no Minecraft crash report or
  Java crash was found. Sunshine.exe faulted during recovery at 18:48:48 and
  appears secondary. Windows installed KB5127648 and KB5127690 at 18:51:28.
  No application code, mouser process, or configuration was changed.

- **2026-10-16 ~19:07 EDT** — Codex inspected the logs after a second black
  screen report. Windows booted at 19:03:14 and logged Kernel-Power 41 at
  19:03:16 with BugcheckCode=0, no WHEA boot errors, and no power-button
  timestamp. Event 6008 reports the preceding shutdown as unexpected; the
  exact hang time is not recorded because the machine stopped logging. No new
  Oct 16 watchdog dump or display-driver-reset event appeared. WER continues
  replaying only older watchdog dumps (latest saved dump remains 2026-09-30).
  This confirms a repeat hard hang/reset, strengthening the AMD GPU/driver or
  GPU stability diagnosis. No application code, mouser process, or
  configuration was changed.

- **2026-10-17 ~00:55 EDT** — opencode on the Windows PC, user asked for a
  read-only edge-case/bug audit of the working tree. Read everything
  (mouser-core/-net/-input, src-tauri, ui, tests, plus the uncommitted diff);
  no code changed, no build, no tests run (live client PID 19592 left paired).
  Highest-impact findings, full report in the session: (1) keys/buttons held
  *across* a handoff never get their release on the handing-off machine — the
  up is suppressed while forwarding, so Shift/drag can stay stuck until
  another press; (2) `hid_from_scan` has Pause and NumLock inverted: plain
  scan 0x45 (Pause) maps to HID 0x53 NumLock, while the real NumLock (E0 45)
  is unmapped and toggles locally instead of forwarding; (3) ISO extra key
  (scan 0x56 → HID 0x64) has no reverse mapping, so `\` on ISO layouts is
  swallowed while forwarding and dead on the peer; (4) XButtons (M4/M5) are
  neither captured nor injected on Windows though SendInput supports them
  (the "cannot express" comment is wrong), yet macOS handles them both ways;
  (5) a simultaneous two-sided handoff can deadlock both machines
  (`take_control` unconditionally clears forwarding) until one app restarts;
  `SessionEvent::ReleaseHotkey` exists but is never emitted, and no UI
  release exists. Docs drift: the live seam is bottom/top (crossings log
  "edge bottom", zero "disagree" warnings) while this file's tables still say
  left/right; mouser-input's hide_cursor doc still calls the Windows hide a
  no-op after 16:45; mouser-net's "length travels inside the ciphertext" note
  is wrong (the 4-byte prefix is cleartext). Tree untouched apart from this
  entry.

- **2026-10-17 ~01:25 EDT** — Antigravity on the Windows PC: implemented audit
  fixes per user request ("do it").
  (1) `src-tauri/src/link.rs`: in `host_loop`, replaced `break` with
  `tokio::time::sleep(RETRY).await; continue;` on `listener.accept()` errors to
  prevent transient failures from killing the host listener. On intentional
  session teardown (queue failure, secret change), send `Message::Bye` with a
  short timeout so peer does not stall for 10s.
  (2) `crates/mouser-input/src/windows.rs`:
  - Removed panics from hook callbacks: `forward()` and `mouse_hook()` now use
    safe `.lock()` checks instead of `.unwrap()` / `.expect()`, avoiding FFI
    unwinding across foreign OS frames.
  - Added system cursor recovery panic hook via
    `install_cursor_restore_panic_hook()` in `WindowsBackend::new()` so if a
    panic ever occurs, `SystemParametersInfoW(SPI_SETCURSORS, ...)` restores
    all system cursors immediately.
  - Implemented mouse side-button (M4/M5 / Back/Forward) capture and injection
    on Windows: capture `WM_XBUTTONDOWN`/`WM_XBUTTONUP` with
    `XBUTTON1`/`XBUTTON2` mapping to `Button::Back`/`Button::Forward`, and
    inject via `MOUSEEVENTF_XDOWN`/`MOUSEEVENTF_XUP` with `XBUTTON1`/`XBUTTON2`.
  - Refactored `mouse_input` helper so `dx`, `dy`, and `mouseData` are explicit
    and never conflated; removed unused `NO_MOVE` constant.
  - Set hardware scan code `wScan` in `keyboard_input()` via
    `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)` for compatibility with games and
    remote desktop tools.
  - Captured active keyboard modifiers (`shift`, `ctrl`, `alt`, `meta`) via
    `GetAsyncKeyState` in `keyboard_hook()` instead of `Modifiers::default()`.
  (3) `crates/mouser-core/src/session.rs`: added `CrossingResult` trait for
  `detect_crossing` in `Session::on_event` so edge crossing fraction is
  preserved and propagated rather than hardcoded to 0.0. Added unit test
  `crossing_propagates_fraction`.
  (4) `crates/mouser-net/src/lib.rs`: combined length prefix and payload into
  a single buffer in `write_frame` to prevent partial frame write corruption
  on timeout; enforced minimum ciphertext length of `TAG_LEN` in `recv` to
  reject unauthenticated 0-byte frame drops.
  Validation: `cargo test --workspace` green (75 passed, 0 failed, 0 warnings).
  Dev binary refreshed, client relaunched detached (PID 19956, session 3),
  TCP Established to 192.168.68.68:47583, paired log verified. No commits or
  pushes made.

- **2026-10-22** — Claude Code on the Windows PC, user asked for a cleanup pass
  over the uncommitted work above. Behavior kept, code tightened:
  (1) reverted the `CrossingResult` trait in `session.rs` — nothing but its own
  test used it (the app computes the fraction via `Rect::crossing` and passes
  `|..| false`); `session.rs` matches HEAD again.
  (2) `windows.rs`: replaced the inline fully-qualified Win32 paths with imports
  and a `CURSOR_SHAPES` table; one `restore_system_cursors()` shared by show,
  drop and the panic hook; **cursors are now also restored on backend startup**
  so a `Stop-Process -Force` while hidden (the update procedure does exactly
  that) no longer leaves the desktop cursorless until the next launch; hide/show
  are idempotent via a `cursor_hidden` flag (no re-creating 13 cursors or
  reloading the scheme on every no-op show); merged the duplicated XBUTTON
  arms; deleted `cursor_hide_and_show_cycles_cleanly`, which hid the real
  system cursor on every `cargo test`.
  (3) `state.rs`: `seam_anchor()` replaces the four-arm park match in `handoff`
  and backs `recenter_seam()` (now ~15 lines instead of ~60); dropped stale
  "Windows hide is a no-op" comments here and in `mouser-input/src/lib.rs`;
  added Bottom-edge cases to the recenter test (the live seam).
  (4) `link.rs`: `say_bye` flag instead of `outcome.starts_with(...)`, and a
  named `BYE_TIMEOUT`.
  `cargo fmt`, `cargo clippy --workspace --all-targets` clean;
  `cargo test --workspace` 73 passed, 0 failed (75 minus the two removed
  tests). **Not rebuilt or relaunched** — the live client still runs the
  pre-cleanup exe. No commits or pushes.
- **2026-10-22 ~02:44 EDT** — Claude Code on the Windows PC, user asked for
  the update procedure: killed the client, `cargo build` dev OK (26.7 s),
  relaunched via the canonical command at 02:43:40 (PID 21556, window
  `mouser` shown). Done-checks partial: process alive, but every connect
  attempt gets `actively refused (os error 10061)` — the Mac answers, but
  nothing listens on 47583, so the **Mac host is down** and needs the canonical
  relaunch there. No pairing, no Established TCP. The client keeps retrying and
  will pair by itself once the listener is back. No commits or pushes.
- **2026-10-22** — Claude Code on the Windows PC, user asked to push to `main`.
  `origin/main` had moved to `a3f2979` (the Mac agent's 10/16 macOS
  warp-spike and cursor-hide fixes), so: stash → `pull --ff-only` → stash pop,
  conflicts resolved by hand. `state.rs`: kept the seam parking
  (`seam_anchor`/`recenter_seam`), dropped the now-unused `recenter_offset`,
  and kept upstream's rule that only Windows warps the parked pointer — written
  as `if cfg!(windows)` so the helpers still compile (no `dead_code` allows)
  on macOS. `AGENTS.md`: both sides' log entries kept, upstream's first.
  Committed everything (prior agents' uncommitted PC work + the cleanup) and
  pushed. Note: the Mac's 10/16 entries log the PC at `192.168.68.60`, not
  the `.67` in the tables above.

- **2026-10-22 ~03:20 EDT** — Claude Code on the Windows PC, user asked for
  the client to find the Mac by name instead of IP (DHCP had moved the Mac
  from `.68` to `.65`; the desktop shortcut still dialled `.68`).
  `Config::peer_addr()` now returns a validated `host:port` string (IP
  literal or hostname, non-zero port) instead of a `SocketAddr`, and
  `client_loop` passes it to `Channel::connect` each attempt, so resolution
  re-runs on every retry. Private-network guard unchanged (checks the
  resolved peer after connect). New test
  `peer_addr_accepts_hostnames_and_requires_a_port`; `cargo test
  --workspace` 74 passed, 0 failed. Built dev, relaunched with
  `--connect eds-MacBook-Pro.local:47583` (PID 15868): five
  `No such host is known` retries while mDNS warmed up, then
  `paired with mac at eds-MacBook-Pro.local:47583 [0788FEF2]`, TCP
  Established to 192.168.68.65. Updated `launch.vbs` and the canonical
  command above. **Mac host needs no change.** Still open: the
  `edge settings disagree` warning (PC says left, Mac says top; the live
  seam is bottom/top per the 10/17 entry). Committed and pushed on user request.
