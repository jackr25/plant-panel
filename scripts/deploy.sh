#!/usr/bin/env bash
# Cross-builds plantd for the Pi and installs it as a systemd service.
#   scripts/deploy.sh pi@plantpi.local
# sudo on the Pi prompts for your password once (or not at all with NOPASSWD).
set -euo pipefail

[ $# -eq 1 ] || { echo "usage: scripts/deploy.sh user@host" >&2; exit 2; }
host=$1
cd "$(dirname "$0")/.."

cargo pi
bin=target/aarch64-unknown-linux-musl/release/plantd
stage=$(ssh "$host" mktemp -d)
scp -q "$bin" deploy/plantd.service deploy/plantd.toml deploy/install.sh "$host:$stage/"

# -t gives sudo a terminal to prompt on.
ssh -t "$host" "sudo bash $stage/install.sh; status=\$?; rm -rf $stage; exit \$status"
