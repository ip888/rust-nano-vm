#!/bin/sh
# Guest-side init: mount /proc, /sys, /dev, /tmp, bring lo up, launch
# the JVM in the background, then hand off to /warmup.sh (which does
# the readiness poll and the actual warmup hits).
#
# Runs as PID 1 inside the microVM (installed at both /sbin/init and,
# via symlink, /init so either kernel-cmdline convention works). No
# busybox reaper: the JVM is the only non-init long-lived process, and
# `warmup.sh` (which this script `exec`s into after launching the JVM)
# uses `wait $JVM_PID` at the end so PID 1 exits when the JVM exits.
# That in turn triggers a kernel panic on the guest — which is what we
# want (the host sees the panic on stdio and knows the guest is
# unrecoverable, no zombie).

# ---- escape-hatch tty via explicit mknod -----------------------------
# `docker export` strips character-device TYPE from `mknod`-created
# nodes, so `/dev/console` in the cpio is a zero-byte regular file.
# The previous round (`cc00dc0`) tried `mount -t devtmpfs none /dev`
# + `exec 1>/dev/console 2>/dev/console` — the panic exit code
# changed from 7 to 1, proving the new code runs, but serial output
# stayed empty. Either the devtmpfs mount silently failed, or the
# fd reopen is landing on the same shadowed file.
#
# This stage bypasses /dev entirely. `mknod /ttyS0_diag c 4 64` is a
# fresh path, so docker-export's stripping doesn't apply; major 4
# minor 64 is ttyS0 (the UART the kernel already uses for console
# output). PID 1 is root with CAP_MKNOD, so the syscall works.
#
# We stage markers so a silent failure anywhere gives a different
# "last marker seen" to localize. This is TEMPORARY diagnostic; once
# we know where it dies, it gets replaced with a clean mknod of
# /dev/console itself + normal flow.
mknod /ttyS0_diag c 4 64 2>/dev/null
echo "[init-diag 1] entered init.sh" > /ttyS0_diag 2>/dev/null

mount_rc=0
mount -t devtmpfs none /dev 2>/dev/null || mount_rc=$?
echo "[init-diag 2] devtmpfs mount rc=$mount_rc" > /ttyS0_diag 2>/dev/null

{ echo "[init-diag 3] /dev inventory:"; ls -la /dev/console /dev/ttyS0 2>&1; } \
    > /ttyS0_diag 2>/dev/null

# Rebind init's stdout/stderr onto the escape-hatch tty so any
# subsequent `set -eu` failure surfaces on serial instead of
# vanishing into the shadowed console file.
exec 1>/ttyS0_diag 2>/ttyS0_diag
echo "[init-diag 4] fds rebound to /ttyS0_diag"

set -eu
echo "[init-diag 5] set -eu active, continuing"

# ---- basic pseudo-filesystems ----------------------------------------
# Each mount is guarded by an idempotency check so the same init works
# under three different launch contexts:
#   1. nanovm KVM guest (nothing mounted yet, we mount everything)
#   2. docker run --privileged (Docker mounted /proc already; skip it)
#   3. docker run WITHOUT --privileged (mount attempts fail; we cope by
#      only failing if a truly required mount is missing after we tried)
#
# `mountpoint -q PATH` returns 0 when PATH is already a mount, non-zero
# otherwise. `2>/dev/null || true` on the mount itself keeps
# unprivileged Docker from taking down PID 1 before the JVM launches;
# the guest bootcase panics anyway if /proc really isn't mounted (curl
# in warmup.sh needs it for network resolution), so a silent no-op
# here just changes where the failure surfaces.
try_mount() {
    fstype="$1"
    target="$2"
    mountpoint -q "$target" 2>/dev/null && return 0
    mount -t "$fstype" none "$target" 2>/dev/null || {
        echo "[init] note: could not mount $fstype at $target (unprivileged?)" >&2
    }
}
try_mount proc     /proc; echo "[init-diag 6a] try_mount proc OK"
try_mount sysfs    /sys;  echo "[init-diag 6b] try_mount sysfs OK"
try_mount devtmpfs /dev;  echo "[init-diag 6c] try_mount devtmpfs OK"
try_mount tmpfs    /tmp;  echo "[init-diag 6d] try_mount tmpfs /tmp OK"
try_mount tmpfs    /run;  echo "[init-diag 6e] try_mount tmpfs /run OK"

# ---- loopback --------------------------------------------------------
# `ip link set lo up` needs CAP_NET_ADMIN. Under unprivileged Docker it
# fails; we tolerate that because Petclinic's warmup driver hits
# 127.0.0.1 which routes over lo — under Docker the kernel bring-up
# already happened at container start, so nothing to do.
echo "[init-diag 7] before: ip link set lo up"
ip link set lo up 2>/dev/null || true
echo "[init-diag 8] after: ip link set lo up"

# ---- JVM launch ------------------------------------------------------
#
# TieredStopAtLevel=1 is deliberately OMITTED: we want the full JIT
# tier progression so the warm-snapshot captures C2-compiled hot code.
# The cold-snapshot variant (host takes it before the warmup script
# signals) sees C1 only — that's the point of comparing the two.
#
# -Xshare:auto lets the JVM pick up the Class Data Sharing archive
# baked at image-build time (see Dockerfile), which shaves ~500 ms off
# initial class loading.
#
# --add-opens is needed for Spring Boot 3.x against JDK 21 (it opens
# java.lang for reflection into records / patterns).
JAVA_OPTS="\
    -Xmx1g \
    -Xshare:auto \
    -XX:+UseZGC \
    --add-opens java.base/java.lang=ALL-UNNAMED \
"

echo "[init-diag 9] JAVA_OPTS set"
echo "[init] launching JVM: java $JAVA_OPTS -jar /opt/petclinic.jar" >&2
/opt/jdk-21/bin/java $JAVA_OPTS -jar /opt/petclinic.jar \
    --server.port=8080 \
    > /tmp/petclinic.log 2>&1 &
JVM_PID=$!
echo "[init-diag 10] JVM backgrounded, pid=$JVM_PID"
echo "[init] JVM pid=$JVM_PID" >&2

# ---- hand off to warmup driver ---------------------------------------
echo "[init-diag 11] exec'ing /warmup.sh"
exec /warmup.sh "$JVM_PID"
