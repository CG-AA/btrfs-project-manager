#!/bin/bash
# Scratch btrfs filesystem on a loop device for end-to-end tests.
#   sudo scripts/loopfs.sh up     create /var/tmp/bpm-e2e.img, mount at /mnt/bpm-e2e, subvolume "space"
#   sudo scripts/loopfs.sh down   unmount and delete the image
set -euo pipefail
IMG=${BPM_E2E_IMG:-/var/tmp/bpm-e2e.img}
MNT=${BPM_E2E_MNT:-/mnt/bpm-e2e}
OWNER=${SUDO_UID:-1000}:${SUDO_GID:-1000}
case "${1:-}" in
  up)
    if mountpoint -q "$MNT"; then echo "already mounted at $MNT"; exit 0; fi
    truncate -s "${BPM_E2E_SIZE:-6G}" "$IMG"
    mkfs.btrfs -q -f "$IMG"
    mkdir -p "$MNT"
    mount -o loop,compress=zstd:1,noatime "$IMG" "$MNT"
    echo "mounted $IMG at $MNT (owner for test projects: $OWNER)"
    ;;
  down)
    if mountpoint -q "$MNT"; then umount "$MNT"; fi
    rm -f "$IMG"
    rmdir "$MNT" 2>/dev/null || true
    echo "removed"
    ;;
  *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
