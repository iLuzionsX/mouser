//! Persisted configuration.
//!
//! Kept deliberately plain: a small JSON document in the OS config
//! directory. The pairing secret is *not* stored here -- see
//! [`Config::secret_fingerprint`], which only persists a hash of it.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::layout::Edge;

/// Default port for the control/forwarding link.
pub const DEFAULT_PORT: u16 = 47583;

/// Which side of the link this machine initiates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Wait for the peer to connect.
    #[default]
    Host,
    /// Connect out to a known peer address.
    Client,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("no config directory available")]
    NoConfigDir,
    #[error("{0} is not a valid socket address")]
    InvalidAddress(String),
}

/// User configuration, serialized as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub role: Role,
    /// Address to bind when [`Role::Host`].
    pub bind_addr: String,
    /// Address to connect to when [`Role::Client`].
    pub peer_addr: Option<String>,
    /// Displayed in the UI on the other machine.
    pub device_name: String,
    /// Where the other machine's screen sits relative to this one.
    pub peer_edge: Edge,
    /// Skip the periodic `Ping`/`Pong` keepalive.
    pub disable_keepalive: bool,
    /// Restrict the peer to link-local and private address ranges.
    ///
    /// On by default: it keeps a leaked secret from being usable from the
    /// public internet. Turn it off only for a deliberate port forward.
    pub require_private_network: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            role: Role::Host,
            bind_addr: format!("0.0.0.0:{DEFAULT_PORT}"),
            peer_addr: None,
            device_name: hostname(),
            peer_edge: Edge::Right,
            disable_keepalive: false,
            require_private_network: true,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf, ConfigError> {
        let dirs = directories::ProjectDirs::from("dev", "mouser", "mouser")
            .ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("config.json"))
    }

    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::path()?;
        // Every read failure is reported the same way. `NotFound` used to be
        // special-cased, but `load_or_default` treats a missing file and an
        // unreadable one identically, so branching only added a way for the
        // two arms to disagree.
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
    }

    /// Load the config, falling back to defaults when none exists yet.
    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    /// Write the config, creating parent directories as needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
        }
        let json = serde_json::to_string_pretty(self).expect("config is always serializable");
        std::fs::write(&path, json).map_err(|source| ConfigError::Write { path, source })
    }

    pub fn bind_addr(&self) -> Result<SocketAddr, ConfigError> {
        self.bind_addr
            .parse()
            .map_err(|_| ConfigError::InvalidAddress(self.bind_addr.clone()))
    }

    pub fn peer_addr(&self) -> Result<SocketAddr, ConfigError> {
        let raw = self
            .peer_addr
            .as_deref()
            .ok_or_else(|| ConfigError::InvalidAddress("<unset>".into()))?;
        raw.parse()
            .map_err(|_| ConfigError::InvalidAddress(raw.to_string()))
    }

    /// Short, human-comparable hash of the pairing secret.
    ///
    /// Both machines display this after connecting so users can confirm they
    /// reached the intended peer. Derived with a domain separator so it can
    /// never collide with a use of the secret elsewhere.
    pub fn secret_fingerprint(secret: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"mouser/pairing-fingerprint/v1\x00");
        hasher.update(secret.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        hex.to_uppercase()
    }

    /// Reject obviously wrong secrets before a handshake is attempted.
    ///
    /// Length is checked rather than character class: a passphrase with
    /// predictable words is fine, a one-character secret is not.
    pub fn validate_secret(secret: &str) -> Result<(), ConfigError> {
        if secret.len() < 8 {
            return Err(ConfigError::InvalidAddress(
                "pairing secret must be at least 8 characters".into(),
            ));
        }
        Ok(())
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "mouser".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_host_on_default_port() {
        let cfg = Config::default();
        assert_eq!(cfg.role, Role::Host);
        assert_eq!(cfg.bind_addr().unwrap().port(), DEFAULT_PORT);
        assert_eq!(cfg.peer_edge, Edge::Right);
        assert!(cfg.require_private_network);
    }

    #[test]
    fn round_trips_through_json() {
        let cfg = Config {
            device_name: "laptop".into(),
            peer_addr: Some("192.168.1.50:47583".into()),
            role: Role::Client,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.peer_addr().unwrap().port(), DEFAULT_PORT);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = r#"{"rol": "host"}"#;
        assert!(serde_json::from_str::<Config>(json).is_err());
    }

    #[test]
    fn fingerprint_is_stable_and_secret_dependent() {
        let a = Config::secret_fingerprint("correct horse battery");
        let b = Config::secret_fingerprint("correct horse battery");
        let c = Config::secret_fingerprint("correct horse battey");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn short_secrets_rejected() {
        assert!(Config::validate_secret("abc").is_err());
        assert!(Config::validate_secret("abcdefgh").is_ok());
    }

    #[test]
    fn invalid_address_reports_the_offending_value() {
        let cfg = Config {
            bind_addr: "not-an-address".into(),
            ..Default::default()
        };
        let err = cfg.bind_addr().unwrap_err().to_string();
        assert!(err.contains("not-an-address"), "got {err}");
    }
}
