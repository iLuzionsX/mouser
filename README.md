# mouser

One mouse and keyboard, two computers on the same local network. Push the
cursor off the shared screen edge and it drives the other machine; push it
back and it returns. Windows and macOS, one small headless binary.

```
  Mac (host)           Windows PC (client)
┌──────────────┐     ┌──────────────┐
│         [ A  │────▶│  B ]         │
└──────────────┘     └──────────────┘
  --edge right          --edge left
```

The crossing position travels as a fraction along the edge, so screens of
different sizes line up.

## What crosses

Mouse motion, buttons (including back/forward) and the wheel cross from either
machine. Only the Mac keyboard is forwarded; the PC keyboard is never hooked
and always types locally.

| | Captures | Injects |
|---|---|---|
| Mac | mouse + keyboard | mouse |
| Windows | mouse | mouse + keyboard |

## Security

- **Noise `NNpsk0`** (Curve25519, ChaCha20-Poly1305, SHA-256, via `snow`):
  neither side completes the handshake without the shared secret, and session
  keys are ephemeral (forward secrecy).
- Fixed 24-byte encrypted frames with implicit per-direction nonces: a
  replayed, dropped or tampered frame ends the link.
- **Private addresses only** unless `--allow-public`; checked before the
  handshake.
- The secret comes only from `MOUSER_SECRET` (never an argument, never
  written to disk) and must be at least 8 characters. Both sides log an
  8-character fingerprint of it, to compare by eye.

The secret is stretched with SHA-256, which is not a password KDF: pick a long
one and pair on a network you trust.

## Build and run

Needs Rust 1.85 or newer.

```sh
cargo test
cargo build --release      # target/release/mouser(.exe), a few hundred KB
```

```sh
# on the host (the Mac here)
MOUSER_SECRET="a long shared phrase" mouser --wait --edge right
# on the client, which re-resolves the host's name on every retry
MOUSER_SECRET="a long shared phrase" mouser --connect eds-MacBook-Pro.local:47583 --edge left
```

| Flag | Meaning |
| --- | --- |
| `--wait` | Listen on `0.0.0.0:47583` |
| `--connect HOST:PORT` | Dial the host; an IP or a name such as `host.local:47583` |
| `--edge left\|right\|top\|bottom` | Which side the other screen is on, from this machine |
| `--allow-public` | Accept peers outside private address ranges |

Status goes to stderr, one timestamped (UTC) line per event: `paired with …`,
`-> peer …`, `-> local control …`, `link down: …`. The Windows release build
has no console window, so launch it with stderr redirected to a file.

The link is kept alive with a frame every 2 s and dropped after 6 s of
silence; the client redials every 2 s, and a host accepts a new pairing at any
time, replacing the old one. Keys and buttons held when the link drops or
control returns are released.

### Permissions

**macOS** needs Accessibility and Input Monitoring for the binary (System
Settings → Privacy & Security). Without them mouser exits with a message
saying so.

**Windows** needs nothing special. Hiding the cursor swaps the system cursor
set; mouser restores it at startup, on Ctrl+C and on panic, so a killed
instance is cleaned up by the next launch.

## Layout

```
src/main.rs   arguments, connect/accept loop, the event loop
src/wire.rs   frame encoding, Noise handshake, HID key table
src/kvm.rs    Local / Forwarding / Remote state machine, seam geometry
src/win.rs    WH_MOUSE_LL capture, SendInput injection, cursor hiding
src/mac.rs    CGEventTap capture, CGEvent injection, cursor hiding
```

The state machine talks to the platform through a five-method `Io` trait, so
crossing, return, parking and held-key release are unit tested without a
screen.

## Licence

Dual licensed, at your option: [`LICENSE-MIT`](LICENSE-MIT) or
[`LICENSE-APACHE`](LICENSE-APACHE).
