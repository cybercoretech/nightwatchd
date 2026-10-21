//! Mission Control, read-only. nightwatchd holds the operator token and the
//! Cloudflare Access service token (in files on this host) and relays a
//! fixed set of GET endpoints for one project, so the phone sees agents,
//! heartbeats and the audit trail without ever holding a credential that
//! could register agents or create projects.

use crate::config;
use crate::upstream::UpError;
use serde_json::Value;
use std::time::Duration;

pub struct Mission {
    cfg: config::Mission,
    http: reqwest::Client,
}

/// The views the phone may ask for.
pub enum View<'a> {
    Project,
    Agents,
    Audit,
    Heartbeats(&'a str),
}

fn read(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl Mission {
    pub fn new(cfg: &config::Mission) -> Mission {
        Mission {
            cfg: config::Mission {
                url: cfg.url.trim_end_matches('/').to_string(),
                project: cfg.project.clone(),
                admin_token_file: cfg.admin_token_file.clone(),
                access_client_id_file: cfg.access_client_id_file.clone(),
                access_client_secret_file: cfg.access_client_secret_file.clone(),
            },
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.cfg.url.is_empty() && !self.cfg.project.is_empty()
    }

    pub fn project(&self) -> &str {
        &self.cfg.project
    }

    pub fn url_for(&self, view: &View) -> Result<reqwest::Url, UpError> {
        let mut url = reqwest::Url::parse(&self.cfg.url).map_err(|e| {
            UpError::Unreachable("mission control", format!("bad url in config: {e}"))
        })?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !loopback {
            return Err(UpError::Unreachable(
                "mission control",
                "url must be https".into(),
            ));
        }
        let mut segs = vec!["api", "projects", self.cfg.project.as_str()];
        match view {
            View::Project => {}
            View::Agents => segs.push("agents"),
            View::Audit => segs.push("audit"),
            View::Heartbeats(agent) => segs.extend(["agents", agent, "heartbeats"]),
        }
        url.path_segments_mut()
            .map_err(|_| UpError::Unreachable("mission control", "bad url in config".into()))?
            .pop_if_empty()
            .extend(segs);
        Ok(url)
    }

    pub async fn get(&self, view: View<'_>) -> Result<Value, UpError> {
        if !self.enabled() {
            return Err(UpError::Disabled("mission control"));
        }
        let url = self.url_for(&view)?;
        let mut rb = self.http.get(url);
        if let Some(t) = read(&self.cfg.admin_token_file) {
            rb = rb.header("x-mission-control-admin", t);
        }
        if let (Some(id), Some(secret)) = (
            read(&self.cfg.access_client_id_file),
            read(&self.cfg.access_client_secret_file),
        ) {
            rb = rb
                .header("CF-Access-Client-Id", id)
                .header("CF-Access-Client-Secret", secret);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| UpError::Unreachable("mission control", e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let code = axum::http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            // Cloudflare Access answers a missing service token with its
            // login page; don't hand that HTML to the phone.
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| format!("mission control answered {status}"));
            return Err(UpError::Status(code, msg));
        }
        serde_json::from_str(&text).map_err(|_| {
            UpError::Unreachable(
                "mission control",
                "answered with something other than JSON (Cloudflare Access login page?)".into(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(url: &str) -> Mission {
        Mission::new(&config::Mission {
            url: url.into(),
            project: "proj_1".into(),
            ..Default::default()
        })
    }

    #[test]
    fn urls_are_fixed_and_encoded() {
        let x = m("https://mc.example/");
        assert_eq!(
            x.url_for(&View::Agents).unwrap().as_str(),
            "https://mc.example/api/projects/proj_1/agents"
        );
        assert_eq!(
            x.url_for(&View::Heartbeats("a/../../x")).unwrap().as_str(),
            "https://mc.example/api/projects/proj_1/agents/a%2F..%2F..%2Fx/heartbeats"
        );
        assert!(m("http://mc.example").url_for(&View::Project).is_err());
        assert!(m("http://127.0.0.1:8787").url_for(&View::Project).is_ok());
        assert!(!m("").enabled());
    }
}
