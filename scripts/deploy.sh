#!/usr/bin/env bash
# Cross-builds plantd for the Pi and installs it as a systemd service.
#   scripts/deploy.sh pi@plantpi.local
# An existing /etc/plant-panel/plantd.toml on the Pi is never overwritten.
set -euo pipefail

host=${1:?usage: scripts/deploy.sh user@host}
cd "$(dirname "$0")/.."

cargo pi
bin=target/aarch64-unknown-linux-musl/release/plantd
stage=$(ssh "$host" mktemp -d)
scp -q "$bin" deploy/plantd.service deploy/plantd.toml "$host:$stage/"

ssh "$host" "stage=$stage bash -s" <<'EOF'
set -euo pipefail
sudo install -m 755 "$stage/plantd" /usr/local/bin/plantd
sudo install -m 644 "$stage/plantd.service" /etc/systemd/system/plantd.service
sudo install -d /etc/plant-panel
[ -f /etc/plant-panel/plantd.toml ] || sudo install -m 644 "$stage/plantd.toml" /etc/plant-panel/plantd.toml
rm -rf "$stage"
/usr/local/bin/plantd --config /etc/plant-panel/plantd.toml check
sudo systemctl daemon-reload
sudo systemctl enable --quiet systemd-time-wait-sync.service
sudo systemctl enable --quiet plantd
sudo systemctl restart plantd
sleep 1
systemctl --no-pager --lines=5 status plantd
EOF
