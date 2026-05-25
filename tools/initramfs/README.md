# Guest-userspace boot test initramfs

Builds a minimal initramfs that proves `vm-kvm` can boot a kernel
**and run a userspace process** inside the guest — the prerequisite
for running the guest agent (M2).

## Build

```sh
tools/kernel/build-tiny-kernel.sh      # once, provides the kernel + source tree
tools/initramfs/build-initramfs.sh
```

Output: `tools/initramfs/cache/initramfs.cpio` (a few KiB).

## Test against it

```sh
cargo test -p vm-kvm --features kvm initramfs_boot -- --nocapture
```

The test boots the tiny kernel with this initramfs, then asserts the
guest's `/init` printed `GUEST_USERSPACE_OK` to the serial console.
Skips (and passes) if the kernel or initramfs fixtures aren't built,
so a fresh checkout stays green.

## What's in the archive

| Entry | Why |
| --- | --- |
| `/init` | static binary from `init.c` — runs as PID 1, prints the marker, reboots |
| `/dev` + `/dev/console` (5:1) | so the kernel wires init's stdio to `console=ttyS0` before exec |

The `/dev/console` node is faked into the cpio by the kernel's own
`gen_init_cpio` helper (compiled from the kernel source), so no root
or `mknod` privilege is needed.

## Note

`init.c` is a **test fixture**, not the product. The real guest
agent (Rust static-musl `nanovm-agent`) replaces it in the next M2
step.
