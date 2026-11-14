#!/usr/bin/env bash
# Build a Linux bzImage for vm-kvm's guest. Two profiles today:
#
#   - "tiny" (default) — minimal kernel for vm-kvm integration tests.
#     Boots + prints to 8250 + panics on missing rootfs, nothing more.
#     Config: tools/kernel/tinyconfig.fragment.
#
#   - "jvm" — adds TCP/IP, virtio-net, filesystems, HW_RANDOM, SMP,
#     HIGH_RES_TIMERS, cgroups, so a JVM + Spring Boot guest can boot.
#     Config: tools/kernel/jvm.fragment.
#
# Select via env var:
#   KERNEL_PROFILE=tiny tools/kernel/build-tiny-kernel.sh   # default
#   KERNEL_PROFILE=jvm  tools/kernel/build-tiny-kernel.sh
#
# Why we ship a build script rather than a checked-in binary:
#   - kernels are big (~1 MB compressed), repo would bloat over time.
#   - operators on different hosts may need slightly different configs
#     (e.g. KASLR enabled or not) — easier to tweak a fragment than
#     to maintain a matrix of fixtures.
#   - we want the kernel to be reproducible by anyone with a Linux
#     toolchain, not just whoever last pushed.
#
# Output (depends on profile):
#   tools/kernel/cache/linux-<VER>/arch/x86/boot/bzImage
#   symlinked to tools/kernel/cache/bzImage-<profile>.
#   For back-compat, when KERNEL_PROFILE=tiny we also symlink
#   tools/kernel/cache/bzImage -> bzImage-tiny; existing tests that
#   look at the unqualified `bzImage` path keep working.
#
# Test harness contract (unchanged):
#   `cargo test -p vm-kvm --features kvm bzimage` reads
#   `NANOVM_TEST_KERNEL` if set, else `tools/kernel/cache/bzImage`.
#   The jvm bench reads `--kernel <path>` from its CLI; point it at
#   `tools/kernel/cache/bzImage-jvm`.
#
# Requirements (Debian/Ubuntu names — equivalents exist on every
# distro):
#   - gcc, make, perl, libssl-dev, libelf-dev, bison, flex, bc
#   - ncurses-dev (only for `make menuconfig`; not used by this script)
#   - About 1 GB of disk and 10–15 min on a modern laptop (per profile).
#
# Idempotent: re-running this script with the same profile doesn't
# re-download or re-build if the target bzImage is newer than its
# fragment. Each profile has its own cache symlink, so flipping
# profiles is safe and does not invalidate the other's output.

set -euo pipefail

# ----------------------------------------------------------------------
# Pinned kernel version. Bump in lock-step with `linux-loader` support;
# the bzImage format the loader parses changed several times pre-5.x
# but 6.12 LTS is a well-supported floor.
# ----------------------------------------------------------------------
readonly KERNEL_VERSION="6.12"
readonly KERNEL_MAJOR="v6.x"
# SHA256 of linux-6.12.tar.xz as served by cdn.kernel.org. Cross-check
# against https://www.kernel.org/category/signatures.html if you're
# security-sensitive. Operators bumping KERNEL_VERSION should rebuild
# with `NANOVM_KERNEL_SKIP_SHA=1` once, then paste the printed `actual`
# value here.
readonly KERNEL_SHA256="b1a2562be56e42afb3f8489d4c2a7ac472ac23098f1ef1c1e40da601f54625eb"

# ----------------------------------------------------------------------
# Profile. Default 'tiny' for backward compat with the vm-kvm boot
# tests; set KERNEL_PROFILE=jvm for the JVM-capable kernel.
# ----------------------------------------------------------------------
readonly KERNEL_PROFILE="${KERNEL_PROFILE:-tiny}"
case "${KERNEL_PROFILE}" in
  tiny) CONFIG_FRAGMENT_NAME="tinyconfig.fragment" ;;
  jvm)  CONFIG_FRAGMENT_NAME="jvm.fragment" ;;
  *) echo "kernel: KERNEL_PROFILE must be 'tiny' or 'jvm' (got '${KERNEL_PROFILE}')" >&2
     exit 1 ;;
esac
readonly CONFIG_FRAGMENT_NAME

# ----------------------------------------------------------------------
# Paths.
# ----------------------------------------------------------------------
readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CACHE="${HERE}/cache"
# Per-profile source tree so building both profiles in sequence does
# not force a re-run of `make tinyconfig` + olddefconfig each time
# (the resulting .config lives inside SRC_DIR).
readonly SRC_DIR="${CACHE}/linux-${KERNEL_VERSION}-${KERNEL_PROFILE}"
readonly TARBALL="${CACHE}/linux-${KERNEL_VERSION}.tar.xz"
readonly URL="https://cdn.kernel.org/pub/linux/kernel/${KERNEL_MAJOR}/linux-${KERNEL_VERSION}.tar.xz"
readonly BZIMAGE_OUT="${SRC_DIR}/arch/x86/boot/bzImage"
# Each profile gets its own named symlink; the unqualified
# cache/bzImage keeps pointing at the tiny build for back-compat.
readonly BZIMAGE_LINK="${CACHE}/bzImage-${KERNEL_PROFILE}"
readonly LEGACY_BZIMAGE_LINK="${CACHE}/bzImage"
readonly CONFIG_FRAGMENT="${HERE}/${CONFIG_FRAGMENT_NAME}"

if [[ ! -f "${CONFIG_FRAGMENT}" ]]; then
  echo "kernel: config fragment not found at ${CONFIG_FRAGMENT}" >&2
  exit 1
fi

mkdir -p "${CACHE}"
readonly LOG_TAG="kernel/${KERNEL_PROFILE}"

# ----------------------------------------------------------------------
# Preflight: fail fast if a required build tool is missing, rather
# than dying several minutes into the compile. `bc` in particular is
# easy to miss — base-devel / build-essential don't pull it in, and
# the kernel only needs it midway through `prepare0`.
# ----------------------------------------------------------------------
missing=()
for tool in make gcc ld bc bison flex perl; do
  command -v "${tool}" >/dev/null 2>&1 || missing+=("${tool}")
done
if (( ${#missing[@]} > 0 )); then
  echo "${LOG_TAG}: missing required build tools: ${missing[*]}" >&2
  echo "${LOG_TAG}:   Debian/Ubuntu: sudo apt-get install build-essential libssl-dev libelf-dev bison flex bc xz-utils" >&2
  echo "${LOG_TAG}:   Arch:          sudo pacman -S --needed base-devel bc bison flex" >&2
  exit 1
fi

# ----------------------------------------------------------------------
# Short-circuit: skip only if the bzImage exists, the symlink points
# at it, AND it's newer than the config fragment. A fragment change
# (e.g. enabling CONFIG_BLK_DEV_INITRD) must force a rebuild —
# otherwise a stale kernel silently lacks the new feature. `git
# checkout` bumps the fragment's mtime to "now", so switching to a
# branch that edited the fragment correctly invalidates the cache.
# ----------------------------------------------------------------------
if [[ -f "${BZIMAGE_OUT}" && -L "${BZIMAGE_LINK}" && "${BZIMAGE_OUT}" -nt "${CONFIG_FRAGMENT}" ]]; then
  echo "${LOG_TAG}: bzImage already built at ${BZIMAGE_OUT} (newer than the config fragment)"
  echo "${LOG_TAG}: 'rm -rf ${CACHE}' forces a full rebuild"
  exit 0
fi
if [[ -f "${BZIMAGE_OUT}" ]]; then
  echo "${LOG_TAG}: config fragment changed since the last build — rebuilding"
fi

# ----------------------------------------------------------------------
# Step 1: download the tarball (if not already cached).
# ----------------------------------------------------------------------
if [[ ! -f "${TARBALL}" ]]; then
  echo "${LOG_TAG}: fetching ${URL}"
  if command -v curl >/dev/null; then
    curl -fL --retry 3 -o "${TARBALL}.partial" "${URL}"
  elif command -v wget >/dev/null; then
    wget -O "${TARBALL}.partial" "${URL}"
  else
    echo "${LOG_TAG}: need curl or wget to fetch ${URL}" >&2
    exit 1
  fi
  mv "${TARBALL}.partial" "${TARBALL}"
fi

# SHA verification — disable with NANOVM_KERNEL_SKIP_SHA=1 for the
# first build (when the operator is updating the pin and hasn't
# rotated the hash yet).
if [[ -z "${NANOVM_KERNEL_SKIP_SHA:-}" ]]; then
  echo "${LOG_TAG}: verifying SHA256"
  actual="$(sha256sum "${TARBALL}" | awk '{print $1}')"
  if [[ "${actual}" != "${KERNEL_SHA256}" ]]; then
    echo "${LOG_TAG}: SHA256 mismatch" >&2
    echo "${LOG_TAG}:   expected ${KERNEL_SHA256}" >&2
    echo "${LOG_TAG}:   got      ${actual}" >&2
    echo "${LOG_TAG}: rerun with NANOVM_KERNEL_SKIP_SHA=1 to bypass" >&2
    exit 1
  fi
fi

# ----------------------------------------------------------------------
# Step 2: extract (if not already extracted).
# ----------------------------------------------------------------------
if [[ ! -d "${SRC_DIR}" ]]; then
  echo "${LOG_TAG}: extracting ${TARBALL}"
  tar -C "${CACHE}" -xJf "${TARBALL}"
fi

# ----------------------------------------------------------------------
# Step 3: configure. `make tinyconfig` strips to the bare minimum;
# then we apply our fragment to re-enable serial console + a few
# bits vm-kvm assumes are present.
# ----------------------------------------------------------------------
cd "${SRC_DIR}"
# Regenerate .config when it's missing OR older than the fragment, so
# a fragment edit actually takes effect rather than reusing a stale
# .config.
if [[ ! -f .config || "${CONFIG_FRAGMENT}" -nt .config ]]; then
  echo "${LOG_TAG}: running make tinyconfig"
  make tinyconfig
  echo "${LOG_TAG}: merging ${CONFIG_FRAGMENT}"
  cat "${CONFIG_FRAGMENT}" >> .config
  # Resolve any dependency fall-outs from our additions.
  make olddefconfig
fi

# ----------------------------------------------------------------------
# Step 4: build. -j defaults to nproc; override with MAKEFLAGS.
#
# We force `-std=gnu11` onto the compiler binary itself. Linux 6.12
# predates the kernel's C23 readiness, and modern toolchains (GCC 15,
# recent Clang) default to `-std=gnu23` where `bool` / `true` /
# `false` are reserved keywords — which breaks headers like
# `include/linux/stddef.h`. The main Makefile sets `-std=gnu11`, but
# `arch/x86/boot/compressed/Makefile` *resets* KBUILD_CFLAGS, so the
# decompressor stub would otherwise compile under the compiler
# default. Baking the flag into CC reaches every compilation unit
# regardless of which sub-Makefile clobbers the flag set. Host tools
# (HOSTCC) already build fine under gnu23, so they're left alone.
# ----------------------------------------------------------------------
readonly CC_BASE="${CC:-gcc}"
echo "${LOG_TAG}: building bzImage (CC=\"${CC_BASE} -std=gnu11\")"
make CC="${CC_BASE} -std=gnu11" -j"$(nproc 2>/dev/null || echo 2)" bzImage

# ----------------------------------------------------------------------
# Step 5: make the result easy for the test harness to find.
# Profile-specific symlink is always written. The unqualified
# cache/bzImage symlink is maintained ONLY for the tiny profile —
# flipping it when building 'jvm' would break the vm-kvm boot tests
# that default to cache/bzImage. The jvm bench reads its kernel
# path from --kernel on the command line, so it does not need the
# unqualified symlink.
# ----------------------------------------------------------------------
ln -sf "${BZIMAGE_OUT}" "${BZIMAGE_LINK}"
if [[ "${KERNEL_PROFILE}" == "tiny" ]]; then
  ln -sf "${BZIMAGE_LINK}" "${LEGACY_BZIMAGE_LINK}"
fi

bzimage_bytes="$(stat -c%s "${BZIMAGE_OUT}" 2>/dev/null || stat -f%z "${BZIMAGE_OUT}")"
echo
echo "${LOG_TAG}: DONE"
echo "${LOG_TAG}:   bzImage at   ${BZIMAGE_OUT}"
echo "${LOG_TAG}:   symlink      ${BZIMAGE_LINK}"
if [[ "${KERNEL_PROFILE}" == "tiny" ]]; then
  echo "${LOG_TAG}:   legacy link  ${LEGACY_BZIMAGE_LINK}"
fi
echo "${LOG_TAG}:   size         ${bzimage_bytes} bytes"
echo
if [[ "${KERNEL_PROFILE}" == "tiny" ]]; then
  echo "Run the boot test:"
  echo "  cargo test -p vm-kvm --features kvm bzimage_boot -- --nocapture"
else
  echo "Run the JVM bench (needs a Java rootfs — see tools/java-rootfs/):"
  echo "  cargo run -p bench --release --features kvm --bin nanovm-jvm-bench -- \\"
  echo "      --kernel     ${BZIMAGE_LINK} \\"
  echo "      --initramfs  tools/java-rootfs/cache/initramfs.cpio.gz \\"
  echo "      --memory-mib 2048 --snapshot-at both --forks 20"
fi
