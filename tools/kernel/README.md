# Tiny kernel build for `vm-kvm` integration tests

This directory builds a minimal Linux bzImage that
`vm-kvm`'s integration test suite (`tests/bzimage_boot.rs`) loads
through the public `Hypervisor` trait. Boots far enough to print
`Linux version …` to the 8250 UART, panics on missing rootfs, halts.
That's the M1 milestone closed against a real kernel.

## Build

```sh
tools/kernel/build-tiny-kernel.sh
```

First run downloads ~140 MB of source, runs `make tinyconfig` +
applies `tinyconfig.fragment`, and builds. Expect 10–15 min on a
modern laptop, then `~1 MB` of bzImage. Subsequent runs short-circuit
if the artefact is already present.

## Test against it

```sh
cargo test -p vm-kvm --features kvm bzimage_boot -- --nocapture
```

The test reads `NANOVM_TEST_KERNEL` for an explicit path, otherwise
falls back to `tools/kernel/cache/bzImage` (the symlink the build
script writes). If neither is found the test prints a skip notice
and passes — so a no-kernel checkout doesn't redden CI.

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
| `build-tiny-kernel.sh` | Downloads + configures + builds. Idempotent. |
| `tinyconfig.fragment` | Config diff applied after `make tinyconfig`. The minimum vm-kvm needs (PRINTK, 8250, panic-on-root-mount-fail). |
| `cache/` | Git-ignored. Holds tarball + extracted source + symlink to bzImage. |
