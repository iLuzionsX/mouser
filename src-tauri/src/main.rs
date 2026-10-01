// Keep the console window from appearing behind the app on Windows release
// builds; debug builds keep it so panics are readable.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Command line entry point.
//!
//! Everything the CLI does is a one-off override of the saved config, so
//! `mouser --edge left` behaves like editing the file and then launching.

use clap::Parser;
use mouser_core::config::{Config, Role};
use mouser_core::layout::Edge;
use tracing_subscriber::EnvFilter;

use mouser_lib::{run, state::Level};

/// Use one mouse and keyboard across two computers.
///
/// Each machine runs mouser with the same pairing secret. Tell each one which
/// side the other screen is on, then push the cursor off that edge.
#[derive(Debug, Parser)]
#[command(name = "mouser", version, about, long_about = None)]
struct Cli {
    /// Act as the client and connect here, e.g. 192.168.1.20:47583.
    #[arg(long, value_name = "ADDR")]
    connect: Option<String>,

    /// Wait for the other machine to connect. This is the default.
    #[arg(long, conflicts_with = "connect")]
    wait: bool,

    /// Which side the other machine's screen is on, from this machine's view.
    #[arg(long, value_enum)]
    edge: Option<EdgeArg>,

    /// Name shown on the other machine.
    #[arg(long)]
    name: Option<String>,

    /// Pairing secret.
    ///
    /// Prefer MOUSER_SECRET or the field in the window: an argument is visible
    /// to other processes and lands in shell history.
    #[arg(long)]
    secret: Option<String>,

    /// Accept connections from public addresses.
    ///
    /// Off by default. Without it the link refuses non-private addresses,
    /// which stops a stray port forward becoming remote control.
    #[arg(long)]
    allow_public: bool,

    /// Log to stderr as well as the window.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum EdgeArg {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<EdgeArg> for Edge {
    fn from(edge: EdgeArg) -> Self {
        match edge {
            EdgeArg::Left => Edge::Left,
            EdgeArg::Right => Edge::Right,
            EdgeArg::Top => Edge::Top,
            EdgeArg::Bottom => Edge::Bottom,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose {
            "info,mouser=debug"
        } else {
            "warn,mouser=info"
        }))
        .with_writer(std::io::stderr)
        .init();

    let mut config = Config::load_or_default();
    if let Some(addr) = cli.connect.clone() {
        config.role = Role::Client;
        config.peer_addr = Some(addr);
    } else if cli.wait {
        config.role = Role::Host;
        config.peer_addr = None;
    }
    if let Some(edge) = cli.edge {
        config.peer_edge = edge.into();
    }
    if let Some(name) = cli.name.clone() {
        config.device_name = name;
    }
    if cli.allow_public {
        config.require_private_network = false;
    }

    // A bad value here would otherwise fail later, with no explanation.
    if let Err(e) = config.bind_addr() {
        anyhow::bail!("bind address is invalid: {e}");
    }
    if config.role == Role::Client {
        config
            .peer_addr()
            .map_err(|e| anyhow::anyhow!("peer address is invalid: {e}"))?;
    }

    let _ = config.save();

    // Precedence: flag, then environment, then the window's field.
    let secret = cli
        .secret
        .clone()
        .or_else(|| std::env::var("MOUSER_SECRET").ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if !secret.is_empty() {
        Config::validate_secret(&secret)?;
        if cli.secret.is_some() {
            tracing::warn!(
                "--secret is visible to other processes and stays in shell history; \
                 use MOUSER_SECRET instead"
            );
        }
    } else {
        tracing::warn!("no pairing secret set: the link will refuse peers until one is entered");
    }

    let _ = Level::Info;
    run(config, secret)
}
