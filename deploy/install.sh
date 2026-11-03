#!/usr/bin/env bash
# Runs on the Pi as root (scripts/deploy.sh invokes it via `sudo`), from the directory
# holding the staged files. An existing /etc/plant-panel/plantd.toml is never overwritten.
set -euo pipefail
cd "$(dirname "$0")"

install -m 755 plantd /usr/local/bin/plantd
install -m 644 plantd.service /etc/systemd/system/plantd.service
install -d /etc/plant-panel
[ -f /etc/plant-panel/plantd.toml ] || install -m 644 plantd.toml /etc/plant-panel/plantd.toml
/usr/local/bin/plantd --config /etc/plant-panel/plantd.toml check

systemctl daemon-reload
systemctl enable --quiet systemd-time-wait-sync.service
systemctl enable --quiet plantd
systemctl restart plantd
sleep 1
systemctl --no-pager --lines=5 status plantd
