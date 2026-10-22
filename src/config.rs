//! Settings in `~/.config/mouser/` (`%USERPROFILE%\.config\mouser\` on
//! Windows): `config` holds `key = value` lines, `secret` the pairing
//! secret, `mouser.log` this run's log. Flags override the file for a run,
//! and whatever a run ends up with is written back, so one run with flags
//! sets up the no-argument launch that starts at login.

use std::path::PathBuf;

use crate::kvm::Edge;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// `HOST:PORT` to dial; `None` waits for the peer instead.
    pub connect: Option<String>,
    /// Where the other screen sits.
    pub edge: Option<Edge>,
    /// Off: no dialing, no listening, no link.
    pub enabled: bool,
    pub allow_public: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { connect: None, edge: None, enabled: true, allow_public: false }
    }
}

pub fn dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    PathBuf::from(home.unwrap_or_default()).join(".config").join("mouser")
}

fn path() -> PathBuf {
    dir().join("config")
}

/// The file's settings, or `None` when there is no file yet.
pub fn load() -> Result<Option<Config>, String> {
    match std::fs::read_to_string(path()) {
        Ok(text) => parse(&text).map(Some).map_err(|e| format!("{}: {e}", path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path().display())),
    }
}

pub fn save(c: &Config) {
    let write = std::fs::create_dir_all(dir()).and_then(|()| std::fs::write(path(), render(c)));
    if let Err(e) = write {
        log!("could not save {}: {e}", path().display());
    }
}

/// `MOUSER_SECRET`, else the `secret` file.
pub fn secret() -> String {
    std::env::var("MOUSER_SECRET")
        .or_else(|_| std::fs::read_to_string(dir().join("secret")))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn parse(text: &str) -> Result<Config, String> {
    let mut c = Config::default();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(format!("bad line {line:?}"))?;
        let value = value.trim();
        let flag = || match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!("{value:?} is not true or false")),
        };
        match key.trim() {
            "connect" => c.connect = Some(value.to_string()),
            "edge" => c.edge = Some(Edge::parse(value).ok_or(format!("unknown edge {value:?}"))?),
            "enabled" => c.enabled = flag()?,
            "allow_public" => c.allow_public = flag()?,
            other => return Err(format!("unknown setting {other:?}")),
        }
    }
    Ok(c)
}

fn render(c: &Config) -> String {
    let mut out = String::from("# Written by mouser; the tray menu changes it.\n");
    if let Some(addr) = &c.connect {
        out += &format!("connect = {addr}\n");
    }
    if let Some(edge) = c.edge {
        out += &format!("edge = {}\n", format!("{edge:?}").to_lowercase());
    }
    out += &format!("enabled = {}\nallow_public = {}\n", c.enabled, c.allow_public);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_typos() {
        let c = Config {
            connect: Some("eds-MacBook-Pro.local:47583".into()),
            edge: Some(Edge::Bottom),
            enabled: false,
            allow_public: false,
        };
        assert_eq!(parse(&render(&c)), Ok(c));
        assert_eq!(parse("# nothing\n\n"), Ok(Config::default()));
        assert!(parse("edge = middle").is_err());
        assert!(parse("enabled = yes").is_err());
        assert!(parse("name = pc").is_err());
    }
}
