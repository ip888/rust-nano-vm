#!/usr/bin/env bash
# Build a minimal initramfs (newc cpio) for the guest-userspace boot
# test. Contains exactly:
#   /init           — the static binary from init.c (PID 1)
#   /dev            — empty dir
#   /dev/console    — char device node 5:1, so the kernel wires
#                     init's stdio to the serial console
#
# Output: tools/initramfs/cache/initramfs.cpio
#   symlinked path the test harness reads via NANOVM_TEST_INITRAMFS
#   or the workspace-relative default.
#
# Requirements: gcc (static libc), and the extracted kernel source
# tree from tools/kernel (we compile the kernel's own gen_init_cpio
# helper from it — it fakes the /dev/console node into the archive
# without needing root). Run tools/kernel/build-tiny-kernel.sh first
# so the source is present.
#
# Idempotent: rebuilds only when init.c is newer than the output.

set -euo pipefail

readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CACHE="${HERE}/cache"
readonly KERNEL_VERSION="6.12"
readonly KERNEL_SRC="${HERE}/../kernel/cache/linux-${KERNEL_VERSION}"
readonly GEN_INIT_CPIO_SRC="${KERNEL_SRC}/usr/gen_init_cpio.c"
readonly INIT_C="${HERE}/init.c"
readonly INIT_BIN="${CACHE}/init"
readonly GEN_INIT_CPIO="${CACHE}/gen_init_cpio"
readonly CPIO_LIST="${CACHE}/initramfs.list"
readonly OUT="${CACHE}/initramfs.cpio"

mkdir -p "${CACHE}"

# ---- preflight -------------------------------------------------------
command -v gcc >/dev/null || { echo "initramfs: need gcc" >&2; exit 1; }
if [[ ! -f "${GEN_INIT_CPIO_SRC}" ]]; then
  echo "initramfs: kernel source not found at ${KERNEL_SRC}" >&2
  echo "initramfs: run tools/kernel/build-tiny-kernel.sh first" >&2
  exit 1
fi

# ---- short-circuit ---------------------------------------------------
if [[ -f "${OUT}" && "${OUT}" -nt "${INIT_C}" ]]; then
  echo "initramfs: ${OUT} is up to date ('rm ${OUT}' forces a rebuild)"
  exit 0
fi

# ---- compile the static init ----------------------------------------
echo "initramfs: compiling init (static)"
gcc -static -Os -s -o "${INIT_BIN}" "${INIT_C}"

# ---- compile gen_init_cpio from the kernel source -------------------
# Self-contained single .c file; lets us declare a device node in the
# archive without root / mknod privileges.
echo "initramfs: compiling gen_init_cpio"
gcc -O2 -o "${GEN_INIT_CPIO}" "${GEN_INIT_CPIO_SRC}"

# ---- write the archive spec -----------------------------------------
# Format reference: usr/gen_init_cpio.c in the kernel tree.
cat > "${CPIO_LIST}" <<EOF
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
file /init ${INIT_BIN} 0755 0 0
EOF

# ---- generate the cpio ----------------------------------------------
echo "initramfs: generating ${OUT}"
"${GEN_INIT_CPIO}" "${CPIO_LIST}" > "${OUT}"

bytes="$(stat -c%s "${OUT}" 2>/dev/null || stat -f%z "${OUT}")"
echo
echo "initramfs: DONE"
echo "initramfs:   archive ${OUT} (${bytes} bytes)"
echo
echo "Run the boot test:"
echo "  cargo test -p vm-kvm --features kvm initramfs_boot -- --nocapture"
