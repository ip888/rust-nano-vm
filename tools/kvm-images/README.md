# `tools/kvm-images/`

A tiny fetch-and-verify script that pulls a **kernel + rootfs pair
suitable for booting a Linux microVM** and drops them into a target
directory. Called from `Dockerfile.kvm`'s builder stage so the
production KVM image ships with a working default `kernel` / `rootfs`
— which is what makes `POST /v1/vms {}` succeed out of the box on
`ghcr.io/ip888/nanovm-control-plane-kvm`.

## What it downloads

| File | Source | Size | Purpose |
| --- | --- | --- | --- |
| `vmlinux` | Firecracker `hello-vmlinux.bin` | ~5 MiB | Minimal Linux kernel with virtio-blk, virtio-net, ext4 built in |
| `rootfs.ext4` | Firecracker `hello-rootfs.ext4` | ~50 MiB | Alpine Linux userspace |

Both come from
[`s3.amazonaws.com/spec.ccfc.min/img/hello/`](https://s3.amazonaws.com/spec.ccfc.min/img/hello/),
the URLs that AWS's [Firecracker "Getting Started" guide](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md)
uses. They've been stable at that path for years.

The images are minimal by design — booting a Python data-science
stack requires a beefier rootfs, either by:

- overriding `NANOVM_DEFAULT_ROOTFS_PATH` to a mounted volume with
  your own image, or
- running the workflow described in `tools/python-rootfs/` to build
  a Python-batteries-included rootfs alongside this one.

## Usage

```sh
./fetch.sh /some/target/dir
```

Writes `/some/target/dir/{vmlinux,rootfs.ext4}`. Fails loudly if a
download errors or the SHA-256 doesn't match.

`Dockerfile.kvm` runs it during the builder stage; nothing needs to
be done on the host manually.

## Refreshing the pins

The URLs above are stable but the *contents* aren't versioned — a
hypothetical AWS-side update to the "hello" images would silently
change the SHA-256. To lock in the current contents:

1. Run `./fetch.sh /tmp/kvm-images` locally.
2. `sha256sum /tmp/kvm-images/*` — copy the two hex strings.
3. Edit `fetch.sh`, replace the `KERNEL_SHA` / `ROOTFS_SHA` constants
   (currently set to `SKIP` so the initial bootstrap can pin them),
   bump `SCHEMA_VERSION` so a stale Docker layer cache rebuilds.

The pinned SHAs let CI catch a silently-mutated upstream. A checksum
mismatch is treated as a build failure, not a warning.

## Why not `apt install` an Alpine rootfs at build time?

Two reasons:

1. **Reproducibility.** A pinned tarball with a SHA-256 lock is trivially
   reproducible; `apk add …` inside a build stage inherits upstream mirror
   volatility, and the resulting image drifts by the day.

2. **Layer size.** Firecracker's `hello-rootfs.ext4` is ~50 MiB and
   already contains a functional Alpine userspace. Rolling our own
   with `debootstrap`-style tooling would triple the layer count for
   no functional gain at the demo scale.

Long-term the right move is a nanovm-specific Alpine rootfs published
under `ghcr.io/ip888/nanovm-images/` — same shape as this script, just
pointing at our own registry. That's a follow-up PR.
