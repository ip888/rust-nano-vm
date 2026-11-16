#!/bin/sh
# Minimal diagnostic probe — ROUND 2.
#
# ROUND 1 (`2ee1564`, PR #305) confirmed:
#   - busybox-static landed in the image (`/bin/busybox` resolves)
#   - `busybox mknod /dev/console c 5 1` SUCCEEDS at the syscall
#   - `exec 1>/dev/console 2>/dev/console` SUCCEEDS
#   - Script runs fully to `exit 99` (panic exitcode=0x00006300)
#
# BUT: zero markers on serial despite exit 99. Conclusion: /dev/console
# writes don't reach the UART even though the char device is valid.
# This is a kernel-console routing issue, not a mknod problem.
#
# Round 2 hypothesis: /dev/kmsg (major 1, minor 11) pipes directly
# into the kernel's printk buffer, which is guaranteed to flow to
# ALL registered consoles (including the ttyS0 the kernel's own
# printk's successfully use). So writing via /dev/kmsg should be
# bulletproof visible on serial.
#
# Also dumps /proc/consoles so we see which consoles the kernel
# thinks are registered — this clinches the diagnosis.

BB=/bin/busybox

# ---- stage 1: mknod /dev/kmsg ---------------------------------------
# No `rm` first — /dev/kmsg isn't in the stripped docker-export tar
# (we didn't `mknod` it in the Dockerfile stage 8/9), so the path
# is free.
$BB mknod /dev/kmsg c 1 11 2>/dev/null
mrc=$?
if [ "$mrc" -ne 0 ]; then
    exit $((20 + mrc))
fi
$BB chmod 622 /dev/kmsg 2>/dev/null

# ---- stage 2: write to /dev/kmsg (should appear on serial) ----------
# Format: `<N>message` where N is the printk level (4 = KERN_WARNING).
# Without `<N>`, the kernel uses the default log level.
echo "<4>[kmsg-diag 1] init.sh: $BB mknod /dev/kmsg SUCCEEDED" > /dev/kmsg

# ---- stage 3: mount /proc so we can read /proc/consoles --------------
$BB mkdir -p /proc 2>/dev/null
$BB mount -t proc none /proc 2>/dev/null
mp_rc=$?
echo "<4>[kmsg-diag 2] mount /proc rc=$mp_rc" > /dev/kmsg

# ---- stage 4: dump /proc/consoles via kmsg ---------------------------
# /proc/consoles lists each registered console with flags like
# `EC`, `tty_driver, tty_dev` — the `E` means enabled.
if [ -r /proc/consoles ]; then
    while IFS= read -r line; do
        echo "<4>[kmsg-diag 3] /proc/consoles: $line" > /dev/kmsg
    done < /proc/consoles
else
    echo "<4>[kmsg-diag 3] /proc/consoles NOT readable" > /dev/kmsg
fi

# ---- stage 5: test write to /dev/console (the failing path) ----------
# Rebuild it as before, then write a marker. We expect this write
# to SUCCEED (rc=0) but NOT appear on serial — confirming that the
# char device is valid but its output goes somewhere other than
# the UART.
rm -f /dev/console 2>/dev/null
$BB mknod /dev/console c 5 1 2>/dev/null
$BB chmod 622 /dev/console 2>/dev/null
echo "[console-diag] If you see THIS on serial, /dev/console works" > /dev/console 2>/dev/null
cons_rc=$?
echo "<4>[kmsg-diag 4] write to /dev/console returned rc=$cons_rc" > /dev/kmsg

# ---- stage 6: also try /dev/ttyS0 directly ---------------------------
$BB mknod /dev/ttyS0 c 4 64 2>/dev/null
$BB chmod 622 /dev/ttyS0 2>/dev/null
echo "[ttyS0-diag] If you see THIS on serial, /dev/ttyS0 works" > /dev/ttyS0 2>/dev/null
ttys_rc=$?
echo "<4>[kmsg-diag 5] write to /dev/ttyS0 returned rc=$ttys_rc" > /dev/kmsg

# ---- stage 7: dump /dev inventory via kmsg ---------------------------
echo "<4>[kmsg-diag 6] /dev inventory follows:" > /dev/kmsg
$BB ls -la /dev 2>&1 | while IFS= read -r line; do
    echo "<4>[kmsg-diag 6]   $line" > /dev/kmsg
done

# ---- done — hold for flush + exit ------------------------------------
$BB sleep 3
exit 99
