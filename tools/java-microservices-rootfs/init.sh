#!/bin/sh
# Guest PID 1 for the microservices rootfs.
#
# Same shape as the M1 single-jar init:
#   - Idempotent pseudo-fs mounts (nanovm guest, Docker --privileged,
#     Docker without --privileged all work — see tools/java-rootfs/init.sh
#     for the rationale on the `try_mount` helper).
#   - Loopback up (tolerant of unprivileged).
#   - `exec` into `tier-launcher.sh`, which starts the seven JVMs in
#     the right order and then `wait`s so PID 1 exits when the stack
#     exits — guest kernel panics on that, host sees a clean failure.

set -eu

try_mount() {
    fstype="$1"
    target="$2"
    mountpoint -q "$target" 2>/dev/null && return 0
    mount -t "$fstype" none "$target" 2>/dev/null || {
        echo "[init] note: could not mount $fstype at $target (unprivileged?)" >&2
    }
}
try_mount proc     /proc
try_mount sysfs    /sys
try_mount devtmpfs /dev
try_mount tmpfs    /tmp
try_mount tmpfs    /run

ip link set lo up 2>/dev/null || true

echo "[init] handing off to tier-launcher" >&2
exec /tier-launcher.sh
