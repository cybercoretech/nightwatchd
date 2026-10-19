# nightwatchd API v1

Base URL: `http://<host>.<tailnet>.ts.net:8769/v1`, published by `tailscale serve`
from `127.0.0.1:8769`.

- **Auth:** every request carries `Authorization: Bearer nw_<id>_<secret>`. Make
  tokens with `nightwatchd token add NAME [--operate]`. A read token can call
  every `GET`. Calls marked **operate** need an operate token.
- **Errors:** `{"ok": false, "message": "..."}`, with the service's own status
  passed through where there is one. If a service doesn't answer you get `502`;
  if it's turned off in the config, `404`. After 20 bad tokens in a minute,
  requests without a valid token get `429` until the minute ends.
- **Caching:** every response is `Cache-Control: no-store`. No CORS headers are
  sent.

## Session

| | |
|---|---|
| `GET /session` | `{host, version, device: {id, name, operate}, services: {daemonhall, dockspace, cyberdeck: bool, mission: project or null, push: bool}}` |
| `GET /overview` | `{host, units: {total, attention, failed, active}, stacks: {total, running, partial, stopped, unknown, working}, alerts: {open, crit}}`. `units` or `stacks` is `null` when that service is down. |

## Units (DaemonHall)

| | |
|---|---|
| `GET /units` | DaemonHall's snapshot `{generated, units: [...]}`. Each unit gains `nw_actions` (what this host lets the phone do) and `nw_protected`. |
| `GET /units/:id` | `{unit, ports}`. The unit file is left out because it may hold secrets. |
| `GET /units/:id/logs?lines=N` | Server-sent events from the journal: `data: {ts, prio, pid, ident, msg}`. Stays open. |
| `POST /units/:id/action/:verb` | **operate.** `verb` is `start`, `stop` or `restart`. Protected units may only `start`. Recorded in DaemonHall as `(via nightwatch:<device>)`. |
| `GET /activity?unit=&limit=` | The DaemonHall timeline, newest first. |

Built-in protected units: `tailscaled`, `sshd`/`ssh`, `NetworkManager`,
`systemd-networkd`, `systemd-resolved`, `iwd`, `dbus`, `polkit`, and DaemonHall
and nightwatchd themselves. Add more with `[policy] protected_units`.

## Stacks and containers (Dockspace)

| | |
|---|---|
| `GET /stacks` | `{stacks: [{id, name, status, services, ports, containers: [{service, state, name}], last_error, nw_actions}]}`. `status` is one of `Running`, `Partial`, `Stopped`, `Unknown`, `Working`. |
| `GET /stacks/:id/logs` | `{logs}`, the last 200 lines. |
| `POST /stacks/:id/action/:verb` | **operate.** `up` or `restart`. It runs in the background, so re-read `/stacks` to see the result. `down` is never allowed. |
| `GET /containers/:name/logs` | `{logs}`. Only for containers that belong to a Dockspace stack. |
| `POST /containers/:name/action/:verb` | **operate.** `start`, `stop` or `restart`, for stack containers only. |

## Diagnostics (Cyberdeck-hub)

| | |
|---|---|
| `GET /deck/reports` | `[{path, folder, title, is_md}]` |
| `GET /deck/reports/*path` | `{path, markdown}` |
| `GET /deck/scans` | `{scans: ["audio", "battery", ...]}` |
| `POST /deck/scans/:module` | **operate.** Runs one scan and returns `{ok, message}`, where `message` is the report path or an error. |

## Alerts

| | |
|---|---|
| `GET /alerts?after=ID&limit=N` | `{alerts: [{id, ts, source, severity, title, body, subject, count, acked}], open, crit}`, newest first. Use `after` to fetch only what's new. |
| `POST /alerts/ack` | **operate.** Body `{"id": N}` acknowledges one alert; `{}` or no body acknowledges all. |

`severity` is `info`, `warn` or `crit`. The sources are:
- **daemonhall:** a unit failed, a unit restarted itself, or DaemonHall stopped
  answering.
- **dockspace:** a stack that was running became partial or stopped.
- **argus:** an integrity change.
- **vortexwall:** a ban.

## Push (UnifiedPush)

| | |
|---|---|
| `GET /push` | `{enabled, endpoint}` for this device. |
| `POST /push` | Body `{"endpoint": "https://ntfy.../upXXXX?up=1"}`. The host must be listed in `[push] allowed_hosts`. There's one endpoint per device token; registering again replaces it. |
| `DELETE /push` | Stops pushes to this device. |

Each new alert of `warn` or `crit` is POSTed to every endpoint as
`{"type": "alert", "host", "id", "severity", "title"}`, with the ntfy `Priority`
header set to 4 or 5. Revoking a token stops its pushes.

## Mission Control (read-only)

| | |
|---|---|
| `GET /mission` | The configured project. |
| `GET /mission/agents` | Agents in that project. |
| `GET /mission/agents/:id/heartbeats` | Recent heartbeats for one agent. |
| `GET /mission/audit` | The project's audit trail. |

The admin token and the Cloudflare Access service token stay in files on the
host (`[mission]` in the config), and the phone never sees them.
