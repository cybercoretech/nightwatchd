//! Device tokens: one per phone, sent as `Authorization: Bearer
//! nw_<id>_<secret>`.
//!
//! Only a SHA-256 of each secret is stored (the secret is 256 random bits,
//! so a slow hash adds nothing), in `~/.config/nightwatch/tokens.json`, so
//! `nightwatchd token add` and `revoke` work while the gateway runs. A
//! token is read-only unless it was made with `--operate`. Same scheme as
//! Holodrive's device tokens.

use crate::jsonfile::JsonFile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// How often `last_used` reaches the disk (it's kept in memory meanwhile).
const SAVE_USE_EVERY: u64 = 60;

#[derive(Clone, Serialize, Deserialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    hash: String,
    #[serde(default)]
    pub operate: bool,
    pub created: u64,
    #[serde(default)]
    pub last_used: Option<u64>,
}

/// Who a valid bearer token belongs to.
#[derive(Clone)]
pub struct Grant {
    pub id: String,
    pub name: String,
    pub operate: bool,
}

impl Grant {
    /// What the services record as the actor.
    pub fn actor(&self) -> String {
        format!("nightwatch:{}", self.name)
    }
}

pub struct Store {
    file: JsonFile<Token>,
    saved_use: AtomicU64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn default_path() -> PathBuf {
    crate::config::dir().join("tokens.json")
}

impl Store {
    pub fn open(path: PathBuf) -> Store {
        Store {
            file: JsonFile::open(path),
            saved_use: AtomicU64::new(0),
        }
    }

    /// No file (tests).
    #[cfg(test)]
    pub fn memory() -> Store {
        Store {
            file: JsonFile::memory(),
            saved_use: AtomicU64::new(0),
        }
    }

    pub fn list(&self) -> Vec<Token> {
        self.file.lock().list.clone()
    }

    /// A new token; the returned secret is shown once and never stored.
    pub fn create(&self, name: &str, operate: bool) -> anyhow::Result<(Token, String)> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty()
                && name.chars().count() <= 40
                && name.chars().all(|c| c.is_ascii_graphic() || c == ' '),
            "a device name is 1–40 plain ASCII characters"
        );
        let mut inner = self.file.lock();
        let id = loop {
            let id = random_hex(4);
            if !inner.list.iter().any(|t| t.id == id) {
                break id;
            }
        };
        let secret = format!("nw_{id}_{}", random_hex(32));
        let token = Token {
            id,
            name: name.to_string(),
            hash: digest(&secret),
            operate,
            created: now(),
            last_used: None,
        };
        inner.list.push(token.clone());
        self.file.save(&mut inner)?;
        Ok((token, secret))
    }

    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut inner = self.file.lock();
        let before = inner.list.len();
        inner.list.retain(|t| t.id != id);
        if inner.list.len() == before {
            return Ok(false);
        }
        self.file.save(&mut inner)?;
        Ok(true)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.file.lock().list.iter().any(|t| t.id == id)
    }

    /// Check a bearer secret and note when it was used.
    pub fn verify(&self, secret: &str) -> Option<Grant> {
        let id = secret.strip_prefix("nw_")?.split('_').next()?;
        let mut inner = self.file.lock();
        let hash = digest(secret);
        let t = inner.list.iter_mut().find(|t| t.id == id)?;
        if !constant_time_eq(t.hash.as_bytes(), hash.as_bytes()) {
            return None;
        }
        let at = now();
        t.last_used = Some(at);
        let grant = Grant {
            id: t.id.clone(),
            name: t.name.clone(),
            operate: t.operate,
        };
        if at.saturating_sub(self.saved_use.load(Ordering::Relaxed)) >= SAVE_USE_EVERY {
            self.saved_use.store(at, Ordering::Relaxed);
            let _ = self.file.save(&mut inner);
        }
        Some(grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_verify_revoke() {
        let s = Store::memory();
        let (t, secret) = s.create("SGS25U", false).unwrap();
        assert!(secret.starts_with(&format!("nw_{}_", t.id)));
        let g = s.verify(&secret).unwrap();
        assert_eq!((g.name.as_str(), g.operate), ("SGS25U", false));
        assert_eq!(g.actor(), "nightwatch:SGS25U");
        let forged = format!("nw_{}_{}", t.id, "0".repeat(64));
        assert!(s.verify(&forged).is_none());
        assert!(s.verify("hd_whatever").is_none());
        assert!(s.revoke(&t.id).unwrap());
        assert!(s.verify(&secret).is_none());
        assert!(s.create("", true).is_err());
        assert!(s.create("bad\nname", true).is_err());
    }

    #[test]
    fn file_is_private_and_holds_no_secret() {
        let dir = std::env::temp_dir().join(format!("nightwatch-tokens-{}", random_hex(4)));
        let path = dir.join("tokens.json");
        let server = Store::open(path.clone());
        let cli = Store::open(path.clone());
        let (_, secret) = cli.create("phone", true).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&secret[12..]), "secret must not be stored");
        assert!(server.verify(&secret).unwrap().operate);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
