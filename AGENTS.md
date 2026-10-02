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
  -ArgumentList "--connect","192.168.68.68:47583","--edge","left","--name","windows-pc","--verbose" `
  -WindowStyle Minimized -RedirectStandardError "$env:TEMP\opencode\mouser-live.err.log"
```

The client does not auto-start at boot; if `Get-Process mouser` comes up
empty after a reboot, relaunch with the command above.

Update procedure (pull → build → restart, same order as the host):

1. `git fetch origin && git status` — clean tree expected; **stash, never
   discard**. Then `git pull --ff-only origin main`. No rebases, no force.
2. **Kill the exe before any build**: `Stop-Process -Name mouser -Force`,
   then wait ~1 s — the running exe locks `target\debug\mouser.exe` and the
   link step fails while it lives. Dropping the link is fine; the client
   re-pairs by itself within ~2 s of the host listener coming back.
3. `cargo test --workspace` — expect **67 passed on Windows, 0 failed**
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
