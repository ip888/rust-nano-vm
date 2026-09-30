# virtio-net

Host-side virtio-net device for the nanovm KVM backend. Landing
across four sub-PRs in Week 2 of the enterprise-Java web rollout:

| Sub-PR | What lands | This file describes |
|---|---|---|
| **#A (this crate's scaffold)** | Crate skeleton + `NetworkBackend` trait + `TapDevice` Linux-only wrapper | ✅ |
| #B | virtio-mmio device state machine, descriptor ring processing, guest cmdline hookup | Later |
| #C | Wire the device into `crates/vm-kvm` (register MMIO region, add to boot params) | Later |
| #D | Host-side bridge + IP allocation + reverse-proxy for per-visitor subdomains | Later |

## Why virtio-net at all

A KVM guest without a network device is a machine with no cable. It
can talk to itself over loopback and that's it — no way for a
browser on the outside to reach the Petclinic instance running
inside. virtio-net is the industry-standard paravirtualised network
interface for KVM guests (Firecracker, Cloud Hypervisor, QEMU, Kata
Containers all use it). Guest Linux kernels have shipped the
virtio-net driver in-tree since 2008.

## How it works

Guest kernel and our host code share two things:

1. **Descriptor rings** — regions of guest RAM where the guest writes
   frames it wants to transmit (TX ring) and where we write frames
   we've received from the host network (RX ring). Both sides know
   the layout via the virtio spec §5.
2. **A "doorbell" MMIO register** — the guest writes to a memory
   address our virtio-net device claims; KVM traps that access and
   delivers control to us with `KVM_EXIT_MMIO`. That's how the guest
   tells us "I put a new frame in the TX ring, please drain it".

The **host side** of the device — the code in this crate — reads
frames from the TX ring and writes them out to a Linux TAP interface.
Linux then routes those frames onto a bridge that connects to the
outside world through iptables NAT.

The **guest side** is the standard virtio-net driver already baked
into every Linux kernel. Nothing to build there.

## Architecture in this crate

```
┌────────────────────────────────────────────────────────────┐
│ crates/virtio-net (this crate)                             │
│                                                            │
│  ┌──────────────────────────────────────────────────┐     │
│  │ NetworkBackend trait                             │     │
│  │   fn read_frame(&self, buf) → usize             │     │
│  │   fn write_frame(&self, frame) → ()             │     │
│  └──────────────────────────────────────────────────┘     │
│              ↑ implemented by                              │
│  ┌──────────────────────────────────────────────────┐     │
│  │ TapDevice  (Linux only, gated behind             │     │
│  │             cfg(target_os = "linux"))            │     │
│  │   fd: RawFd → /dev/net/tun                       │     │
│  │   read/write raw Ethernet frames                 │     │
│  │   Drop closes the fd                             │     │
│  └──────────────────────────────────────────────────┘     │
│  ┌──────────────────────────────────────────────────┐     │
│  │ MockBackend  (unit tests, all platforms)         │     │
│  │   in-memory VecDeque of frames                   │     │
│  │   symmetric read/write for round-tripping        │     │
│  └──────────────────────────────────────────────────┘     │
│                                                            │
│  Sub-PR #B adds VirtioNetDevice which owns a               │
│  Box<dyn NetworkBackend> — that's the abstraction seam     │
│  between the virtio protocol code and the transport.       │
└────────────────────────────────────────────────────────────┘
```

The trait separation exists so that:
- The virtio device state machine (sub-PR #B) tests without touching
  the kernel — mock backend replays scripted frames.
- The TAP backend tests without touching the virtio spec — its own
  tests just verify byte-for-byte round-trip through
  `/dev/net/tun`.

## What TAP is (for readers new to Linux networking)

A **TAP interface** is a virtual Ethernet device that exists in
software. When you `open("/dev/net/tun")` and issue the `TUNSETIFF`
ioctl with a name, the kernel creates a new interface (e.g.
`nanovm-tap-42`) that other userspace processes see with `ip link`.
Any packet written to the fd shows up on the interface as if it came
"from the wire". Any packet the kernel routes to that interface can
be read from the fd.

That's the whole magic. Our device is going to:
1. Open a TAP fd for each guest.
2. Copy TX ring → TAP fd.
3. Copy TAP fd → RX ring.

The kernel handles the rest — L2 bridging, L3 routing, NAT — with
tools every Linux admin knows (`ip`, `iptables`, `nftables`).

## Feature gating

TAP is Linux-only. The `TapDevice` module is gated behind
`#[cfg(target_os = "linux")]`. Cross-compiling to macOS or Windows
still builds the crate — you just don't get a working `TapDevice`,
which is fine because you can't run nanovm's KVM backend on those
platforms either.

The `NetworkBackend` trait and `MockBackend` build everywhere so
unit tests for sub-PRs #B and later work on macOS contributor
laptops.
