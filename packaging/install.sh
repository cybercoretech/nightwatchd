#!/usr/bin/env bash
# Build and install nightwatchd as a user service, published on the tailnet
# with `tailscale serve`. No root needed. Re-run to upgrade.
set -euo pipefail
cd "$(dirname "$0")/.."
PORT=8769

cargo build --release --locked
BIN="${CARGO_TARGET_DIR:-target}/release/nightwatchd"
install -Dm755 "$BIN" "$HOME/.local/bin/nightwatchd"
# The unit's ReadWritePaths= must exist before it starts.
install -dm700 "$HOME/.config/nightwatch" "$HOME/.local/state/nightwatch"
install -Dm644 packaging/nightwatchd.service "$HOME/.config/systemd/user/nightwatchd.service"
systemctl --user daemon-reload
systemctl --user enable nightwatchd.service >/dev/null
systemctl --user restart nightwatchd.service

if command -v tailscale >/dev/null; then
  tailscale serve --bg --http="$PORT" "http://127.0.0.1:$PORT" >/dev/null
  ts_name="$(tailscale status --json 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null || true)"
  echo "nightwatchd is on the tailnet at http://${ts_name:-<this host>}:$PORT"
else
  echo "tailscale not found: nightwatchd is only on http://127.0.0.1:$PORT"
fi
echo
"$HOME/.local/bin/nightwatchd" check || true
echo
echo "Next: nightwatchd token add \"My phone\" --operate   (then enter it in Nightwatch)"
