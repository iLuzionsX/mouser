// Release builds run without a console window; launch with stderr redirected.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! One mouse and keyboard across two computers: push the cursor off the
//! shared edge and it, plus the keyboard, drives the other machine.

/// A line on stderr, stamped with the UTC time of day.
macro_rules! log {
    ($($arg:tt)*) => {{
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        eprintln!("{:02}:{:02}:{:02}Z {}", t / 3600 % 24, t / 60 % 60, t % 60, format!($($arg)*));
    }};
}

mod kvm;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;
mod wire;

#[cfg(target_os = "macos")]
use mac as platform;
#[cfg(windows)]
use win as platform;

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use kvm::{CLAIMS, Captured, Edge, Kvm};
use wire::{Link, Msg};

const LISTEN: &str = "0.0.0.0:47583";
const RETRY: Duration = Duration::from_secs(2);
/// Each side sends at least this often, and gives up on a peer silent for
/// three times as long.
const KEEPALIVE: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(6);

const USAGE: &str =
    "usage: mouser (--wait | --connect HOST:PORT) --edge left|right|top|bottom [--allow-public]
  The pairing secret (8+ characters, same on both machines) comes from MOUSER_SECRET.";

/// Everything the state machine reacts to, from every thread.
pub enum Ev {
    Local(Captured),
    Up(u64, Link, SocketAddr),
    Peer(u64, Msg),
    Down(u64, String),
}

struct Args {
    connect: Option<String>,
    edge: Edge,
    allow_public: bool,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut connect, mut wait, mut edge, mut allow_public) = (None, false, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wait" => wait = true,
            "--allow-public" => allow_public = true,
            "--connect" => connect = Some(args.next().ok_or("--connect needs HOST:PORT")?),
            "--edge" => {
                let e = args.next().unwrap_or_default();
                edge = Some(Edge::parse(&e).ok_or(format!("unknown edge {e:?}"))?);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if wait == connect.is_some() {
        return Err("pass exactly one of --wait and --connect".into());
    }
    if let Some(addr) = &connect {
        let port = addr.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
        if !matches!(port, Some(1..)) {
            return Err(format!("{addr:?} is not HOST:PORT"));
        }
    }
    let edge = edge.ok_or("--edge is required")?;
    Ok(Args { connect, edge, allow_public })
}

fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    });
    let secret = std::env::var("MOUSER_SECRET").unwrap_or_default();
    let secret = secret.trim();
    if secret.len() < 8 {
        eprintln!("MOUSER_SECRET must hold the pairing secret, at least 8 characters\n{USAGE}");
        std::process::exit(2);
    }
    let psk = wire::psk(secret);
    let tag = wire::fingerprint(secret);

    let (tx, rx) = channel();
    let io = platform::start(tx.clone()).unwrap_or_else(|e| {
        log!("could not capture input: {e}");
        std::process::exit(1);
    });
    log!("input capture running; peer is to the {:?}", args.edge);

    let host = args.connect.is_none();
    thread::spawn(move || match args.connect {
        Some(addr) => dial(&addr, &psk, args.allow_public, &tx),
        None => listen(&psk, args.allow_public, &tx),
    });

    let mut kvm = Kvm::new(io, &CLAIMS, args.edge, host);
    let mut link: Option<(u64, Link)> = None;
    let mut last_sent = Instant::now();
    loop {
        let out = match rx.recv_timeout(KEEPALIVE) {
            Ok(Ev::Local(ev)) => kvm.local(ev),
            Ok(Ev::Peer(id, msg)) if link.as_ref().is_some_and(|l| l.0 == id) => kvm.peer(msg),
            Ok(Ev::Up(id, new, addr)) => {
                // The newest pairing wins: the old peer is gone or restarting.
                if let Some((_, mut old)) = link.replace((id, new)) {
                    let _ = old.send(Msg::Bye);
                    old.close();
                }
                kvm.link(true);
                log!("paired with {addr} [{tag}]");
                None
            }
            Ok(Ev::Down(id, why)) if link.as_ref().is_some_and(|l| l.0 == id) => {
                link = None;
                kvm.link(false);
                log!("link down: {why}");
                None
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let Some((_, l)) = &mut link else { continue };
        let Some(msg) = out.or_else(|| (last_sent.elapsed() >= KEEPALIVE).then_some(Msg::Nop))
        else {
            continue;
        };
        last_sent = Instant::now();
        if let Err(e) = l.send(msg) {
            l.close();
            link = None;
            kvm.link(false);
            log!("link down: send failed: {e}");
        }
    }
}

/// Client: dial the host, re-resolving its name on every attempt so DHCP
/// moving it does not break pairing.
fn dial(addr: &str, psk: &[u8; 32], allow_public: bool, tx: &Sender<Ev>) {
    let mut last_error = String::new();
    for id in 0..=u64::MAX {
        let stream = addr.to_socket_addrs().and_then(|addrs| {
            let mut err = std::io::Error::other("name resolved to no address");
            for a in addrs {
                match TcpStream::connect_timeout(&a, RETRY) {
                    Ok(s) => return Ok(s),
                    Err(e) => err = e,
                }
            }
            Err(err)
        });
        match stream {
            Ok(s) => {
                session(s, true, psk, allow_public, id, tx);
                last_error.clear();
            }
            // Logged once per distinct error, not every two seconds.
            Err(e) if e.to_string() != last_error => {
                last_error = e.to_string();
                log!("connect to {addr} failed: {e}; retrying");
            }
            Err(_) => {}
        }
        thread::sleep(RETRY);
    }
}

/// Host: accept peers; each runs on its own thread, and the newest wins.
fn listen(psk: &[u8; 32], allow_public: bool, tx: &Sender<Ev>) {
    let listener = loop {
        match TcpListener::bind(LISTEN) {
            Ok(l) => break l,
            Err(e) => log!("could not listen on {LISTEN}: {e}; retrying"),
        }
        thread::sleep(RETRY);
    };
    log!("listening on {LISTEN}");
    for id in 0..=u64::MAX {
        match listener.accept() {
            Ok((s, _)) => {
                let (psk, tx) = (*psk, tx.clone());
                thread::spawn(move || session(s, false, &psk, allow_public, id, &tx));
            }
            Err(e) => {
                log!("accept failed: {e}");
                thread::sleep(RETRY);
            }
        }
    }
}

/// Handshake, announce the link, then pump decrypted frames until it fails.
fn session(s: TcpStream, initiator: bool, psk: &[u8; 32], public: bool, id: u64, tx: &Sender<Ev>) {
    let Ok(addr) = s.peer_addr() else { return };
    if !public && !wire::is_private(addr) {
        log!("refused {addr}: not a private address (see --allow-public)");
        return;
    }
    let setup = (|| {
        // Input events are tiny; each should leave immediately.
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(TIMEOUT))?;
        s.set_write_timeout(Some(KEEPALIVE))?;
        wire::handshake(s, psk, initiator)
    })();
    let (link, mut reader) = match setup {
        Ok(pair) => pair,
        Err(e) => {
            log!("handshake with {addr} failed: {e} (same MOUSER_SECRET and version on both?)");
            return;
        }
    };
    if tx.send(Ev::Up(id, link, addr)).is_err() {
        return;
    }
    let why = loop {
        match reader.recv() {
            Ok(Msg::Nop) => {}
            Ok(Msg::Bye) => break "peer said goodbye".to_string(),
            Ok(msg) => {
                if tx.send(Ev::Peer(id, msg)).is_err() {
                    return;
                }
            }
            Err(e) => break e.to_string(),
        }
    };
    let _ = tx.send(Ev::Down(id, why));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Args, String> {
        parse_args(s.split_whitespace().map(String::from))
    }

    #[test]
    fn arguments() {
        let a = parse("--connect eds-MacBook-Pro.local:47583 --edge left").unwrap();
        assert_eq!(a.connect.as_deref(), Some("eds-MacBook-Pro.local:47583"));
        assert_eq!(a.edge, Edge::Left);
        assert!(!a.allow_public);
        assert!(parse("--wait --edge RIGHT --allow-public").unwrap().allow_public);
        for bad in [
            "--wait",
            "--edge left",
            "--wait --connect a:1 --edge left",
            "--connect host --edge left",
            "--connect host:0 --edge left",
            "--wait --edge middle",
            "--wait --edge left --name pc",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
