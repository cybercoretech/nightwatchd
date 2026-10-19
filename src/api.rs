//! The phone's API, under `/v1`. Every request needs a device token
//! (`Authorization: Bearer nw_…`); actions also need an operate token and
//! pass `policy`. Nothing here is cached by the phone or a proxy
//! (`no-store`), and no CORS headers are ever sent.

// Handlers return early with a ready `Response` as their error; boxing it
// would only add noise.
#![allow(clippy::result_large_err)]

use crate::alerts::Store as Alerts;
use crate::mission::{Mission, View};
use crate::policy::{self, Policy};
use crate::push::Registry;
use crate::tokens::{Grant, Store as Tokens};
use crate::upstream::{UpError, Upstream};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

pub struct App {
    pub host: String,
    pub tokens: Arc<Tokens>,
    pub policy: Policy,
    pub dh: Arc<Upstream>,
    pub dh_stream: Upstream,
    pub ds: Arc<Upstream>,
    pub deck: Upstream,
    pub alerts: Arc<Alerts>,
    pub push: Arc<Registry>,
    pub mission: Mission,
    pub brake: Brake,
    /// Flips to true on shutdown so open log streams end.
    pub stopping: watch::Receiver<bool>,
}

type Shared = Arc<App>;

pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/v1/session", get(session))
        .route("/v1/overview", get(overview))
        // DaemonHall
        .route("/v1/units", get(units))
        .route("/v1/units/:id", get(unit_detail))
        .route("/v1/units/:id/logs", get(unit_logs))
        .route("/v1/units/:id/action/:verb", post(unit_action))
        .route("/v1/activity", get(activity))
        // Dockspace
        .route("/v1/stacks", get(stacks))
        .route("/v1/stacks/:id/logs", get(stack_logs))
        .route("/v1/stacks/:id/action/:verb", post(stack_action))
        .route("/v1/containers/:name/logs", get(container_logs))
        .route("/v1/containers/:name/action/:verb", post(container_action))
        // Cyberdeck-hub
        .route("/v1/deck/reports", get(deck_reports))
        .route("/v1/deck/reports/*path", get(deck_report))
        .route("/v1/deck/scans", get(deck_scans))
        .route("/v1/deck/scans/:module", post(deck_scan))
        // Alerts and push
        .route("/v1/alerts", get(alerts))
        .route("/v1/alerts/ack", post(alerts_ack))
        .route("/v1/push", get(push_get).post(push_register).delete(push_unregister))
        // Mission Control
        .route("/v1/mission", get(mission_project))
        .route("/v1/mission/agents", get(mission_agents))
        .route("/v1/mission/agents/:id/heartbeats", get(mission_heartbeats))
        .route("/v1/mission/audit", get(mission_audit))
        .fallback(|| async { err(StatusCode::NOT_FOUND, "no such endpoint") })
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}

// ── auth ───────────────────────────────────────────────────────────────

/// Slows down token guessing: after 20 bad tokens in a minute, everyone
/// without a valid token waits out the rest of that minute.
#[derive(Default)]
pub struct Brake {
    window: AtomicU64,
    failures: AtomicU64,
}

impl Brake {
    fn blocked(&self, now: u64) -> bool {
        self.window.load(Ordering::Relaxed) == now / 60 && self.failures.load(Ordering::Relaxed) >= 20
    }

    fn fail(&self, now: u64) {
        let minute = now / 60;
        if self.window.swap(minute, Ordering::Relaxed) != minute {
            self.failures.store(0, Ordering::Relaxed);
        }
        self.failures.fetch_add(1, Ordering::Relaxed);
    }
}

async fn auth(State(app): State<Shared>, mut req: Request, next: Next) -> Response {
    let now = crate::tokens::now();
    if app.brake.blocked(now) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many bad tokens; wait a minute");
    }
    let grant = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| app.tokens.verify(t.trim()));
    let Some(grant) = grant else {
        app.brake.fail(now);
        return err(StatusCode::UNAUTHORIZED, "missing or unknown device token");
    };
    req.extensions_mut().insert(grant);
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    resp
}

fn operate(g: &Grant) -> Result<(), Response> {
    if g.operate {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "this device token is read-only"))
    }
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "ok": false, "message": message.into() }))).into_response()
}

fn up_err(e: UpError) -> Response {
    err(e.status(), e.message())
}

fn reply(r: Result<Value, UpError>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => up_err(e),
    }
}

// ── session and overview ───────────────────────────────────────────────

async fn session(State(app): State<Shared>, Extension(g): Extension<Grant>) -> Json<Value> {
    let (dh, ds, deck) = tokio::join!(
        app.dh.ping(&["api", "meta"]),
        app.ds.ping(&["api", "activity"]),
        app.deck.ping(&["cyberdeck", "api", "state"]),
    );
    Json(json!({
        "host": app.host,
        "version": env!("CARGO_PKG_VERSION"),
        "device": { "id": g.id, "name": g.name, "operate": g.operate },
        "services": {
            "daemonhall": app.dh.enabled() && dh,
            "dockspace": app.ds.enabled() && ds,
            "cyberdeck": app.deck.enabled() && deck,
            "mission": app.mission.enabled().then(|| app.mission.project()),
            "push": app.push.enabled(),
        },
    }))
}

async fn overview(State(app): State<Shared>) -> Json<Value> {
    let (units, summary) = tokio::join!(
        app.dh.get_json(&["api", "units"], &[]),
        app.ds.get_json(&["api", "summary"], &[]),
    );
    let units = units.ok().map(|v| {
        let list = v.get("units").and_then(Value::as_array).cloned().unwrap_or_default();
        let count = |f: &dyn Fn(&Value) -> bool| list.iter().filter(|u| f(u)).count();
        json!({
            "total": list.len(),
            "attention": count(&|u| u.get("needs_attention").and_then(Value::as_bool).unwrap_or(false)),
            "failed": count(&|u| u.get("active_state").and_then(Value::as_str) == Some("failed")),
            "active": count(&|u| u.get("active_state").and_then(Value::as_str) == Some("active")),
        })
    });
    let (open, crit) = app.alerts.unacked();
    Json(json!({
        "host": app.host,
        "units": units,
        "stacks": summary.ok(),
        "alerts": { "open": open, "crit": crit },
    }))
}

// ── DaemonHall ─────────────────────────────────────────────────────────

async fn units(State(app): State<Shared>) -> Response {
    match app.dh.get_json(&["api", "units"], &[]).await {
        Ok(mut v) => {
            if let Some(list) = v.get_mut("units").and_then(Value::as_array_mut) {
                for u in list {
                    annotate_unit(&app.policy, u);
                }
            }
            Json(v).into_response()
        }
        Err(e) => up_err(e),
    }
}

/// Adds `nw_actions` (what the phone may do) and `nw_protected`.
fn annotate_unit(policy: &Policy, u: &mut Value) {
    let id = u.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let controllable = u.get("controllable").and_then(Value::as_bool).unwrap_or(false);
    if let Some(o) = u.as_object_mut() {
        o.insert("nw_actions".into(), json!(policy.unit_verbs(&id, controllable)));
        o.insert("nw_protected".into(), json!(policy.is_protected(&id)));
    }
}

async fn unit_detail(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.dh.get_json(&["api", "unit", &id], &[]).await {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                // Unit files can carry secrets in Environment= lines; the
                // phone has no use for them.
                o.remove("file");
                if let Some(u) = o.get_mut("unit") {
                    annotate_unit(&app.policy, u);
                }
            }
            Json(v).into_response()
        }
        Err(e) => up_err(e),
    }
}

#[derive(Deserialize)]
struct LinesQ {
    lines: Option<u32>,
}

/// Live journal lines as server-sent events, straight from DaemonHall.
async fn unit_logs(State(app): State<Shared>, Path(id): Path<String>, Query(q): Query<LinesQ>) -> Response {
    let lines = q.lines.unwrap_or(200).min(2000).to_string();
    let url = match app.dh_stream.url(&["api", "logs"], &[("unit", id), ("lines", lines)]) {
        Ok(u) => u,
        Err(e) => return up_err(e),
    };
    let resp = match app.dh_stream.send(reqwest::Method::GET, url, None, None).await {
        Ok(r) => r,
        Err(e) => return up_err(e),
    };
    let mut stopping = app.stopping.clone();
    let stop = async move {
        let _ = stopping.wait_for(|s| *s).await;
    };
    let stream = resp.bytes_stream().take_until(stop);
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "stream"))
}

async fn unit_action(
    State(app): State<Shared>,
    Extension(g): Extension<Grant>,
    Path((id, verb)): Path<(String, String)>,
) -> Response {
    if let Err(r) = operate(&g) {
        return r;
    }
    if let Err(m) = app.policy.check_unit(&id, &verb) {
        return err(StatusCode::FORBIDDEN, m);
    }
    reply(
        app.dh
            .post(&["api", "unit", &id, "action", &verb], None, &g.actor())
            .await,
    )
}

#[derive(Deserialize)]
struct ActivityQ {
    unit: Option<String>,
    limit: Option<usize>,
}

async fn activity(State(app): State<Shared>, Query(q): Query<ActivityQ>) -> Response {
    let mut query = vec![("limit", q.limit.unwrap_or(100).clamp(10, 500).to_string())];
    if let Some(u) = q.unit {
        query.push(("unit", u));
    }
    reply(app.dh.get_json(&["api", "activity"], &query).await)
}

// ── Dockspace ──────────────────────────────────────────────────────────

async fn stacks(State(app): State<Shared>) -> Response {
    match app.ds.get_json(&["api", "stacks"], &[]).await {
        Ok(mut v) => {
            if let Some(list) = v.get_mut("stacks").and_then(Value::as_array_mut) {
                for s in list {
                    if let Some(o) = s.as_object_mut() {
                        o.insert("nw_actions".into(), json!(policy::STACK_VERBS));
                    }
                }
            }
            Json(v).into_response()
        }
        Err(e) => up_err(e),
    }
}

async fn stack_logs(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    reply(app.ds.get_json(&["api", "stacks", &id, "logs"], &[]).await)
}

async fn stack_action(
    State(app): State<Shared>,
    Extension(g): Extension<Grant>,
    Path((id, verb)): Path<(String, String)>,
) -> Response {
    if let Err(r) = operate(&g) {
        return r;
    }
    if let Err(m) = policy::check_stack(&verb) {
        return err(StatusCode::FORBIDDEN, m);
    }
    // Dockspace answers with an HTML card and runs the action in the
    // background; the phone re-reads /v1/stacks for the outcome.
    match app.ds.post(&["stacks", &id, &verb], None, &g.actor()).await {
        Ok(_) => Json(json!({ "ok": true, "message": format!("{verb} started for {id}") })).into_response(),
        Err(e) => up_err(e),
    }
}

/// Only containers that belong to a stack Dockspace scans may be touched,
/// not any container on the host.
async fn known_container(app: &App, name: &str) -> Result<(), Response> {
    let v = app.ds.get_json(&["api", "stacks"], &[]).await.map_err(up_err)?;
    let found = v
        .get("stacks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("containers").and_then(Value::as_array))
        .flatten()
        .any(|c| c.get("name").and_then(Value::as_str) == Some(name));
    if found {
        Ok(())
    } else {
        Err(err(StatusCode::NOT_FOUND, format!("{name} is not a container of a Dockspace stack")))
    }
}

async fn container_logs(State(app): State<Shared>, Path(name): Path<String>) -> Response {
    if let Err(r) = known_container(&app, &name).await {
        return r;
    }
    reply(app.ds.get_json(&["api", "container", &name, "logs"], &[]).await)
}

async fn container_action(
    State(app): State<Shared>,
    Extension(g): Extension<Grant>,
    Path((name, verb)): Path<(String, String)>,
) -> Response {
    if let Err(r) = operate(&g) {
        return r;
    }
    if let Err(m) = policy::check_container(&verb) {
        return err(StatusCode::FORBIDDEN, m);
    }
    if let Err(r) = known_container(&app, &name).await {
        return r;
    }
    reply(app.ds.post(&["api", "container", &name, &verb], None, &g.actor()).await)
}

// ── Cyberdeck-hub ──────────────────────────────────────────────────────

async fn deck_reports(State(app): State<Shared>) -> Response {
    reply(app.deck.get_json(&["cyberdeck", "api", "list"], &[]).await)
}

async fn deck_report(State(app): State<Shared>, Path(path): Path<String>) -> Response {
    if path.split('/').any(|s| s.is_empty() || s == ".." || s == ".") {
        return err(StatusCode::BAD_REQUEST, "invalid report path");
    }
    let mut segs = vec!["api", "reports-md"];
    segs.extend(path.split('/'));
    match app.deck.get_text(&segs).await {
        Ok(md) => Json(json!({ "path": path, "markdown": md })).into_response(),
        Err(e) => up_err(e),
    }
}

async fn deck_scans() -> Json<Value> {
    Json(json!({ "scans": policy::SCANS.iter().map(|(m, _)| m).collect::<Vec<_>>() }))
}

async fn deck_scan(
    State(app): State<Shared>,
    Extension(g): Extension<Grant>,
    Path(module): Path<String>,
) -> Response {
    if let Err(r) = operate(&g) {
        return r;
    }
    let Some(cmd) = policy::scan_command(&module) else {
        return err(StatusCode::NOT_FOUND, format!("no scan called {module:?}"));
    };
    let body = json!({ cmd: "./diagnostics" });
    match app.deck.post(&["cyberdeck", "api", "command"], Some(&body), &g.actor()).await {
        Ok(v) => Json(json!({ "ok": true, "message": v })).into_response(),
        Err(e) => up_err(e),
    }
}

// ── alerts and push ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AlertsQ {
    after: Option<u64>,
    limit: Option<usize>,
}

async fn alerts(State(app): State<Shared>, Query(q): Query<AlertsQ>) -> Json<Value> {
    let list = app.alerts.list(q.after, q.limit.unwrap_or(100).min(500));
    let (open, crit) = app.alerts.unacked();
    Json(json!({ "alerts": list, "open": open, "crit": crit }))
}

#[derive(Deserialize, Default)]
struct AckBody {
    /// Omitted: acknowledge every open alert.
    id: Option<u64>,
}

async fn alerts_ack(
    State(app): State<Shared>,
    Extension(g): Extension<Grant>,
    body: Option<Json<AckBody>>,
) -> Response {
    if let Err(r) = operate(&g) {
        return r;
    }
    let id = body.map(|Json(b)| b).unwrap_or_default().id;
    Json(json!({ "ok": true, "acked": app.alerts.ack(id) })).into_response()
}

async fn push_get(State(app): State<Shared>, Extension(g): Extension<Grant>) -> Json<Value> {
    Json(json!({
        "enabled": app.push.enabled(),
        "endpoint": app.push.get(&g.id).map(|r| r.endpoint),
    }))
}

#[derive(Deserialize)]
struct PushBody {
    endpoint: String,
}

/// Any token may register its own phone; it only ever receives alerts it
/// could read anyway.
async fn push_register(State(app): State<Shared>, Extension(g): Extension<Grant>, Json(b): Json<PushBody>) -> Response {
    match app.push.register(&g.id, b.endpoint.trim()) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(m) => err(StatusCode::UNPROCESSABLE_ENTITY, m),
    }
}

async fn push_unregister(State(app): State<Shared>, Extension(g): Extension<Grant>) -> Json<Value> {
    Json(json!({ "ok": true, "removed": app.push.unregister(&g.id) }))
}

// ── Mission Control ────────────────────────────────────────────────────

async fn mission_project(State(app): State<Shared>) -> Response {
    reply(app.mission.get(View::Project).await)
}

async fn mission_agents(State(app): State<Shared>) -> Response {
    reply(app.mission.get(View::Agents).await)
}

async fn mission_heartbeats(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    reply(app.mission.get(View::Heartbeats(&id)).await)
}

async fn mission_audit(State(app): State<Shared>) -> Response {
    reply(app.mission.get(View::Audit).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// A gateway whose services all point at a dead port.
    fn app(tokens: Arc<Tokens>) -> Router {
        let dead = config::Upstream {
            enabled: true,
            url: "http://127.0.0.1:9".into(),
            token_file: "/nonexistent".into(),
        };
        let t = std::time::Duration::from_secs(1);
        let (_tx, rx) = watch::channel(false);
        std::mem::forget(_tx);
        router(Arc::new(App {
            host: "testbox".into(),
            tokens,
            policy: Policy::new(&[]),
            dh: Arc::new(Upstream::new("daemonhall", &dead, t)),
            dh_stream: Upstream::streaming("daemonhall", &dead),
            ds: Arc::new(Upstream::new("dockspace", &dead, t)),
            deck: Upstream::new("cyberdeck", &dead, t),
            alerts: Arc::new(Alerts::memory()),
            push: Arc::new(Registry::memory(vec!["ntfy.test".into()])),
            mission: Mission::new(&config::Mission::default()),
            brake: Brake::default(),
            stopping: rx,
        }))
    }

    async fn call(r: &Router, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = HttpRequest::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let resp = r.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn tokens_and_scopes_are_enforced() {
        let tokens = Arc::new(Tokens::memory());
        let (_, read) = tokens.create("reader", false).unwrap();
        let (_, op) = tokens.create("operator", true).unwrap();
        let r = app(tokens);

        assert_eq!(call(&r, "GET", "/v1/session", None, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&r, "GET", "/v1/session", Some("nw_x_y"), None).await.0, StatusCode::UNAUTHORIZED);
        let (s, v) = call(&r, "GET", "/v1/session", Some(&read), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["device"]["operate"], false);
        assert_eq!(v["services"]["daemonhall"], false);

        // Read tokens can't act; operate tokens pass the policy first.
        let (s, v) = call(&r, "POST", "/v1/units/dockspace.service/action/restart", Some(&read), None).await;
        assert_eq!((s, v["message"].as_str()), (StatusCode::FORBIDDEN, Some("this device token is read-only")));
        let (s, _) = call(&r, "POST", "/v1/units/tailscaled.service/action/stop", Some(&op), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(&r, "POST", "/v1/units/dockspace.service/action/enable", Some(&op), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(&r, "POST", "/v1/stacks/web/action/down", Some(&op), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        // Allowed by policy, so it reaches the (dead) service.
        let (s, v) = call(&r, "POST", "/v1/units/dockspace.service/action/restart", Some(&op), None).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(v["message"].as_str().unwrap().contains("not answering"));
        let (s, _) = call(&r, "POST", "/v1/deck/scans/ArchiveFiles", Some(&op), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(&r, "GET", "/v1/deck/reports/a/../b.md", Some(&op), None).await;
        assert_ne!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn alerts_and_push_registration() {
        let tokens = Arc::new(Tokens::memory());
        let (_, read) = tokens.create("reader", false).unwrap();
        let r = app(tokens);
        let (s, v) = call(&r, "GET", "/v1/alerts", Some(&read), None).await;
        assert_eq!((s, v["open"].as_u64()), (StatusCode::OK, Some(0)));
        assert_eq!(call(&r, "POST", "/v1/alerts/ack", Some(&read), None).await.0, StatusCode::FORBIDDEN);

        let bad = json!({ "endpoint": "http://127.0.0.1:8767/api/daemon-reload" });
        assert_eq!(call(&r, "POST", "/v1/push", Some(&read), Some(bad)).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        let good = json!({ "endpoint": "https://ntfy.test/upABC?up=1" });
        assert_eq!(call(&r, "POST", "/v1/push", Some(&read), Some(good)).await.0, StatusCode::OK);
        let (_, v) = call(&r, "GET", "/v1/push", Some(&read), None).await;
        assert_eq!(v["endpoint"], "https://ntfy.test/upABC?up=1");
        let (_, v) = call(&r, "GET", "/v1/mission", Some(&read), None).await;
        assert!(v["message"].as_str().unwrap().contains("turned off"));
    }

    #[tokio::test]
    async fn bad_tokens_are_braked() {
        let r = app(Arc::new(Tokens::memory()));
        for _ in 0..20 {
            call(&r, "GET", "/v1/session", Some("nw_bad_bad"), None).await;
        }
        let (s, _) = call(&r, "GET", "/v1/session", Some("nw_bad_bad"), None).await;
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    }
}
