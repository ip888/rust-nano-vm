#!/bin/sh
# MINIMAL diagnostic init — encodes progress in exit code.
# Goal: localise *why* 3 previous PRs got zero markers on serial.
#
# The kernel always reports PID 1's exit code in its panic
# message (`exitcode=0xNNNN`, where the exit status is bits 8–15),
# independent of whether userspace serial actually works. So we
# bake our diagnostic into the exit code: whichever one appears
# tells us how far the script got and which step failed.
#
# Scheme:
#   exit 10  — neither /bin/busybox nor /usr/bin/busybox is executable
#   exit 20+mrc — found busybox but `mknod /dev/console` failed
#                  with return code `mrc` (so exit 21 = rc 1,
#                  exit 22 = rc 2, etc.)
#   exit 40+rc  — exec redirect onto /dev/console failed with rc
#                  (bizarre case — would mean the newly-mknod'd
#                  char device isn't openable)
#   exit 99     — everything worked, slept 2s, exiting cleanly
#
# A marker appearing on serial (via the `echo` after exec redirect)
# is the SECOND, orthogonal confirmation.
#
# This replaces the real init.sh for one diag round only. Once we
# know which stage fails, the next PR restores the proper init
# flow with the correct mknod approach.

BB=""
for p in /bin/busybox /usr/bin/busybox; do
    if [ -x "$p" ]; then
        BB="$p"
        break
    fi
done

if [ -z "$BB" ]; then
    exit 10
fi

# Remove shadowed regular file at /dev/console before mknod
# (otherwise mknod returns EEXIST).
rm -f /dev/console 2>/dev/null
$BB mknod /dev/console c 5 1 2>/dev/null
mrc=$?
if [ "$mrc" -ne 0 ]; then
    exit $((20 + mrc))
fi

# Also rebuild /dev/null so subsequent 2>/dev/null works properly
# (otherwise stderr goes to a regular file in / named "dev/null").
$BB mknod /dev/null c 1 3 2>/dev/null
$BB chmod 622 /dev/console 2>/dev/null
$BB chmod 666 /dev/null    2>/dev/null

# Reopen stdio onto the restored char device. If this fails for
# any weird reason (shouldn't — opening a char device for write
# is always allowed for root), the exit path captures that.
exec 1>/dev/console 2>/dev/console
erc=$?
if [ "$erc" -ne 0 ]; then
    exit $((40 + erc))
fi

# This echo lands on the real UART if everything worked. If
# markers still don't appear despite exit code 99, something is
# fundamentally wrong with the UART write path (very unlikely).
echo "===================================="
echo "[init-diag MINIMAL]: busybox mknod of /dev/console SUCCEEDED"
echo "[init-diag MINIMAL]: using BB=$BB"
echo "[init-diag MINIMAL]: ls -la /dev:"
$BB ls -la /dev 2>&1
echo "===================================="

# Give the UART a moment to flush, then exit with distinctive code
# so the kernel panic exitcode confirms we got here.
$BB sleep 2
exit 99
