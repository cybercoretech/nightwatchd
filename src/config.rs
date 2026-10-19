//! `~/.config/nightwatch/config.toml`. Every key is optional: the defaults
//! match a stock Cybercore host (DaemonHall :8767, Dockspace :7070,
//! Cyberdeck-hub :8080, Argus and VortexWall on this machine).

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8769";

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Loopback only; `tailscale serve` publishes it on the tailnet.
    pub listen: String,
    /// Shown in the app's host switcher. Defaults to /etc/hostname.
    pub host_label: String,
    pub daemonhall: Upstream,
    pub dockspace: Upstream,
    pub cyberdeck: Upstream,
    pub policy: Policy,
    pub alerts: Alerts,
    pub push: Push,
    pub mission: Mission,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub url: String,
    /// The service's local-token/v1 file.
    pub token_file: PathBuf,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Units the phone may start but never stop or restart, on top of the
    /// built-in list (tailscaled, sshd, NetworkManager, DaemonHall, ...).
    pub protected_units: Vec<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Alerts {
    pub enabled: bool,
    /// Seconds between DaemonHall / Dockspace checks.
    pub poll_secs: u64,
    /// Argus's event log; empty string turns the watcher off.
    pub argus_events: String,
    /// VortexWall's enforcing unit, read from the journal; empty = off.
    pub vortexwall_unit: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Push {
    /// Hosts a phone may register as its UnifiedPush endpoint (your ntfy
    /// server). Empty means push is off: anyone holding a device token
    /// could otherwise make nightwatchd POST to any URL.
    pub allowed_hosts: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Mission {
    /// e.g. "https://mission.example.com"; empty = Mission Control off.
    pub url: String,
    /// Mission Control project to show.
    pub project: String,
    /// Files holding the admin token and the Cloudflare Access service
    /// token. They stay on this host; the phone never sees them.
    pub admin_token_file: PathBuf,
    pub access_client_id_file: PathBuf,
    pub access_client_secret_file: PathBuf,
}

fn yes() -> bool {
    true
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            use std::os::unix::fs::MetadataExt;
            let uid = dirs::home_dir()
                .and_then(|h| std::fs::metadata(h).ok())
                .map(|m| m.uid())
                .unwrap_or(1000);
            PathBuf::from(format!("/run/user/{uid}"))
        })
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.into(),
            host_label: std::fs::read_to_string("/etc/hostname")
                .unwrap_or_default()
                .trim()
                .to_string(),
            daemonhall: Upstream {
                enabled: true,
                url: "http://127.0.0.1:8767".into(),
                token_file: runtime_dir().join("daemonhall/local-token"),
            },
            dockspace: Upstream {
                enabled: true,
                url: "http://127.0.0.1:7070".into(),
                token_file: "/run/dockspace/local-token".into(),
            },
            cyberdeck: Upstream {
                enabled: true,
                url: "http://127.0.0.1:8080".into(),
                token_file: "/run/cyberdeck/local-token".into(),
            },
            policy: Policy::default(),
            alerts: Alerts::default(),
            push: Push::default(),
            mission: Mission::default(),
        }
    }
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts {
            enabled: true,
            poll_secs: 15,
            argus_events: home()
                .join(".local/state/argus/events.jsonl")
                .display()
                .to_string(),
            vortexwall_unit: "vortexwall-enforce.service".into(),
        }
    }
}

pub fn dir() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("nightwatch")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| home().join(".local/state"))
        .join("nightwatch")
}

pub fn default_path() -> PathBuf {
    dir().join("config.toml")
}

impl Config {
    /// A missing file is the defaults; a broken one is an error, so a typo
    /// can't silently turn the policy or push allowlist off.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let c: Config = toml::from_str(
            "listen = \"127.0.0.1:9000\"\n[push]\nallowed_hosts = [\"ntfy.example\"]\n",
        )
        .unwrap();
        assert_eq!(c.listen, "127.0.0.1:9000");
        assert_eq!(c.dockspace.url, "http://127.0.0.1:7070");
        assert_eq!(c.push.allowed_hosts, vec!["ntfy.example"]);
        assert!(c.alerts.enabled);
    }

    #[test]
    fn typos_are_errors() {
        assert!(toml::from_str::<Config>("lisen = \"x\"").is_err());
        assert!(toml::from_str::<Config>("[push]\nallowed_host = []").is_err());
    }
}
