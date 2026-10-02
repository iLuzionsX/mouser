# AGENT HANDOFF — read this first, append before you finish

This file is the shared memory between agent sessions working on a mouser
host. Read the whole file, do your task, then append an entry to the
**Handoff log** at the bottom before reporting done. Do not delete history;
append only. The values below describe the primary host; on another machine,
adapt paths and addresses the same way.

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

## Handoff log (append-only)

- **2026-10-02 ~13:20** — opencode (system.ai.glm-5-3): pulled `b6b6517`
  "Pump input at full rate..." then `efa8453` "Bound the client's connect...".
  Tests 66/66 on macOS. Host rebuilt (dev) and relaunched detached —
  PID 97894, `--wait --edge right`, port 47583, secret via env. PC paired
  immediately from 192.168.68.67:64717. Tree clean. Created this file,
  `~/.config/mouser/host-secret`, and the `.git/info/exclude` entry.
