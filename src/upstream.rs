//! Calls to the local services, authenticated with their local-token/v1
//! files (see docs/local-token-v1.md). The token is re-read on every call:
//! services write a fresh one each time they start, and reading a 65-byte
//! file is cheaper than noticing a restart.

use crate::config;
use axum::http::StatusCode;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug)]
pub enum UpError {
    /// The service is turned off in the config.
    Disabled(&'static str),
    /// It didn't answer.
    Unreachable(&'static str, String),
    /// It answered with an error; passed through to the phone.
    Status(StatusCode, String),
}

impl UpError {
    pub fn status(&self) -> StatusCode {
        match self {
            UpError::Disabled(_) => StatusCode::NOT_FOUND,
            UpError::Unreachable(..) => StatusCode::BAD_GATEWAY,
            UpError::Status(s, _) => *s,
        }
    }

    pub fn message(&self) -> String {
        match self {
            UpError::Disabled(n) => format!("{n} is turned off on this host"),
            UpError::Unreachable(n, e) => format!("{n} is not answering: {e}"),
            UpError::Status(_, m) => m.clone(),
        }
    }
}

pub struct Upstream {
    pub name: &'static str,
    cfg: config::Upstream,
    http: reqwest::Client,
}

impl Upstream {
    pub fn new(name: &'static str, cfg: &config::Upstream, timeout: Duration) -> Upstream {
        Upstream {
            name,
            cfg: cfg.clone(),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("http client"),
        }
    }

    /// No timeout: for SSE streams that stay open.
    pub fn streaming(name: &'static str, cfg: &config::Upstream) -> Upstream {
        Upstream {
            name,
            cfg: cfg.clone(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("http client"),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn token(&self) -> Option<String> {
        std::fs::read_to_string(&self.cfg.token_file)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    /// `segments` are URL-encoded one by one, so a unit id like
    /// `user:foo@bar.service` can't escape its path segment.
    pub fn url(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<reqwest::Url, UpError> {
        let mut url = reqwest::Url::parse(&self.cfg.url)
            .map_err(|e| UpError::Unreachable(self.name, format!("bad url in config: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| UpError::Unreachable(self.name, "bad url in config".into()))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        actor: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, url);
        if let Some(t) = self.token() {
            rb = rb.bearer_auth(t);
        }
        if let Some(a) = actor {
            rb = rb.header("X-Cybercore-Actor", a);
        }
        rb
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        body: Option<&Value>,
        actor: Option<&str>,
    ) -> Result<reqwest::Response, UpError> {
        if !self.cfg.enabled {
            return Err(UpError::Disabled(self.name));
        }
        let mut rb = self.request(method, url, actor);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| UpError::Unreachable(self.name, short(&e)))?;
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        // A service that refuses our token most likely restarted after we
        // read the file, or its unit lacks the runtime directory.
        let msg = match status {
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED if self.token().is_none() => format!(
                "{} refused the request and its local token ({}) is missing — is it running a local-token/v1 build?",
                self.name,
                self.cfg.token_file.display()
            ),
            _ => message_of(&text).unwrap_or_else(|| format!("{} answered {status}", self.name)),
        };
        Err(UpError::Status(status, msg))
    }

    pub async fn get_json(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> Result<Value, UpError> {
        let url = self.url(segments, query)?;
        let resp = self.send(reqwest::Method::GET, url, None, None).await?;
        resp.json()
            .await
            .map_err(|e| UpError::Unreachable(self.name, format!("bad JSON: {}", short(&e))))
    }

    pub async fn get_text(&self, segments: &[&str]) -> Result<String, UpError> {
        let url = self.url(segments, &[])?;
        let resp = self.send(reqwest::Method::GET, url, None, None).await?;
        resp.text()
            .await
            .map_err(|e| UpError::Unreachable(self.name, short(&e)))
    }

    /// POST and return the body as JSON when it is JSON, else as a string.
    pub async fn post(
        &self,
        segments: &[&str],
        body: Option<&Value>,
        actor: &str,
    ) -> Result<Value, UpError> {
        let url = self.url(segments, &[])?;
        let resp = self
            .send(reqwest::Method::POST, url, body, Some(actor))
            .await?;
        let text = resp.text().await.unwrap_or_default();
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// Is it up at all? A cheap GET that every service serves.
    pub async fn ping(&self, segments: &[&str]) -> bool {
        match self.url(segments, &[]) {
            Ok(url) => self
                .send(reqwest::Method::GET, url, None, None)
                .await
                .is_ok(),
            Err(_) => false,
        }
    }
}

fn short(e: &reqwest::Error) -> String {
    if e.is_connect() {
        "connection refused".into()
    } else if e.is_timeout() {
        "timed out".into()
    } else {
        e.to_string()
    }
}

/// Services answer errors as `{"message": ...}` or plain text.
fn message_of(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        for key in ["message", "error"] {
            if let Some(m) = v.get(key).and_then(Value::as_str) {
                return Some(m.to_string());
            }
        }
    }
    Some(text.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> Upstream {
        Upstream::new(
            "daemonhall",
            &config::Upstream {
                enabled: true,
                url: "http://127.0.0.1:8767".into(),
                token_file: "/nonexistent".into(),
            },
            Duration::from_secs(1),
        )
    }

    #[test]
    fn path_segments_are_encoded() {
        let u = up()
            .url(
                &["api", "unit", "user:a/../b@x.service", "action", "stop"],
                &[],
            )
            .unwrap();
        assert_eq!(
            u.as_str(),
            "http://127.0.0.1:8767/api/unit/user:a%2F..%2Fb@x.service/action/stop"
        );
        let u = up()
            .url(&["api", "logs"], &[("unit", "a b&c".into())])
            .unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:8767/api/logs?unit=a+b%26c");
    }

    #[test]
    fn error_bodies_become_messages() {
        assert_eq!(
            message_of(r#"{"ok":false,"message":"nope"}"#).as_deref(),
            Some("nope")
        );
        assert_eq!(
            message_of("not a watched unit").as_deref(),
            Some("not a watched unit")
        );
        assert_eq!(message_of("  "), None);
    }
}
