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

# ---- rebuild /dev/console directly via busybox-static ----------------
# `docker export` strips character-device TYPE from `mknod`-created
# nodes, so the `/dev/console` the kernel opened for PID 1's stdio
# is a zero-byte regular file. PR #303 tried coreutils' mknod to
# rebuild it — ZERO markers reached serial, so either `mknod` wasn't
# in debian:12-slim, or PATH resolution in the kernel-execv'd dash
# couldn't find it. This round uses `busybox-static` (installed via
# the Dockerfile) at its absolute path `/bin/busybox` — bulletproof.
#
# `rm -f` first because the existing shadowed regular file prevents
# `mknod` from succeeding at the same path with EEXIST.
BB=/bin/busybox
rm -f /dev/console 2>/dev/null
$BB mknod /dev/console c 5 1 2>/dev/null
$BB chmod 622 /dev/console 2>/dev/null

# Reopen stdout/stderr on the now-restored char device. The fds the
# kernel inherited for us still point at the old regular file; this
# replaces them with handles on the real char device.
exec 1>/dev/console 2>/dev/console
echo "[init-diag 1] stdio rebound via busybox mknod of /dev/console"

# Also rebuild /dev/null et al so `2>/dev/null` further down works.
rm -f /dev/null /dev/zero /dev/random /dev/urandom /dev/tty /dev/ttyS0 /dev/kmsg 2>/dev/null
$BB mknod /dev/null    c 1 3  2>/dev/null; $BB chmod 666 /dev/null
$BB mknod /dev/zero    c 1 5  2>/dev/null; $BB chmod 666 /dev/zero
$BB mknod /dev/random  c 1 8  2>/dev/null; $BB chmod 666 /dev/random
$BB mknod /dev/urandom c 1 9  2>/dev/null; $BB chmod 666 /dev/urandom
$BB mknod /dev/tty     c 5 0  2>/dev/null; $BB chmod 666 /dev/tty
$BB mknod /dev/ttyS0   c 4 64 2>/dev/null; $BB chmod 622 /dev/ttyS0
$BB mknod /dev/kmsg    c 1 11 2>/dev/null; $BB chmod 644 /dev/kmsg
echo "[init-diag 2] /dev inventory after busybox mknod:"
$BB ls -la /dev/console /dev/ttyS0 /dev/null /dev/kmsg /bin/busybox 2>&1 || true

echo "[init-diag 2a] confirming busybox and coreutils mknod binaries:"
$BB ls -la /bin/busybox /bin/mknod /usr/bin/mknod 2>&1 || true

set -eu
echo "[init-diag 3] set -eu active, continuing"

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
try_mount proc     /proc; echo "[init-diag 4a] try_mount proc OK"
try_mount sysfs    /sys;  echo "[init-diag 4b] try_mount sysfs OK"
try_mount devtmpfs /dev;  echo "[init-diag 4c] try_mount devtmpfs OK"
try_mount tmpfs    /tmp;  echo "[init-diag 4d] try_mount tmpfs /tmp OK"
try_mount tmpfs    /run;  echo "[init-diag 4e] try_mount tmpfs /run OK"

# ---- loopback --------------------------------------------------------
# `ip link set lo up` needs CAP_NET_ADMIN. Under unprivileged Docker it
# fails; we tolerate that because Petclinic's warmup driver hits
# 127.0.0.1 which routes over lo — under Docker the kernel bring-up
# already happened at container start, so nothing to do.
echo "[init-diag 5] before: ip link set lo up"
ip link set lo up 2>/dev/null || true
echo "[init-diag 6] after: ip link set lo up"

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

echo "[init-diag 7] JAVA_OPTS set"
echo "[init-diag 7a] checking /opt/jdk-21/bin/java exists:"
ls -la /opt/jdk-21/bin/java 2>&1 || true
echo "[init-diag 7b] checking /opt/petclinic.jar exists:"
ls -la /opt/petclinic.jar 2>&1 || true

echo "[init] launching JVM: java $JAVA_OPTS -jar /opt/petclinic.jar" >&2
/opt/jdk-21/bin/java $JAVA_OPTS -jar /opt/petclinic.jar \
    --server.port=8080 \
    > /tmp/petclinic.log 2>&1 &
JVM_PID=$!
echo "[init-diag 8] JVM backgrounded, pid=$JVM_PID"
echo "[init] JVM pid=$JVM_PID" >&2

# Give the JVM a moment to produce any launch error, then dump the log
# so we see what happened before warmup.sh's `kill -0` check fires
# `exit 1`.
sleep 2
echo "[init-diag 8a] /tmp/petclinic.log tail after 2s:"
tail -30 /tmp/petclinic.log 2>&1 || true
echo "[init-diag 8b] is JVM alive?"
kill -0 "$JVM_PID" 2>&1 && echo "YES pid=$JVM_PID" || echo "NO, dead"

# ---- hand off to warmup driver ---------------------------------------
echo "[init-diag 9] exec'ing /warmup.sh"
exec /warmup.sh "$JVM_PID"
