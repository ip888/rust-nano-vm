#!/usr/bin/env bash
# Build a minimal initramfs (newc cpio) for the guest-boot tests.
#
# Three variants, selected by NANOVM_INIT (default: test):
#
#   NANOVM_INIT=test      → /init is the tiny C fixture from init.c.
#                           Output: cache/initramfs.cpio
#                           (used by tests/initramfs_boot.rs — proves
#                           guest userspace runs at all).
#
#   NANOVM_INIT=agent     → /init is the real Rust guest-agent, built
#                           as a static x86_64-unknown-linux-musl
#                           binary. Output: cache/initramfs-agent.cpio
#                           (used by tests/agent_init_boot.rs — proves
#                           the agent launches inside the guest).
#
#   NANOVM_INIT=net-echo  → /init is a shell script that brings eth0
#                           up on the smoltcp link-local address
#                           (169.254.0.2/24), then starts a TCP echo
#                           listener on :8080 via `busybox nc`.
#                           Uses busybox-static from the host (install
#                           via `apt install busybox-static`, or point
#                           NANOVM_BUSYBOX_STATIC at a local copy).
#                           Output: cache/initramfs-net-echo.cpio
#                           (used by tests/virtio_net_smoltcp_data_path_boot.rs
#                           — proves the Host→Guest TCP path via
#                           TcpProxy + SmoltcpBackend + virtio-net
#                           ring works end to end).
#
# All variants ship /dev + /dev/console (char 5:1) so the kernel
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
  net-echo)
    OUT="${CACHE}/initramfs-net-echo.cpio"
    # Locate the host's busybox-static binary. We don't exec it
    # directly via `#!/bin/busybox sh` shebang — that path was
    # observed to exit init 1 within 2 ms with zero output on
    # Omarchy 6.12 even though the same binary + script run fine
    # on the host. Instead the `/init` ELF wrapper (compiled from
    # init-net-echo.c) execve's busybox directly, same code path
    # as the agent variant's static-musl Rust binary.
    BUSYBOX="${NANOVM_BUSYBOX_STATIC:-}"
    if [[ -z "${BUSYBOX}" ]]; then
      for cand in /bin/busybox /usr/bin/busybox /usr/local/bin/busybox; do
        if [[ -x "${cand}" ]]; then
          BUSYBOX="${cand}"
          break
        fi
      done
    fi
    if [[ -z "${BUSYBOX}" || ! -x "${BUSYBOX}" ]]; then
      echo "initramfs: variant=net-echo needs busybox-static" >&2
      echo "initramfs:   Debian/Ubuntu: apt install busybox-static" >&2
      echo "initramfs:   Or set NANOVM_BUSYBOX_STATIC=/path/to/busybox" >&2
      exit 1
    fi
    # Reject a dynamically-linked busybox: the tinyconfig guest has
    # no /lib/ld-musl or /lib/x86_64-linux-gnu, so a dynamic binary
    # faceplants at exec with ENOENT on the interpreter.
    if ! file "${BUSYBOX}" | grep -q 'statically linked'; then
      echo "initramfs: ${BUSYBOX} is not statically linked" >&2
      echo "initramfs: the guest has no ld.so; install busybox-static (not busybox)" >&2
      exit 1
    fi
    echo "initramfs: variant=net-echo — using busybox at ${BUSYBOX}"

    # The /init itself is an ELF wrapper (init-net-echo.c). It
    # prints a wrapper-alive marker then execve's /bin/busybox
    # directly. Same code path as the agent variant's ELF /init —
    # avoids the kernel's binfmt_script shebang recursion that
    # observably breaks on this stack (zero output, exit 1 in 2 ms).
    INIT_WRAPPER_SRC="${HERE}/init-net-echo.c"
    INIT_WRAPPER_BIN="${CACHE}/init-net-echo"
    if [[ ! -f "${INIT_WRAPPER_SRC}" ]]; then
      echo "initramfs: ${INIT_WRAPPER_SRC} missing" >&2
      exit 1
    fi
    echo "initramfs: compiling init-net-echo wrapper (static, non-PIE)"
    gcc -static -no-pie -Os -s -o "${INIT_WRAPPER_BIN}" "${INIT_WRAPPER_SRC}"
    INIT_BIN="${INIT_WRAPPER_BIN}"
    # Short-circuit uses INIT_SRC newer-than check. Point it at
    # the C source so editing init-net-echo.c forces a cpio rebuild.
    INIT_SRC="${INIT_WRAPPER_SRC}"

    # Companion shell script that the wrapper execve's. Diagnostic-
    # rich: every subcommand has a preceding `echo [stage N]` so a
    # failure between stages localises to the last visible marker
    # on serial.
    INIT_SCRIPT_SRC="${CACHE}/net-echo-init.sh"
    cat > "${INIT_SCRIPT_SRC}" <<'INITSCRIPT'
# Guest-side /init.sh for the virtio-net smoltcp data-path test.
#
# Launched by /init (the ELF wrapper from init-net-echo.c):
#     execve("/bin/busybox", ["sh", "/init.sh"], NULL)
#
# DIAGNOSTIC MODE: staged marker chain around the real work. If
# any command hangs or crashes, the last visible `[stage N]` on
# serial names it.

echo "[stage 1] busybox sh applet alive, running /init.sh"
BB=/bin/busybox

echo "[stage 2] /bin/busybox: $($BB ls -la /bin/busybox 2>&1 | head -1)"
echo "[stage 2b] /dev contents: $($BB ls /dev 2>&1)"

echo "[stage 3] mount proc:"
$BB mount -t proc  proc  /proc  2>&1 || echo "  (mount proc failed, ignoring)"
echo "[stage 4] mount sys:"
$BB mount -t sysfs sysfs /sys   2>&1 || echo "  (mount sys failed, ignoring)"

echo "[stage 5] ip link show (pre-up):"
$BB ip link show 2>&1 || echo "  (ip link show failed)"

echo "[stage 6] ip link set lo up:"
$BB ip link set lo up 2>&1 || echo "  (lo up failed, ignoring)"
echo "[stage 7] ip link set eth0 up:"
$BB ip link set eth0 up 2>&1 || echo "  (eth0 up failed, ignoring)"
echo "[stage 8] ip addr add 169.254.0.2/24 dev eth0:"
$BB ip addr add 169.254.0.2/24 dev eth0 2>&1 || echo "  (addr add failed, ignoring)"

echo "[stage 9] ip addr show (post-up):"
$BB ip addr show 2>&1 || echo "  (ip addr show failed)"

# Readiness marker — the host-side test polls serial for this
# substring before attaching TcpProxy.
echo "NANOVM_NET_ECHO_READY"

echo "[stage 10] busybox nc --help (brief):"
$BB nc --help 2>&1 | head -5 || echo "  (nc --help failed)"

echo "[stage 11] exec nc listener:"
exec $BB nc -lk -p 8080 -e $BB cat
INITSCRIPT
    chmod +x "${INIT_SCRIPT_SRC}"
    ;;
  *)
    echo "initramfs: unknown NANOVM_INIT='${VARIANT}'" >&2
    echo "initramfs:   want one of: test, agent, net-echo" >&2
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

# net-echo needs:
#   - /bin/busybox (the static binary; the ELF wrapper at /init
#     execve's it directly)
#   - /init.sh (the shell script the wrapper hands to busybox sh)
#   - /bin, /proc, /sys directories
# Added AFTER the base entries so a test-variant archive stays
# byte-identical to before.
if [[ "${VARIANT}" == "net-echo" ]]; then
  cat >> "${CPIO_LIST}" <<EOF
dir /bin 0755 0 0
dir /proc 0755 0 0
dir /sys 0755 0 0
file /bin/busybox ${BUSYBOX} 0755 0 0
file /init.sh ${INIT_SCRIPT_SRC} 0755 0 0
EOF
fi

# ---- generate the cpio ----------------------------------------------
echo "initramfs: generating ${OUT}"
"${GEN_INIT_CPIO}" "${CPIO_LIST}" > "${OUT}"

bytes="$(stat -c%s "${OUT}" 2>/dev/null || stat -f%z "${OUT}")"
echo
echo "initramfs: DONE (variant=${VARIANT})"
echo "initramfs:   archive ${OUT} (${bytes} bytes)"
echo
case "${VARIANT}" in
  test)     echo "Run: cargo test -p vm-kvm --features kvm initramfs_boot -- --nocapture" ;;
  agent)    echo "Run: cargo test -p vm-kvm --features kvm agent_init_boot -- --nocapture" ;;
  net-echo) echo "Run: cargo test -p vm-kvm --features kvm,virtio-net-smoltcp virtio_net_smoltcp_data_path_boot -- --nocapture" ;;
esac
