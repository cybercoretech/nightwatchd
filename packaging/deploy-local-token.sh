#!/usr/bin/env bash
# One-time rollout of local-token/v1 on a Cybercore host: builds DaemonHall,
# Dockspace and Cyberdeck-hub from their feat/local-token checkouts,
# installs them where their units expect them (old binaries kept as
# *.bak-<date>), adds the RuntimeDirectory= drop-ins for the two system
# units and restarts all three. Asks for sudo for the system parts.
#
# CYBERCORE=~/Devspace/Cybercore by default.
set -euo pipefail
C="${CYBERCORE:-$HOME/Devspace/Cybercore}"
HERE="$(cd "$(dirname "$0")" && pwd)"
T="${CARGO_TARGET_DIR:-}"
stamp="$(date +%Y%m%d-%H%M)"
bin() { echo "${T:-$1/target}/release/$2"; }

for repo in core/DaemonHall tooling/dockspace core/cyberdeck-hub; do
  branch="$(git -C "$C/$repo" branch --show-current)"
  echo "== $repo ($branch)"
  grep -q 'local-token/v1' "$C/$repo/src/localtoken.rs" 2>/dev/null \
    || { echo "   $repo has no local-token/v1 support on this branch; stopping"; exit 1; }
done

# DaemonHall: user service, ~/.local/bin. Its cyberwatch git dependency
# isn't on github.com/cybercoretech yet (org migration), so cargo fetches
# that exact revision from the local checkout instead.
(cd "$C/core/DaemonHall" \
  && CARGO_NET_GIT_FETCH_WITH_CLI=true GIT_CONFIG_COUNT=1 \
     GIT_CONFIG_KEY_0="url.file://$C/integrations/cyberwatch.insteadOf" \
     GIT_CONFIG_VALUE_0="https://github.com/cybercoretech/cyberwatch" \
     cargo build --release --locked)
cp -a "$HOME/.local/bin/daemonhall" "$HOME/.local/bin/daemonhall.bak-$stamp" 2>/dev/null || true
install -m755 "$(bin "$C/core/DaemonHall" daemonhall)" "$HOME/.local/bin/daemonhall"

# Cyberdeck-hub: ~/.local/bin/cyberdeck_core. core/cyberdeck builds a binary
# with the same name into a shared target dir, so copy right after building.
(cd "$C/core/cyberdeck-hub" && cargo build --release --locked)
cp -a "$HOME/.local/bin/cyberdeck_core" "$HOME/.local/bin/cyberdeck_core.bak-$stamp" 2>/dev/null || true
install -m755 "$(bin "$C/core/cyberdeck-hub" cyberdeck_core)" "$HOME/.local/bin/cyberdeck_core"

# Dockspace: root-owned /usr/local/bin (it holds docker access).
(cd "$C/tooling/dockspace" && cargo build --release --locked)
sudo cp -a /usr/local/bin/dockspace "/usr/local/bin/dockspace.bak-$stamp" 2>/dev/null || true
sudo install -o root -g root -m755 "$(bin "$C/tooling/dockspace" dockspace)" /usr/local/bin/dockspace

sudo install -Dm644 "$HERE/dropins/dockspace-local-token.conf" /etc/systemd/system/dockspace.service.d/40-local-token.conf
sudo install -Dm644 "$HERE/dropins/cyberdeck-local-token.conf" /etc/systemd/system/cyberdeck.service.d/40-local-token.conf
sudo systemctl daemon-reload
sudo systemctl restart dockspace.service cyberdeck.service
systemctl --user restart daemonhall.service
sleep 2

echo
for f in "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/daemonhall/local-token" /run/dockspace/local-token /run/cyberdeck/local-token; do
  if [[ -r "$f" ]]; then echo "ok       $f"; else echo "MISSING  $f"; fi
done
