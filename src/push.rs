//! UnifiedPush delivery. A phone registers the endpoint its distributor
//! (the ntfy app, pointed at your own ntfy server) gave it; each new alert
//! of `warn` or worse is POSTed there. The message carries only the alert's
//! id, severity and title: the app fetches the rest over the tailnet.
//!
//! Endpoints must be on a host listed in `[push] allowed_hosts`, since
//! nightwatchd will POST to whatever is registered.

use crate::alerts::{Alert, Severity, Store as Alerts};
use crate::jsonfile::JsonFile;
use crate::tokens::Store as Tokens;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub struct Registration {
    /// The device token this endpoint belongs to; revoking the token
    /// stops its pushes.
    pub token_id: String,
    pub endpoint: String,
    pub registered: u64,
}

pub struct Registry {
    file: JsonFile<Registration>,
    allowed_hosts: Vec<String>,
}

pub fn default_path() -> std::path::PathBuf {
    crate::config::dir().join("push.json")
}

impl Registry {
    pub fn open(path: std::path::PathBuf, allowed_hosts: Vec<String>) -> Registry {
        Registry {
            file: JsonFile::open(path),
            allowed_hosts,
        }
    }

    #[cfg(test)]
    pub fn memory(allowed_hosts: Vec<String>) -> Registry {
        Registry {
            file: JsonFile::memory(),
            allowed_hosts,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.allowed_hosts.is_empty()
    }

    pub fn check(&self, endpoint: &str) -> Result<(), String> {
        if !self.enabled() {
            return Err(
                "push is off on this host: set [push] allowed_hosts in nightwatch's config.toml"
                    .into(),
            );
        }
        let url = reqwest::Url::parse(endpoint).map_err(|e| format!("not a URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("endpoint must be http or https".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("endpoint must not carry credentials".into());
        }
        let host = url.host_str().unwrap_or_default();
        if !self
            .allowed_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
        {
            return Err(format!("{host} is not in [push] allowed_hosts"));
        }
        Ok(())
    }

    /// One endpoint per device token; registering again replaces it.
    pub fn register(&self, token_id: &str, endpoint: &str) -> Result<(), String> {
        self.check(endpoint)?;
        let mut inner = self.file.lock();
        inner.list.retain(|r| r.token_id != token_id);
        inner.list.push(Registration {
            token_id: token_id.into(),
            endpoint: endpoint.into(),
            registered: crate::tokens::now(),
        });
        self.file.save(&mut inner).map_err(|e| e.to_string())
    }

    pub fn unregister(&self, token_id: &str) -> bool {
        let mut inner = self.file.lock();
        let before = inner.list.len();
        inner.list.retain(|r| r.token_id != token_id);
        let changed = inner.list.len() != before;
        if changed {
            let _ = self.file.save(&mut inner);
        }
        changed
    }

    pub fn get(&self, token_id: &str) -> Option<Registration> {
        self.file
            .lock()
            .list
            .iter()
            .find(|r| r.token_id == token_id)
            .cloned()
    }

    /// Endpoints of tokens that still exist and are still allowed.
    fn targets(&self, tokens: &Tokens) -> Vec<String> {
        self.file
            .lock()
            .list
            .iter()
            .filter(|r| tokens.exists(&r.token_id) && self.check(&r.endpoint).is_ok())
            .map(|r| r.endpoint.clone())
            .collect()
    }
}

pub fn message(alert: &Alert, host: &str) -> serde_json::Value {
    json!({
        "type": "alert",
        "host": host,
        "id": alert.id,
        "severity": alert.severity,
        "title": alert.title,
    })
}

/// Sends `{"type": "test", "host"}` to one phone's endpoint, so the user can
/// check the whole path (gateway → ntfy → phone) without waiting for an
/// alert. The app shows its own fixed text for it, never the payload.
pub async fn send_test(registry: &Registry, token_id: &str, host: &str) -> Result<(), String> {
    let reg = registry
        .get(token_id)
        .ok_or("this phone has no push endpoint registered yet")?;
    registry.check(&reg.endpoint)?;
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .post(&reg.endpoint)
        .header("Priority", "3")
        .body(json!({ "type": "test", "host": host }).to_string())
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map(|_| ())
        .map_err(|e| format!("the push server refused it: {e}"))
}

/// Forward new alerts to every registered phone.
pub async fn run(
    alerts: Arc<Alerts>,
    registry: Arc<Registry>,
    tokens: Arc<Tokens>,
    host: String,
    quiet: Option<warnflare::quiet::QuietHours>,
) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client");
    let mut rx = alerts.subscribe();
    // Alerts held during quiet hours. They're already in the store (the
    // Alerts tab shows them); only the push waits for the morning digest.
    let mut held: Vec<warnflare::Event> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            r = rx.recv() => {
                let alert = match r {
                    Ok(a) => a,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                if alert.severity < Severity::Warn {
                    continue;
                }
                if let Some(q) = &quiet {
                    let ev = as_event(&alert);
                    if q.delivery(&ev, local_minute()) == warnflare::quiet::Delivery::Hold {
                        held.push(ev);
                        continue;
                    }
                }
                deliver(&http, &registry, &tokens, &alert, &host).await;
            }
            _ = tick.tick() => {
                if let Some(q) = &quiet
                    && !held.is_empty()
                    && !q.active(local_minute())
                {
                    // Raised as an alert so it's stored too; the store
                    // broadcasts it back here and it pushes once.
                    if let Some(d) = warnflare::quiet::digest(&held) {
                        crate::alerts::raise_event(&alerts, d);
                    }
                    held.clear();
                }
            }
        }
    }
}

async fn deliver(
    http: &reqwest::Client,
    registry: &Registry,
    tokens: &Tokens,
    alert: &Alert,
    host: &str,
) {
    let body = message(alert, host).to_string();
    for endpoint in registry.targets(tokens) {
        let r = http
            .post(&endpoint)
            // ntfy honours these when it relays to the phone.
            .header(
                "Priority",
                if alert.severity == Severity::Crit {
                    "5"
                } else {
                    "4"
                },
            )
            .body(body.clone())
            .send()
            .await;
        if let Err(e) = r.and_then(|r| r.error_for_status()) {
            eprintln!("[nightwatchd] push to {endpoint} failed: {e}");
        }
    }
}

fn as_event(a: &Alert) -> warnflare::Event {
    let severity = match a.severity {
        Severity::Info => warnflare::Severity::Info,
        Severity::Warn => warnflare::Severity::Warn,
        Severity::Crit => warnflare::Severity::Crit,
    };
    warnflare::Event::new(
        &a.source,
        severity,
        a.key.clone(),
        a.title.clone(),
        a.body.clone(),
        a.subject.clone(),
    )
}

/// Minutes after local midnight, from the system time zone (`/etc/localtime`).
fn local_minute() -> u16 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: localtime_r only writes into the tm we pass it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&now, &mut tm).is_null() };
    if ok {
        (tm.tm_hour * 60 + tm.tm_min) as u16
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_on_allowed_hosts() {
        let r = Registry::memory(vec!["darkbox".into()]);
        assert!(r.check("http://darkbox:2586/upAbc123").is_ok());
        assert!(r.check("https://DARKBOX/upAbc").is_ok());
        assert!(r.check("http://127.0.0.1:8767/api/daemon-reload").is_err());
        assert!(r.check("http://darkbox.evil.example/up").is_err());
        assert!(r.check("http://user:pw@darkbox/up").is_err());
        assert!(r.check("file:///etc/passwd").is_err());
        assert!(Registry::memory(vec![]).check("http://darkbox/up").is_err());
    }

    #[test]
    fn one_endpoint_per_token_and_revoked_tokens_get_nothing() {
        let r = Registry::memory(vec!["darkbox".into()]);
        let tokens = Tokens::memory();
        let (t, _) = tokens.create("phone", false).unwrap();
        r.register(&t.id, "http://darkbox/up1").unwrap();
        r.register(&t.id, "http://darkbox/up2").unwrap();
        r.register("gone", "http://darkbox/up3").unwrap();
        assert_eq!(r.targets(&tokens), vec!["http://darkbox/up2"]);
        tokens.revoke(&t.id).unwrap();
        assert!(r.targets(&tokens).is_empty());
        assert!(r.unregister("gone"));
    }
}
