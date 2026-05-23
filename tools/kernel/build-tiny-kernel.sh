#!/usr/bin/env bash
# Build a tiny Linux bzImage for `vm-kvm`'s integration tests.
#
# Why we ship a build script rather than a checked-in binary:
#   - kernels are big (~1 MB compressed), repo would bloat over time.
#   - operators on different hosts may need slightly different configs
#     (e.g. KASLR enabled or not) — easier to tweak a fragment than
#     to maintain a matrix of fixtures.
#   - we want the kernel to be reproducible by anyone with a Linux
#     toolchain, not just whoever last pushed.
#
# Usage:
#   tools/kernel/build-tiny-kernel.sh
#
# Output:
#   tools/kernel/cache/linux-<VER>/arch/x86/boot/bzImage
#   symlinked to tools/kernel/cache/bzImage for the test harness.
#
# Test harness contract:
#   `cargo test -p vm-kvm --features kvm bzimage` reads
#   `NANOVM_TEST_KERNEL` if set, else `tools/kernel/cache/bzImage`.
#
# Requirements (Debian/Ubuntu names — equivalents exist on every
# distro):
#   - gcc, make, perl, libssl-dev, libelf-dev, bison, flex, bc
#   - ncurses-dev (only for `make menuconfig`; not used by this script)
#   - About 1 GB of disk and 10–15 min on a modern laptop.
#
# Idempotent: re-running this script doesn't re-download or re-build
# if the target bzImage is already present.

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
# Paths.
# ----------------------------------------------------------------------
readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly CACHE="${HERE}/cache"
readonly SRC_DIR="${CACHE}/linux-${KERNEL_VERSION}"
readonly TARBALL="${CACHE}/linux-${KERNEL_VERSION}.tar.xz"
readonly URL="https://cdn.kernel.org/pub/linux/kernel/${KERNEL_MAJOR}/linux-${KERNEL_VERSION}.tar.xz"
readonly BZIMAGE_OUT="${SRC_DIR}/arch/x86/boot/bzImage"
readonly BZIMAGE_LINK="${CACHE}/bzImage"
readonly CONFIG_FRAGMENT="${HERE}/tinyconfig.fragment"

mkdir -p "${CACHE}"

# ----------------------------------------------------------------------
# Short-circuit: if the bzImage already exists and the symlink points
# at it, we're done.
# ----------------------------------------------------------------------
if [[ -f "${BZIMAGE_OUT}" && -L "${BZIMAGE_LINK}" ]]; then
  echo "tiny-kernel: bzImage already built at ${BZIMAGE_OUT}"
  echo "tiny-kernel: removing the file or 'rm -rf ${CACHE}' forces a rebuild"
  exit 0
fi

# ----------------------------------------------------------------------
# Step 1: download the tarball (if not already cached).
# ----------------------------------------------------------------------
if [[ ! -f "${TARBALL}" ]]; then
  echo "tiny-kernel: fetching ${URL}"
  if command -v curl >/dev/null; then
    curl -fL --retry 3 -o "${TARBALL}.partial" "${URL}"
  elif command -v wget >/dev/null; then
    wget -O "${TARBALL}.partial" "${URL}"
  else
    echo "tiny-kernel: need curl or wget to fetch ${URL}" >&2
    exit 1
  fi
  mv "${TARBALL}.partial" "${TARBALL}"
fi

# SHA verification — disable with NANOVM_KERNEL_SKIP_SHA=1 for the
# first build (when the operator is updating the pin and hasn't
# rotated the hash yet).
if [[ -z "${NANOVM_KERNEL_SKIP_SHA:-}" ]]; then
  echo "tiny-kernel: verifying SHA256"
  actual="$(sha256sum "${TARBALL}" | awk '{print $1}')"
  if [[ "${actual}" != "${KERNEL_SHA256}" ]]; then
    echo "tiny-kernel: SHA256 mismatch" >&2
    echo "tiny-kernel:   expected ${KERNEL_SHA256}" >&2
    echo "tiny-kernel:   got      ${actual}" >&2
    echo "tiny-kernel: rerun with NANOVM_KERNEL_SKIP_SHA=1 to bypass" >&2
    exit 1
  fi
fi

# ----------------------------------------------------------------------
# Step 2: extract (if not already extracted).
# ----------------------------------------------------------------------
if [[ ! -d "${SRC_DIR}" ]]; then
  echo "tiny-kernel: extracting ${TARBALL}"
  tar -C "${CACHE}" -xJf "${TARBALL}"
fi

# ----------------------------------------------------------------------
# Step 3: configure. `make tinyconfig` strips to the bare minimum;
# then we apply our fragment to re-enable serial console + a few
# bits vm-kvm assumes are present.
# ----------------------------------------------------------------------
cd "${SRC_DIR}"
if [[ ! -f .config ]]; then
  echo "tiny-kernel: running make tinyconfig"
  make tinyconfig
  echo "tiny-kernel: merging ${CONFIG_FRAGMENT}"
  cat "${CONFIG_FRAGMENT}" >> .config
  # Resolve any dependency fall-outs from our additions.
  make olddefconfig
fi

# ----------------------------------------------------------------------
# Step 4: build. -j defaults to nproc; override with MAKEFLAGS.
# ----------------------------------------------------------------------
echo "tiny-kernel: building bzImage"
make -j"$(nproc 2>/dev/null || echo 2)" bzImage

# ----------------------------------------------------------------------
# Step 5: make the result easy for the test harness to find.
# ----------------------------------------------------------------------
ln -sf "${BZIMAGE_OUT}" "${BZIMAGE_LINK}"

bzimage_bytes="$(stat -c%s "${BZIMAGE_OUT}" 2>/dev/null || stat -f%z "${BZIMAGE_OUT}")"
echo
echo "tiny-kernel: DONE"
echo "tiny-kernel:   bzImage at ${BZIMAGE_OUT}"
echo "tiny-kernel:   symlink   ${BZIMAGE_LINK}"
echo "tiny-kernel:   size      ${bzimage_bytes} bytes"
echo
echo "Run the boot test:"
echo "  cargo test -p vm-kvm --features kvm bzimage_boot -- --nocapture"
