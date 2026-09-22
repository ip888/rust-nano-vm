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

set -eu

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
try_mount proc     /proc
try_mount sysfs    /sys
try_mount devtmpfs /dev
try_mount tmpfs    /tmp
try_mount tmpfs    /run

# ---- loopback --------------------------------------------------------
# `ip link set lo up` needs CAP_NET_ADMIN. Under unprivileged Docker it
# fails; we tolerate that because Petclinic's warmup driver hits
# 127.0.0.1 which routes over lo — under Docker the kernel bring-up
# already happened at container start, so nothing to do.
ip link set lo up 2>/dev/null || true

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

echo "[init] launching JVM: java $JAVA_OPTS -jar /opt/petclinic.jar" >&2
/opt/jdk-21/bin/java $JAVA_OPTS -jar /opt/petclinic.jar \
    --server.port=8080 \
    > /tmp/petclinic.log 2>&1 &
JVM_PID=$!
echo "[init] JVM pid=$JVM_PID" >&2

# ---- hand off to warmup driver ---------------------------------------
exec /warmup.sh "$JVM_PID"
