# Python guest-rootfs builder

Produces an initramfs containing the `nanovm-agent` plus a working
**Python 3** interpreter. This is the rootfs you want when you need
to demo `python -c "print(1+1)"` running inside a forked microVM —
the cash unit-of-value moment for the project.

## Build

Requires Docker (with `buildx`) and the Rust musl target:

```sh
rustup target add x86_64-unknown-linux-musl
tools/python-rootfs/build.sh
```

Output: `tools/python-rootfs/cache/initramfs-python.cpio.gz`

The script:

1. Cross-compiles `nanovm-agent` as `x86_64-unknown-linux-musl`,
   static, non-PIE (same constraints the existing
   `tools/initramfs/build-initramfs.sh` script handles for the
   minimal init).
2. Runs `docker buildx build` against the Dockerfile here.
3. Emits the cpio archive into `cache/` (just the artifact — no
   docker layers).

Why Docker as the builder: the rootfs needs Alpine's `apk add
python3` which is Linux-only. Docker makes the build reproducible on
macOS (M1) and Linux without per-host tooling.

## What's in the archive

| Path | Source |
| --- | --- |
| `/init` | `nanovm-agent`, static musl, non-PIE |
| `/usr/bin/python3` | from Alpine 3.20's `python3` package |
| `/etc/passwd`, `/lib/ld-musl-x86_64.so.1`, ... | Alpine 3.20 minirootfs |
| `/dev/console`, `/dev/null`, `/dev/zero`, `/dev/kmsg` | created in the packer stage |
| `/proc`, `/sys`, `/tmp`, `/run` | empty mountpoints |

Compressed size: ~15–25 MiB (Alpine + Python 3 takes roughly that
much; cpio + gzip don't help much because Python's `.pyc` files
are already entropy-rich).

## Test it

On a Linux + KVM host (your i5 dev box, an Oracle A1 instance, etc.):

```sh
tools/kernel/build-tiny-kernel.sh         # one-time
tools/python-rootfs/build.sh
cargo test -p vm-kvm --features kvm exec_python_boot -- --nocapture
```

The test boots the kernel with this initramfs, waits for the agent
to connect over vsock, runs `python3 -c "print(1+1)"` inside the
guest, and asserts stdout is `"2\n"`. A clean pass proves the demo
path end-to-end.

## When to rebuild

Rebuild after any change to `nanovm-agent`, or when bumping the
Alpine base image (edit the `FROM alpine:3.20` line in the
`Dockerfile` and bump). Docker layer caching makes incremental
rebuilds fast — the slow step is the agent's cargo build, not the
docker build itself.

## What this is NOT

- Not a production-grade base image. No package signing checks,
  no minimum-priv user setup, no /etc/resolv.conf — that's all
  out-of-scope for an eval sandbox.
- Not aarch64-aware. The Dockerfile pins the agent target to
  `x86_64-unknown-linux-musl`. When the project ships aarch64 KVM,
  this will fork to platform-aware builds.
- Not the only image you can ship. Same recipe with Node, R, or
  any language toolchain.
