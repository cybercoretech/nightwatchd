//! nightwatchd — the gateway between the Nightwatch phone app and this
//! host's Cybercore services.
//!
//! Listens on loopback only (default 127.0.0.1:8769); `tailscale serve`
//! publishes it on the tailnet. Phones authenticate with device tokens
//! (`nightwatchd token add`). The services behind it stay loopback-only and
//! are reached with their local-token/v1 files.

mod alerts;
mod api;
mod config;
mod jsonfile;
mod mission;
mod policy;
mod push;
mod tokens;
mod upstream;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn usage() -> ! {
    println!(
        "nightwatchd — Nightwatch's gateway to this host's Cybercore services\n\n\
         USAGE:\n  \
         nightwatchd [serve] [--config FILE]   run the gateway\n  \
         nightwatchd token add NAME [--operate]   new device token (read-only unless --operate)\n  \
         nightwatchd token list\n  \
         nightwatchd token revoke ID\n  \
         nightwatchd check [--config FILE]   test the connection to each service\n\n\
         Config: {} (optional)\nTokens: {}",
        config::default_path().display(),
        tokens::default_path().display(),
    );
    std::process::exit(0)
}

fn die(e: impl std::fmt::Display) -> ! {
    eprintln!("nightwatchd: {e}");
    std::process::exit(1)
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = config::default_path();
    if let Some(i) = args.iter().position(|a| a == "--config") {
        if i + 1 >= args.len() {
            usage();
        }
        config_path = PathBuf::from(args.remove(i + 1));
        args.remove(i);
    }
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(config_path).await,
        Some("token") => token(&args[1..]),
        Some("check") => check(config_path).await,
        Some("-h" | "--help" | "help") => usage(),
        Some(other) => die(format!("unknown command {other:?} (try --help)")),
    }
}

fn token(rest: &[String]) {
    let store = tokens::Store::open(tokens::default_path());
    match rest.first().map(String::as_str) {
        Some("add") => {
            let operate = rest.iter().any(|a| a == "--operate");
            let name: Vec<&str> = rest[1..]
                .iter()
                .filter(|a| *a != "--operate")
                .map(String::as_str)
                .collect();
            let (t, secret) = store.create(&name.join(" "), operate).unwrap_or_else(|e| die(e));
            println!(
                "token for {:?} ({}), id {}:\n\n  {secret}\n\nShown once; it isn't stored. Enter it in Nightwatch on the phone.\nRevoke with `nightwatchd token revoke {}`.",
                t.name,
                if t.operate { "read + operate" } else { "read-only" },
                t.id,
                t.id
            );
        }
        Some("list") => {
            let list = store.list();
            if list.is_empty() {
                println!("no device tokens yet: nightwatchd token add \"My phone\" --operate");
            }
            for t in list {
                println!(
                    "{}  {:<24} {:<14} last used {}",
                    t.id,
                    t.name,
                    if t.operate { "read + operate" } else { "read-only" },
                    t.last_used.map(ago).unwrap_or_else(|| "never".into())
                );
            }
        }
        Some("revoke") => {
            let id = rest.get(1).unwrap_or_else(|| usage());
            match store.revoke(id) {
                Ok(true) => println!("revoked {id}; that phone is locked out from now on"),
                Ok(false) => die(format!("no token with id {id}")),
                Err(e) => die(e),
            }
        }
        _ => usage(),
    }
}

fn ago(t: u64) -> String {
    match tokens::now().saturating_sub(t) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

fn upstreams(cfg: &config::Config) -> (upstream::Upstream, upstream::Upstream, upstream::Upstream) {
    let t = Duration::from_secs(15);
    (
        upstream::Upstream::new("daemonhall", &cfg.daemonhall, t),
        upstream::Upstream::new("dockspace", &cfg.dockspace, t),
        upstream::Upstream::new("cyberdeck", &cfg.cyberdeck, t),
    )
}

async fn check(config_path: PathBuf) {
    let cfg = config::Config::load(&config_path).unwrap_or_else(|e| die(e));
    let (dh, ds, deck) = upstreams(&cfg);
    for (u, path, file) in [
        (&dh, &["api", "units"][..], &cfg.daemonhall.token_file),
        (&ds, &["api", "summary"][..], &cfg.dockspace.token_file),
        (&deck, &["cyberdeck", "api", "state"][..], &cfg.cyberdeck.token_file),
    ] {
        if !u.enabled() {
            println!("{:<11} off", u.name);
            continue;
        }
        let token = if std::fs::metadata(file).is_ok() { "token ok" } else { "NO TOKEN FILE" };
        let reach = match u.get_json(path, &[]).await {
            Ok(_) => "answering".to_string(),
            Err(e) => e.message(),
        };
        println!("{:<11} {reach}; {token} ({})", u.name, file.display());
    }
    let m = mission::Mission::new(&cfg.mission);
    println!("{:<11} {}", "mission", if m.enabled() { "configured" } else { "off" });
    println!(
        "{:<11} {}",
        "push",
        if cfg.push.allowed_hosts.is_empty() {
            "off (no [push] allowed_hosts)".to_string()
        } else {
            format!("to {}", cfg.push.allowed_hosts.join(", "))
        }
    );
}

async fn serve(config_path: PathBuf) {
    let cfg = config::Config::load(&config_path).unwrap_or_else(|e| die(e));
    let (dh, ds, deck) = upstreams(&cfg);
    let dh = Arc::new(dh);
    let ds = Arc::new(ds);
    let tokens = Arc::new(tokens::Store::open(tokens::default_path()));
    let alerts = Arc::new(alerts::Store::open(config::state_dir().join("alerts.jsonl")));
    let push = Arc::new(push::Registry::open(push::default_path(), cfg.push.allowed_hosts.clone()));
    let (stop_tx, stopping) = tokio::sync::watch::channel(false);

    if cfg.alerts.enabled {
        let every = Duration::from_secs(cfg.alerts.poll_secs.max(5));
        if dh.enabled() {
            tokio::spawn(alerts::watch_daemonhall(alerts.clone(), dh.clone(), every));
        }
        if ds.enabled() {
            tokio::spawn(alerts::watch_dockspace(alerts.clone(), ds.clone(), every * 4));
        }
        if !cfg.alerts.argus_events.is_empty() {
            tokio::spawn(alerts::watch_argus(alerts.clone(), cfg.alerts.argus_events.clone().into()));
        }
        if !cfg.alerts.vortexwall_unit.is_empty() {
            tokio::spawn(alerts::watch_vortexwall(alerts.clone(), cfg.alerts.vortexwall_unit.clone()));
        }
    }
    tokio::spawn(push::run(alerts.clone(), push.clone(), tokens.clone(), cfg.host_label.clone()));

    let app = Arc::new(api::App {
        host: cfg.host_label.clone(),
        tokens,
        policy: policy::Policy::new(&cfg.policy.protected_units),
        dh_stream: upstream::Upstream::streaming("daemonhall", &cfg.daemonhall),
        dh,
        ds,
        deck,
        alerts,
        push,
        mission: mission::Mission::new(&cfg.mission),
        brake: api::Brake::default(),
        stopping,
    });

    let addr: std::net::SocketAddr = cfg
        .listen
        .parse()
        .unwrap_or_else(|e| die(format!("listen {:?}: {e}", cfg.listen)));
    if !addr.ip().is_loopback() {
        die(format!(
            "listen {addr} is not a loopback address; publish nightwatchd with `tailscale serve` instead"
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| die(format!("can't bind {addr}: {e}")));
    println!("[nightwatchd] {} listening on http://{addr}/v1", cfg.host_label);
    let shutdown = async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = stop_tx.send(true);
    };
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(shutdown)
        .await
        .unwrap_or_else(|e| die(e));
}
