//! Alerts: what's worth a notification, collected from the services this
//! host runs, kept in `~/.local/state/nightwatch/alerts.jsonl` and pushed
//! to registered phones (see `push`).
//!
//! Sources:
//! - DaemonHall: a watched unit entering `failed`, or systemd restarting it
//!   on its own; DaemonHall itself going quiet.
//! - Dockspace: a stack that was running dropping to partial or stopped.
//! - Argus: new, unaccepted file-integrity events.
//! - VortexWall: bans, read from its unit's journal.
//!
//! The first poll of each source is a silent baseline, so restarting
//! nightwatchd doesn't re-announce old trouble. Repeats of the same alert
//! within ten minutes fold into one (its `count` goes up, no new push).

use crate::upstream::Upstream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

const KEEP: usize = 500;
const FOLD_SECS: u64 = 600;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Crit,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: u64,
    /// Seconds since the epoch (last occurrence).
    pub ts: u64,
    pub source: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    /// The unit, stack or path it is about, for linking in the app.
    pub subject: String,
    #[serde(default)]
    pub key: String,
    #[serde(default = "one")]
    pub count: u32,
    #[serde(default)]
    pub acked: bool,
}

fn one() -> u32 {
    1
}

pub struct Store {
    path: Option<PathBuf>,
    inner: Mutex<VecDeque<Alert>>,
    tx: broadcast::Sender<Alert>,
}

pub struct NewAlert<'a> {
    pub source: &'a str,
    pub severity: Severity,
    /// Alerts with the same key within ten minutes fold together.
    pub key: String,
    pub title: String,
    pub body: String,
    pub subject: String,
}

impl Store {
    pub fn open(path: PathBuf) -> Store {
        let list: VecDeque<Alert> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Store {
            path: Some(path),
            inner: Mutex::new(list),
            tx: broadcast::channel(64).0,
        }
    }

    #[cfg(test)]
    pub fn memory() -> Store {
        Store {
            path: None,
            inner: Mutex::new(VecDeque::new()),
            tx: broadcast::channel(64).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Alert> {
        self.tx.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Alert>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, list: &VecDeque<Alert>) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("jsonl.tmp");
        let write = || -> std::io::Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            for a in list {
                writeln!(f, "{}", serde_json::to_string(a).unwrap_or_default())?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            eprintln!("[nightwatchd] can't save alerts: {e}");
        }
    }

    /// Record an alert. Returns it when it's new (worth a push), `None`
    /// when it folded into a recent one.
    pub fn raise(&self, n: NewAlert) -> Option<Alert> {
        let now = crate::tokens::now();
        let mut list = self.lock();
        if let Some(a) = list
            .iter_mut()
            .rev()
            .find(|a| a.key == n.key && !a.acked && now.saturating_sub(a.ts) < FOLD_SECS)
        {
            a.count += 1;
            a.ts = now;
            a.body = n.body;
            let snapshot = list.clone();
            drop(list);
            self.save(&snapshot);
            return None;
        }
        let id = list.back().map(|a| a.id + 1).unwrap_or(1);
        let alert = Alert {
            id,
            ts: now,
            source: n.source.into(),
            severity: n.severity,
            title: n.title,
            body: n.body,
            subject: n.subject,
            key: n.key,
            count: 1,
            acked: false,
        };
        list.push_back(alert.clone());
        while list.len() > KEEP {
            list.pop_front();
        }
        let snapshot = list.clone();
        drop(list);
        self.save(&snapshot);
        let _ = self.tx.send(alert.clone());
        Some(alert)
    }

    /// Newest first; `after` returns only alerts with a larger id.
    pub fn list(&self, after: Option<u64>, limit: usize) -> Vec<Alert> {
        self.lock()
            .iter()
            .rev()
            .filter(|a| after.is_none_or(|x| a.id > x))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn unacked(&self) -> (usize, usize) {
        let list = self.lock();
        let open = list.iter().filter(|a| !a.acked);
        let crit = open.clone().filter(|a| a.severity == Severity::Crit).count();
        (open.count(), crit)
    }

    /// `None` acks everything.
    pub fn ack(&self, id: Option<u64>) -> usize {
        let mut list = self.lock();
        let mut n = 0;
        for a in list.iter_mut().filter(|a| !a.acked && id.is_none_or(|i| a.id == i)) {
            a.acked = true;
            n += 1;
        }
        let snapshot = list.clone();
        drop(list);
        if n > 0 {
            self.save(&snapshot);
        }
        n
    }
}

// ── watchers ───────────────────────────────────────────────────────────

/// DaemonHall: failed units, auto-restarts, and DaemonHall itself.
pub async fn watch_daemonhall(store: Arc<Store>, dh: Arc<Upstream>, every: Duration) {
    let mut seen: Option<HashMap<String, (String, u64)>> = None;
    let mut misses = 0u32;
    loop {
        match dh.get_json(&["api", "units"], &[]).await {
            Ok(v) => {
                if misses >= 3 {
                    store.raise(NewAlert {
                        source: "daemonhall",
                        severity: Severity::Info,
                        key: "daemonhall:down".into(),
                        title: "DaemonHall is answering again".into(),
                        body: String::new(),
                        subject: String::new(),
                    });
                }
                misses = 0;
                let now = units_state(&v);
                if let Some(before) = &seen {
                    for a in unit_transitions(before, &now) {
                        store.raise(a);
                    }
                }
                seen = Some(now);
            }
            Err(e) => {
                misses += 1;
                if misses == 3 {
                    store.raise(NewAlert {
                        source: "daemonhall",
                        severity: Severity::Warn,
                        key: "daemonhall:down".into(),
                        title: "DaemonHall is not answering".into(),
                        body: e.message(),
                        subject: String::new(),
                    });
                }
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// id → (active_state, n_restarts)
fn units_state(v: &Value) -> HashMap<String, (String, u64)> {
    v.get("units")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|u| {
            Some((
                u.get("id")?.as_str()?.to_string(),
                (
                    u.get("active_state")?.as_str()?.to_string(),
                    u.get("n_restarts").and_then(Value::as_u64).unwrap_or(0),
                ),
            ))
        })
        .collect()
}

fn unit_transitions<'a>(
    before: &HashMap<String, (String, u64)>,
    now: &HashMap<String, (String, u64)>,
) -> Vec<NewAlert<'a>> {
    let mut out = vec![];
    for (id, (state, restarts)) in now {
        let Some((was, was_restarts)) = before.get(id) else { continue };
        if state == "failed" && was != "failed" {
            out.push(NewAlert {
                source: "daemonhall",
                severity: Severity::Crit,
                key: format!("unit-failed:{id}"),
                title: format!("{} failed", short_unit(id)),
                body: format!("{id} entered the failed state."),
                subject: id.clone(),
            });
        } else if restarts > was_restarts {
            out.push(NewAlert {
                source: "daemonhall",
                severity: Severity::Warn,
                key: format!("unit-restart:{id}"),
                title: format!("{} restarted itself", short_unit(id)),
                body: format!("systemd restarted {id} ({restarts} restarts so far)."),
                subject: id.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

fn short_unit(id: &str) -> &str {
    let id = id.strip_prefix("user:").unwrap_or(id);
    id.strip_suffix(".service").unwrap_or(id)
}

/// Dockspace: a running stack dropping to partial or stopped.
pub async fn watch_dockspace(store: Arc<Store>, ds: Arc<Upstream>, every: Duration) {
    let mut seen: Option<HashMap<String, String>> = None;
    loop {
        if let Ok(v) = ds.get_json(&["api", "stacks"], &[]).await {
            let now: HashMap<String, String> = v
                .get("stacks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| {
                    Some((
                        s.get("name")?.as_str()?.to_string(),
                        s.get("status")?.as_str()?.to_string(),
                    ))
                })
                .collect();
            if let Some(before) = &seen {
                for (name, status) in &now {
                    let was = before.get(name).map(String::as_str);
                    if was == Some("Running") && matches!(status.as_str(), "Partial" | "Stopped") {
                        store.raise(NewAlert {
                            source: "dockspace",
                            severity: Severity::Warn,
                            key: format!("stack:{name}"),
                            title: format!("Stack {name} is {}", status.to_lowercase()),
                            body: format!("{name} was running and is now {}.", status.to_lowercase()),
                            subject: name.clone(),
                        });
                    }
                }
            }
            seen = Some(now);
        }
        tokio::time::sleep(every).await;
    }
}

/// Argus: follow its JSONL event log from the end.
pub async fn watch_argus(store: Arc<Store>, path: PathBuf) {
    let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < offset {
            offset = 0; // rotated or truncated
        }
        if len == offset {
            continue;
        }
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut paths = vec![];
        let mut reader = std::io::BufReader::new(f);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if !line.ends_with('\n') {
                break; // half-written; read it next time
            }
            offset += line.len() as u64;
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && !v.get("accepted").and_then(Value::as_bool).unwrap_or(false)
                && let Some(p) = v.get("path").and_then(Value::as_str)
            {
                let change = v.get("change").and_then(Value::as_str).unwrap_or("changed");
                paths.push(format!("{change}: {p}"));
            }
            line.clear();
        }
        if let Some(a) = argus_alert(&paths) {
            store.raise(a);
        }
    }
}

fn argus_alert<'a>(paths: &[String]) -> Option<NewAlert<'a>> {
    let first = paths.first()?;
    let title = if paths.len() == 1 {
        "Argus: 1 integrity change".to_string()
    } else {
        format!("Argus: {} integrity changes", paths.len())
    };
    let mut body = paths.iter().take(5).cloned().collect::<Vec<_>>().join("\n");
    if paths.len() > 5 {
        body.push_str(&format!("\n… and {} more", paths.len() - 5));
    }
    Some(NewAlert {
        source: "argus",
        severity: Severity::Crit,
        key: "argus".into(),
        title,
        body,
        subject: first.split_once(": ").map(|x| x.1).unwrap_or(first).to_string(),
    })
}

/// VortexWall: follow its unit's journal for bans.
pub async fn watch_vortexwall(store: Arc<Store>, unit: String) {
    use tokio::io::AsyncBufReadExt;
    loop {
        let child = tokio::process::Command::new("journalctl")
            .args(["-f", "-n", "0", "-o", "cat", "-u", &unit])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child
            && let Some(out) = child.stdout.take()
        {
            let mut lines = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(a) = ban_alert(&line) {
                    store.raise(a);
                }
            }
            let _ = child.wait().await;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// `[BANNED] 203.0.113.9 for 3600s (threshold 5 reached)`
fn ban_alert<'a>(line: &str) -> Option<NewAlert<'a>> {
    let rest = line.trim().strip_prefix("[BANNED] ")?;
    let ip = rest.split_whitespace().next()?.to_string();
    Some(NewAlert {
        source: "vortexwall",
        severity: Severity::Warn,
        key: format!("ban:{ip}"),
        title: format!("VortexWall banned {ip}"),
        body: rest.to_string(),
        subject: ip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new(key: &str) -> NewAlert<'static> {
        NewAlert {
            source: "test",
            severity: Severity::Warn,
            key: key.into(),
            title: key.into(),
            body: String::new(),
            subject: String::new(),
        }
    }

    #[test]
    fn repeats_fold_until_acked() {
        let s = Store::memory();
        assert!(s.raise(new("a")).is_some());
        assert!(s.raise(new("a")).is_none());
        assert_eq!(s.list(None, 10)[0].count, 2);
        assert!(s.raise(new("b")).is_some());
        assert_eq!(s.unacked(), (2, 0));
        assert_eq!(s.ack(Some(1)), 1);
        assert!(s.raise(new("a")).is_some(), "acked alerts don't absorb new ones");
        assert_eq!(s.list(Some(2), 10).len(), 1);
        assert_eq!(s.ack(None), 2);
        assert_eq!(s.unacked(), (0, 0));
    }

    #[test]
    fn store_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("nw-alerts-{}.jsonl", crate::tokens::random_hex(4)));
        let s = Store::open(path.clone());
        s.raise(new("x"));
        s.raise(new("y"));
        let again = Store::open(path.clone());
        let ids: Vec<u64> = again.list(None, 10).iter().map(|a| a.id).collect();
        assert_eq!(ids, vec![2, 1]);
        assert_eq!(again.raise(new("z")).unwrap().id, 3);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unit_failures_and_restarts_alert() {
        let before = units_state(&json!({"units": [
            {"id": "a.service", "active_state": "active", "n_restarts": 0},
            {"id": "user:b.service", "active_state": "active", "n_restarts": 1},
            {"id": "c.service", "active_state": "failed", "n_restarts": 0},
        ]}));
        let now = units_state(&json!({"units": [
            {"id": "a.service", "active_state": "failed", "n_restarts": 0},
            {"id": "user:b.service", "active_state": "active", "n_restarts": 2},
            {"id": "c.service", "active_state": "failed", "n_restarts": 0},
            {"id": "new.service", "active_state": "failed", "n_restarts": 0},
        ]}));
        let alerts = unit_transitions(&before, &now);
        let titles: Vec<&str> = alerts.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["a failed", "b restarted itself"]);
        assert_eq!(alerts[0].severity, Severity::Crit);
    }

    #[test]
    fn argus_batches_and_vortexwall_parses() {
        assert!(argus_alert(&[]).is_none());
        let paths: Vec<String> = (0..7).map(|i| format!("New: /etc/f{i}")).collect();
        let a = argus_alert(&paths).unwrap();
        assert_eq!(a.title, "Argus: 7 integrity changes");
        assert!(a.body.ends_with("… and 2 more"));
        assert_eq!(a.subject, "/etc/f0");
        let b = ban_alert("[BANNED] 203.0.113.9 for 3600s (threshold 5 reached)").unwrap();
        assert_eq!(b.subject, "203.0.113.9");
        assert!(ban_alert("[DRY-RUN] would ban 1.2.3.4").is_none());
    }
}
