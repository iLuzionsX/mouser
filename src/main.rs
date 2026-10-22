// Release builds run without a console window; launch with stderr redirected.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! One mouse and keyboard across two computers: push the cursor off the
//! shared edge and it, plus the keyboard, drives the other machine.

/// A line on stderr and in the log file, stamped with the UTC time of day.
macro_rules! log {
    ($($arg:tt)*) => { $crate::log_line(format!($($arg)*)) };
}

mod autostart;
mod config;
mod kvm;
#[cfg(target_os = "macos")]
mod mac;
mod tray;
#[cfg(windows)]
mod win;
mod wire;

#[cfg(target_os = "macos")]
use mac as platform;
#[cfg(windows)]
use win as platform;

use std::fs::File;
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use config::Config;
use kvm::{CLAIMS, Captured, Edge, Kvm};
use wire::{Link, Msg};

const LISTEN: &str = "0.0.0.0:47583";
/// Held for the process lifetime so a second copy (say, login plus a manual
/// launch) exits instead of fighting over the input hooks.
const LOCK: &str = "127.0.0.1:47584";
const RETRY: Duration = Duration::from_secs(2);
/// Each side sends at least this often, and gives up on a peer silent for
/// three times as long.
const KEEPALIVE: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(6);

const USAGE: &str =
    "usage: mouser [--wait | --connect HOST:PORT] [--edge left|right|top|bottom] [--allow-public]
  Flags are saved to ~/.config/mouser/config, so later runs need none.
  The pairing secret (8+ characters, same on both machines) comes from
  MOUSER_SECRET or ~/.config/mouser/secret.";

/// Off from the tray: no dialing, no listening.
static ENABLED: AtomicBool = AtomicBool::new(true);
static LOG: OnceLock<Mutex<File>> = OnceLock::new();

pub fn log_line(line: String) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = format!("{:02}:{:02}:{:02}Z {line}", t / 3600 % 24, t / 60 % 60, t % 60);
    eprintln!("{line}");
    if let Some(Ok(mut f)) = LOG.get().map(Mutex::lock) {
        let _ = writeln!(f, "{line}");
    }
}

/// Everything the state machine reacts to, from every thread.
pub enum Ev {
    Local(Captured),
    Up(u64, Link, SocketAddr),
    Peer(u64, Msg),
    Down(u64, String),
    Enable(bool),
    Edge(Edge),
    Quit,
}

/// The saved settings with the flags applied over them.
fn parse_args(mut args: impl Iterator<Item = String>, mut c: Config) -> Result<Config, String> {
    let (mut connect, mut wait) = (None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wait" => wait = true,
            "--allow-public" => c.allow_public = true,
            "--connect" => connect = Some(args.next().ok_or("--connect needs HOST:PORT")?),
            "--edge" => {
                let e = args.next().unwrap_or_default();
                c.edge = Some(Edge::parse(&e).ok_or(format!("unknown edge {e:?}"))?);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    match (wait, connect) {
        (true, Some(_)) => return Err("pass only one of --wait and --connect".into()),
        (true, None) => c.connect = None,
        (false, Some(addr)) => c.connect = Some(addr),
        (false, None) => {}
    }
    if let Some(addr) = &c.connect {
        let port = addr.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
        if !matches!(port, Some(1..)) {
            return Err(format!("{addr:?} is not HOST:PORT"));
        }
    }
    if c.edge.is_none() {
        return Err("--edge is required on the first run".into());
    }
    Ok(c)
}

fn main() {
    let fail = |e: String| -> ! {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    };
    let saved = config::load().unwrap_or_else(|e| fail(e));
    let first_run = saved.is_none();
    let cfg =
        parse_args(std::env::args().skip(1), saved.unwrap_or_default()).unwrap_or_else(|e| fail(e));
    let secret = config::secret();
    if secret.len() < 8 {
        fail("no pairing secret: set MOUSER_SECRET or write ~/.config/mouser/secret".into());
    }
    let Ok(_lock) = TcpListener::bind(LOCK) else {
        eprintln!("mouser is already running");
        return;
    };
    if let Ok(f) = File::create(config::dir().join("mouser.log")) {
        let _ = LOG.set(Mutex::new(f));
    }
    config::save(&cfg);
    if first_run && let Err(e) = autostart::set(true) {
        log!("could not set up start at login: {e}");
    }
    #[cfg(windows)]
    match autostart::upgrade() {
        Ok(true) => log!("start at login moved to a scheduled task"),
        Ok(false) => {}
        Err(e) => log!("could not move start at login to a scheduled task: {e}"),
    }
    ENABLED.store(cfg.enabled, Relaxed);

    let (tx, rx) = channel();
    let worker_tx = tx.clone();
    tray::run(&cfg.clone(), tx, move |status| {
        thread::spawn(move || work(cfg, &secret, worker_tx, rx, status));
    });
}

/// Capture, the network thread, and the state machine, until Quit.
fn work(mut cfg: Config, secret: &str, tx: Sender<Ev>, rx: Receiver<Ev>, status: tray::Status) {
    let (psk, tag) = (wire::psk(secret), wire::fingerprint(secret));
    let io = match platform::start(tx.clone()) {
        Ok(io) => io,
        Err(e) => {
            log!("could not capture input: {e}");
            status.set("Needs Accessibility and Input Monitoring permission", false);
            // Keep the tray up so Quit still works.
            while !matches!(rx.recv(), Ok(Ev::Quit) | Err(_)) {}
            std::process::exit(1);
        }
    };
    let edge = cfg.edge.unwrap_or(Edge::Top);
    log!("input capture running; peer is to the {edge:?}");

    let host = cfg.connect.is_none();
    let idle = match &cfg.connect {
        Some(addr) => format!("Looking for {}", addr.rsplit_once(':').map_or(&**addr, |a| a.0)),
        None => "Waiting for the other computer".to_string(),
    };
    let show_idle = |status: &tray::Status| {
        status.set(if ENABLED.load(Relaxed) { &*idle } else { "Connection off" }, false);
    };
    show_idle(&status);
    let (connect, public) = (cfg.connect.clone(), cfg.allow_public);
    thread::spawn(move || match connect {
        Some(addr) => dial(&addr, &psk, public, &tx),
        None => listen(&psk, public, &tx),
    });

    let mut kvm = Kvm::new(io, &CLAIMS, edge, host);
    let mut link: Option<(u64, Link)> = None;
    let mut last_sent = Instant::now();
    let hang_up = |link: &mut Option<(u64, Link)>| {
        if let Some((_, mut old)) = link.take() {
            let _ = old.send(Msg::Bye);
            old.close();
        }
    };
    loop {
        let out = match rx.recv_timeout(KEEPALIVE) {
            Ok(Ev::Local(ev)) => kvm.local(ev),
            Ok(Ev::Peer(id, msg)) if link.as_ref().is_some_and(|l| l.0 == id) => kvm.peer(msg),
            Ok(Ev::Up(id, new, addr)) => {
                // The newest pairing wins: the old peer is gone or restarting.
                hang_up(&mut link);
                link = Some((id, new));
                kvm.link(true);
                log!("paired with {addr} [{tag}]");
                status.set(format!("Connected to {}", addr.ip()), true);
                None
            }
            Ok(Ev::Down(id, why)) if link.as_ref().is_some_and(|l| l.0 == id) => {
                link = None;
                kvm.link(false);
                log!("link down: {why}");
                show_idle(&status);
                None
            }
            Ok(Ev::Enable(on)) => {
                ENABLED.store(on, Relaxed);
                cfg.enabled = on;
                config::save(&cfg);
                log!("connection turned {}", if on { "on" } else { "off" });
                if !on {
                    hang_up(&mut link);
                    kvm.link(false);
                }
                show_idle(&status);
                None
            }
            Ok(Ev::Edge(e)) => {
                cfg.edge = Some(e);
                config::save(&cfg);
                log!("peer is now to the {e:?}");
                kvm.set_edge(e)
            }
            Ok(Ev::Quit) => {
                hang_up(&mut link);
                kvm.link(false); // shows the cursor, releases held keys
                log!("quit from the menu");
                std::process::exit(0);
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
            show_idle(&status);
        }
    }
}

/// Client: dial the host, re-resolving its name on every attempt so DHCP
/// moving it does not break pairing.
fn dial(addr: &str, psk: &[u8; 32], allow_public: bool, tx: &Sender<Ev>) {
    let mut last_error = String::new();
    for id in 0..=u64::MAX {
        if !ENABLED.load(Relaxed) {
            last_error.clear();
            thread::sleep(RETRY);
            continue;
        }
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
/// The port is closed while the connection is off, so the peer sees a
/// plain refusal rather than failed handshakes.
fn listen(psk: &[u8; 32], allow_public: bool, tx: &Sender<Ev>) {
    let mut id = 0u64;
    loop {
        if !ENABLED.load(Relaxed) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let listener = match TcpListener::bind(LISTEN).and_then(|l| {
            l.set_nonblocking(true)?;
            Ok(l)
        }) {
            Ok(l) => l,
            Err(e) => {
                log!("could not listen on {LISTEN}: {e}; retrying");
                thread::sleep(RETRY);
                continue;
            }
        };
        log!("listening on {LISTEN}");
        while ENABLED.load(Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    id += 1;
                    let (psk, tx) = (*psk, tx.clone());
                    let _ = s.set_nonblocking(false);
                    thread::spawn(move || session(s, false, &psk, allow_public, id, &tx));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    log!("accept failed: {e}");
                    thread::sleep(RETRY);
                }
            }
        }
        log!("stopped listening: connection off");
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
    if !ENABLED.load(Relaxed) || tx.send(Ev::Up(id, link, addr)).is_err() {
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

    fn parse(s: &str) -> Result<Config, String> {
        parse_args(s.split_whitespace().map(String::from), Config::default())
    }

    #[test]
    fn arguments() {
        let a = parse("--connect eds-MacBook-Pro.local:47583 --edge left").unwrap();
        assert_eq!(a.connect.as_deref(), Some("eds-MacBook-Pro.local:47583"));
        assert_eq!(a.edge, Some(Edge::Left));
        assert!(!a.allow_public);
        assert!(parse("--wait --edge RIGHT --allow-public").unwrap().allow_public);
        // Flags override the saved settings; no flags keep them.
        let saved = Config { edge: Some(Edge::Top), ..parse("--connect h:1 --edge left").unwrap() };
        let none = std::iter::empty();
        assert_eq!(parse_args(none, saved.clone()), Ok(saved.clone()));
        let waited = parse_args(["--wait".to_string()].into_iter(), saved).unwrap();
        assert_eq!((waited.connect, waited.edge), (None, Some(Edge::Top)));
        for bad in [
            "--wait",
            "",
            "--wait --connect a:1 --edge left",
            "--connect host --edge left",
            "--connect host:0 --edge left",
            "--wait --edge middle",
            "--wait --edge left --name pc",
            "--edge left --secret x",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
