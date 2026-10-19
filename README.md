# 🌙 nightwatchd

**The gateway between the Nightwatch phone app and a Cybercore host.**

Your services stay exactly as locked down as they are: loopback-only, and
guarded against the browser. `nightwatchd` is the one new thing on the tailnet.
It checks the phone's device token and the action allowlist, then calls the
services locally with their `local-token/v1` files.

```
Phone (Nightwatch)
   │  http://<host>.<tailnet>.ts.net:8769/v1, device token
   ▼
nightwatchd (127.0.0.1:8769, published by tailscale serve)
   ├── DaemonHall      :8767   units, logs, start/stop/restart
   ├── Dockspace       :7070   stacks, containers, up/restart
   ├── Cyberdeck-hub   :8080   diagnostics reports, scans
   ├── Argus · VortexWall      alerts (event log, journal)
   └── Mission Control         read-only, credentials stay on the host
```

`Rust` · `axum` · `tokio` · `reqwest`

## 🛰️ What it does

- **👁️ Watch.** Every watched unit with its state, resources and restarts. Live
  journal logs, the DaemonHall timeline, compose stacks and their containers,
  Cyberdeck reports as Markdown, and Mission Control agents, heartbeats and
  audit.
- **🕹️ Operate,** with an operate token only:
  - start, stop or restart a watched unit
  - start, stop or restart a container that belongs to a stack
  - `up` or `restart` a stack
  - run a Cyberdeck scan
  - acknowledge alerts
- **🚨 Alert.** A unit failing or restarting itself, a running stack dropping,
  an Argus integrity change, a VortexWall ban, DaemonHall going quiet. Alerts
  are kept in `~/.local/state/nightwatch/alerts.jsonl` and pushed through
  UnifiedPush to your own ntfy server.
- **🧾 Attribute.** Every action shows up in the service's own log as
  `(via nightwatch:<phone>)`.

## 🛡️ What the phone can never do

It can't edit or delete unit files, `daemon-reload`, sync polkit, change
settings, edit compose files, create stacks, run `down` or prune, touch the
firewall, or re-baseline Argus or SigilWard.

**Protected units** can be started but never stopped or restarted, because
losing them would cut the phone off: `tailscaled`, `sshd`, `NetworkManager`,
`systemd-networkd`/`resolved`, `iwd`, `dbus`, `polkit`, DaemonHall and
nightwatchd itself. Add your own with `[policy] protected_units`.

The rules live in [`src/policy.rs`](src/policy.rs). The gateway enforces them,
so a lost phone or a buggy app can't go past them.

## 📦 Install

On each host, as your normal user:

```bash
# 1. Once per host: local-token/v1 builds of the services, plus the
#    RuntimeDirectory= drop-ins for the two system units (asks for sudo).
packaging/deploy-local-token.sh

# 2. The gateway: user service + tailscale serve on :8769.
packaging/install.sh

# 3. A token for your phone. Leave out --operate for a look-only phone.
nightwatchd token add "SGS25U" --operate
```

In Nightwatch, add the host as `http://<host>.<tailnet>.ts.net:8769` and paste
the token.

`nightwatchd check` shows, for each service, whether it answers and whether its
token file is there.

## ⚙️ Configuration

`~/.config/nightwatch/config.toml`. Every key is optional; these are the
defaults:

```toml
listen = "127.0.0.1:8769"     # loopback only; refuses anything else
host_label = "blackbox"        # default: /etc/hostname

[daemonhall]
url = "http://127.0.0.1:8767"
token_file = "/run/user/1000/daemonhall/local-token"

[dockspace]
url = "http://127.0.0.1:7070"
token_file = "/run/dockspace/local-token"

[cyberdeck]
url = "http://127.0.0.1:8080"
token_file = "/run/cyberdeck/local-token"
# enabled = false   turns a service off

[policy]
protected_units = []           # added to the built-in list

[alerts]
enabled = true
poll_secs = 15                 # DaemonHall; Dockspace is polled 4× less often
argus_events = "~/.local/state/argus/events.jsonl"   # "" = off
vortexwall_unit = "vortexwall-enforce.service"       # "" = off

[push]
allowed_hosts = []             # e.g. ["darkbox.tail9311b5.ts.net"]; empty = push off

[mission]
url = ""                       # https://… ; empty = off
project = ""
admin_token_file = ""
access_client_id_file = ""
access_client_secret_file = ""
```

A typo in a key is an error, not a silent default. That way a misspelled
`allowed_hosts` can't quietly turn the push allowlist off.

## 🔔 Push with ntfy

1. Run ntfy on a host the tailnet can reach, e.g. as a Dockspace stack on
   darkbox.
2. Add that host to `[push] allowed_hosts` and restart nightwatchd.
3. On the phone, install the ntfy app and set its default server to yours.
   Then, in Nightwatch, go to More → Push and pick ntfy.

A push carries only `{type, host, id, severity, title}`. Nightwatch treats it as
a wake-up and fetches the real alert over the tailnet, so a forged push can't
put words on your screen. Without push, the app still checks every host every
15 minutes.

## 🔑 Tokens

```bash
nightwatchd token add NAME [--operate]
nightwatchd token list
nightwatchd token revoke ID
```

Tokens look like `nw_<id>_<secret>`. Only a SHA-256 of each secret is kept, in
`~/.config/nightwatch/tokens.json` (mode 0600). Revoking a token locks that
phone out immediately and stops its pushes. After 20 bad tokens in a minute,
the gateway answers `429` for the rest of that minute.

## 📚 Docs

- [`docs/API.md`](docs/API.md): every endpoint.
- [`docs/local-token-v1.md`](docs/local-token-v1.md): how services let a local
  client in, and how to add that to another service.
- [`AGENT_HANDOFF.md`](AGENT_HANDOFF.md): state, decisions and gotchas.

## 🧪 Development

```bash
cargo test            # policy, tokens, alerts, push, mission, API scopes
cargo clippy --all-targets
```

The app lives in its own repo: `~/Devspace/Android/Nightwatch`.

MIT © Cybercore Tech
