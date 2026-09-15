#!/bin/bash
# Build a release binary and install it to /usr/local/bin/bpm.
set -euo pipefail
cd "$(dirname "$0")/.."
CARGO=${CARGO:-cargo}
$CARGO build --release
sudo install -m 0755 target/release/bpm /usr/local/bin/bpm
/usr/local/bin/bpm --version
echo "next: sudo bpm setup   (installs /etc/bpm/config.toml, the store subvolume and the systemd timer)"
