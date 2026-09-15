#!/bin/bash
# Build as the current user, then run the real-btrfs end-to-end tests as root on a loop device.
set -euo pipefail
cd "$(dirname "$0")/.."
CARGO=${CARGO:-cargo}
MNT=${BPM_E2E_MNT:-/mnt/bpm-e2e}
sudo "$(pwd)/scripts/loopfs.sh" up
trap 'sudo "$(pwd)/scripts/loopfs.sh" down' EXIT
exe=$($CARGO test --test e2e --no-run --message-format=json 2>/dev/null \
  | python3 -c 'import sys,json
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("target",{}).get("name")=="e2e" and m.get("executable"): print(m["executable"])' | tail -1)
[ -n "$exe" ] || { echo "could not build e2e tests" >&2; exit 1; }
sudo env BPM_E2E_MNT="$MNT" BPM_E2E_UID="$(id -u)" BPM_E2E_GID="$(id -g)" "$exe" --ignored --test-threads=1 "$@"
