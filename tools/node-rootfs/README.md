# Node.js guest-rootfs builder

Produces an initramfs containing the `nanovm-agent` plus a working
**Node.js** runtime. The JavaScript counterpart to
[`tools/python-rootfs/`](../python-rootfs/) — same recipe, different
language toolchain — so the project demonstrates running customer
code in more than one language family without re-engineering the
guest path.

## Build

Requires Docker (with `buildx`) and the Rust musl target:

```sh
rustup target add x86_64-unknown-linux-musl
tools/node-rootfs/build.sh
```

Output: `tools/node-rootfs/cache/initramfs-node.cpio.gz`

The script:

1. Cross-compiles `nanovm-agent` as `x86_64-unknown-linux-musl`,
   static, non-PIE (same constraints `tools/python-rootfs/build.sh`
   handles for the kernel-can't-exec-ET_DYN reason).
2. Runs `docker buildx build` against the Dockerfile here.
3. Emits the cpio archive into `cache/` (just the artifact — no
   docker layers).

## What's in the archive

| Path | Source |
| --- | --- |
| `/init` | `nanovm-agent`, static musl, non-PIE |
| `/usr/bin/node` | from Alpine 3.20's `nodejs` package (Node 20.x) |
| `/etc/passwd`, `/lib/ld-musl-x86_64.so.1`, ... | Alpine 3.20 minirootfs |
| `/dev/console`, `/dev/null`, `/dev/zero`, `/dev/kmsg` | created in the packer stage |
| `/proc`, `/sys`, `/tmp`, `/run` | empty mountpoints |

Compressed size: ~12–18 MiB. Alpine's Node ships a smaller stdlib
than glibc-based distros, so the archive is in line with the Python
rootfs.

## Test it

On a Linux + KVM host (your i5 dev box, an Oracle A1 instance, etc.):

```sh
tools/kernel/build-tiny-kernel.sh         # one-time
tools/node-rootfs/build.sh
cargo test -p vm-kvm --features kvm --test exec_node_boot -- --nocapture
```

The test boots the kernel with this initramfs, waits for the agent
to connect over vsock, runs `node -e "console.log(1+1)"` inside the
guest, and asserts stdout is `"2\n"`. A clean pass proves the demo
path works for JavaScript as well as Python.

## When to rebuild

Rebuild after any change to `nanovm-agent`, or when bumping the
Alpine base image (edit the `FROM alpine:3.20` line in the
`Dockerfile`). Docker layer caching makes incremental rebuilds
fast — the slow step is the agent's cargo build.

## What this is NOT

- Not a production-grade base image. No package signing checks,
  no minimum-priv user setup, no `/etc/resolv.conf` — out-of-scope
  for an eval sandbox.
- Not aarch64-aware. The Dockerfile pins the agent target to
  `x86_64-unknown-linux-musl`. When the project ships aarch64 KVM,
  this will fork to platform-aware builds.
- Not the only language image. The Python variant lives at
  [`tools/python-rootfs/`](../python-rootfs/); same Dockerfile shape,
  different `apk add`.
