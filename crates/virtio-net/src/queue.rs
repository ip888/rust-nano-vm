//! # Virtqueue consumer for the virtio-net device
//!
//! This module is the piece of the device that **moves bytes between
//! the guest's memory and the host's [`NetworkBackend`]**. Sub-PR #B
//! landed the registers the guest driver pokes to discover and set up
//! the device; this file is the actual data path that runs every time
//! the guest kicks a queue.
//!
//! New reader? Start with the **terminology** box below, then read the
//! two **data-flow diagrams** (TX and RX). The rest of the module is
//! the Rust implementation of exactly those two flows.
//!
//! ## Terminology (every acronym in one place)
//!
//! | Term | What it means |
//! |---|---|
//! | **virtio** | Industry-standard paravirtualised I/O protocol for guest ↔ host communication in KVM-based VMs. Guest kernels ship drivers (`virtio_net`, `virtio_blk`, …); hosts ship devices (us). |
//! | **virtio-net** | The specific virtio device class for a network interface card. Spec: *Virtio 1.3 §5.1*. |
//! | **virtqueue (vq)** | A FIFO of buffer descriptors shared between guest and host, living in guest-physical RAM. One vq per direction: `vq[0] = RX`, `vq[1] = TX` for a basic virtio-net. |
//! | **TX** | *Transmit* — guest → host direction. Guest writes an outgoing Ethernet frame into a descriptor chain, kicks us, we read it and hand it to the backend (TAP, userspace stack). |
//! | **RX** | *Receive* — host → guest direction. Backend hands us an incoming Ethernet frame; we write it into a chain the guest driver pre-posted for us; guest reads. |
//! | **descriptor** | 16-byte record `{ addr: u64, len: u32, flags: u16, next: u16 }` that points at a buffer somewhere in guest RAM. Does not contain the data itself. |
//! | **descriptor table** | A flat array of descriptors in guest RAM. Size is a power of two (we use 256 max). The guest allocates it at device setup. |
//! | **descriptor chain** | A linked list through the descriptor table via each descriptor's `next` field, terminated when `flags & DESC_F_NEXT == 0`. One chain = one logical message. For virtio-net it holds the net header + Ethernet frame. |
//! | **avail ring** | A circular queue the **guest writes** and we read. Each slot is a `u16` index into the descriptor table — the "head" of one chain the guest has prepared for us to consume. |
//! | **used ring** | A circular queue **we write** and the guest reads. Each slot is `{ head: u32, written_len: u32 }` — the head of a chain we've finished with, plus how many bytes we wrote into the chain's writable buffers (0 on TX, 12 + frame.len() on RX). |
//! | **head** | Descriptor-table index of the first descriptor in a chain. The avail ring publishes heads; the used ring returns them. |
//! | **slot** | A position inside the avail or used ring. Slots are indexed modulo queue size — the ring is circular. |
//! | **idx** | Producer counter at the top of each ring. Driver's `avail.idx` ↑ when it adds a head; our `used.idx` ↑ when we return one. The counter is `u16` and allowed to wrap. |
//! | **cursor** | Our local "last seen" copy of `avail.idx`. Difference between `avail.idx` and the cursor tells us how many new chains the guest has made available since our last run. |
//! | **MMIO** | *Memory-Mapped I/O.* A memory address range that, when the guest reads/writes it, triggers a trap into KVM (`KVM_EXIT_MMIO`) which hands control to us. That's how the guest "kicks" the device. |
//! | **MMIO doorbell** | The specific MMIO register `QueueNotify` the guest writes to tell us "I just added heads to queue N, come drain it". |
//! | **VRING interrupt** | The IRQ bit we set in `InterruptStatus` to tell the guest "I just added used entries, come consume them". From the guest's POV an IRQ fires on the device's vector. |
//! | **virtio-net header** | 12-byte struct that prefixes every frame on the wire of a virtio-net queue. Carries offload hints the real NIC would set. We advertise no offload, so it's all zeros, but it's still mandatory per spec. |
//! | **MTU** | *Maximum Transmission Unit.* The largest Ethernet payload we accept — 1500 bytes, standard Ethernet. 14-byte L2 header + 1500 payload = 1514 bytes per frame on the wire. |
//! | **FCS** | *Frame Check Sequence* — 4-byte CRC Ethernet uses on physical wires. Virtual interfaces (TAP, virtio-net) do not carry it; the host kernel adds/strips it at the real NIC. So our frames are 1514 bytes max, not 1518. |
//! | **GSO / TSO** | *Generic / TCP Segmentation Offload* — hardware feature where guest hands over one big TCP segment and the NIC splits it. We don't negotiate it; `gso_type = NONE` in every header. |
//!
//! ## Split-virtqueue layout in guest RAM
//!
//! Each queue is three contiguous regions in guest-physical memory,
//! allocated by the guest driver and whose base addresses it programs
//! into our MMIO registers (`QueueDescLow/High`, `QueueDriverLow/High`,
//! `QueueDeviceLow/High`). Our side **never allocates guest memory** —
//! the guest sized it, we read/write at the addresses it gave us.
//!
//! ```text
//!   descriptor table (QueueDesc...): N * 16 bytes
//!   ┌─────────┬─────────┬─────────┬─────┬─────────┐
//!   │ desc[0] │ desc[1] │ desc[2] │ ... │ desc[N-1]│   N = queue size
//!   └─────────┴─────────┴─────────┴─────┴─────────┘   (power of two)
//!   each desc = { addr: u64, len: u32, flags: u16, next: u16 }
//!
//!   avail ring (QueueDriver...): 6 + 2*N bytes
//!   ┌────────┬────────┬──────────────────────────┬──────────────┐
//!   │ flags  │ idx    │ ring[0..N] : u16 heads   │ used_event   │
//!   │ u16    │ u16 ↑  │                          │ u16          │
//!   └────────┴────────┴──────────────────────────┴──────────────┘
//!             │
//!             └── driver increments this when it adds a head to ring[]
//!
//!   used ring (QueueDevice...): 6 + 8*N bytes
//!   ┌────────┬────────┬──────────────────────────┬──────────────┐
//!   │ flags  │ idx    │ ring[0..N] :             │ avail_event  │
//!   │ u16    │ u16 ↑  │   { head: u32, len: u32 } │ u16          │
//!   └────────┴────────┴──────────────────────────┴──────────────┘
//!             │
//!             └── WE increment this when we return a head to ring[]
//! ```
//!
//! Keep three facts in your head while reading the code:
//! - **The guest OWNS the memory,** we just read/write at offsets it
//!   gave us. Not a single `malloc` or `mmap` happens on our side.
//! - **The driver is the producer of avail, the consumer of used.**
//!   We are the opposite — consumer of avail, producer of used.
//! - **Indices wrap at `u16::MAX`.** Difference-based reasoning
//!   (`avail.idx.wrapping_sub(cursor)`) is correct across the wrap;
//!   absolute comparisons are not.
//!
//! ## TX data flow: guest sends an Ethernet frame
//!
//! ```text
//!   GUEST                                      HOST (process_tx)
//!   ─────                                      ─────────────────
//!   1. Spring Boot does socket.write(bytes)
//!   2. Linux TCP/IP stack builds Ethernet frame
//!   3. virtio_net driver:
//!        a. picks a free descriptor chain in TX desc table
//!        b. puts  { net_header[12] || frame[N] }  in it
//!        c. publishes head index to avail.ring[avail.idx % qsize]
//!        d. increments avail.idx
//!        e. writes QueueNotify = 1   ← MMIO trap
//!                                              │
//!                                              ▼
//!                                              KVM_EXIT_MMIO
//!                                              │
//!                                              ▼
//!                                              VirtioNetDevice::mmio_write
//!                                              sees QueueNotify, returns
//!                                              Some(TX_QUEUE_INDEX)
//!                                              │
//!                                              ▼
//!                                              process_tx(cfg, cursor, mem, backend):
//!                                                for head in avail.iter_new(cursor):
//!                                                  chain = walk desc_table from head
//!                                                  payload = concat readable descs
//!                                                  if payload too small / big: drop
//!                                                  else:
//!                                                    strip 12-byte net header
//!                                                    backend.write_frame(&frame)
//!                                                  used.push(head, 0)
//!                                                  cursor += 1
//!                                              │
//!                                              ▼
//!                                              VirtioNetDevice sets VRING in
//!                                              InterruptStatus; vm-kvm injects
//!                                              the device IRQ into the guest.
//!   4. virtio_net driver IRQ handler:
//!        a. reads used.ring[used.idx % qsize]
//!        b. frees the descriptors in the chain
//!        c. ACKs the IRQ by writing InterruptACK
//! ```
//!
//! TX used-elem writes `written_len = 0` because the TX chain is all
//! **device-readable** descriptors — we never write anything back to
//! guest memory on TX. The virtio spec pins this (§2.7.8).
//!
//! ## RX data flow: host delivers an Ethernet frame to the guest
//!
//! ```text
//!   GUEST                                      HOST (process_rx)
//!   ─────                                      ─────────────────
//!   Before any traffic:
//!     virtio_net driver pre-posts N empty
//!     writable buffers on RX avail ring,
//!     ready for us to fill.
//!
//!   (meanwhile, on the wire or TAP backend...)
//!                                              some frame arrives on TAP fd
//!                                              epoll wakes us, we call process_rx:
//!                                                for each avail entry:
//!                                                  frame = backend.read_frame(&mut buf)
//!                                                  if 0 bytes: stop (no more backend data)
//!                                                  else:
//!                                                    head = avail.ring[slot]
//!                                                    walk writable desc chain
//!                                                    if total_cap < 12+frame.len(): drop
//!                                                    else:
//!                                                      payload = net_hdr(no_offload) || frame
//!                                                      scatter-write payload across writable descs
//!                                                      used.push(head, 12 + frame.len())
//!                                                      cursor += 1
//!                                                raise VRING interrupt
//!                                              │
//!                                              ▼
//!                                              IRQ delivered to guest
//!   virtio_net driver IRQ handler:
//!     a. reads used.ring[used.idx]
//!     b. strips the 12-byte net header
//!     c. hands the Ethernet frame to the Linux
//!        TCP/IP stack
//!     d. Spring Boot's socket.read() returns.
//! ```
//!
//! Note the asymmetry: on TX we strip the header before giving the
//! frame to the backend; on RX we prepend the header after reading
//! from the backend. The backend only ever sees raw Ethernet frames,
//! never virtio-net headers — that's the layering boundary.
//!
//! ## Cursor arithmetic
//!
//! The cursor is our local `last_seen_avail`. Each call to
//! `process_tx` or `process_rx` walks from `cursor` up to the guest's
//! current `avail.idx`:
//!
//! ```text
//!    cursor         avail.idx
//!      │                │
//!      ▼                ▼
//!    ╭───┬───┬───┬───┬───┬───┬───┬───╮
//!    │ A │ B │ C │ D │ E │ F │   │   │   avail.ring (circular, qsize=8)
//!    ╰───┴───┴───┴───┴───┴───┴───┴───╯
//!      ▲───── chains we need to drain ──▲
//!
//!   After process_tx returns:
//!    cursor = avail.idx
//!    used.idx += number of chains we processed
//! ```
//!
//! Both counters wrap at `u16::MAX`. `cursor != avail.idx` is the
//! loop condition — compared for inequality, not ordering, so wrap is
//! transparent.
//!
//! ## Guest memory boundary (why this file is pure logic)
//!
//! Every descriptor's `addr` field is a **guest-physical address** —
//! an address inside the guest VM's RAM, meaningless to our process on
//! its own. We never dereference it as a raw pointer. All guest-memory
//! access goes through [`virtio_queue::GuestMemory`], a trait with
//! two methods:
//!
//! ```text
//!     fn read (&self,     addr: u64, dst: &mut [u8]) -> Result<(), QueueError>;
//!     fn write(&mut self, addr: u64, src: &    [u8]) -> Result<(), QueueError>;
//! ```
//!
//! `vm-kvm` implements this over its `mmap`'d guest RAM (one method
//! call = one bounds-checked `memcpy`). Unit tests use
//! `SliceGuestMemory`, a `Vec<u8>`-backed fake. The queue consumer
//! does not know or care which it has — this is why the whole module
//! is testable without KVM, a real guest, or any `unsafe`.
//!
//! ## Why no `unsafe`
//!
//! The crate root is `#![deny(unsafe_code)]`. This file obeys that:
//! every byte of guest memory flows through the `GuestMemory` trait,
//! which bounds-checks internally. From this file's perspective it's
//! just `read(addr, &mut buf)` / `write(addr, &buf)` — safe Rust end
//! to end. The only `allow(unsafe_code)` escape in this crate lives
//! in `tap.rs`, which wraps the `TUNSETIFF` ioctl FFI.

use virtio_queue::{Descriptor, DescriptorChain, GuestMemory, QueueError, DESC_SIZE};

use crate::{mmio::QueueConfig, NetworkBackend, VirtioNetError, VirtioNetHdr, VIRTIO_NET_HDR_LEN};

/// Maximum Ethernet frame bytes the device accepts on TX or produces on
/// RX. **1518** = 14-byte Ethernet header + **4-byte 802.1Q VLAN tag**
/// + 1500 MTU; no FCS over TAP/virtio-net.
///
/// The VLAN overhead is included because Linux's `virtio_net` driver
/// happily hands us VLAN-tagged frames when the guest is on a
/// VLAN-aware bridge; an untagged max frame is 1514, so this constant
/// has to cover both the tagged and untagged cases.
///
/// A guest frame larger than this is dropped. Jumbo frames (>1500
/// MTU) would require advertising `VIRTIO_NET_F_MTU` with a larger
/// value in the device config, which we don't do today.
pub const MAX_FRAME_LEN: usize = 1518;

/// Maximum bytes a single descriptor chain may carry, including the
/// 12-byte virtio-net header. Hard cap to stop a malicious guest from
/// forcing the device to allocate unbounded scratch by chaining 65535
/// 4 KiB descriptors. `MAX_FRAME_LEN + VIRTIO_NET_HDR_LEN = 1530`.
pub const MAX_CHAIN_BYTES: usize = MAX_FRAME_LEN + VIRTIO_NET_HDR_LEN;

/// Minimum Ethernet frame bytes the device will push to the backend:
/// 14-byte Ethernet header + at least 46 bytes of payload (per 802.3
/// minimum). Below this we drop the frame — a conformant guest driver
/// pads with zeros so we never see short frames in practice, but a
/// bug or an attack could.
pub const MIN_FRAME_LEN: usize = 14 + 46;

/// Maximum virtqueue size the device will consent to operate on,
/// mirroring [`crate::mmio::QUEUE_SIZE_MAX`]. A guest can program any
/// `u32` into the `QueueNum` MMIO register, but we refuse to walk a
/// ring whose size is outside this bound or not a power of two.
///
/// The alternative — trusting the guest's `cfg.size` and casting
/// `as u16` — is a **DoS vector**: a value of 65536 wraps to 0, and
/// `slot % 0` panics the whole VMM. See [`validate_qsize`].
const MAX_SUPPORTED_QUEUE_SIZE: u32 = crate::mmio::QUEUE_SIZE_MAX;

/// Validate the guest-programmed queue size and return it as a `u16`.
///
/// Rejects:
/// - zero (would trigger division by zero on `%`),
/// - sizes above the advertised [`MAX_SUPPORTED_QUEUE_SIZE`],
/// - sizes that are not a power of two (required by virtio spec
///   §2.7 for split rings).
///
/// Returning `None` means "treat the queue as malformed and skip
/// this call" — we deliberately do not raise an error, because an
/// Err would propagate through every MMIO exit handler for what is
/// a well-contained guest misbehaviour. Silent skip + per-call
/// stats = `0` is the Firecracker-style posture: don't crash the
/// VMM for a buggy guest.
fn validate_qsize(cfg_size: u32) -> Option<u16> {
    if cfg_size == 0 || cfg_size > MAX_SUPPORTED_QUEUE_SIZE {
        return None;
    }
    if !cfg_size.is_power_of_two() {
        return None;
    }
    // Safe: cfg_size is now in [1, MAX_SUPPORTED_QUEUE_SIZE = 256].
    Some(cfg_size as u16)
}

/// Validate that the guest's `avail.idx` cursor has moved by **at most
/// `qsize`** entries since our last visit. The spec limits in-flight
/// entries to the queue size — any more means the guest is either
/// buggy or malicious. A huge jump would make the drain loop reuse
/// the same ring slot many times, re-sending the same frame to the
/// backend tens of thousands of times from a single kick.
///
/// `u16::wrapping_sub` makes this correct across the natural ring
/// wrap at 65535.
fn avail_delta_is_sane(last_seen: u16, avail_idx: u16, qsize: u16) -> bool {
    avail_idx.wrapping_sub(last_seen) <= qsize
}

/// Per-queue cursor the consumer carries across calls. Tracks how far
/// through the avail ring we've already drained — this is the `last
/// seen` state the virtio spec calls `last_avail_idx`.
///
/// # Rust concept: why a separate struct instead of a `u16` field
///
/// We could just store a `u16` on [`VirtioNetDevice`] directly; the
/// wrapper buys us two things. First, `Debug` + `Default` + `Clone` are
/// derived once here, not duplicated on every consumer. Second — more
/// importantly — any future per-queue state (packed-ring descriptor
/// counter, event-idx cache, …) lives next to the cursor without
/// changing the `VirtioNetDevice` API shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCursor {
    /// Driver's producer index (`avail.idx`) we've already consumed up
    /// to. Monotonic mod 2^16; the virtio spec's wrap arithmetic
    /// handles overflow.
    pub last_seen_avail: u16,
}

/// Outcome of a single process-tx or process-rx call. Returned to the
/// caller so `vm-kvm` can decide when to raise the device interrupt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessStats {
    /// Number of descriptor chains that returned to the used ring.
    pub completed_chains: usize,
    /// Number of frames actually passed to / pulled from the backend
    /// (i.e. chains that passed all validation). Always
    /// `<= completed_chains`; the difference counts chains the device
    /// returned to the driver unused because validation failed.
    pub frames_moved: usize,
}

impl ProcessStats {
    /// `true` when the device should assert the used-buffer interrupt
    /// (`VIRTIO_MMIO_INT_VRING`). The caller decides whether to actually
    /// raise it based on queue-selected notification suppression (not
    /// implemented yet; just mirror `completed_chains > 0` for now).
    pub fn should_raise_interrupt(self) -> bool {
        self.completed_chains > 0
    }
}

/// Drain the TX (guest-to-host) queue.
///
/// Walks every descriptor chain the driver has published since the
/// cursor's last run. For each chain:
///
/// 1. Concatenate the device-readable buffers (there may be several
///    chained via `DESC_F_NEXT`) into one contiguous `Vec<u8>` — this
///    is the virtio-net header followed by the Ethernet frame.
/// 2. Validate size: at least `VIRTIO_NET_HDR_LEN + MIN_FRAME_LEN`,
///    at most [`MAX_CHAIN_BYTES`]. On failure the chain still goes
///    to the used ring (the driver's descriptors need to come back)
///    but the frame is dropped silently — matching the virtio spec's
///    guidance that the device must not deadlock the driver.
/// 3. Strip the 12-byte header, call [`NetworkBackend::write_frame`].
/// 4. Publish the chain head to the used ring with `written_len = 0`
///    (TX used entries carry no bytes written per spec §2.7.8).
/// 5. Advance the cursor.
///
/// Returns per-call stats. The caller raises the device IRQ when
/// [`ProcessStats::should_raise_interrupt`] is true.
pub fn process_tx<M: GuestMemory>(
    cfg: &QueueConfig,
    cursor: &mut QueueCursor,
    mem: &mut M,
    backend: &dyn NetworkBackend,
) -> Result<ProcessStats, VirtioNetError> {
    let mut stats = ProcessStats::default();
    if !cfg.ready {
        return Ok(stats);
    }
    // Validate the guest-programmed queue size before the `as u16`
    // cast. A bogus value (0, 65536, non-power-of-two) would either
    // panic on `% qsize` or let the guest mis-configure the ring.
    // Silent skip + Ok(default stats) — don't crash the VMM for a
    // malformed queue (same posture as Firecracker).
    let Some(qsize) = validate_qsize(cfg.size) else {
        return Ok(stats);
    };

    // Snapshot the descriptor table and avail ring up front, so we
    // can walk the chains with just reads before coming back for the
    // used-ring writes. Avoids the GuestMemory `&self` vs `&mut self`
    // split halfway through the loop.
    let table = read_descriptor_table(mem, cfg.desc, qsize)?;
    let avail_idx = read_u16(mem, cfg.driver.wrapping_add(2))?;

    // Reject bogus avail-index jumps. The virtio spec caps in-flight
    // entries at `qsize`; a larger `avail.idx - cursor` means the
    // guest is lying or buggy. Loop silently drops this call — the
    // guest kicks again if it fixes itself.
    if !avail_delta_is_sane(cursor.last_seen_avail, avail_idx, qsize) {
        return Ok(stats);
    }

    while cursor.last_seen_avail != avail_idx {
        let slot = (cursor.last_seen_avail % qsize) as u64;
        let head = read_u16(mem, cfg.driver.wrapping_add(4 + slot * 2))?;

        let mut scratch = Vec::new();
        let mut chain_valid = true;

        // Walk the chain, gather the device-readable descriptors'
        // payload into `scratch`. Device-writable descriptors on a TX
        // chain are a protocol error — a conformant driver never
        // produces them — but we tolerate the mixed case by skipping
        // writable segments. The chain head is still consumed so the
        // driver gets its slot back.
        for desc in DescriptorChain::new(&table, head) {
            let desc = desc?;
            if desc.is_indirect() {
                // Indirect descriptors are a feature bit we don't
                // advertise. Treat as drop + warn (no logging layer
                // today; stats carry the signal).
                chain_valid = false;
                break;
            }
            if desc.is_writable() {
                // Shouldn't happen on TX; skip the segment.
                continue;
            }
            let would_be = scratch.len().saturating_add(desc.len as usize);
            if would_be > MAX_CHAIN_BYTES {
                chain_valid = false;
                break;
            }
            let bytes = desc.read_from(mem)?;
            scratch.extend_from_slice(&bytes);
        }

        if chain_valid
            && scratch.len() >= VIRTIO_NET_HDR_LEN + MIN_FRAME_LEN
            && scratch.len() <= MAX_CHAIN_BYTES
        {
            // Parse the header purely to validate it — we don't
            // currently honour checksum offload / GSO flags, so the
            // decoded value is thrown away. A future PR that enables
            // offloads will branch on `hdr.gso_type` here.
            let _hdr = VirtioNetHdr::from_bytes(&scratch[..VIRTIO_NET_HDR_LEN])?;
            let frame = &scratch[VIRTIO_NET_HDR_LEN..];
            backend.write_frame(frame)?;
            stats.frames_moved += 1;
        }
        // The spec's used-elem format for TX writes 0 for `written_len`
        // because TX descriptors are device-readable only (nothing
        // written back into guest buffers).
        push_used(mem, cfg.device, qsize, head as u32, 0)?;
        cursor.last_seen_avail = cursor.last_seen_avail.wrapping_add(1);
        stats.completed_chains += 1;
    }
    Ok(stats)
}

/// Fill the RX (host-to-guest) queue.
///
/// The driver has posted empty **writable** descriptor chains to the
/// RX avail ring. For each one we try to pull a frame from the
/// backend; if there's one available we prepend a 12-byte
/// `no_offload` virtio-net header, scatter-write header + frame
/// across the chain's writable descriptors, and push the head onto
/// the used ring with `written_len = 12 + frame.len()`.
///
/// Stops when either (a) no more avail entries, or (b) the backend
/// returned `Ok(0)` (no frame right now). The caller is expected to
/// call again when its readiness fd signals readable, or at the next
/// MMIO queue kick.
pub fn process_rx<M: GuestMemory>(
    cfg: &QueueConfig,
    cursor: &mut QueueCursor,
    mem: &mut M,
    backend: &dyn NetworkBackend,
) -> Result<ProcessStats, VirtioNetError> {
    let mut stats = ProcessStats::default();
    if !cfg.ready {
        return Ok(stats);
    }
    // Same validation as process_tx — reject malformed queue sizes
    // and avail-index jumps larger than qsize before touching guest
    // memory with the bad values.
    let Some(qsize) = validate_qsize(cfg.size) else {
        return Ok(stats);
    };

    let table = read_descriptor_table(mem, cfg.desc, qsize)?;
    let avail_idx = read_u16(mem, cfg.driver.wrapping_add(2))?;

    if !avail_delta_is_sane(cursor.last_seen_avail, avail_idx, qsize) {
        return Ok(stats);
    }

    // Reusable scratch for the backend's frame read, sized for the
    // largest frame we'd accept. Allocation lives for the whole
    // duration of the function.
    let mut frame_buf = vec![0u8; MAX_FRAME_LEN];

    while cursor.last_seen_avail != avail_idx {
        let n = backend.read_frame(&mut frame_buf)?;
        if n == 0 {
            // Nothing to deliver right now. Leave the avail slot for
            // the next poll — it is still available to the driver.
            break;
        }
        let frame = &frame_buf[..n];

        let slot = (cursor.last_seen_avail % qsize) as u64;
        let head = read_u16(mem, cfg.driver.wrapping_add(4 + slot * 2))?;

        // Materialize the chain's writable descriptors into a Vec so
        // we can both measure total capacity and then write each one
        // in order. We cap chain length at `table.len()` via
        // DescriptorChain's cycle guard.
        let mut writable: Vec<Descriptor> = Vec::new();
        let mut chain_valid = true;
        for desc in DescriptorChain::new(&table, head) {
            let desc = desc?;
            if desc.is_indirect() {
                chain_valid = false;
                break;
            }
            if !desc.is_writable() {
                // Non-writable segments on an RX chain — spec-wise
                // not allowed, but tolerate by skipping.
                continue;
            }
            writable.push(*desc);
        }
        let total_cap: usize = writable.iter().map(|d| d.len as usize).sum();
        let need = VIRTIO_NET_HDR_LEN + frame.len();

        if !chain_valid || total_cap < need {
            // Driver's chain is too small or malformed. Return it
            // unused (written_len = 0). The frame stays dropped —
            // backend.read_frame already consumed it. In production
            // the backend would need a one-slot pushback queue; for
            // v1 we accept rare loss here, matching Firecracker's
            // behaviour.
            push_used(mem, cfg.device, qsize, head as u32, 0)?;
            cursor.last_seen_avail = cursor.last_seen_avail.wrapping_add(1);
            stats.completed_chains += 1;
            continue;
        }

        // Build the header + frame as one contiguous Vec, then scatter
        // across the writable descriptors in chain order. Each
        // descriptor takes `min(desc.len, remaining)` bytes.
        let header = VirtioNetHdr::no_offload().to_bytes();
        let mut payload = Vec::with_capacity(need);
        payload.extend_from_slice(&header);
        payload.extend_from_slice(frame);

        let mut written = 0usize;
        for desc in &writable {
            if written == payload.len() {
                break;
            }
            let n = desc.write_to_guest(mem, &payload[written..])?;
            written += n;
        }

        push_used(mem, cfg.device, qsize, head as u32, written as u32)?;
        cursor.last_seen_avail = cursor.last_seen_avail.wrapping_add(1);
        stats.completed_chains += 1;
        stats.frames_moved += 1;
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Low-level helpers over `GuestMemory`.
//
// These exist because the `AvailRing` / `UsedRing` types in
// `virtio-queue` operate on byte slices, not on a `GuestMemory`
// accessor. Rather than copy whole rings into scratch each poll, we
// read/write the specific fields we need directly. Each helper is
// infallible beyond what `GuestMemory` already surfaces, so error
// handling collapses to a single `?`.
// ---------------------------------------------------------------------------

fn read_u16<M: GuestMemory>(mem: &M, gpa: u64) -> Result<u16, QueueError> {
    let mut b = [0u8; 2];
    mem.read(gpa, &mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_descriptor_table<M: GuestMemory>(
    mem: &M,
    base: u64,
    qsize: u16,
) -> Result<Vec<Descriptor>, QueueError> {
    let mut out = Vec::with_capacity(qsize as usize);
    let mut buf = [0u8; DESC_SIZE];
    for i in 0..qsize {
        mem.read(base.wrapping_add(i as u64 * DESC_SIZE as u64), &mut buf)?;
        out.push(Descriptor::from_bytes(&buf)?);
    }
    Ok(out)
}

/// Write one used-elem at the slot indicated by the current `used.idx`
/// and advance `used.idx` by one (wrapping mod 2^16).
///
/// `base` is the guest-physical address of the used ring. Layout:
///
/// ```text
///   base + 0   flags       : le16
///   base + 2   idx         : le16  <- advanced
///   base + 4   ring[0..n]  : {id: le32, len: le32}, 8 bytes each
///   base + 4+8*n  avail_event : le16
/// ```
fn push_used<M: GuestMemory>(
    mem: &mut M,
    base: u64,
    qsize: u16,
    head_idx: u32,
    written_len: u32,
) -> Result<(), QueueError> {
    let mut idx_bytes = [0u8; 2];
    mem.read(base.wrapping_add(2), &mut idx_bytes)?;
    let cur = u16::from_le_bytes(idx_bytes);
    let slot = (cur % qsize) as u64;
    let off = base.wrapping_add(4 + slot * 8);
    mem.write(off, &head_idx.to_le_bytes())?;
    mem.write(off.wrapping_add(4), &written_len.to_le_bytes())?;
    let new_idx = cur.wrapping_add(1);
    mem.write(base.wrapping_add(2), &new_idx.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::{RX_QUEUE_INDEX, TX_QUEUE_INDEX};
    use crate::{MockBackend, VirtioNetDevice};
    use std::sync::Arc;
    use virtio_queue::{SliceGuestMemory, DESC_F_NEXT, DESC_F_WRITE, DESC_SIZE};

    // Toy layout we use across tests. Guest memory starts at 0x1_0000
    // and holds (in order): descriptor table, avail ring, used ring,
    // then a page of buffer backing.
    const BASE: u64 = 0x1_0000;
    const QSIZE: u16 = 8;
    const DESC_BASE: u64 = BASE;
    const AVAIL_BASE: u64 = DESC_BASE + DESC_SIZE as u64 * QSIZE as u64;
    const USED_BASE: u64 = AVAIL_BASE + 4 + 2 * QSIZE as u64 + 2;
    const BUF_BASE: u64 = USED_BASE + 4 + 8 * QSIZE as u64 + 2;

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x01];

    fn fresh_mem() -> Vec<u8> {
        // Enough room for rings + 16 KiB of buffer backing.
        vec![0u8; (BUF_BASE - BASE) as usize + 16 * 1024]
    }

    fn fresh_cfg() -> QueueConfig {
        QueueConfig {
            size: QSIZE as u32,
            ready: true,
            desc: DESC_BASE,
            driver: AVAIL_BASE,
            device: USED_BASE,
        }
    }

    fn write_desc(mem: &mut SliceGuestMemory, index: u16, d: Descriptor) {
        mem.write(DESC_BASE + index as u64 * DESC_SIZE as u64, &d.to_bytes())
            .unwrap();
    }

    fn write_avail_head(mem: &mut SliceGuestMemory, slot: u16, head: u16) {
        mem.write(AVAIL_BASE + 4 + slot as u64 * 2, &head.to_le_bytes())
            .unwrap();
    }

    fn set_avail_idx(mem: &mut SliceGuestMemory, idx: u16) {
        mem.write(AVAIL_BASE + 2, &idx.to_le_bytes()).unwrap();
    }

    fn read_used_idx(mem: &SliceGuestMemory) -> u16 {
        let mut b = [0u8; 2];
        mem.read(USED_BASE + 2, &mut b).unwrap();
        u16::from_le_bytes(b)
    }

    fn read_used_elem(mem: &SliceGuestMemory, slot: u16) -> (u32, u32) {
        let off = USED_BASE + 4 + slot as u64 * 8;
        let mut id = [0u8; 4];
        let mut ln = [0u8; 4];
        mem.read(off, &mut id).unwrap();
        mem.read(off + 4, &mut ln).unwrap();
        (u32::from_le_bytes(id), u32::from_le_bytes(ln))
    }

    fn new_device() -> (VirtioNetDevice, Arc<MockBackend>) {
        let mock = Arc::new(MockBackend::new());
        let backend: Arc<dyn NetworkBackend> = Arc::clone(&mock) as Arc<dyn NetworkBackend>;
        (VirtioNetDevice::with_backend_arc(backend, MAC), mock)
    }

    #[test]
    fn tx_single_descriptor_frame_goes_to_backend() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Lay out one TX descriptor: 12-byte header + 60-byte frame.
        let payload_addr = BUF_BASE;
        let header = VirtioNetHdr::no_offload().to_bytes();
        let frame: Vec<u8> = (0..60u8).collect();
        let mut payload = Vec::new();
        payload.extend_from_slice(&header);
        payload.extend_from_slice(&frame);
        mem.write(payload_addr, &payload).unwrap();

        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: payload_addr,
                len: payload.len() as u32,
                flags: 0, // readable, no NEXT
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.completed_chains, 1);
        assert_eq!(stats.frames_moved, 1);
        assert_eq!(cursor.last_seen_avail, 1);
        assert_eq!(read_used_idx(&mem), 1);
        assert_eq!(read_used_elem(&mem, 0), (0, 0)); // TX written_len is 0.

        let delivered = mock.pop_tx().expect("backend got the frame");
        assert_eq!(
            delivered, frame,
            "header was stripped before backend got it"
        );
    }

    #[test]
    fn tx_multi_descriptor_chain_concatenates_segments() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Chain: desc0 holds the 12-byte header, desc1 holds the frame.
        let header = VirtioNetHdr::no_offload().to_bytes();
        let frame: Vec<u8> = (0..100u8).collect();
        mem.write(BUF_BASE, &header).unwrap();
        mem.write(BUF_BASE + 64, &frame).unwrap();

        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: VIRTIO_NET_HDR_LEN as u32,
                flags: DESC_F_NEXT,
                next: 1,
            },
        );
        write_desc(
            &mut mem,
            1,
            Descriptor {
                addr: BUF_BASE + 64,
                len: frame.len() as u32,
                flags: 0,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.frames_moved, 1);
        assert_eq!(mock.pop_tx().unwrap(), frame);
    }

    #[test]
    fn tx_drops_undersized_chain_but_still_frees_descriptor() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Short frame: 12-byte header + 10 bytes payload — below the
        // 60-byte Ethernet minimum we enforce. Must be dropped silently
        // but the descriptor must come back on the used ring.
        let header = VirtioNetHdr::no_offload().to_bytes();
        let mut payload = Vec::new();
        payload.extend_from_slice(&header);
        payload.extend_from_slice(&[0xAB; 10]);
        mem.write(BUF_BASE, &payload).unwrap();

        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: payload.len() as u32,
                flags: 0,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.completed_chains, 1);
        assert_eq!(stats.frames_moved, 0, "undersized frame dropped");
        assert_eq!(mock.pop_tx(), None, "nothing reaches backend");
        assert_eq!(
            read_used_idx(&mem),
            1,
            "descriptor must still return to driver"
        );
    }

    #[test]
    fn tx_rejects_oversize_chain() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Make desc 0 claim 2000 bytes (above MAX_CHAIN_BYTES=1526).
        // Buffer backing isn't large enough actually, but we only need
        // the LEN check to trip.
        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: 2000,
                flags: 0,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.frames_moved, 0);
        assert_eq!(stats.completed_chains, 1);
        assert_eq!(mock.pop_tx(), None);
    }

    #[test]
    fn tx_processes_several_chains_in_one_call() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Three independent single-desc chains at indices 0, 1, 2.
        let header = VirtioNetHdr::no_offload().to_bytes();
        for i in 0..3u16 {
            let addr = BUF_BASE + i as u64 * 128;
            let frame: Vec<u8> = vec![i as u8; 60];
            let mut p = Vec::new();
            p.extend_from_slice(&header);
            p.extend_from_slice(&frame);
            mem.write(addr, &p).unwrap();
            write_desc(
                &mut mem,
                i,
                Descriptor {
                    addr,
                    len: p.len() as u32,
                    flags: 0,
                    next: 0,
                },
            );
            write_avail_head(&mut mem, i, i);
        }
        set_avail_idx(&mut mem, 3);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.frames_moved, 3);
        assert_eq!(cursor.last_seen_avail, 3);
        assert_eq!(read_used_idx(&mem), 3);
        for i in 0u8..3 {
            let delivered = mock.pop_tx().unwrap();
            assert_eq!(delivered[0], i);
            assert_eq!(delivered.len(), 60);
        }
    }

    #[test]
    fn tx_noop_when_queue_not_ready() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        let mut cfg = fresh_cfg();
        cfg.ready = false;

        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();
        assert_eq!(stats, ProcessStats::default());
    }

    #[test]
    fn rx_single_chain_receives_header_plus_frame() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Driver supplies one writable 2 KiB buffer.
        let rx_buf_addr = BUF_BASE;
        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: rx_buf_addr,
                len: 2048,
                flags: DESC_F_WRITE,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        // Backend has one frame queued.
        let frame: Vec<u8> = (0..80u8).collect();
        mock.inject_rx(frame.clone());

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_rx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.frames_moved, 1);
        assert_eq!(cursor.last_seen_avail, 1);
        assert_eq!(read_used_idx(&mem), 1);
        let (used_head, used_len) = read_used_elem(&mem, 0);
        assert_eq!(used_head, 0);
        assert_eq!(used_len as usize, VIRTIO_NET_HDR_LEN + frame.len());

        // Verify what the guest actually sees at rx_buf_addr: 12-byte
        // no_offload header, then the Ethernet frame.
        let mut seen = vec![0u8; VIRTIO_NET_HDR_LEN + frame.len()];
        mem.read(rx_buf_addr, &mut seen).unwrap();
        assert_eq!(
            &seen[..VIRTIO_NET_HDR_LEN],
            &VirtioNetHdr::no_offload().to_bytes()
        );
        assert_eq!(&seen[VIRTIO_NET_HDR_LEN..], &frame[..]);
    }

    #[test]
    fn rx_spans_two_writable_descriptors() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Driver's chain: two writable 64-byte descriptors. A 100-byte
        // frame + 12-byte header = 112 bytes, splitting 64 + 48 across
        // the two buffers.
        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: 64,
                flags: DESC_F_WRITE | DESC_F_NEXT,
                next: 1,
            },
        );
        write_desc(
            &mut mem,
            1,
            Descriptor {
                addr: BUF_BASE + 128,
                len: 64,
                flags: DESC_F_WRITE,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        let frame: Vec<u8> = (0..100u8).collect();
        mock.inject_rx(frame.clone());

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        process_rx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        // First buffer: 12-byte header + first 52 bytes of frame.
        let mut first = vec![0u8; 64];
        mem.read(BUF_BASE, &mut first).unwrap();
        assert_eq!(
            &first[..VIRTIO_NET_HDR_LEN],
            &VirtioNetHdr::no_offload().to_bytes()
        );
        assert_eq!(&first[VIRTIO_NET_HDR_LEN..], &frame[..52]);

        // Second buffer: remaining 48 bytes of frame, then zeros.
        let mut second = vec![0u8; 64];
        mem.read(BUF_BASE + 128, &mut second).unwrap();
        assert_eq!(&second[..48], &frame[52..]);
        assert_eq!(&second[48..], &[0u8; 16]);
    }

    #[test]
    fn rx_returns_small_chain_unused_without_consuming_cursor_future_slots() {
        // Driver supplies one 20-byte writable buffer — too small for
        // header (12) + minimum frame (60). We expect: used entry
        // written with len=0, cursor advanced, no backend read for
        // this slot. The frame we injected gets lost (acceptable v1).
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: 20,
                flags: DESC_F_WRITE,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 1);

        mock.inject_rx(vec![0xAAu8; 80]);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_rx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.completed_chains, 1);
        assert_eq!(stats.frames_moved, 0);
        let (_, used_len) = read_used_elem(&mem, 0);
        assert_eq!(used_len, 0);
    }

    #[test]
    fn rx_stops_when_backend_empty() {
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Driver has two writable chains pending; backend has only one
        // frame. We should process exactly one and stop; the second
        // avail slot remains for the next poll.
        for i in 0..2u16 {
            write_desc(
                &mut mem,
                i,
                Descriptor {
                    addr: BUF_BASE + i as u64 * 2048,
                    len: 2048,
                    flags: DESC_F_WRITE,
                    next: 0,
                },
            );
            write_avail_head(&mut mem, i, i);
        }
        set_avail_idx(&mut mem, 2);
        mock.inject_rx(vec![0x55u8; 80]);

        let cfg = fresh_cfg();
        let mut cursor = QueueCursor::default();
        let stats = process_rx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        assert_eq!(stats.frames_moved, 1);
        assert_eq!(cursor.last_seen_avail, 1, "only one slot consumed");
    }

    #[test]
    fn process_stats_should_raise_interrupt_mirrors_chain_count() {
        let empty = ProcessStats::default();
        assert!(!empty.should_raise_interrupt());
        let one = ProcessStats {
            completed_chains: 1,
            frames_moved: 1,
        };
        assert!(one.should_raise_interrupt());
    }

    #[test]
    fn tx_queue_indices_are_as_expected_from_mmio() {
        assert_eq!(TX_QUEUE_INDEX, 1);
        assert_eq!(RX_QUEUE_INDEX, 0);
    }

    // --------------------------------------------------------------
    // Guest-safety hardening tests (Copilot review on PR #277).
    //
    // Each of these drives the device through a malformed-queue
    // scenario a hostile or buggy guest could set up, and asserts
    // we neither panic nor flood the backend. Posture: silent skip,
    // return default stats, keep the VMM alive.
    // --------------------------------------------------------------

    #[test]
    fn tx_rejects_zero_queue_size_without_panicking() {
        // Division by zero on `% qsize` would crash the VMM.
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        let mut cfg = fresh_cfg();
        cfg.size = 0;
        set_avail_idx(&mut mem, 7);

        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();
        assert_eq!(stats, ProcessStats::default());
        assert_eq!(mock.queue_depths(), (0, 0));
    }

    #[test]
    fn tx_rejects_oversized_queue_size_u16_wrap() {
        // 65536 as u16 == 0 — exactly the panic a hostile guest could
        // trigger. Must be caught by validate_qsize before the cast.
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        let mut cfg = fresh_cfg();
        cfg.size = 65536;
        set_avail_idx(&mut mem, 7);

        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();
        assert_eq!(stats, ProcessStats::default());
    }

    #[test]
    fn tx_rejects_non_power_of_two_queue_size() {
        // The virtio spec requires qsize to be a power of two. A guest
        // setting 7 is spec-violation territory; we skip the queue.
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        let mut cfg = fresh_cfg();
        cfg.size = 7;
        set_avail_idx(&mut mem, 3);

        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();
        assert_eq!(stats, ProcessStats::default());
    }

    #[test]
    fn tx_rejects_avail_idx_jump_exceeding_qsize() {
        // Guest publishes avail.idx = 9, queue size = 8, cursor = 0.
        // 9 - 0 > 8 => malformed. If we processed this naively we'd
        // walk ring[0]..ring[8] = ring[0..0] reusing the same desc,
        // delivering the same frame 9 times to the backend.
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        // Lay out a single legitimate TX descriptor at index 0 so IF
        // we DID loop we'd actually hand things to the backend.
        let header = VirtioNetHdr::no_offload().to_bytes();
        let frame: Vec<u8> = (0..60u8).collect();
        let mut payload = Vec::new();
        payload.extend_from_slice(&header);
        payload.extend_from_slice(&frame);
        mem.write(BUF_BASE, &payload).unwrap();
        write_desc(
            &mut mem,
            0,
            Descriptor {
                addr: BUF_BASE,
                len: payload.len() as u32,
                flags: 0,
                next: 0,
            },
        );
        write_avail_head(&mut mem, 0, 0);
        set_avail_idx(&mut mem, 9); // jump > qsize=8

        let cfg = fresh_cfg(); // qsize = 8
        let mut cursor = QueueCursor::default();
        let stats = process_tx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();

        // Nothing processed, nothing delivered, cursor untouched.
        assert_eq!(stats, ProcessStats::default());
        assert_eq!(cursor.last_seen_avail, 0);
        assert_eq!(mock.pop_tx(), None, "backend must NOT be flooded");
    }

    #[test]
    fn rx_rejects_malformed_queue_the_same_way_as_tx() {
        // Symmetric check — zero qsize, oversized qsize, bad delta
        // all three must leave rx a no-op.
        let mut raw = fresh_mem();
        let mut mem = SliceGuestMemory::new(BASE, &mut raw);
        let (_dev, mock) = new_device();

        mock.inject_rx(vec![0xAA; 80]);

        for bad_size in [0u32, 65536, 65537, 7, 5] {
            let mut cfg = fresh_cfg();
            cfg.size = bad_size;
            set_avail_idx(&mut mem, 1);
            let mut cursor = QueueCursor::default();
            let stats =
                process_rx(&cfg, &mut cursor, &mut mem, &*mock as &dyn NetworkBackend).unwrap();
            assert_eq!(
                stats,
                ProcessStats::default(),
                "qsize={bad_size} must be rejected"
            );
        }
        // Frame still sitting in backend — never got pulled.
        assert_eq!(mock.queue_depths().0, 1);
    }

    #[test]
    fn validate_qsize_accepts_only_valid_values() {
        // Direct unit test on the helper. Spec: power of two in 1..=256.
        for good in [1u32, 2, 4, 8, 16, 32, 64, 128, 256] {
            assert_eq!(validate_qsize(good), Some(good as u16), "qsize={good}");
        }
        for bad in [0u32, 3, 5, 7, 100, 257, 1024, 65536, u32::MAX] {
            assert_eq!(validate_qsize(bad), None, "qsize={bad}");
        }
    }

    #[test]
    fn avail_delta_wraps_correctly_at_u16_max() {
        // Cursor near wrap, avail just past: wrapping_sub gives the
        // small real delta even though the raw numbers look huge.
        // last_seen = 65_530, avail_idx = 2 → 2.wrapping_sub(65_530)
        //           = 2 + (65_536 - 65_530) = 8. qsize=8 → ≤ 8 ✓
        assert!(avail_delta_is_sane(65_530, 2, 8));
        // delta = 7 → ≤ 8 ✓
        assert!(avail_delta_is_sane(65_530, 1, 8));
        // Delta exactly qsize is allowed.
        assert!(avail_delta_is_sane(0, 8, 8));
        // Delta > qsize is not.
        assert!(!avail_delta_is_sane(0, 9, 8));
        // last_seen=65_530, avail_idx=3 → delta = 9 → > 8 ✗
        assert!(!avail_delta_is_sane(65_530, 3, 8));
        assert!(!avail_delta_is_sane(65_530, 100, 8));
    }

    #[test]
    fn max_frame_len_covers_vlan_tagged_ethernet() {
        // Copilot MEDIUM: 802.1Q VLAN tag adds 4 bytes.
        // Untagged max = 14 + 1500 = 1514
        // Tagged   max = 14 + 4 + 1500 = 1518
        // We must accept the tagged case to not drop legit VLAN traffic.
        assert_eq!(MAX_FRAME_LEN, 1518);
        assert_eq!(MAX_CHAIN_BYTES, 1518 + VIRTIO_NET_HDR_LEN);
    }
}
