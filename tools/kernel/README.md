# Kernel builds for `vm-kvm`

This directory builds Linux bzImages for `vm-kvm`'s guest. Two
profiles today, chosen via the `KERNEL_PROFILE` env var:

| Profile | Fragment | Use case | Approx. size |
| --- | --- | --- | --- |
| **`tiny`** (default) | `tinyconfig.fragment` | `vm-kvm` integration tests (`bzimage_boot`, `vsock_probe_boot`, …). Boots, prints `Linux version` on 8250, panics on missing rootfs, halts. | ~1 MB |
| **`jvm`** | `jvm.fragment` | JVM + Spring Boot guest for the Petclinic demo and `nanovm-jvm-bench`. Adds TCP/IP + virtio-net, filesystems, HW\_RANDOM + `RANDOM_TRUST_CPU`, SMP, HIGH\_RES\_TIMERS, cgroups. | ~1.5-2 MB |

Both profiles start from the same `make tinyconfig` baseline and are
applied the same way — see `build-tiny-kernel.sh` and either
fragment file for the full set of options and why each is on.

## Build

```sh
# Default — tiny profile for the vm-kvm boot tests.
tools/kernel/build-tiny-kernel.sh

# Explicitly tiny:
KERNEL_PROFILE=tiny tools/kernel/build-tiny-kernel.sh

# JVM profile for the Petclinic / Spring Boot path:
KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh
```

First run of each profile downloads the kernel tarball (~140 MB)
once (shared between profiles), extracts into a profile-specific
source tree, runs `make tinyconfig`, appends the chosen fragment,
resolves with `make olddefconfig`, and builds. Expect 10–15 min on a
modern laptop for each profile.

Subsequent runs of the same profile short-circuit if the artifact is
present and newer than its fragment file. Each profile has its own
source tree (`cache/linux-<ver>-tiny/`, `cache/linux-<ver>-jvm/`) and
bzImage symlink (`cache/bzImage-tiny`, `cache/bzImage-jvm`), so
alternating profiles does not invalidate the other's build.

## Output layout

```
tools/kernel/cache/
├── linux-6.12.tar.xz         # tarball (shared between profiles)
├── linux-6.12-tiny/          # source tree for tiny profile
├── linux-6.12-jvm/           # source tree for jvm profile
├── bzImage-tiny -> linux-6.12-tiny/arch/x86/boot/bzImage
├── bzImage-jvm  -> linux-6.12-jvm/arch/x86/boot/bzImage
└── bzImage      -> bzImage-tiny     (legacy, only set when the last
                                      tiny build happened)
```

The unqualified `cache/bzImage` symlink is maintained **only** when
the last build was `KERNEL_PROFILE=tiny`. The `vm-kvm` boot tests
default to this path (`NANOVM_TEST_KERNEL` overrides it); building
`jvm` after `tiny` leaves the legacy symlink pointing at the tiny
build on purpose, so those tests keep working. The jvm bench reads
`--kernel <path>` from its CLI and does not rely on the legacy
symlink.

## Test against the tiny kernel

```sh
cargo test -p vm-kvm --features kvm bzimage_boot -- --nocapture
```

Reads `NANOVM_TEST_KERNEL` for an explicit path, otherwise falls
back to `tools/kernel/cache/bzImage`. If neither is found the test
prints a skip notice and passes — so a no-kernel checkout does not
redden CI.

## Bench with the jvm kernel

Needs a Java rootfs (`tools/java-rootfs/`) built separately.

```sh
# 1. Build the Java rootfs (one-time):
SKIP_EXT4=1 tools/java-rootfs/build.sh
# 2. Build the jvm kernel (one-time):
KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh
# 3. Run the bench:
cargo run -p bench --release --features kvm --bin nanovm-jvm-bench -- \
    --kernel     tools/kernel/cache/bzImage-jvm \
    --initramfs  tools/java-rootfs/cache/initramfs.cpio.gz \
    --memory-mib 2048 \
    --snapshot-at both \
    --forks 20
```

See `crates/bench/src/bin/jvm_bench.rs` for the full `--help`.

## Build deps

Most are stock for any distro that builds the kernel. On
Debian/Ubuntu:

```sh
sudo apt-get install build-essential libssl-dev libelf-dev \
    bison flex bc xz-utils
```

On Arch:

```sh
sudo pacman -S base-devel bc bison flex
```

## SHA mismatch on first run?

If you're updating the pinned `KERNEL_VERSION` in the script, the
checked-in `KERNEL_SHA256` won't match the new tarball. Re-run
with `NANOVM_KERNEL_SKIP_SHA=1` once, then update the SHA in the
script (you can copy the printed `actual` value, but cross-check
against `https://www.kernel.org/category/signatures.html` before
committing it).

## Files

| File | Purpose |
| --- | --- |
| `build-tiny-kernel.sh` | Downloads + configures + builds. Idempotent. Profile-aware (via `KERNEL_PROFILE=tiny\|jvm`). |
| `tinyconfig.fragment` | Config diff for the `tiny` profile. The minimum vm-kvm needs (PRINTK, 8250, panic-on-root-mount-fail, virtio-mmio, vsock). |
| `jvm.fragment` | Config diff for the `jvm` profile. Tiny baseline + TCP/IP, virtio-net, HW\_RANDOM, cgroups, SMP, HIGH\_RES\_TIMERS, filesystems. |
| `cache/` | Git-ignored. Holds tarball, per-profile source trees, and bzImage symlinks. |
