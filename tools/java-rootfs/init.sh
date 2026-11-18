#!/bin/sh
# Guest-side init: mount /proc, /sys, /dev, /tmp, bring lo up, launch
# the JVM in the background, then hand off to /warmup.sh (which does
# the readiness poll and the actual warmup hits).
#
# Runs as PID 1 inside the microVM. No busybox reaper: the JVM is the
# only non-init long-lived process, and `warmup.sh` (which this script
# `exec`s into after launching the JVM) uses `wait $JVM_PID` at the
# end so PID 1 exits when the JVM exits. That in turn triggers a
# kernel panic on the guest — which is what we want (the host sees
# the panic on stdio and knows the guest is unrecoverable, no zombie).

# ---- route all stdio through /dev/kmsg ------------------------------
# Why kmsg instead of /dev/console: PR #306 confirmed that the
# kernel-registered console_driver on this microVM boot path is only
# reachable via printk, not via the tty_driver's userspace-write
# method. `write()` to /dev/console (major 5 minor 1) and /dev/ttyS0
# (major 4 minor 64) both return rc=1, even after the mknod chain
# recreates them as valid char devices. Most likely CONFIG_VT=n in
# our kernel fragment — the VT console_driver is what normally
# backs /dev/console's write(), and without it the tty layer's
# write path fails.
#
# /dev/kmsg (major 1 minor 11) pipes userspace writes DIRECTLY into
# the kernel printk ring buffer, which flows to every registered
# console including ttyS0 regardless of tty_driver state. Plain
# `echo text` without a `<N>` prefix gets KERN_DEFAULT priority
# (shows on serial at the default console_loglevel of 7).
#
# busybox-static provides a static mknod at /bin/busybox.
BB=/bin/busybox
$BB mknod /dev/kmsg c 1 11 2>/dev/null
$BB chmod 622 /dev/kmsg 2>/dev/null

# Rebind init's stdin / stdout / stderr onto kmsg. All subsequent
# `echo`s (including NANOVM_PETCLINIC_COLD / NANOVM_PETCLINIC_READY
# from warmup.sh, which we `exec` into) land in the kernel log, which
# flows to the serial console the host is tailing.
#
# The host bench substring-matches on the marker text, so the
# `[  N.NNNNNN] ` timestamp prefix the kernel adds to each kmsg
# record doesn't break detection.
exec 1>/dev/kmsg 2>/dev/kmsg

echo "[init] kmsg bound; booting Petclinic microVM"

# Also rebuild /dev/null as a real char device so `2>/dev/null`
# further down actually discards writes rather than appending them
# to a regular file in /dev.
rm -f /dev/null 2>/dev/null
$BB mknod /dev/null c 1 3 2>/dev/null
$BB chmod 666 /dev/null 2>/dev/null

set -eu

# ---- basic pseudo-filesystems ----------------------------------------
# Each mount is guarded by an idempotency check so the same init works
# under three different launch contexts:
#   1. nanovm KVM guest (nothing mounted yet, we mount everything)
#   2. docker run --privileged (Docker mounted /proc already; skip it)
#   3. docker run WITHOUT --privileged (mount attempts fail; we cope by
#      only failing if a truly required mount is missing after we tried)
try_mount() {
    fstype="$1"
    target="$2"
    $BB mountpoint -q "$target" 2>/dev/null && return 0
    $BB mount -t "$fstype" none "$target" 2>/dev/null || {
        echo "[init] note: could not mount $fstype at $target"
    }
}
try_mount proc     /proc
try_mount sysfs    /sys
try_mount tmpfs    /tmp
try_mount tmpfs    /run
echo "[init] filesystems mounted"

# ---- loopback + smoltcp link ----------------------------------------
# `ip link set lo up` needs CAP_NET_ADMIN. eth0 is backed by the
# vm-kvm virtio-net device; the production demo deliberately uses the
# same static link-local address expected by the smoltcp host backend.
ip link set lo up 2>/dev/null || true
ip link set eth0 up
ip addr add 169.254.0.2/24 dev eth0 2>/dev/null || true
echo "[init] loopback and eth0 up (169.254.0.2/24)"

# Readiness is a separate, tiny HTTP server so the host can distinguish
# "Spring accepted a TCP connection" from "the full JIT warmup completed".
# warmup.sh creates the served file immediately before the ready marker.
/bin/busybox httpd -f -h /run -p 0.0.0.0:8081 &
HTTPD_PID=$!
echo "[init] readiness HTTP server pid=${HTTPD_PID}, port=8081"

# ---- JVM launch ------------------------------------------------------
JAVA_OPTS="\
    -Xmx1g \
    -Xshare:auto \
    -XX:+UseZGC \
    --add-opens java.base/java.lang=ALL-UNNAMED \
"

echo "[init] launching JVM: java $JAVA_OPTS -jar /opt/petclinic.jar"
echo "[init] jdk path exists: $($BB ls -la /opt/jdk-21/bin/java 2>&1 | head -1)"
echo "[init] jar path exists: $($BB ls -la /opt/petclinic.jar  2>&1 | head -1)"

/opt/jdk-21/bin/java $JAVA_OPTS -jar /opt/petclinic.jar \
    --server.port=8080 \
    > /tmp/petclinic.log 2>&1 &
JVM_PID=$!
echo "[init] JVM backgrounded, pid=$JVM_PID"

# Short pause so a mis-exec can surface before the warmup.sh `kill -0`
# check races the dying process.
$BB sleep 1
if ! $BB kill -0 "$JVM_PID" 2>/dev/null; then
    echo "[init] FATAL: JVM died within 1s. Petclinic log tail:"
    $BB cat /tmp/petclinic.log 2>&1 || echo "[init]   (no log)"
    exit 1
fi
echo "[init] JVM still alive after 1s — handing off to warmup.sh"

# ---- hand off to warmup driver ---------------------------------------
exec /warmup.sh "$JVM_PID"
