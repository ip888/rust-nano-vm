#!/usr/bin/env bash
# Build a minimal initramfs (newc cpio) for the guest-boot tests.
#
# Two variants, selected by NANOVM_INIT (default: test):
#
#   NANOVM_INIT=test   → /init is the tiny C fixture from init.c.
#                        Output: cache/initramfs.cpio
#                        (used by tests/initramfs_boot.rs — proves
#                        guest userspace runs at all).
#
#   NANOVM_INIT=agent  → /init is the real Rust guest-agent, built
#                        as a static x86_64-unknown-linux-musl binary.
#                        Output: cache/initramfs-agent.cpio
#                        (used by tests/agent_init_boot.rs — proves
#                        the agent launches inside the guest).
#
# Both variants ship /dev + /dev/console (char 5:1) so the kernel
# wires init's stdio to the serial console (console=ttyS0).
#
# Requirements: gcc, the extracted kernel source from tools/kernel
# (for gen_init_cpio), and — for the agent variant — the
# x86_64-unknown-linux-musl Rust target
# (`rustup target add x86_64-unknown-linux-musl`).
#
# Run tools/kernel/build-tiny-kernel.sh first so the source is present.

set -euo pipefail

readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CACHE="${HERE}/cache"
readonly WORKSPACE="$(cd "${HERE}/../.." && pwd)"
readonly KERNEL_VERSION="6.12"
readonly KERNEL_SRC="${HERE}/../kernel/cache/linux-${KERNEL_VERSION}"
readonly GEN_INIT_CPIO_SRC="${KERNEL_SRC}/usr/gen_init_cpio.c"
readonly GEN_INIT_CPIO="${CACHE}/gen_init_cpio"
readonly MUSL_TARGET="x86_64-unknown-linux-musl"
readonly VARIANT="${NANOVM_INIT:-test}"

mkdir -p "${CACHE}"

# ---- preflight -------------------------------------------------------
command -v gcc >/dev/null || { echo "initramfs: need gcc" >&2; exit 1; }
if [[ ! -f "${GEN_INIT_CPIO_SRC}" ]]; then
  echo "initramfs: kernel source not found at ${KERNEL_SRC}" >&2
  echo "initramfs: run tools/kernel/build-tiny-kernel.sh first" >&2
  exit 1
fi

# ---- choose the /init binary per variant -----------------------------
case "${VARIANT}" in
  test)
    INIT_SRC="${HERE}/init.c"
    INIT_BIN="${CACHE}/init"
    OUT="${CACHE}/initramfs.cpio"
    # -no-pie is load-bearing: the tinyconfig kernel can't load a
    # static-PIE (ET_DYN) init — execve returns ENOENT. Distros with
    # a default-PIE toolchain (Arch, recent Ubuntu) produce static-PIE
    # from `-static` alone, so force ET_EXEC explicitly.
    echo "initramfs: variant=test — compiling init.c (static, non-PIE)"
    gcc -static -no-pie -Os -s -o "${INIT_BIN}" "${INIT_SRC}"
    ;;
  agent)
    OUT="${CACHE}/initramfs-agent.cpio"
    if ! rustup target list --installed 2>/dev/null | grep -q "${MUSL_TARGET}"; then
      echo "initramfs: missing Rust target ${MUSL_TARGET}" >&2
      echo "initramfs:   rustup target add ${MUSL_TARGET}" >&2
      exit 1
    fi
    # relocation-model=static forces a non-PIE (ET_EXEC) binary. The
    # musl target defaults to static-PIE (ET_DYN), which the
    # tinyconfig kernel can't exec as init (ENOENT). See the `test`
    # variant note above.
    echo "initramfs: variant=agent — building guest-agent (static musl, non-PIE)"
    ( cd "${WORKSPACE}" \
        && RUSTFLAGS="-C relocation-model=static" \
           cargo build -p guest-agent --target "${MUSL_TARGET}" --release )
    INIT_BIN="${WORKSPACE}/target/${MUSL_TARGET}/release/nanovm-agent"
    INIT_SRC="${INIT_BIN}"
    ;;
  *)
    echo "initramfs: unknown NANOVM_INIT='${VARIANT}' (want 'test' or 'agent')" >&2
    exit 1
    ;;
esac

# ---- short-circuit ---------------------------------------------------
if [[ -f "${OUT}" && "${OUT}" -nt "${INIT_SRC}" ]]; then
  echo "initramfs: ${OUT} is up to date ('rm ${OUT}' forces a rebuild)"
  exit 0
fi

# ---- compile gen_init_cpio from the kernel source -------------------
# Self-contained single .c file; lets us declare a device node in the
# archive without root / mknod privileges.
if [[ ! -x "${GEN_INIT_CPIO}" || "${GEN_INIT_CPIO_SRC}" -nt "${GEN_INIT_CPIO}" ]]; then
  echo "initramfs: compiling gen_init_cpio"
  gcc -O2 -o "${GEN_INIT_CPIO}" "${GEN_INIT_CPIO_SRC}"
fi

# ---- write the archive spec -----------------------------------------
# Format reference: usr/gen_init_cpio.c in the kernel tree.
# /dev/kmsg (1:11) is how early userspace logs reliably: writes go
# straight through printk to the serial console, even when init's
# stdio isn't wired to a tty. /dev/console (5:1) is kept for the
# kernel's own init-console wiring. /dev/null (1:3) and /dev/zero
# (1:5) are required the moment the agent spawns a child: Rust's
# `Command` opens /dev/null for any null stdio (e.g. `Stdio::null()`),
# and a missing node makes the spawn fail with ENOENT.
CPIO_LIST="${CACHE}/${VARIANT}.list"
cat > "${CPIO_LIST}" <<EOF
dir /dev 0755 0 0
nod /dev/console 0600 0 0 c 5 1
nod /dev/kmsg 0644 0 0 c 1 11
nod /dev/null 0666 0 0 c 1 3
nod /dev/zero 0666 0 0 c 1 5
file /init ${INIT_BIN} 0755 0 0
EOF

# ---- generate the cpio ----------------------------------------------
echo "initramfs: generating ${OUT}"
"${GEN_INIT_CPIO}" "${CPIO_LIST}" > "${OUT}"

bytes="$(stat -c%s "${OUT}" 2>/dev/null || stat -f%z "${OUT}")"
echo
echo "initramfs: DONE (variant=${VARIANT})"
echo "initramfs:   archive ${OUT} (${bytes} bytes)"
echo
case "${VARIANT}" in
  test)  echo "Run: cargo test -p vm-kvm --features kvm initramfs_boot -- --nocapture" ;;
  agent) echo "Run: cargo test -p vm-kvm --features kvm agent_init_boot -- --nocapture" ;;
esac
