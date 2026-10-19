# Cybercore local-token/v1

How a program on the same machine (Nightwatch's gateway, `nightwatchd`)
authenticates to a loopback-only Cybercore service that was built to be driven
from its own browser page.

Implemented by: DaemonHall, Dockspace, Cyberdeck-hub (`src/localtoken.rs`, the
same file in each).

## Why

The services bind `127.0.0.1` and defend against the browser: a `Host` check
stops DNS rebinding, and an Origin / `Sec-Fetch-Site` / page-token check stops
cross-site requests. None of that gives a *local program* a clean way in.
DaemonHall's write token only exists in memory and in its page's `<meta>` tag;
Dockspace and Cyberdeck-hub let header-less requests through, so they can't tell
a trusted local client from anything else on the box.

## The service side

1. At startup, generate 32 random bytes (from `/dev/urandom`) and hex-encode them.
2. Write them, plus a newline, to `<dir>/local-token`, where `<dir>` is
   - `$RUNTIME_DIRECTORY` (the first entry), when systemd's
     `RuntimeDirectory=` set it, or else
   - `$XDG_RUNTIME_DIR/<service>`.

   Create the directory with mode `0700` and the file with `0600`, by writing a
   temporary file and renaming it over the old one. Write a new token every
   start; never reuse one.
3. If neither variable is set, log it and carry on: the token still exists in
   memory, but no client can read it, so local clients are effectively off.
4. Accept `Authorization: Bearer <token>`, compared in constant time.
   - A request with the bearer **skips the browser checks** (Origin,
     `Sec-Fetch-Site`, page token). Browsers never attach that header on their
     own, so it can't be forged cross-site.
   - The **`Host` check still applies**.
5. On a bearer request, read `X-Cybercore-Actor` (printable ASCII, at most 80
   characters, trimmed) and record it with whatever the request changes:
   DaemonHall appends ` (via <actor>)` to the activity message, Dockspace stores
   `actor` on the activity entry, and Cyberdeck-hub logs it. Ignore the header
   on requests without a valid bearer.

## The client side

- Re-read the file before each request (it's tiny), or at least whenever the
  service answers 401/403: the token changes every time the service restarts.
- Send `X-Cybercore-Actor: <program>:<who>`, e.g. `nightwatch:SGS25U`.
- Only the service's own user can read the file. `nightwatchd` therefore runs
  as the same user as the services: `raven` on blackbox.

## Where the files are

| Service | Unit | Token file |
|---|---|---|
| DaemonHall | user `daemonhall.service` | `/run/user/<uid>/daemonhall/local-token` |
| Dockspace | system `dockspace.service`, `User=raven` | `/run/dockspace/local-token` (needs the drop-in) |
| Cyberdeck-hub | system `cyberdeck.service`, `User=raven` | `/run/cyberdeck/local-token` (needs the drop-in) |

The two system units use `ProtectSystem=strict`, so they need
`RuntimeDirectory=` for somewhere writable. The drop-ins in
`packaging/dropins/` add it with `RuntimeDirectoryMode=0700`, and
`packaging/deploy-local-token.sh` installs them.

## Adding it to another service

Copy `src/localtoken.rs` from one of the three. Call
`LocalToken::create("<service>")` at startup and check `verify()` in the request
guard before the browser checks. If a guard spawns background work, carry the
actor into it: Dockspace re-enters its `ACTOR` task-local inside
`tokio::spawn`.
