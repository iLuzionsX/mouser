# mouser

One mouse and keyboard, two computers on the same local network.

Point each machine at the side the other screen is on, then push the cursor off
that edge. It appears on the other computer; push it back and it returns.

Works on Windows and macOS, built with Rust and Tauri.

> **Status: early.** The pieces are in place and tested, but the macOS backend
> has not been exercised end-to-end under real Accessibility permission yet,
> and there is no installer or auto-discovery yet. See [Status](#status) for
> what is and is not finished.

## How it works

Each machine runs `mouser`. One waits for connections, the other dials it.
Once connected they exchange a Noise handshake keyed by a secret you choose,
after which every event is encrypted.

The cursor crosses between machines by going off an edge. Neither machine
moves its real cursor to another monitor — instead, mouser watches for the
cursor reaching the chosen edge *and* continuing to move outwards. Since the
OS clamps the pointer at the screen border, that "pushing past the edge" is
the signal to hand over control:

```
  machine A            machine B
┌──────────────┐     ┌──────────────┐
│              │     │              │
│         [ A  │────▶│  B ]         │
│              │     │              │
└──────────────┘     └──────────────┘
   other screen is on the right
```

A selects `right >` and B selects `< left`. The cursor leaves A's right edge
and appears on B's left edge; pushing past B's left edge brings it home.

Because the two screens can differ in size or resolution, the crossing
position travels as a *fraction* along the shared edge, not as a pixel
coordinate. The cursor lands at the matching relative spot on whatever screen
it arrives at.

## Security

This app injects input into your computer. Anything that could do that is
worth being careful about, so the design assumes the network is not friendly.

- **Mutual authentication.** The link uses Noise `NNpsk0`: neither machine can
  complete the handshake without the shared secret, so an attacker who
  connects gets no session at all.
- **Forward secrecy.** Session keys are ephemeral. A recording of the traffic
  stays unreadable even if the secret is later exposed.
- **Encryption and integrity** on every message, including the length prefix,
  so the stream cannot be desynchronized or tampered with.
- **Nonces never repeat**, because each direction has its own counter.
- **Private addresses only**, by default. A connection from a public address is
  refused before the handshake, so a stray port forward does not become remote
  control. `--allow-public` turns this off if you really want it.
- **Short secrets are rejected.** Under 8 characters is refused outright.
- **Fingerprints.** Both machines show an 8-character hash of the secret. After
  connecting, check they match — this catches a mistyped or stale secret.
- **The secret is never written to disk** and never sent back to the UI. Only
  the derived fingerprint is.

What this does **not** do: the secret is stretched with SHA-256, which is not
a password KDF and will not stop an attacker from guessing a weak secret
offline. Pair over a network you trust, and pick something long.

## Install

Needs Rust 1.85 or newer.

```sh
git clone https://github.com/iLuzionsX/mouser
cd mouser

# Icons are committed; regenerate after editing icon.svg with:
#   cargo tauri icon icon.svg

cargo run            # run it
cargo test           # run the tests
cargo tauri build    # produce installers
```

## Use

On the machine that has the physical mouse and keyboard:

```sh
mouser --edge right
```

On the other machine:

```sh
mouser --wait --edge left
```

Enter the same secret on both, and check the fingerprints match.

The pairing secret can come from the window's field or from the environment:

```sh
MOUSER_SECRET="a long shared phrase" mouser --edge right
```

Prefer the environment variable. `--secret` works, but it is visible to other
processes and stays in your shell history.

### Options

| Flag | Meaning |
| --- | --- |
| `--connect ADDR` | Dial a peer instead of waiting, e.g. `192.168.1.20:47583` |
| `--wait` | Wait for the peer to connect (the default) |
| `--edge left\|right\|top\|bottom` | Which side the other screen is on |
| `--name NAME` | Name shown on the other machine |
| `--allow-public` | Accept connections from public addresses |
| `-v, --verbose` | Log to stderr as well as the window |

Settings persist to the platform config directory. A flag overrides the saved
value for that run only.

### Permissions

**macOS** will not deliver input events to an app without Accessibility
permission, and there is no way around it. Go to System Settings → Privacy &
Security → Accessibility, add `mouser`, and restart it. Input Recording is
needed too. If hooks silently do nothing, this is almost always why.

**Windows** needs no special permission. If the pointer stops responding
immediately, something is holding an exclusive mouse hook — check other
remote-control tools.

## Layout

```
crates/
  mouser-core/    layout math, wire protocol, session state machine
  mouser-net/     Noise handshake and encrypted framing
  mouser-input/   platform capture and injection
src-tauri/        Tauri shell, link driver, CLI
ui/               frontend (plain HTML, CSS, and JS)
```

The three libraries have no GUI or platform-window dependencies, so the
interesting logic is unit tested: edge detection, coordinate mapping, the
protocol encoding, ownership transitions, and the security properties.

`mouser-core` holds the rules. `mouser-net` holds the crypto. `mouser-input`
holds the platform calls. The Tauri crate wires them together and exposes four
commands to the webview, which is the whole surface the frontend can reach.

## Status

Working and tested:

- Layout math, including size- and resolution-mismatched screens
- Wire protocol, with round-trip and size-bound tests
- Noise handshake, encrypted framing, and refusal of wrong secrets
- Link state machine and its ownership transitions
- Edge handoff and return, including the mirrored-cursor return crossing
- Windows capture and injection
- macOS capture and injection, including drag events, modifier keys, tap
  re-enabling, and unioned multi-display bounds
- The UI

Not finished:

- **The macOS backend has not been run end-to-end.** Every API it touches has
  been checked against the `core-graphics` 0.25 and `core-foundation` 0.10
  sources and CI confirms it compiles, and its translation logic is unit
  tested, but a `CGEventTap` capture/inject round trip has not been observed
  live. It needs one session under real Accessibility permission.
- No installers or release packaging beyond `cargo tauri build`
- No peer auto-discovery; addresses are typed or configured
- No clipboard sharing
- Back/forward mouse buttons are not sent on Windows, because `SendInput`
  cannot express `XBUTTON1`/`XBUTTON2`
- Keypad digits are not mapped on Windows: their scan codes are ambiguous
  under NumLock
- No release hotkey, so control returns only by crossing the edge back

## Porting notes

Each platform's file is self-contained and depends only on `mouser-core` and
its own crate's event types:

| Platform | Capture | Injection | Key identity |
| --- | --- | --- | --- |
| Windows | `WH_MOUSE_LL`, `WH_KEYBOARD_LL` | `SendInput` | scan code (Set 1) |
| macOS | `CGEventTap` | `CGEvent` | virtual keycode |

A backend implements `mouser_input::InputBackend` — six methods, no platform
types in the signature. Everything above that layer is shared, so a new
platform means one new file and no changes elsewhere.

The Windows scan-code table and the shared keymap are cross-checked against
each other by a test (`scan_table_agrees_with_the_keymap`), because both are
hand-written tables encoding the same physical keys and drift apart silently.

### macOS specifics

The tap must be serviced by a run loop, so capture runs on a dedicated thread
holding `CFRunLoop::run_current`; `stop_capture` stops that loop and joins the
thread. `CFRunLoop` is `Send`, which is what makes stopping it from another
thread possible.

Two details are worth knowing:

- A tap disabled by the system (timeout or user input) is re-enabled from
  inside the callback. The callback is not handed the tap handle, so the
  backend keeps its Mach port and calls `CGEventTapEnable` directly: without
  this, one slow callback would silently stop capture until a restart.
- `screen_info` unions every active display, not just the main one. macOS
  reports `CGDisplayBounds` and `CGEventGetLocation` in the same top-left-origin
  global frame, so no coordinate flip is needed between the two.

## Licence

Dual licensed, at your option: [`LICENSE-MIT`](LICENSE-MIT) or
[`LICENSE-APACHE`](LICENSE-APACHE).