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
mount -t proc     none /proc
mount -t sysfs    none /sys
mount -t devtmpfs none /dev  || true   # optional; kernel may auto-mount
mount -t tmpfs    none /tmp
mount -t tmpfs    none /run

# ---- loopback --------------------------------------------------------
ip link set lo up

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
