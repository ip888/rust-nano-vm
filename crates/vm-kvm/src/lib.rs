//! KVM-backed [`Hypervisor`] implementation.
//!
//! Scope: M0 ships only the skeleton. Real boot (kernel load, vCPU run,
//! serial console) arrives in M1 behind the `kvm` feature flag. Keeping the
//! heavy dependencies (`kvm-ioctls`, `vm-memory`, `linux-loader`) off the
//! default build ensures `cargo build --workspace` stays portable — in
//! particular this crate compiles cleanly in the gVisor sandbox that has no
//! `/dev/kvm`.
//!
//! `vm-kvm` now contains a small amount of required `unsafe` code for the
//! KVM userspace ABI: registering guest RAM requires passing a host virtual
//! address to `KVM_SET_USER_MEMORY_REGION`, and the safety invariants are
//! documented at each call site.

#![warn(missing_docs)]

#[cfg(feature = "kvm")]
mod seccomp;
#[cfg(feature = "kvm")]
mod vmstate;

// Public re-export so integration tests (and downstream callers that
// want to install the sandbox before opening /dev/kvm themselves)
// can reach `install_default_filter` without going through the
// `KvmHypervisor::new` env-var bootstrap.
#[cfg(feature = "kvm")]
pub use seccomp::install_default_filter;

use vm_core::{
    GuestExecRequest, GuestExecResult, Hypervisor, SnapshotId, SnapshotMeta, VmConfig, VmError,
    VmHandle, VmId, VmMeta, VmResult, VmState,
};

#[cfg(feature = "kvm")]
use std::collections::HashMap;
#[cfg(feature = "kvm")]
use std::fs::File;
#[cfg(feature = "kvm")]
use std::io::{self, Read, Write};
#[cfg(feature = "kvm")]
use std::mem;
#[cfg(feature = "kvm")]
use std::path::{Path, PathBuf};
#[cfg(feature = "kvm")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "kvm")]
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[cfg(feature = "kvm")]
use std::thread::{self, JoinHandle};

#[cfg(feature = "kvm")]
use kvm_bindings::{
    kvm_fpu, kvm_pit_config, kvm_regs, kvm_segment, kvm_sregs, kvm_userspace_memory_region,
    KVM_MAX_CPUID_ENTRIES, KVM_PIT_SPEAKER_DUMMY,
};
#[cfg(feature = "kvm")]
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
#[cfg(feature = "kvm")]
use libc::{c_int, c_void, siginfo_t, EINTR};
#[cfg(feature = "kvm")]
use linux_loader::cmdline::Cmdline;
#[cfg(feature = "kvm")]
use linux_loader::configurator::linux::LinuxBootConfigurator;
#[cfg(feature = "kvm")]
use linux_loader::configurator::{BootConfigurator, BootParams};
#[cfg(feature = "kvm")]
use linux_loader::loader::bootparam::{boot_params, setup_header};
#[cfg(feature = "kvm")]
use linux_loader::loader::{load_cmdline, BzImage, KernelLoader};
#[cfg(feature = "kvm")]
use snapshot::Manifest;
#[cfg(feature = "kvm")]
use vm_memory::mmap::MmapRegion;
#[cfg(feature = "kvm")]
use vm_memory::{
    Address, Bytes, FileOffset, GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap, MemoryRegionAddress,
};
#[cfg(feature = "kvm")]
use vmm_sys_util::signal::{register_signal_handler, Killable};

/// KVM-backed hypervisor.
///
/// On platforms without `/dev/kvm` (non-Linux or the `kvm` feature disabled)
/// every method returns [`VmError::Unsupported`]. This lets the rest of the
/// workspace depend on the type unconditionally.
#[cfg(feature = "kvm")]
#[derive(Debug)]
pub struct KvmHypervisor {
    kvm: Kvm,
    inner: Mutex<Inner>,
    kick_signal: c_int,
    /// MSR indices to capture in a snapshot, queried once from the kernel.
    msr_indices: Arc<Vec<u32>>,
}

/// Non-KVM build of [`KvmHypervisor`]. All methods return
/// [`VmError::Unsupported`]; build with `--features kvm` on a Linux
/// host with `/dev/kvm` to enable the real backend.
#[cfg(not(feature = "kvm"))]
#[derive(Debug, Default)]
pub struct KvmHypervisor {
    _private: (),
}

#[cfg(feature = "kvm")]
#[derive(Debug, Default)]
struct Inner {
    vms: HashMap<VmId, KvmVm>,
    /// Captured snapshots, keyed by id → on-disk directory + metadata.
    snapshots: HashMap<SnapshotId, SnapshotEntry>,
}

/// A snapshot the hypervisor captured this session.
#[cfg(feature = "kvm")]
#[derive(Debug, Clone)]
struct SnapshotEntry {
    dir: PathBuf,
    meta: SnapshotMeta,
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVm {
    config: VmConfig,
    state: VmState,
    runtime: KvmVmRuntime,
    vcpu: Option<KvmVcpuThread>,
    last_run_error: Option<String>,
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVmRuntime {
    vm_fd: Arc<VmFd>,
    boot_plan: KvmBootPlan,
    entry_point: GuestAddress,
    serial_output: Arc<Mutex<Vec<u8>>>,
    /// When `true`, the vCPU starts in 16-bit real mode at
    /// `CS:IP = 0000:0000`. Used by [`VmConfig::flat_binary`].
    /// `false` selects the existing protected-mode Linux boot path.
    real_mode: bool,
    /// virtio-MMIO vsock device, present when `VmConfig.vsock_cid` is
    /// set. Shared with the vCPU thread, which routes MMIO exits in
    /// the device's register window into it and injects the device
    /// IRQ when the device completes virtqueue buffers. `None` = no
    /// vsock device.
    vsock: Option<VsockBackend>,
}

/// The host side of the virtio-vsock device, shared between the
/// hypervisor (which inspects status / drives the host stream API)
/// and the vCPU thread (which services MMIO exits and injects IRQs).
///
/// Cloning is cheap: the device sits behind an `Arc<Mutex<_>>`, the
/// guest memory handle shares its mappings, and the `VmFd` is shared
/// for `set_irq_line`.
#[cfg(feature = "kvm")]
#[derive(Debug, Clone)]
struct VsockBackend {
    device: Arc<Mutex<virtio_vsock::VsockDevice>>,
    guest_mem: GuestMemoryMmap,
    vm_fd: Arc<VmFd>,
    irq: u32,
}

#[cfg(feature = "kvm")]
impl VsockBackend {
    /// If `addr` is within the device's MMIO register window, return the
    /// register offset; otherwise `None`.
    fn window_offset(&self, addr: u64) -> Option<u64> {
        let offset = addr.checked_sub(VSOCK_MMIO_BASE)?;
        (offset < VSOCK_MMIO_SIZE).then_some(offset)
    }

    fn lock_device(&self) -> std::sync::MutexGuard<'_, virtio_vsock::VsockDevice> {
        self.device
            .lock()
            .expect("vm-kvm: vsock device mutex poisoned")
    }

    /// Service a guest MMIO read from the register window into `data`.
    fn read(&self, offset: u64, data: &mut [u8]) {
        let val = self.lock_device().mmio_read(offset, data.len());
        let bytes = val.to_le_bytes();
        for (i, slot) in data.iter_mut().enumerate() {
            *slot = bytes.get(i).copied().unwrap_or(0);
        }
    }

    /// Service a guest MMIO write to the register window. A write that
    /// kicks a virtqueue (`QueueNotify`) drives one device cycle; if that
    /// completes buffers, inject the device IRQ.
    fn write(&self, offset: u64, data: &[u8]) -> VmResult<()> {
        let mut buf = [0u8; 8];
        for (i, b) in data.iter().take(buf.len()).enumerate() {
            buf[i] = *b;
        }
        let value = u64::from_le_bytes(buf);

        let mut device = self.lock_device();
        let notified = device.mmio_write(offset, data.len(), value);
        if notified.is_none() {
            return Ok(());
        }
        // The guest kicked a queue — run the device against guest memory.
        let mem = GuestRamMem(&self.guest_mem);
        match device.process(&mem) {
            Ok(true) => {
                drop(device);
                self.pulse_irq()?;
            }
            Ok(false) => {}
            Err(e) => eprintln!("vm-kvm: vsock device process error: {e}"),
        }
        Ok(())
    }

    /// The id of the guest agent's connection once it has connected, or
    /// `None` if no connection is established yet.
    fn established_connection(&self) -> Option<virtio_vsock::ConnectionId> {
        self.lock_device().established_connection()
    }

    /// Frame already-encoded `bytes` to the guest on `conn`, then run a
    /// device cycle so the data lands in the guest's rx ring and inject
    /// the IRQ if any buffer completed.
    fn host_send(&self, conn: virtio_vsock::ConnectionId, bytes: &[u8]) -> VmResult<()> {
        let mut device = self.lock_device();
        if !device.send(conn, bytes) {
            return Err(VmError::Backend(
                "vm-kvm: vsock send on a non-established connection".into(),
            ));
        }
        let mem = GuestRamMem(&self.guest_mem);
        let completed = device
            .process(&mem)
            .map_err(|e| VmError::Backend(format!("vm-kvm: vsock process during send: {e}")))?;
        drop(device);
        if completed {
            self.pulse_irq()?;
        }
        Ok(())
    }

    /// Append any guest→host stream payloads received so far to `buf`.
    /// The vCPU thread fills the inbound queue as the guest sends; this
    /// drains it. Returns the number of bytes appended.
    fn drain_inbound(&self, buf: &mut Vec<u8>) -> usize {
        let mut device = self.lock_device();
        let before = buf.len();
        while let Some((_conn, data)) = device.recv() {
            buf.extend_from_slice(&data);
        }
        buf.len() - before
    }

    /// Inject an edge on the device's IRQ line. GSI 5 is a legacy ISA
    /// line on the in-kernel PIC (edge-triggered), so raise-then-lower
    /// delivers one interrupt; the guest's ISR reads `InterruptStatus`,
    /// drains the used rings, and writes `InterruptACK`.
    fn pulse_irq(&self) -> VmResult<()> {
        self.vm_fd
            .set_irq_line(self.irq, true)
            .map_err(|e| VmError::Backend(format!("assert vsock IRQ {}: {e}", self.irq)))?;
        self.vm_fd
            .set_irq_line(self.irq, false)
            .map_err(|e| VmError::Backend(format!("deassert vsock IRQ {}: {e}", self.irq)))?;
        Ok(())
    }
}

/// Drive one host→guest exec RPC over the vsock device: wait for the
/// agent to connect, send a one-shot `Exec` request, and read back the
/// `ExecResult`.
///
/// Runs on the caller's thread (not the vCPU thread). The vCPU thread
/// fills the inbound queue as the guest agent replies; we poll it. All
/// device access is serialized by the device mutex inside `backend`.
#[cfg(feature = "kvm")]
fn exec_over_vsock(backend: &VsockBackend, req: GuestExecRequest) -> VmResult<GuestExecResult> {
    use std::time::{Duration, Instant};

    // 1. Wait for the guest agent to connect out to our listener.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let conn = loop {
        if let Some(c) = backend.established_connection() {
            break c;
        }
        if Instant::now() >= connect_deadline {
            return Err(VmError::Backend(
                "vm-kvm: guest agent did not connect over vsock within 15s".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // 2. Encode and send the one-shot Exec request.
    let request = proto::Request {
        version: proto::PROTOCOL_VERSION,
        id: proto::RequestId(1),
        body: proto::RequestBody::Exec {
            program: req.program,
            args: req.args,
            cwd: req.cwd,
            env: req.env,
            timeout_ms: req.timeout_ms,
        },
    };
    let mut frame = Vec::new();
    proto::frame::encode_request(&request, &mut frame)
        .map_err(|e| VmError::Backend(format!("vm-kvm: encode exec request: {e}")))?;
    backend.host_send(conn, &frame)?;

    // 3. Reassemble and decode the response. A one-shot Exec yields a
    //    single Response frame, possibly split across several rx packets.
    let reply_timeout = req
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(30))
        // Allow headroom over the guest-side timeout for transport latency.
        .saturating_add(Duration::from_secs(5));
    let reply_deadline = Instant::now() + reply_timeout;
    let mut buf = Vec::new();
    loop {
        backend.drain_inbound(&mut buf);
        match proto::frame::decode_response(&buf) {
            Ok((resp, _consumed)) => return response_to_exec_result(resp),
            Err(e) if e.is_incomplete() => {
                if Instant::now() >= reply_deadline {
                    return Err(VmError::Backend(
                        "vm-kvm: timed out waiting for the agent's exec response".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(VmError::Backend(format!(
                    "vm-kvm: decode exec response: {e}"
                )))
            }
        }
    }
}

/// Map the agent's `Response` to a [`GuestExecResult`], surfacing an
/// `RpcError` or an unexpected response body as a backend error.
#[cfg(feature = "kvm")]
fn response_to_exec_result(resp: proto::Response) -> VmResult<GuestExecResult> {
    match resp.result {
        Ok(proto::ResponseBody::ExecResult {
            exit_code,
            signal,
            stdout,
            stderr,
            duration_ms,
        }) => Ok(GuestExecResult {
            exit_code,
            signal,
            stdout,
            stderr,
            duration_ms,
        }),
        Ok(other) => Err(VmError::Backend(format!(
            "vm-kvm: unexpected exec response body: {other:?}"
        ))),
        Err(rpc) => Err(VmError::Backend(format!(
            "vm-kvm: agent exec error [{:?}]: {}",
            rpc.code, rpc.message
        ))),
    }
}

/// Send one [`proto::Request`] over vsock and wait for the matching
/// single [`proto::Response`] frame. Mirrors the connect-send-receive
/// dance in [`exec_over_vsock`] but for fixed-shape RPCs (WriteFile /
/// ReadFile) where the timeout is the call's wall-clock budget rather
/// than a guest-side process deadline.
///
/// The device mutex inside `backend` serializes calls — successive
/// requests can reuse `RequestId(1)` because they never overlap.
#[cfg(feature = "kvm")]
fn rpc_oneshot(
    backend: &VsockBackend,
    body: proto::RequestBody,
    timeout: std::time::Duration,
) -> VmResult<proto::ResponseBody> {
    use std::time::{Duration, Instant};

    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let conn = loop {
        if let Some(c) = backend.established_connection() {
            break c;
        }
        if Instant::now() >= connect_deadline {
            return Err(VmError::Backend(
                "vm-kvm: guest agent did not connect over vsock within 15s".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let request = proto::Request {
        version: proto::PROTOCOL_VERSION,
        id: proto::RequestId(1),
        body,
    };
    let mut frame = Vec::new();
    proto::frame::encode_request(&request, &mut frame)
        .map_err(|e| VmError::Backend(format!("vm-kvm: encode rpc request: {e}")))?;
    backend.host_send(conn, &frame)?;

    let reply_deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    loop {
        backend.drain_inbound(&mut buf);
        match proto::frame::decode_response(&buf) {
            Ok((resp, _consumed)) => return rpc_response_body(resp),
            Err(e) if e.is_incomplete() => {
                if Instant::now() >= reply_deadline {
                    return Err(VmError::Backend(
                        "vm-kvm: timed out waiting for the agent's rpc response".into(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(VmError::Backend(format!(
                    "vm-kvm: decode rpc response: {e}"
                )))
            }
        }
    }
}

/// Lift a [`proto::Response`] into its body, surfacing any guest-side
/// `RpcError` as a `VmError::Backend` carrying the stable error code
/// so callers can match on it (instead of grepping the message).
#[cfg(feature = "kvm")]
fn rpc_response_body(resp: proto::Response) -> VmResult<proto::ResponseBody> {
    match resp.result {
        Ok(body) => Ok(body),
        Err(rpc) => Err(VmError::Backend(format!(
            "vm-kvm: agent rpc error [{:?}]: {}",
            rpc.code, rpc.message
        ))),
    }
}

/// Streaming exec over vsock: drives a guest `ExecStart` and yields
/// [`vm_core::ExecFrame`]s as the agent's response frames arrive.
///
/// One [`KvmExecStream`] is one logical exec. The stream is fed by
/// polling the shared [`VsockBackend`]: each `next_frame` call drains
/// any newly-buffered vsock bytes, tries to decode a response, and
/// either yields an `ExecFrame` or sleeps and retries.
///
/// Cancellation note: dropping the stream stops further polling but
/// does NOT kill the guest-side child. That requires sending a
/// `RequestBody::Signal` over the same vsock (out of scope for v1 —
/// the SSE handler in the control plane doesn't surface client
/// disconnect to the stream yet).
#[cfg(feature = "kvm")]
struct KvmExecStream {
    backend: VsockBackend,
    buf: Vec<u8>,
    /// Set once we've yielded the terminal `ExecExited` frame so
    /// further `next_frame` calls return `Ok(None)` instead of
    /// blocking on a guest that's already done talking.
    done: bool,
}

#[cfg(feature = "kvm")]
impl std::fmt::Debug for KvmExecStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmExecStream")
            .field("buf_len", &self.buf.len())
            .field("done", &self.done)
            .finish()
    }
}

#[cfg(feature = "kvm")]
impl vm_core::ExecStream for KvmExecStream {
    fn next_frame(&mut self) -> VmResult<Option<vm_core::ExecFrame>> {
        use std::time::Duration;
        if self.done {
            return Ok(None);
        }
        loop {
            // Try to peel off one complete frame from whatever bytes
            // we already buffered.
            match proto::frame::decode_response(&self.buf) {
                Ok((resp, consumed)) => {
                    self.buf.drain(..consumed);
                    match resp.result {
                        Ok(proto::ResponseBody::ExecStarted { .. }) => {
                            // First frame of a stream — confirms the
                            // child spawned. We don't surface a pid to
                            // the trait, so keep going.
                            continue;
                        }
                        Ok(proto::ResponseBody::ExecOutput { stream, data, .. }) => {
                            let frame = match stream {
                                proto::StdStream::Stdout => vm_core::ExecFrame::Stdout(data),
                                proto::StdStream::Stderr => vm_core::ExecFrame::Stderr(data),
                            };
                            return Ok(Some(frame));
                        }
                        Ok(proto::ResponseBody::ExecExited {
                            exit_code,
                            signal,
                            duration_ms,
                            ..
                        }) => {
                            self.done = true;
                            return Ok(Some(vm_core::ExecFrame::Exit {
                                exit_code,
                                signal,
                                duration_ms,
                            }));
                        }
                        Ok(other) => {
                            // Anything else (Pong, Written, …) means
                            // we got crossed wires with another RPC.
                            // Mark done so we don't keep spinning.
                            self.done = true;
                            return Err(VmError::Backend(format!(
                                "vm-kvm: unexpected response on exec stream: {other:?}"
                            )));
                        }
                        Err(rpc) => {
                            self.done = true;
                            return Err(VmError::Backend(format!(
                                "vm-kvm: agent exec_stream error [{:?}]: {}",
                                rpc.code, rpc.message
                            )));
                        }
                    }
                }
                Err(e) if e.is_incomplete() => {
                    // Need more bytes — drain whatever the vCPU
                    // thread has pushed, then sleep briefly so we
                    // don't spin the CPU.
                    self.backend.drain_inbound(&mut self.buf);
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    self.done = true;
                    return Err(VmError::Backend(format!(
                        "vm-kvm: decode exec stream frame: {e}"
                    )));
                }
            }
        }
    }
}

/// Send a streaming `ExecStart` over vsock and return a
/// [`KvmExecStream`] that yields frames as they arrive. Mirrors the
/// connect-encode-send opening of [`exec_over_vsock`]; the streaming
/// reply loop lives in `KvmExecStream::next_frame`.
#[cfg(feature = "kvm")]
fn exec_stream_over_vsock(
    backend: &VsockBackend,
    req: GuestExecRequest,
) -> VmResult<Box<dyn vm_core::ExecStream>> {
    use std::time::{Duration, Instant};

    let connect_deadline = Instant::now() + Duration::from_secs(15);
    let conn = loop {
        if let Some(c) = backend.established_connection() {
            break c;
        }
        if Instant::now() >= connect_deadline {
            return Err(VmError::Backend(
                "vm-kvm: guest agent did not connect over vsock within 15s".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // `ExecStart` doesn't carry timeout_ms — the agent treats the
    // child as long-running by design. If a caller-supplied
    // `timeout_ms` becomes important later we'd add a host-side
    // deadline in `KvmExecStream::next_frame`.
    let _ = req.timeout_ms;
    let request = proto::Request {
        version: proto::PROTOCOL_VERSION,
        id: proto::RequestId(1),
        body: proto::RequestBody::ExecStart {
            program: req.program,
            args: req.args,
            cwd: req.cwd,
            env: req.env,
        },
    };
    let mut frame = Vec::new();
    proto::frame::encode_request(&request, &mut frame)
        .map_err(|e| VmError::Backend(format!("vm-kvm: encode exec_start: {e}")))?;
    backend.host_send(conn, &frame)?;

    Ok(Box::new(KvmExecStream {
        backend: backend.clone(),
        buf: Vec::new(),
        done: false,
    }))
}

/// Adapts `vm-memory`'s guest RAM to the virtqueue layer's [`GuestRam`]
/// trait. A newtype is required because both the trait and
/// `GuestMemoryMmap` are foreign to this crate (orphan rule).
#[cfg(feature = "kvm")]
struct GuestRamMem<'a>(&'a GuestMemoryMmap);

#[cfg(feature = "kvm")]
impl virtio_vsock::GuestRam for GuestRamMem<'_> {
    fn read_at(&self, gpa: u64, buf: &mut [u8]) -> Result<(), virtio_vsock::QueueError> {
        self.0.read_slice(buf, GuestAddress(gpa)).map_err(|_| {
            virtio_vsock::QueueError::OutOfBounds {
                gpa,
                len: buf.len(),
            }
        })
    }

    fn write_at(&self, gpa: u64, buf: &[u8]) -> Result<(), virtio_vsock::QueueError> {
        self.0.write_slice(buf, GuestAddress(gpa)).map_err(|_| {
            virtio_vsock::QueueError::OutOfBounds {
                gpa,
                len: buf.len(),
            }
        })
    }
}

#[cfg(feature = "kvm")]
#[derive(Debug)]
struct KvmVcpuThread {
    control: Arc<VcpuControl>,
    handle: JoinHandle<VmResult<()>>,
}

/// Coordination channel between the hypervisor and a running vCPU thread.
///
/// `stop` and `pause` are polled by the loop; a kick signal (`SIGRTMIN`)
/// breaks the vCPU out of a blocking `KVM_RUN` so it notices a flag
/// promptly. On a pause request the thread captures its own [`VcpuState`]
/// (only it owns the `VcpuFd`) into `slot`, signals `cvar`, and parks until
/// the controller takes the state and sets `resume`.
#[cfg(feature = "kvm")]
#[derive(Debug, Default)]
struct VcpuControl {
    stop: AtomicBool,
    pause: AtomicBool,
    slot: Mutex<PauseSlot>,
    cvar: Condvar,
}

/// The pause hand-off slot guarded by [`VcpuControl::slot`].
#[cfg(feature = "kvm")]
#[derive(Debug, Default)]
struct PauseSlot {
    /// Set by the vCPU thread once parked: `Ok(state)` or `Err(msg)` if the
    /// capture ioctls failed. The controller `take()`s it.
    captured: Option<Result<vmstate::VcpuState, String>>,
    /// `true` while the thread is parked in the pause wait loop.
    parked: bool,
    /// Set by the controller to release a parked thread.
    resume: bool,
}

impl KvmHypervisor {
    /// Construct a new KVM hypervisor handle.
    ///
    /// When `NANOVM_SECCOMP=1` is set, also installs a default
    /// seccomp-BPF filter ([`seccomp::install_default_filter`])
    /// before opening `/dev/kvm`. Filter install happens *first* so
    /// any subsequent KVM ioctl crossing the sandbox is the first
    /// real check that the allow-list is wide enough — failing fast
    /// at startup beats failing under load.
    #[cfg(feature = "kvm")]
    pub fn new() -> VmResult<Self> {
        if seccomp::env_opts_in() {
            seccomp::install_default_filter()?;
        }
        let kvm = KvmBootPlan::open_kvm()?;
        let msr_indices = Arc::new(vmstate::snapshotable_msr_indices(&kvm)?);
        Ok(Self {
            kvm,
            inner: Mutex::new(Inner::default()),
            kick_signal: vcpu_kick_signal()?,
            msr_indices,
        })
    }

    /// Construct a non-KVM build handle; all methods return
    /// [`VmError::Unsupported`].
    #[cfg(not(feature = "kvm"))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the bytes the guest has emitted to the serial port
    /// (COM1, `0x3f8`) so far. Returns an empty `Vec` if the VM has
    /// not written anything; returns [`VmError::UnknownVm`] if `id`
    /// is not tracked. Used by tests / integration harnesses that
    /// need to assert what the guest produced.
    #[cfg(feature = "kvm")]
    pub fn serial_output(&self, id: VmId) -> VmResult<Vec<u8>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        let buf = vm
            .runtime
            .serial_output
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: serial output mutex poisoned".into()))?;
        Ok(buf.clone())
    }

    /// Non-KVM build: returns [`VmError::Unsupported`].
    #[cfg(not(feature = "kvm"))]
    pub fn serial_output(&self, _id: VmId) -> VmResult<Vec<u8>> {
        Err(VmError::Unsupported(
            "vm-kvm: serial_output requires the `kvm` feature",
        ))
    }

    /// The reason the VM's vCPU thread last terminated, if it has.
    /// `None` while still running or never started; `Some("")` after
    /// a clean `HLT`; `Some(msg)` carrying the diagnostic (e.g. a
    /// triple-fault register dump) when the vCPU stopped abnormally.
    /// Used by tests / triage to see *why* a guest stopped.
    #[cfg(feature = "kvm")]
    pub fn last_run_error(&self, id: VmId) -> VmResult<Option<String>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(vm.last_run_error.clone())
    }

    /// Non-KVM build: returns [`VmError::Unsupported`].
    #[cfg(not(feature = "kvm"))]
    pub fn last_run_error(&self, _id: VmId) -> VmResult<Option<String>> {
        Err(VmError::Unsupported(
            "vm-kvm: last_run_error requires the `kvm` feature",
        ))
    }

    /// The vsock device's virtio status register, if this VM has a
    /// vsock device (`VmConfig.vsock_cid` was set). `None` when there
    /// is no device. The guest's virtio core sets `ACKNOWLEDGE` |
    /// `DRIVER` once it recognizes the device during probe, so a
    /// non-zero status is a host-observable signal that the guest
    /// discovered our virtio-vsock device.
    #[cfg(feature = "kvm")]
    pub fn vsock_status(&self, id: VmId) -> VmResult<Option<u32>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(vm
            .runtime
            .vsock
            .as_ref()
            .map(|b| b.device.lock().map(|d| d.transport().status()).unwrap_or(0)))
    }

    /// Non-KVM build: returns [`VmError::Unsupported`].
    #[cfg(not(feature = "kvm"))]
    pub fn vsock_status(&self, _id: VmId) -> VmResult<Option<u32>> {
        Err(VmError::Unsupported(
            "vm-kvm: vsock_status requires the `kvm` feature",
        ))
    }

    /// `true` once the guest's vsock driver has finished setup and set
    /// `DRIVER_OK` (it negotiated features and programmed the
    /// virtqueues). `None` when this VM has no vsock device. Unlike
    /// [`vsock_status`](Self::vsock_status) — which only needs the
    /// generic virtio-MMIO core to probe the device — `DRIVER_OK`
    /// requires a real in-guest virtio-vsock driver
    /// (`CONFIG_VIRTIO_VSOCKETS`), so it's the signal that the data
    /// path is live.
    #[cfg(feature = "kvm")]
    pub fn vsock_driver_ok(&self, id: VmId) -> VmResult<Option<bool>> {
        let inner = self.lock_inner()?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(vm.runtime.vsock.as_ref().map(|b| {
            b.device
                .lock()
                .map(|d| d.transport().driver_ok())
                .unwrap_or(false)
        }))
    }

    /// Non-KVM build: returns [`VmError::Unsupported`].
    #[cfg(not(feature = "kvm"))]
    pub fn vsock_driver_ok(&self, _id: VmId) -> VmResult<Option<bool>> {
        Err(VmError::Unsupported(
            "vm-kvm: vsock_driver_ok requires the `kvm` feature",
        ))
    }

    /// Build a minimal, compile-time-validated boot plan without touching
    /// `/dev/kvm`.
    ///
    /// This is the first M1 slice we can land in a non-KVM sandbox: it proves
    /// the feature-gated dependencies compile together and that we can derive
    /// guest memory / command-line structures from [`VmConfig`].
    #[cfg(feature = "kvm")]
    #[allow(dead_code)]
    pub(crate) fn boot_plan(cfg: &VmConfig) -> VmResult<KvmBootPlan> {
        KvmBootPlan::from_config(cfg)
    }
}

#[cfg(feature = "kvm")]
impl KvmHypervisor {
    fn lock_inner(&self) -> VmResult<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: hypervisor state mutex poisoned".into()))
    }

    fn build_runtime(&self, cfg: &VmConfig) -> VmResult<KvmVmRuntime> {
        if cfg.snapshot_dir.is_some() {
            return Err(VmError::Unsupported(
                "vm-kvm: snapshot restore is not implemented in M1",
            ));
        }
        if cfg.vcpus != 1 {
            return Err(VmError::Unsupported(
                "vm-kvm: M1 currently supports exactly one vCPU",
            ));
        }
        if cfg.flat_binary.is_some() && cfg.kernel.is_some() {
            return Err(VmError::Backend(
                "vm-kvm: VmConfig.kernel and VmConfig.flat_binary are mutually exclusive".into(),
            ));
        }
        if let Some(bytes) = cfg.flat_binary.as_ref() {
            return self.build_flat_runtime(cfg, bytes);
        }
        let kernel = cfg
            .kernel
            .as_ref()
            .ok_or_else(|| VmError::Backend("vm-kvm: kernel path is required".into()))?;
        let boot_plan = KvmBootPlan::from_config(cfg)?;
        let vm_fd = Arc::new(
            self.kvm
                .create_vm()
                .map_err(|e| VmError::Backend(format!("create VM: {e}")))?,
        );
        vm_fd
            .set_tss_address(
                usize::try_from(KVM_TSS_ADDRESS)
                    .map_err(|_| VmError::Backend("vm-kvm: TSS address overflow".into()))?,
            )
            .map_err(|e| VmError::Backend(format!("set TSS address: {e}")))?;
        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm_fd
            .create_irq_chip()
            .map_err(|e| VmError::Backend(format!("create irqchip: {e}")))?;
        vm_fd
            .create_pit2(pit_config)
            .map_err(|e| VmError::Backend(format!("create PIT: {e}")))?;
        register_guest_memory(&vm_fd, &boot_plan.guest_mem)?;

        let mut kernel_file = File::open(kernel)
            .map_err(|e| VmError::Backend(format!("open kernel {}: {e}", kernel.display())))?;
        let kernel_load = BzImage::load(
            &boot_plan.guest_mem,
            Some(boot_plan.kernel_load_addr),
            &mut kernel_file,
            Some(GuestAddress(HIMEM_START)),
        )
        .map_err(|e| VmError::Backend(format!("load bzImage {}: {e}", kernel.display())))?;
        // linux-loader returns the load base in `kernel_load`; the
        // 64-bit entry (`startup_64`) is BZIMAGE_64BIT_ENTRY_OFFSET
        // past it. We enter the vCPU already in long mode, so jumping
        // to the load base (offset 0 = the 32-bit `startup_32`) would
        // decode 32-bit boot code as 64-bit instructions and triple-
        // fault. Add the offset to land on `startup_64`.
        let entry_point = GuestAddress(
            kernel_load
                .kernel_load
                .raw_value()
                .checked_add(BZIMAGE_64BIT_ENTRY_OFFSET)
                .ok_or_else(|| VmError::Backend("vm-kvm: entry point overflow".into()))?,
        );
        // Diagnostic: surface where linux-loader placed the kernel and
        // the computed 64-bit entry. Shown under `cargo test
        // --nocapture`; cheap and only on the kernel-boot path.
        eprintln!(
            "vm-kvm: bzImage loaded — load_base={:#x} entry(startup_64)={:#x} himem={:#x}",
            kernel_load.kernel_load.raw_value(),
            entry_point.raw_value(),
            HIMEM_START,
        );
        let cmdline_size = boot_plan.cmdline_size()?;
        load_cmdline(
            &boot_plan.guest_mem,
            GuestAddress(CMDLINE_START),
            &boot_plan.cmdline,
        )
        .map_err(|e| VmError::Backend(format!("load kernel cmdline: {e}")))?;
        // Optional initramfs: load high in guest RAM and tell the
        // kernel about it via the boot params' ramdisk fields.
        let initrd = match cfg.initrd.as_ref() {
            Some(path) => Some(load_initrd(&boot_plan.guest_mem, path)?),
            None => None,
        };
        if let Some((addr, size)) = initrd {
            eprintln!(
                "vm-kvm: initramfs loaded — addr={:#x} size={size}",
                addr.raw_value(),
            );
        }
        configure_linux_boot(
            &boot_plan.guest_mem,
            kernel_load.setup_header,
            GuestAddress(CMDLINE_START),
            cmdline_size,
            initrd,
        )?;

        // virtio-MMIO vsock device. The guest discovers it via the
        // `virtio_mmio.device=` cmdline param that from_config appends
        // when vsock_cid is set (the kernel has no device-tree/ACPI in
        // this minimal boot). The device is shared with the vCPU
        // thread, which routes MMIO exits in VSOCK_MMIO_BASE..+SIZE
        // into it and injects VSOCK_MMIO_IRQ when virtqueue buffers
        // complete.
        let vsock = cfg.vsock_cid.map(|cid| {
            eprintln!("vm-kvm: virtio-vsock device at {VSOCK_MMIO_BASE:#x} (guest_cid={cid})",);
            let mut device = virtio_vsock::VsockDevice::new(
                u64::from(cid),
                virtio_vsock::TableConfig {
                    local_cid: virtio_vsock::HOST_CID,
                    default_buf_alloc: VSOCK_DEFAULT_BUF_ALLOC,
                },
            );
            // Accept guest-initiated connections to the agent port. The
            // guest agent connects out to (HOST_CID, VSOCK_HOST_PORT)
            // in a later slice; registering the listener now is
            // harmless and keeps the host ready.
            device.listen(VSOCK_HOST_PORT);
            VsockBackend {
                device: Arc::new(Mutex::new(device)),
                guest_mem: boot_plan.guest_mem.clone(),
                vm_fd: Arc::clone(&vm_fd),
                irq: VSOCK_MMIO_IRQ,
            }
        });

        Ok(KvmVmRuntime {
            vm_fd,
            boot_plan,
            entry_point,
            serial_output: Arc::new(Mutex::new(Vec::new())),
            real_mode: false,
            vsock,
        })
    }

    /// Build a runtime that executes `bytes` directly in 16-bit real
    /// mode at GPA 0. Skips the Linux bzImage / cmdline / GDT setup
    /// entirely — purely for tests / examples that exercise the KVM
    /// bring-up surface without a real kernel.
    fn build_flat_runtime(&self, cfg: &VmConfig, bytes: &[u8]) -> VmResult<KvmVmRuntime> {
        // `from_config_flat` skips the 2 MiB kernel-load-address
        // floor that the bzImage path requires — for a hand-rolled
        // real-mode test program, even a 4 KiB guest is plenty.
        let boot_plan = KvmBootPlan::from_config_flat(cfg)?;
        if (bytes.len() as u64) > boot_plan.memory_size_bytes() {
            return Err(VmError::Backend(format!(
                "vm-kvm: flat_binary ({} bytes) exceeds guest memory ({} bytes)",
                bytes.len(),
                boot_plan.memory_size_bytes(),
            )));
        }
        let vm_fd = Arc::new(
            self.kvm
                .create_vm()
                .map_err(|e| VmError::Backend(format!("create VM: {e}")))?,
        );
        // Deliberately NOT calling create_irq_chip / create_pit2
        // here. With an in-kernel LAPIC active, `HLT` enters
        // halted-waiting-for-interrupt and KVM_RUN doesn't return
        // VcpuExit::Hlt to userspace — the vCPU thread would block
        // forever. Real-mode test code doesn't poke the PIC anyway,
        // so the minimal-bring-up that `hello_kvm` uses is correct
        // here too.
        register_guest_memory(&vm_fd, &boot_plan.guest_mem)?;
        // Write the program at GPA 0 via vm-memory's checked write —
        // no unsafe required at this layer.
        boot_plan
            .guest_mem
            .write_slice(bytes, GuestAddress(0))
            .map_err(|e| VmError::Backend(format!("write flat_binary at GPA 0: {e}")))?;
        Ok(KvmVmRuntime {
            vm_fd,
            boot_plan,
            entry_point: GuestAddress(0),
            serial_output: Arc::new(Mutex::new(Vec::new())),
            real_mode: true,
            // Real-mode flat binaries don't get a virtio device.
            vsock: None,
        })
    }

    fn spawn_vcpu(&self, id: VmId, runtime: &KvmVmRuntime) -> VmResult<KvmVcpuThread> {
        let mut vcpu = runtime
            .vm_fd
            .create_vcpu(0)
            .map_err(|e| VmError::Backend(format!("create vCPU for {id}: {e}")))?;
        if runtime.real_mode {
            configure_boot_vcpu_realmode(&self.kvm, &mut vcpu, runtime.entry_point)?;
        } else {
            configure_boot_vcpu(
                &self.kvm,
                &mut vcpu,
                &runtime.boot_plan.guest_mem,
                runtime.entry_point,
            )?;
        }
        Ok(self.spawn_vcpu_thread(id, vcpu, runtime))
    }

    /// Spawn the vCPU run loop for an already-created-and-configured `vcpu`
    /// (boot-configured or snapshot-restored). Wires up the stop/pause
    /// control channel, the serial sink, the vsock backend, and the MSR
    /// index list the pause path captures.
    fn spawn_vcpu_thread(&self, id: VmId, vcpu: VcpuFd, runtime: &KvmVmRuntime) -> KvmVcpuThread {
        let control = Arc::new(VcpuControl::default());
        let control_for_thread = Arc::clone(&control);
        let serial_output = Arc::clone(&runtime.serial_output);
        let vsock = runtime.vsock.clone();
        let msr_indices = Arc::clone(&self.msr_indices);
        let handle = thread::Builder::new()
            .name(format!("kvm-vcpu-{}", id.0))
            .spawn(move || {
                run_vcpu_loop(vcpu, serial_output, control_for_thread, vsock, msr_indices)
            })
            .expect("spawn vCPU thread");
        KvmVcpuThread { control, handle }
    }

    fn reap_finished_vcpus(inner: &mut Inner) -> VmResult<()> {
        for vm in inner.vms.values_mut() {
            if vm
                .vcpu
                .as_ref()
                .is_some_and(|vcpu| vcpu.handle.is_finished())
            {
                let vcpu = vm.vcpu.take().expect("checked is_some above");
                vm.state = VmState::Stopped;
                vm.last_run_error = Some(join_vcpu_thread(vcpu)?);
            }
        }
        Ok(())
    }

    fn stop_vm(vm: &mut KvmVm, kick_signal: c_int) -> VmResult<()> {
        let Some(vcpu) = vm.vcpu.take() else {
            vm.state = VmState::Stopped;
            return Ok(());
        };
        vcpu.control.stop.store(true, Ordering::SeqCst);
        // Wake a paused thread too, so a stop during snapshot unblocks it.
        vcpu.control.cvar.notify_all();
        vcpu.handle
            .kill(kick_signal)
            .map_err(|e| VmError::Backend(format!("kick vCPU thread: {e}")))?;
        vm.last_run_error = Some(join_vcpu_thread(vcpu)?);
        vm.state = VmState::Stopped;
        Ok(())
    }

    /// On-disk directory for a snapshot id, under the process temp dir.
    fn snapshot_dir(&self, id: SnapshotId) -> PathBuf {
        std::env::temp_dir()
            .join("nanovm-snapshots")
            .join(id.0.to_string())
    }

    /// Request a pause, kick the vCPU out of `KVM_RUN`, and wait for the
    /// thread to park with its captured state. Returns that state; the
    /// caller must call [`resume_vcpu`](Self::resume_vcpu) afterwards.
    fn pause_and_take_state(&self, thread: &KvmVcpuThread) -> VmResult<vmstate::VcpuState> {
        use std::time::{Duration, Instant};
        thread.control.pause.store(true, Ordering::SeqCst);
        thread
            .handle
            .kill(self.kick_signal)
            .map_err(|e| VmError::Backend(format!("vm-kvm: kick vCPU for snapshot: {e}")))?;

        let mut slot = thread
            .control
            .slot
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: vcpu pause slot poisoned".into()))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while slot.captured.is_none() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| VmError::Backend("vm-kvm: timed out pausing vCPU".into()))?;
            let (next, timeout) = thread
                .control
                .cvar
                .wait_timeout(slot, remaining)
                .map_err(|_| VmError::Backend("vm-kvm: vcpu pause slot poisoned".into()))?;
            slot = next;
            if timeout.timed_out() && slot.captured.is_none() {
                return Err(VmError::Backend("vm-kvm: timed out pausing vCPU".into()));
            }
        }
        let captured = slot.captured.take().expect("captured is Some");
        drop(slot);
        captured.map_err(VmError::Backend)
    }

    /// Release a vCPU thread parked by [`pause_and_take_state`].
    fn resume_vcpu(&self, thread: &KvmVcpuThread) {
        if let Ok(mut slot) = thread.control.slot.lock() {
            slot.resume = true;
            thread.control.cvar.notify_all();
        }
    }

    /// Rebuild a VM from a snapshot directory: fresh `VmFd` + irqchip/PIT,
    /// guest RAM loaded from the memory image, machine + vCPU state restored.
    /// Returns the runtime and the configured `VcpuFd` ready to run.
    fn restore_runtime(&self, manifest: &Manifest, dir: &Path) -> VmResult<(KvmVmRuntime, VcpuFd)> {
        // Lazy CoW mapping: pages fault in from the snapshot file as the
        // guest touches them, and writes go to private anonymous copies.
        // N restores of the same snapshot share the read pages via the
        // page cache — "snapshot once, fork many".
        let guest_mem = cow_guest_memory(&manifest.backing_file_path(dir), manifest.memory_bytes)?;
        let vm_fd = Arc::new(
            self.kvm
                .create_vm()
                .map_err(|e| VmError::Backend(format!("create VM: {e}")))?,
        );
        vm_fd
            .set_tss_address(
                usize::try_from(KVM_TSS_ADDRESS)
                    .map_err(|_| VmError::Backend("vm-kvm: TSS address overflow".into()))?,
            )
            .map_err(|e| VmError::Backend(format!("set TSS address: {e}")))?;
        let pit_config = kvm_pit_config {
            flags: KVM_PIT_SPEAKER_DUMMY,
            ..Default::default()
        };
        vm_fd
            .create_irq_chip()
            .map_err(|e| VmError::Backend(format!("create irqchip: {e}")))?;
        vm_fd
            .create_pit2(pit_config)
            .map_err(|e| VmError::Backend(format!("create PIT: {e}")))?;
        // Guest memory is already populated via the file-backed CoW mmap
        // built above — no eager load step.
        register_guest_memory(&vm_fd, &guest_mem)?;

        // Machine devices first (irqchip/PIT/clock), then the vCPU.
        vmstate::MachineState::read_from_dir(dir)?.restore(&vm_fd)?;
        let vcpu = vm_fd
            .create_vcpu(0)
            .map_err(|e| VmError::Backend(format!("create vCPU: {e}")))?;
        let cpuid = self
            .kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| VmError::Backend(format!("get supported CPUID: {e}")))?;
        vcpu.set_cpuid2(&cpuid)
            .map_err(|e| VmError::Backend(format!("set CPUID: {e}")))?;
        vmstate::VcpuState::read_from_dir(dir)?.restore(&vcpu)?;

        let mut cmdline = Cmdline::new(KvmBootPlan::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        if !manifest.kernel_cmdline.is_empty() {
            cmdline
                .insert_str(&manifest.kernel_cmdline)
                .map_err(|e| VmError::Backend(format!("restore kernel cmdline: {e}")))?;
        }
        let boot_plan = KvmBootPlan {
            guest_mem,
            kernel_load_addr: GuestAddress(0),
            initrd_load_addr: None,
            cmdline,
        };
        let runtime = KvmVmRuntime {
            vm_fd,
            boot_plan,
            entry_point: GuestAddress(0),
            serial_output: Arc::new(Mutex::new(Vec::new())),
            real_mode: false,
            vsock: None,
        };
        Ok((runtime, vcpu))
    }
}

/// Minimal KVM boot resources that can be prepared without creating a VM.
#[cfg(feature = "kvm")]
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct KvmBootPlan {
    guest_mem: GuestMemoryMmap,
    kernel_load_addr: GuestAddress,
    initrd_load_addr: Option<GuestAddress>,
    cmdline: Cmdline,
}

#[cfg(feature = "kvm")]
#[allow(dead_code)]
impl KvmBootPlan {
    const CMDLINE_CAPACITY: usize = 4096;
    const KERNEL_LOAD_ADDR: u64 = 0x20_0000;
    const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 pci=off";

    fn from_config(cfg: &VmConfig) -> VmResult<Self> {
        let mem_size = cfg
            .memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| VmError::Backend("memory size overflow".into()))?;
        let mem_len = usize::try_from(mem_size)
            .map_err(|_| VmError::Backend(format!("memory size {mem_size} does not fit usize")))?;
        let guest_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_len)])
            .map_err(|e| VmError::Backend(format!("guest memory layout: {e}")))?;
        if guest_mem.last_addr().raw_value() < Self::KERNEL_LOAD_ADDR {
            return Err(VmError::Backend(format!(
                "vm-kvm: guest memory must extend past kernel load address {:#x}",
                Self::KERNEL_LOAD_ADDR,
            )));
        }
        let mut cmdline = Cmdline::new(Self::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        cmdline
            .insert_str(Self::DEFAULT_CMDLINE)
            .map_err(|e| VmError::Backend(format!("default kernel cmdline: {e}")))?;
        if !cfg.cmdline.is_empty() {
            cmdline
                .insert_str(&cfg.cmdline)
                .map_err(|e| VmError::Backend(format!("kernel cmdline: {e}")))?;
        }
        // Tell the guest's virtio_mmio driver where to find our vsock
        // device. Format: <size>@<base>:<irq>. No device-tree/ACPI in
        // this minimal boot, so this cmdline param is how the guest
        // discovers an MMIO virtio device.
        if cfg.vsock_cid.is_some() {
            cmdline
                .insert_str(format!(
                    "virtio_mmio.device={VSOCK_MMIO_SIZE:#x}@{VSOCK_MMIO_BASE:#x}:{VSOCK_MMIO_IRQ}"
                ))
                .map_err(|e| VmError::Backend(format!("vsock cmdline: {e}")))?;
            // The Linux init path hands unknown `key=value` cmdline
            // tokens to PID 1 as environment variables. The guest agent
            // reads NANOVM_AGENT_VSOCK to switch its transport from
            // stdio to AF_VSOCK and connect to (HOST_CID, this port).
            cmdline
                .insert_str(format!("NANOVM_AGENT_VSOCK={VSOCK_HOST_PORT}"))
                .map_err(|e| VmError::Backend(format!("vsock agent cmdline: {e}")))?;
        }
        Ok(Self {
            guest_mem,
            kernel_load_addr: GuestAddress(Self::KERNEL_LOAD_ADDR),
            initrd_load_addr: None,
            cmdline,
        })
    }

    /// Like [`Self::from_config`] but skips the kernel-load-address
    /// floor and the kernel cmdline plumbing. Intended for the
    /// `flat_binary` boot mode — a hand-rolled real-mode program at
    /// GPA 0 doesn't need either, and the floor would reject the
    /// tiny guest sizes flat-binary tests use.
    fn from_config_flat(cfg: &VmConfig) -> VmResult<Self> {
        let mem_size = cfg
            .memory_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| VmError::Backend("memory size overflow".into()))?;
        let mem_len = usize::try_from(mem_size)
            .map_err(|_| VmError::Backend(format!("memory size {mem_size} does not fit usize")))?;
        if mem_len == 0 {
            return Err(VmError::Backend(
                "vm-kvm: flat_binary mode requires memory_mib >= 1".into(),
            ));
        }
        let guest_mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_len)])
            .map_err(|e| VmError::Backend(format!("guest memory layout: {e}")))?;
        let cmdline = Cmdline::new(Self::CMDLINE_CAPACITY)
            .map_err(|e| VmError::Backend(format!("kernel cmdline capacity: {e}")))?;
        Ok(Self {
            guest_mem,
            kernel_load_addr: GuestAddress(0),
            initrd_load_addr: None,
            cmdline,
        })
    }

    fn cmdline_size(&self) -> VmResult<usize> {
        self.cmdline
            .as_cstring()
            .map(|cmdline| cmdline.as_bytes_with_nul().len())
            .map_err(|e| VmError::Backend(format!("kernel cmdline CString: {e}")))
    }

    fn cmdline_string(&self) -> VmResult<String> {
        self.cmdline
            .as_cstring()
            .map_err(|e| VmError::Backend(format!("kernel cmdline CString: {e}")))
            .and_then(|cmdline| {
                cmdline
                    .into_string()
                    .map_err(|e| VmError::Backend(format!("kernel cmdline UTF-8: {e}")))
            })
    }

    fn memory_size_bytes(&self) -> u64 {
        self.guest_mem.last_addr().raw_value() + 1
    }

    fn open_kvm() -> VmResult<Kvm> {
        Kvm::new().map_err(|e| VmError::Backend(format!("open /dev/kvm: {e}")))
    }
}

#[cfg(feature = "kvm")]
impl Hypervisor for KvmHypervisor {
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle> {
        let runtime = self.build_runtime(cfg)?;
        let id = VmId::next();
        let vm = KvmVm {
            config: cfg.clone(),
            state: VmState::Created,
            runtime,
            vcpu: None,
            last_run_error: None,
        };
        let mut inner = self.lock_inner()?;
        inner.vms.insert(id, vm);
        Ok(VmHandle {
            id,
            state: VmState::Created,
        })
    }

    fn start(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Created => {}
            VmState::Stopped => {
                vm.runtime = self.build_runtime(&vm.config)?;
                vm.last_run_error = None;
            }
            VmState::Running => {
                return Err(VmError::InvalidTransition {
                    id,
                    from: VmState::Running,
                    to: VmState::Running,
                })
            }
        }
        vm.vcpu = Some(self.spawn_vcpu(id, &vm.runtime)?);
        vm.state = VmState::Running;
        Ok(())
    }

    fn stop(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get_mut(&id).ok_or(VmError::UnknownVm(id))?;
        match vm.state {
            VmState::Running => Self::stop_vm(vm, self.kick_signal),
            other => Err(VmError::InvalidTransition {
                id,
                from: other,
                to: VmState::Stopped,
            }),
        }
    }

    fn state(&self, id: VmId) -> VmResult<VmState> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        inner
            .vms
            .get(&id)
            .map(|vm| vm.state)
            .ok_or(VmError::UnknownVm(id))
    }

    fn snapshot(&self, id: VmId) -> VmResult<SnapshotId> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let snap_id = SnapshotId::next();
        let dir = self.snapshot_dir(snap_id);

        let meta = {
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            if vm.runtime.real_mode {
                return Err(VmError::Unsupported(
                    "vm-kvm: snapshot of a flat-binary VM is not supported",
                ));
            }
            if vm.runtime.vsock.is_some() {
                return Err(VmError::Unsupported(
                    "vm-kvm: snapshot of a VM with a vsock device is not yet supported \
                     (device-state capture lands in a later slice)",
                ));
            }
            if vm.state != VmState::Running {
                return Err(VmError::Backend(
                    "vm-kvm: snapshot requires a Running VM".into(),
                ));
            }
            let thread = vm
                .vcpu
                .as_ref()
                .ok_or_else(|| VmError::Backend("vm-kvm: Running VM has no vCPU thread".into()))?;

            // Pause the vCPU and take its self-captured architectural state.
            let vcpu_state = self.pause_and_take_state(thread)?;
            // The vCPU is parked: capture machine + memory consistently.
            let machine = vmstate::MachineState::capture(&vm.runtime.vm_fd);
            let mem_bytes = vm.runtime.boot_plan.memory_size_bytes();
            let cmdline = vm.runtime.boot_plan.cmdline_string().unwrap_or_default();

            // Write the snapshot dir; resume the vCPU regardless of the result.
            let write_result = (|| -> VmResult<()> {
                std::fs::create_dir_all(&dir)
                    .map_err(|e| VmError::Backend(format!("vm-kvm: create snapshot dir: {e}")))?;
                let mut manifest =
                    Manifest::new(snap_id.0, mem_bytes, PAGE_SIZE as u32, vm.config.vcpus);
                manifest.created_at_unix_ms = now_unix_ms();
                manifest.kernel_cmdline = cmdline.clone();
                manifest
                    .write_to_dir(&dir)
                    .map_err(|e| VmError::Backend(format!("vm-kvm: write manifest: {e}")))?;
                dump_memory(
                    &vm.runtime.boot_plan.guest_mem,
                    mem_bytes,
                    &manifest.backing_file_path(&dir),
                )?;
                vcpu_state.write_to_dir(&dir)?;
                machine?.write_to_dir(&dir)?;
                Ok(())
            })();

            self.resume_vcpu(thread);
            write_result?;

            SnapshotMeta {
                id: snap_id,
                vcpu_count: vm.config.vcpus,
                memory_bytes: mem_bytes,
                page_size: PAGE_SIZE as u32,
                kernel_cmdline: cmdline,
            }
        };

        inner.snapshots.insert(snap_id, SnapshotEntry { dir, meta });
        Ok(snap_id)
    }

    fn restore(&self, snap: SnapshotId) -> VmResult<VmHandle> {
        let (dir, meta) = {
            let inner = self.lock_inner()?;
            let entry = inner
                .snapshots
                .get(&snap)
                .ok_or(VmError::UnknownSnapshot(snap))?;
            (entry.dir.clone(), entry.meta.clone())
        };
        let manifest = Manifest::read_from_dir(&dir)
            .map_err(|e| VmError::Backend(format!("vm-kvm: read snapshot manifest: {e}")))?;
        // Reconstruct the VM and resume it from the captured rip.
        let (runtime, vcpu) = self.restore_runtime(&manifest, &dir)?;
        let id = VmId::next();
        let thread = self.spawn_vcpu_thread(id, vcpu, &runtime);
        let config = VmConfig {
            vcpus: meta.vcpu_count,
            memory_mib: meta.memory_bytes / (1024 * 1024),
            cmdline: meta.kernel_cmdline.clone(),
            ..VmConfig::default()
        };
        let mut inner = self.lock_inner()?;
        inner.vms.insert(
            id,
            KvmVm {
                config,
                state: VmState::Running,
                runtime,
                vcpu: Some(thread),
                last_run_error: None,
            },
        );
        Ok(VmHandle {
            id,
            state: VmState::Running,
        })
    }

    fn destroy(&self, id: VmId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let mut vm = inner.vms.remove(&id).ok_or(VmError::UnknownVm(id))?;
        if vm.state == VmState::Running {
            Self::stop_vm(&mut vm, self.kick_signal)?;
        }
        Ok(())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        Ok(inner
            .vms
            .iter()
            .map(|(id, vm)| VmHandle {
                id: *id,
                state: vm.state,
            })
            .collect())
    }

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        Ok(self.lock_inner()?.snapshots.keys().copied().collect())
    }

    fn delete_snapshot(&self, snap: SnapshotId) -> VmResult<()> {
        let mut inner = self.lock_inner()?;
        let entry = inner
            .snapshots
            .remove(&snap)
            .ok_or(VmError::UnknownSnapshot(snap))?;
        // Best-effort directory removal: the id is already gone from the map,
        // so a leftover directory is a disk-space concern, not a correctness one.
        let _ = std::fs::remove_dir_all(&entry.dir);
        Ok(())
    }

    fn snapshot_meta(&self, snap: SnapshotId) -> VmResult<SnapshotMeta> {
        self.lock_inner()?
            .snapshots
            .get(&snap)
            .map(|e| e.meta.clone())
            .ok_or(VmError::UnknownSnapshot(snap))
    }

    fn vm_meta(&self, id: VmId) -> VmResult<VmMeta> {
        let mut inner = self.lock_inner()?;
        Self::reap_finished_vcpus(&mut inner)?;
        let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
        Ok(VmMeta {
            id,
            state: vm.state,
            vcpus: vm.config.vcpus,
            memory_mib: vm.config.memory_mib,
            kernel_cmdline: vm.runtime.boot_plan.cmdline_string()?,
            snapshot_dir: vm.config.snapshot_dir.clone(),
        })
    }

    fn exec_in_guest(&self, id: VmId, req: GuestExecRequest) -> VmResult<GuestExecResult> {
        // Clone the backend out so we don't hold the hypervisor-wide
        // lock while polling for the agent connection and its reply.
        let backend = {
            let inner = self.lock_inner()?;
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            vm.runtime.vsock.clone().ok_or(VmError::Unsupported(
                "vm-kvm: exec_in_guest requires a vsock device (set VmConfig.vsock_cid)",
            ))?
        };
        exec_over_vsock(&backend, req)
    }

    fn exec_in_guest_stream(
        &self,
        id: VmId,
        req: GuestExecRequest,
    ) -> VmResult<Box<dyn vm_core::ExecStream>> {
        let backend = {
            let inner = self.lock_inner()?;
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            vm.runtime.vsock.clone().ok_or(VmError::Unsupported(
                "vm-kvm: exec_in_guest_stream requires a vsock device (set VmConfig.vsock_cid)",
            ))?
        };
        exec_stream_over_vsock(&backend, req)
    }

    fn write_file(&self, id: VmId, path: String, content: Vec<u8>, mode: u32) -> VmResult<u64> {
        let backend = {
            let inner = self.lock_inner()?;
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            vm.runtime.vsock.clone().ok_or(VmError::Unsupported(
                "vm-kvm: write_file requires a vsock device (set VmConfig.vsock_cid)",
            ))?
        };
        // Body size is capped by the proto framing (MAX_FRAME_BYTES);
        // surface that to the caller as Backend rather than silently
        // truncating downstream.
        let path_for_msg = path.clone();
        let resp = rpc_oneshot(
            &backend,
            proto::RequestBody::WriteFile {
                path,
                content,
                mode,
            },
            std::time::Duration::from_secs(30),
        )?;
        match resp {
            proto::ResponseBody::Written { bytes } => Ok(bytes),
            other => Err(VmError::Backend(format!(
                "vm-kvm: unexpected write_file response for {path_for_msg:?}: {other:?}"
            ))),
        }
    }

    fn read_file(&self, id: VmId, path: String) -> VmResult<Vec<u8>> {
        let backend = {
            let inner = self.lock_inner()?;
            let vm = inner.vms.get(&id).ok_or(VmError::UnknownVm(id))?;
            vm.runtime.vsock.clone().ok_or(VmError::Unsupported(
                "vm-kvm: read_file requires a vsock device (set VmConfig.vsock_cid)",
            ))?
        };
        let path_for_msg = path.clone();
        let resp = rpc_oneshot(
            &backend,
            proto::RequestBody::ReadFile { path },
            std::time::Duration::from_secs(30),
        )?;
        match resp {
            proto::ResponseBody::FileContent { content } => Ok(content),
            other => Err(VmError::Backend(format!(
                "vm-kvm: unexpected read_file response for {path_for_msg:?}: {other:?}"
            ))),
        }
    }
}

#[cfg(not(feature = "kvm"))]
impl Hypervisor for KvmHypervisor {
    fn create_vm(&self, _cfg: &VmConfig) -> VmResult<VmHandle> {
        Err(unsupported())
    }

    fn start(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn stop(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn state(&self, _id: VmId) -> VmResult<VmState> {
        Err(unsupported())
    }

    fn snapshot(&self, _id: VmId) -> VmResult<SnapshotId> {
        Err(unsupported())
    }

    fn restore(&self, _snap: SnapshotId) -> VmResult<VmHandle> {
        Err(unsupported())
    }

    fn destroy(&self, _id: VmId) -> VmResult<()> {
        Err(unsupported())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        Err(unsupported())
    }

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        Err(unsupported())
    }

    fn delete_snapshot(&self, _snap: SnapshotId) -> VmResult<()> {
        Err(unsupported())
    }

    fn snapshot_meta(&self, _snap: SnapshotId) -> VmResult<SnapshotMeta> {
        Err(unsupported())
    }

    fn vm_meta(&self, _id: VmId) -> VmResult<VmMeta> {
        Err(unsupported())
    }

    fn exec_in_guest(&self, _id: VmId, _req: GuestExecRequest) -> VmResult<GuestExecResult> {
        Err(unsupported())
    }

    fn write_file(&self, _id: VmId, _path: String, _content: Vec<u8>, _mode: u32) -> VmResult<u64> {
        Err(unsupported())
    }

    fn read_file(&self, _id: VmId, _path: String) -> VmResult<Vec<u8>> {
        Err(unsupported())
    }
}

#[cfg(feature = "kvm")]
fn register_guest_memory(vm_fd: &VmFd, guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    for (slot, region) in guest_mem.iter().enumerate() {
        let userspace_addr = region
            .get_host_address(MemoryRegionAddress(0))
            .map_err(|e| {
                VmError::Backend(format!("resolve host address for memslot {slot}: {e}"))
            })? as u64;
        let mem_region = kvm_userspace_memory_region {
            slot: u32::try_from(slot)
                .map_err(|_| VmError::Backend(format!("too many guest memory regions: {slot}")))?,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr,
            flags: 0,
        };
        // SAFETY: `GuestMemoryMmap` owns the backing mapping for the lifetime of the VM runtime,
        // each memslot is registered once with a non-overlapping region, and the host address
        // returned by `vm-memory` points at the beginning of that live mapping.
        unsafe {
            vm_fd
                .set_user_memory_region(mem_region)
                .map_err(|e| VmError::Backend(format!("register memslot {slot}: {e}")))?;
        }
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn configure_linux_boot(
    guest_mem: &GuestMemoryMmap,
    setup_header: Option<setup_header>,
    cmdline_addr: GuestAddress,
    cmdline_size: usize,
    initrd: Option<(GuestAddress, usize)>,
) -> VmResult<()> {
    let mut params = boot_params::default();
    if let Some(header) = setup_header {
        params.hdr = header;
    }
    params.hdr.type_of_loader = KERNEL_LOADER_OTHER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC;
    params.hdr.header = KERNEL_HDR_MAGIC;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    params.hdr.cmd_line_ptr = u32::try_from(cmdline_addr.raw_value())
        .map_err(|_| VmError::Backend("vm-kvm: cmdline address does not fit boot params".into()))?;
    params.hdr.cmdline_size = u32::try_from(cmdline_size)
        .map_err(|_| VmError::Backend("vm-kvm: cmdline size does not fit boot params".into()))?;
    if let Some((addr, size)) = initrd {
        // The 32-bit ramdisk fields only reach 4 GiB; our guests are
        // far smaller, and load_initrd places the image well under
        // that, but check anyway so a future large-RAM guest fails
        // loudly instead of truncating the pointer.
        params.hdr.ramdisk_image = u32::try_from(addr.raw_value()).map_err(|_| {
            VmError::Backend("vm-kvm: initrd address does not fit boot params".into())
        })?;
        params.hdr.ramdisk_size = u32::try_from(size)
            .map_err(|_| VmError::Backend("vm-kvm: initrd size does not fit boot params".into()))?;
    }
    add_e820_entry(&mut params, 0, SYSTEM_MEM_START, E820_RAM)?;
    add_e820_entry(
        &mut params,
        SYSTEM_MEM_START,
        SYSTEM_MEM_SIZE,
        E820_RESERVED,
    )?;
    let mem_end = guest_mem.last_addr().raw_value() + 1;
    if mem_end > HIMEM_START {
        add_e820_entry(&mut params, HIMEM_START, mem_end - HIMEM_START, E820_RAM)?;
    }
    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE_START)),
        guest_mem,
    )
    .map_err(|e| VmError::Backend(format!("write Linux boot params: {e}")))
}

#[cfg(feature = "kvm")]
fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, mem_type: u32) -> VmResult<()> {
    if usize::from(params.e820_entries) >= params.e820_table.len() {
        return Err(VmError::Backend("vm-kvm: e820 table is full".into()));
    }
    let entry = &mut params.e820_table[usize::from(params.e820_entries)];
    entry.addr = addr;
    entry.size = size;
    entry.type_ = mem_type;
    params.e820_entries += 1;
    Ok(())
}

/// Read an initramfs from `path` and copy it into guest RAM as high
/// as it'll fit, page-aligned. Returns `(load_addr, size)` for the
/// caller to stuff into the boot params' ramdisk fields.
///
/// Placing it high keeps it clear of the kernel image (loaded at
/// `KERNEL_LOAD_ADDR`), the cmdline, and the zero page — all of
/// which live low. The kernel relocates/unpacks it during early
/// boot, so the exact address only needs to be valid RAM the kernel
/// won't clobber before it consumes the ramdisk.
#[cfg(feature = "kvm")]
fn load_initrd(
    guest_mem: &GuestMemoryMmap,
    path: &std::path::Path,
) -> VmResult<(GuestAddress, usize)> {
    let bytes = std::fs::read(path)
        .map_err(|e| VmError::Backend(format!("read initrd {}: {e}", path.display())))?;
    let size = bytes.len();
    if size == 0 {
        return Err(VmError::Backend("vm-kvm: initrd is empty".into()));
    }
    let mem_end = guest_mem.last_addr().raw_value() + 1;
    // Page-align the load address down from the top of RAM.
    let raw = mem_end
        .checked_sub(size as u64)
        .ok_or_else(|| VmError::Backend("vm-kvm: initrd larger than guest memory".into()))?;
    let load_addr = raw & !(PAGE_SIZE - 1);
    if load_addr < HIMEM_START {
        return Err(VmError::Backend(format!(
            "vm-kvm: initrd ({size} bytes) leaves no room above himem {HIMEM_START:#x} \
             in a {mem_end:#x}-byte guest",
        )));
    }
    guest_mem
        .write_slice(&bytes, GuestAddress(load_addr))
        .map_err(|e| VmError::Backend(format!("write initrd into guest memory: {e}")))?;
    Ok((GuestAddress(load_addr), size))
}

#[cfg(feature = "kvm")]
fn configure_boot_vcpu(
    kvm: &Kvm,
    vcpu: &mut VcpuFd,
    guest_mem: &GuestMemoryMmap,
    entry_point: GuestAddress,
) -> VmResult<()> {
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmError::Backend(format!("get supported CPUID: {e}")))?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmError::Backend(format!("set CPUID: {e}")))?;
    setup_fpu(vcpu)?;
    setup_regs(vcpu, entry_point)?;
    setup_sregs(guest_mem, vcpu)?;
    Ok(())
}

/// Bring up a vCPU in 16-bit real mode at `CS:base=0, selector=0,
/// rip=entry_point`. Mirrors what the `hello_kvm` example does for
/// the raw kvm-ioctls path, just shared through the Hypervisor
/// trait. Skips the protected-mode / long-mode / paging setup
/// `setup_sregs` does for kernel boots — propagating that to a
/// hand-rolled real-mode binary would leave the vCPU unable to
/// execute the program at GPA 0.
#[cfg(feature = "kvm")]
fn configure_boot_vcpu_realmode(
    kvm: &Kvm,
    vcpu: &mut VcpuFd,
    entry_point: GuestAddress,
) -> VmResult<()> {
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmError::Backend(format!("get supported CPUID: {e}")))?;
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmError::Backend(format!("set CPUID: {e}")))?;
    setup_fpu(vcpu)?;
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmError::Backend(format!("get sregs (realmode): {e}")))?;
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs)
        .map_err(|e| VmError::Backend(format!("set sregs (realmode): {e}")))?;
    let regs = kvm_regs {
        rip: entry_point.raw_value(),
        rflags: 0x2,
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .map_err(|e| VmError::Backend(format!("set regs (realmode): {e}")))?;
    Ok(())
}

#[cfg(feature = "kvm")]
fn setup_fpu(vcpu: &VcpuFd) -> VmResult<()> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu)
        .map_err(|e| VmError::Backend(format!("set FPU registers: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_regs(vcpu: &VcpuFd, entry_point: GuestAddress) -> VmResult<()> {
    let regs = kvm_regs {
        rflags: 0x2,
        rip: entry_point.raw_value(),
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rsi: ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs)
        .map_err(|e| VmError::Backend(format!("set base registers: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_sregs(guest_mem: &GuestMemoryMmap, vcpu: &VcpuFd) -> VmResult<()> {
    let mut sregs = vcpu
        .get_sregs()
        .map_err(|e| VmError::Backend(format!("get special registers: {e}")))?;
    configure_segments_and_sregs(guest_mem, &mut sregs)?;
    setup_page_tables(guest_mem, &mut sregs)?;
    vcpu.set_sregs(&sregs)
        .map_err(|e| VmError::Backend(format!("set special registers: {e}")))
}

#[cfg(feature = "kvm")]
fn configure_segments_and_sregs(
    guest_mem: &GuestMemoryMmap,
    sregs: &mut kvm_sregs,
) -> VmResult<()> {
    let gdt_table = [
        gdt_entry(0, 0, 0),
        gdt_entry(0xa09b, 0, 0xfffff),
        gdt_entry(0xc093, 0, 0xfffff),
        gdt_entry(0x808b, 0, 0xfffff),
    ];
    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    write_gdt_table(&gdt_table, guest_mem)?;
    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = u16::try_from(mem::size_of_val(&gdt_table) - 1)
        .map_err(|_| VmError::Backend("vm-kvm: GDT limit overflow".into()))?;

    write_idt_value(0, guest_mem)?;
    sregs.idt.base = BOOT_IDT_OFFSET;
    sregs.idt.limit = u16::try_from(mem::size_of::<u64>() - 1)
        .map_err(|_| VmError::Backend("vm-kvm: IDT limit overflow".into()))?;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;
    sregs.cr0 |= X86_CR0_PE;
    sregs.efer |= EFER_LME | EFER_LMA;
    Ok(())
}

#[cfg(feature = "kvm")]
fn write_gdt_table(table: &[u64], guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    let boot_gdt_addr = GuestAddress(BOOT_GDT_OFFSET);
    for (index, entry) in table.iter().enumerate() {
        let addr = guest_mem
            .checked_offset(boot_gdt_addr, index * mem::size_of::<u64>())
            .ok_or_else(|| VmError::Backend("vm-kvm: GDT write overflow".into()))?;
        guest_mem
            .write_obj(*entry, addr)
            .map_err(|e| VmError::Backend(format!("write GDT entry {index}: {e}")))?;
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn write_idt_value(value: u64, guest_mem: &GuestMemoryMmap) -> VmResult<()> {
    guest_mem
        .write_obj(value, GuestAddress(BOOT_IDT_OFFSET))
        .map_err(|e| VmError::Backend(format!("write IDT: {e}")))
}

#[cfg(feature = "kvm")]
fn setup_page_tables(guest_mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> VmResult<()> {
    let boot_pml4_addr = GuestAddress(PML4_START);
    let boot_pdpte_addr = GuestAddress(PDPTE_START);
    let boot_pde_addr = GuestAddress(PDE_START);

    guest_mem
        .write_obj(boot_pdpte_addr.raw_value() | 0x03, boot_pml4_addr)
        .map_err(|e| VmError::Backend(format!("write PML4 entry: {e}")))?;
    guest_mem
        .write_obj(boot_pde_addr.raw_value() | 0x03, boot_pdpte_addr)
        .map_err(|e| VmError::Backend(format!("write PDPTE entry: {e}")))?;
    for index in 0..512u64 {
        guest_mem
            .write_obj((index << 21) | 0x83, GuestAddress(PDE_START + (index * 8)))
            .map_err(|e| VmError::Backend(format!("write PDE entry {index}: {e}")))?;
    }

    sregs.cr3 = boot_pml4_addr.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG | X86_CR0_ET;
    Ok(())
}

#[cfg(feature = "kvm")]
fn run_vcpu_loop(
    mut vcpu: VcpuFd,
    serial_output: Arc<Mutex<Vec<u8>>>,
    control: Arc<VcpuControl>,
    vsock: Option<VsockBackend>,
    msr_indices: Arc<Vec<u32>>,
) -> VmResult<()> {
    loop {
        // Re-check stop before every KVM_RUN. The kick signal handler is a
        // no-op; if a kick lands between KVM_RUN calls it's consumed
        // silently, leaving only this check to surface a stop request.
        if control.stop.load(Ordering::SeqCst) {
            break;
        }
        // A snapshot request parks the thread here after capturing its own
        // state; this is the only place that owns the VcpuFd.
        if control.pause.load(Ordering::SeqCst) {
            pause_and_capture(&vcpu, &control, &msr_indices);
            if control.stop.load(Ordering::SeqCst) {
                break;
            }
            continue;
        }
        match vcpu.run() {
            Ok(VcpuExit::IoOut(port, data)) => handle_io_out(port, data, &serial_output)?,
            Ok(VcpuExit::IoIn(port, data)) => handle_io_in(port, data),
            Ok(VcpuExit::MmioRead(addr, data)) => {
                data.fill(0);
                if let Some(backend) = vsock.as_ref() {
                    if let Some(offset) = backend.window_offset(addr) {
                        backend.read(offset, data);
                    }
                }
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                if let Some(backend) = vsock.as_ref() {
                    if let Some(offset) = backend.window_offset(addr) {
                        backend.write(offset, data)?;
                    }
                }
            }
            Ok(VcpuExit::Hlt) => break,
            Ok(VcpuExit::Shutdown) => {
                // Shutdown == triple fault (or an explicit guest
                // shutdown). During kernel bring-up it almost always
                // means the guest faulted on or near entry. Capture
                // register state so the failure is diagnosable rather
                // than a silent "0 bytes, Stopped". A real-mode test
                // program that HLTs hits the Hlt arm above, so this
                // path is kernel-boot-specific.
                let regs_diag = match vcpu.get_regs() {
                    Ok(r) => format!(
                        "rip={:#x} rsp={:#x} rflags={:#x} rax={:#x} rbx={:#x} rsi={:#x} rdi={:#x}",
                        r.rip, r.rsp, r.rflags, r.rax, r.rbx, r.rsi, r.rdi,
                    ),
                    Err(e) => format!("get_regs failed: {e}"),
                };
                let sregs_diag = match vcpu.get_sregs() {
                    Ok(s) => format!(
                        "cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x} \
                         cs.base={:#x} cs.sel={:#x} cs.l={} cs.db={} cs.present={}",
                        s.cr0,
                        s.cr3,
                        s.cr4,
                        s.efer,
                        s.cs.base,
                        s.cs.selector,
                        s.cs.l,
                        s.cs.db,
                        s.cs.present,
                    ),
                    Err(e) => format!("get_sregs failed: {e}"),
                };
                return Err(VmError::Backend(format!(
                    "vcpu SHUTDOWN (triple fault?): {regs_diag} | {sregs_diag}"
                )));
            }
            Ok(VcpuExit::Intr) if control.stop.load(Ordering::SeqCst) => break,
            // A kick that's a pause request: loop back to the pause check.
            Ok(VcpuExit::Intr) => continue,
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                return Err(VmError::Backend(format!(
                    "KVM fail entry: reason={reason:#x} cpu={cpu}",
                )))
            }
            Ok(VcpuExit::InternalError) => {
                return Err(VmError::Backend("KVM internal error exit".into()))
            }
            Ok(other) => {
                return Err(VmError::Backend(format!(
                    "unexpected KVM vCPU exit: {other:?}",
                )))
            }
            Err(err) if err.errno() == EINTR && control.stop.load(Ordering::SeqCst) => break,
            Err(err) if err.errno() == EINTR => continue,
            Err(err) => return Err(VmError::Backend(format!("KVM_RUN failed: {err}"))),
        }
    }
    Ok(())
}

/// Capture the vCPU's state and park until the controller resumes or stops.
/// Runs on the vCPU thread (the sole owner of `vcpu`); the controller drives
/// it via `control.pause` + a kick signal and collects the state from the
/// slot.
#[cfg(feature = "kvm")]
fn pause_and_capture(vcpu: &VcpuFd, control: &VcpuControl, msr_indices: &[u32]) {
    let captured = vmstate::VcpuState::capture(vcpu, msr_indices).map_err(|e| e.to_string());
    let mut slot = control.slot.lock().expect("vcpu pause slot poisoned");
    slot.captured = Some(captured);
    slot.parked = true;
    control.cvar.notify_all();
    while !slot.resume && !control.stop.load(Ordering::SeqCst) {
        slot = control.cvar.wait(slot).expect("vcpu pause slot poisoned");
    }
    slot.parked = false;
    slot.resume = false;
    slot.captured = None;
    control.pause.store(false, Ordering::SeqCst);
}

#[cfg(feature = "kvm")]
fn handle_io_out(port: u16, data: &[u8], serial_output: &Arc<Mutex<Vec<u8>>>) -> VmResult<()> {
    if port == SERIAL_PORT_BASE {
        serial_output
            .lock()
            .map_err(|_| VmError::Backend("vm-kvm: serial output mutex poisoned".into()))?
            .extend_from_slice(data);
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(data)
            .and_then(|_| stdout.flush())
            .map_err(|e| VmError::Backend(format!("write serial output: {e}")))?;
    }
    Ok(())
}

#[cfg(feature = "kvm")]
fn handle_io_in(port: u16, data: &mut [u8]) {
    data.fill(0);
    if data.is_empty() {
        return;
    }
    match port {
        port if port == SERIAL_PORT_BASE + 5 => data[0] = 0x60,
        port if port == SERIAL_PORT_BASE + 2 => data[0] = 0x01,
        _ => {}
    }
}

/// Dump the whole guest RAM to a snapshot memory image: a
/// [`snapshot::BackingFileHeader`] followed by zero padding to
/// [`snapshot::MEMORY_DATA_OFFSET`] (so the page data is page-aligned in the
/// file and can be `mmap(MAP_PRIVATE)`-ed directly on restore — the
/// foundation of fork-many CoW).
#[cfg(feature = "kvm")]
fn dump_memory(guest_mem: &GuestMemoryMmap, mem_bytes: u64, path: &Path) -> VmResult<()> {
    let page_count = mem_bytes / PAGE_SIZE;
    let header = snapshot::BackingFileHeader::new(PAGE_SIZE as u32, page_count)
        .map_err(|e| VmError::Backend(format!("vm-kvm: snapshot memory header: {e}")))?;
    let mut file = File::create(path)
        .map_err(|e| VmError::Backend(format!("vm-kvm: create memory image: {e}")))?;
    file.write_all(&header.to_bytes())
        .map_err(|e| VmError::Backend(format!("vm-kvm: write memory header: {e}")))?;
    // Pad header → MEMORY_DATA_OFFSET so the page data is page-aligned.
    let pad_len = snapshot::MEMORY_DATA_OFFSET as usize - snapshot::BACKING_HDR_LEN;
    file.write_all(&vec![0u8; pad_len])
        .map_err(|e| VmError::Backend(format!("vm-kvm: write memory header pad: {e}")))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < mem_bytes {
        let n = ((mem_bytes - off) as usize).min(buf.len());
        guest_mem
            .read_slice(&mut buf[..n], GuestAddress(off))
            .map_err(|e| VmError::Backend(format!("vm-kvm: read guest memory: {e}")))?;
        file.write_all(&buf[..n])
            .map_err(|e| VmError::Backend(format!("vm-kvm: write memory image: {e}")))?;
        off += n as u64;
    }
    Ok(())
}

/// Build a guest-memory view that lazily reads pages from `path` and
/// copies-on-write on the first guest store: `mmap(MAP_PRIVATE, fd, …)` on
/// the memory image. Multiple forks of the same snapshot share their
/// unmodified pages via the kernel's page cache and only diverge for
/// pages they actually dirty — the unit-economics win.
#[cfg(feature = "kvm")]
fn cow_guest_memory(path: &Path, mem_bytes: u64) -> VmResult<GuestMemoryMmap> {
    let file = File::open(path)
        .map_err(|e| VmError::Backend(format!("vm-kvm: open memory image: {e}")))?;
    // Quick header sanity check (validates magic, page-count consistency)
    // so we fail loudly on a foreign or corrupt file before mmap'ing.
    {
        let mut hdr = [0u8; snapshot::BACKING_HDR_LEN];
        let mut h = &file;
        h.read_exact(&mut hdr)
            .map_err(|e| VmError::Backend(format!("vm-kvm: read memory header: {e}")))?;
        let header = snapshot::BackingFileHeader::from_bytes(&hdr)
            .map_err(|e| VmError::Backend(format!("vm-kvm: parse memory header: {e}")))?;
        header
            .validate()
            .map_err(|e| VmError::Backend(format!("vm-kvm: invalid memory header: {e}")))?;
        if header.memory_bytes != mem_bytes {
            return Err(VmError::Backend(format!(
                "vm-kvm: snapshot memory size {} does not match manifest {mem_bytes}",
                header.memory_bytes,
            )));
        }
    }
    let mem_len = usize::try_from(mem_bytes)
        .map_err(|_| VmError::Backend("vm-kvm: snapshot memory size overflows usize".into()))?;
    let file_offset = FileOffset::new(file, snapshot::MEMORY_DATA_OFFSET);
    // PRIVATE so guest writes CoW into the fork's own anonymous pages.
    // NORESERVE matches vm-memory's anonymous default (no overcommit reservation).
    let region = MmapRegion::build(
        Some(file_offset),
        mem_len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_NORESERVE | libc::MAP_PRIVATE,
    )
    .map_err(|e| VmError::Backend(format!("vm-kvm: mmap snapshot memory: {e}")))?;
    let region = GuestRegionMmap::new(region, GuestAddress(0))
        .ok_or_else(|| VmError::Backend("vm-kvm: build guest region failed".into()))?;
    GuestMemoryMmap::from_regions(vec![region])
        .map_err(|e| VmError::Backend(format!("vm-kvm: build guest memory: {e}")))
}

/// Milliseconds since the UNIX epoch, or 0 if the clock is before it.
#[cfg(feature = "kvm")]
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(feature = "kvm")]
fn join_vcpu_thread(vcpu: KvmVcpuThread) -> VmResult<String> {
    match vcpu.handle.join() {
        Ok(Ok(())) => Ok(String::new()),
        Ok(Err(err)) => Ok(err.to_string()),
        Err(_) => Err(VmError::Backend("vm-kvm: vCPU thread panicked".into())),
    }
}

#[cfg(feature = "kvm")]
static KICK_SIGNAL_INIT: OnceLock<Result<c_int, String>> = OnceLock::new();

#[cfg(feature = "kvm")]
fn vcpu_kick_signal() -> VmResult<c_int> {
    KICK_SIGNAL_INIT
        .get_or_init(|| {
            let signal = libc::SIGRTMIN();
            register_signal_handler(signal, handle_vcpu_kick)
                .map_err(|e| format!("register vCPU kick signal handler: {e}"))?;
            Ok(signal)
        })
        .clone()
        .map_err(VmError::Backend)
}

#[cfg(feature = "kvm")]
extern "C" fn handle_vcpu_kick(_: c_int, _: *mut siginfo_t, _: *mut c_void) {}

#[cfg(not(feature = "kvm"))]
fn unsupported() -> VmError {
    VmError::Unsupported("vm-kvm: build without the `kvm` feature")
}

#[cfg(feature = "kvm")]
const SERIAL_PORT_BASE: u16 = 0x3f8;
/// Guest-physical base of the virtio-MMIO vsock device register
/// window. Chosen well above guest RAM (≤128 MiB here) in the
/// PCI-hole area, so accesses miss every memory slot and KVM
/// delivers them as MMIO exits. Matches the `virtio_mmio.device=`
/// cmdline param `from_config` appends.
#[cfg(feature = "kvm")]
const VSOCK_MMIO_BASE: u64 = 0xd000_0000;
/// Size of the vsock device's MMIO register window (one page; the
/// register block + config space fit well within 0x1000).
#[cfg(feature = "kvm")]
const VSOCK_MMIO_SIZE: u64 = 0x1000;
/// IRQ (GSI) the vsock device raises. Advertised to the guest via
/// the `virtio_mmio.device=` cmdline param.
#[cfg(feature = "kvm")]
const VSOCK_MMIO_IRQ: u32 = 5;
/// Host-side port the guest agent connects out to. The connection
/// table accepts `Request`s addressed here.
#[cfg(feature = "kvm")]
const VSOCK_HOST_PORT: u32 = 1024;
/// Receive-buffer credit the host advertises to the guest per
/// connection (the `buf_alloc` in our control packets).
#[cfg(feature = "kvm")]
const VSOCK_DEFAULT_BUF_ALLOC: u32 = 64 * 1024;
/// Guest page size. Used to page-align the initramfs load address.
#[cfg(feature = "kvm")]
const PAGE_SIZE: u64 = 0x1000;
#[cfg(feature = "kvm")]
const BOOT_STACK_POINTER: u64 = 0x8ff0;
#[cfg(feature = "kvm")]
const CMDLINE_START: u64 = 0x20_000;
#[cfg(feature = "kvm")]
const ZERO_PAGE_START: u64 = 0x7000;
#[cfg(feature = "kvm")]
const HIMEM_START: u64 = 0x10_0000;
/// Offset of the 64-bit entry point (`startup_64`) from the start of
/// the protected-mode kernel in a bzImage. The Linux x86 boot
/// protocol places the 32-bit entry (`startup_32`) at offset 0 and
/// the 64-bit entry at offset 0x200 (see
/// `arch/x86/boot/compressed/head_64.S`). Since vm-kvm hands the
/// vCPU to KVM already in long mode, we must enter at `startup_64`,
/// not `startup_32` — `linux-loader` returns the load base in
/// `kernel_load`, so we add this offset ourselves.
#[cfg(feature = "kvm")]
const BZIMAGE_64BIT_ENTRY_OFFSET: u64 = 0x200;
#[cfg(feature = "kvm")]
const SYSTEM_MEM_START: u64 = 0x9fc00;
#[cfg(feature = "kvm")]
const SYSTEM_MEM_SIZE: u64 = HIMEM_START - SYSTEM_MEM_START;
#[cfg(feature = "kvm")]
const KVM_TSS_ADDRESS: u64 = 0xfffb_d000;
#[cfg(feature = "kvm")]
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
#[cfg(feature = "kvm")]
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448;
#[cfg(feature = "kvm")]
const KERNEL_LOADER_OTHER: u8 = 0xff;
#[cfg(feature = "kvm")]
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;
#[cfg(feature = "kvm")]
const E820_RAM: u32 = 1;
#[cfg(feature = "kvm")]
const E820_RESERVED: u32 = 2;
#[cfg(feature = "kvm")]
const BOOT_GDT_OFFSET: u64 = 0x500;
#[cfg(feature = "kvm")]
const BOOT_IDT_OFFSET: u64 = 0x520;
#[cfg(feature = "kvm")]
const PML4_START: u64 = 0x9000;
#[cfg(feature = "kvm")]
const PDPTE_START: u64 = 0xa000;
#[cfg(feature = "kvm")]
const PDE_START: u64 = 0xb000;
#[cfg(feature = "kvm")]
const EFER_LMA: u64 = 0x400;
#[cfg(feature = "kvm")]
const EFER_LME: u64 = 0x100;
#[cfg(feature = "kvm")]
const X86_CR0_PE: u64 = 0x1;
#[cfg(feature = "kvm")]
const X86_CR0_ET: u64 = 0x10;
#[cfg(feature = "kvm")]
const X86_CR0_PG: u64 = 0x8000_0000;
#[cfg(feature = "kvm")]
const X86_CR4_PAE: u64 = 0x20;

#[cfg(feature = "kvm")]
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << 32)
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << 32)
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

#[cfg(feature = "kvm")]
fn get_base(entry: u64) -> u64 {
    ((entry & 0xff00_0000_0000_0000) >> 32)
        | ((entry & 0x0000_00ff_0000_0000) >> 16)
        | ((entry & 0x0000_0000_ffff_0000) >> 16)
}

#[cfg(feature = "kvm")]
fn get_limit(entry: u64) -> u32 {
    let limit =
        ((((entry) & 0x000f_0000_0000_0000) >> 32) | ((entry) & 0x0000_0000_0000_ffff)) as u32;
    if get_g(entry) == 0 {
        limit
    } else {
        (limit << 12) | 0x0fff
    }
}

#[cfg(feature = "kvm")]
fn get_g(entry: u64) -> u8 {
    ((entry & 0x0080_0000_0000_0000) >> 55) as u8
}

#[cfg(feature = "kvm")]
fn get_db(entry: u64) -> u8 {
    ((entry & 0x0040_0000_0000_0000) >> 54) as u8
}

#[cfg(feature = "kvm")]
fn get_l(entry: u64) -> u8 {
    ((entry & 0x0020_0000_0000_0000) >> 53) as u8
}

#[cfg(feature = "kvm")]
fn get_avl(entry: u64) -> u8 {
    ((entry & 0x0010_0000_0000_0000) >> 52) as u8
}

#[cfg(feature = "kvm")]
fn get_p(entry: u64) -> u8 {
    ((entry & 0x0000_8000_0000_0000) >> 47) as u8
}

#[cfg(feature = "kvm")]
fn get_dpl(entry: u64) -> u8 {
    ((entry & 0x0000_6000_0000_0000) >> 45) as u8
}

#[cfg(feature = "kvm")]
fn get_s(entry: u64) -> u8 {
    ((entry & 0x0000_1000_0000_0000) >> 44) as u8
}

#[cfg(feature = "kvm")]
fn get_type(entry: u64) -> u8 {
    ((entry & 0x0000_0f00_0000_0000) >> 40) as u8
}

#[cfg(feature = "kvm")]
fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: u16::from(table_index) * 8,
        type_: get_type(entry),
        present: get_p(entry),
        dpl: get_dpl(entry),
        db: get_db(entry),
        s: get_s(entry),
        l: get_l(entry),
        g: get_g(entry),
        avl: get_avl(entry),
        padding: 0,
        unusable: if get_p(entry) == 0 { 1 } else { 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "kvm"))]
    fn all_methods_return_unsupported_without_kvm_feature() {
        let hv = KvmHypervisor::new();
        assert!(matches!(
            hv.create_vm(&VmConfig::default()).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.start(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.stop(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.state(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.snapshot(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.restore(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.destroy(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.list_vms().unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.list_snapshots().unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.delete_snapshot(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.snapshot_meta(SnapshotId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.vm_meta(VmId(1)).unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.exec_in_guest(
                VmId(1),
                GuestExecRequest {
                    program: "echo".into(),
                    args: vec![],
                    cwd: None,
                    env: vec![],
                    timeout_ms: None,
                }
            )
            .unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.write_file(VmId(1), "/tmp/x".into(), vec![], 0o644)
                .unwrap_err(),
            VmError::Unsupported(_)
        ));
        assert!(matches!(
            hv.read_file(VmId(1), "/tmp/x".into()).unwrap_err(),
            VmError::Unsupported(_)
        ));
    }

    #[cfg(feature = "kvm")]
    #[test]
    fn boot_plan_derives_memory_and_cmdline() {
        let plan = KvmHypervisor::boot_plan(&VmConfig {
            memory_mib: 64,
            cmdline: "foo=bar".into(),
            ..VmConfig::default()
        })
        .expect("boot plan");
        let cmdline = plan.cmdline.as_cstring().expect("cmdline CString");
        let cmdline = cmdline.to_str().expect("utf-8 cmdline");
        assert_eq!(plan.memory_size_bytes(), 64 * 1024 * 1024);
        assert_eq!(plan.kernel_load_addr, GuestAddress(0x20_0000));
        assert!(plan.initrd_load_addr.is_none());
        assert!(cmdline.contains("console=ttyS0"));
        assert!(cmdline.contains("reboot=k"));
        assert!(cmdline.contains("foo=bar"));
    }

    #[cfg(feature = "kvm")]
    #[test]
    fn configure_linux_boot_writes_zero_page_and_e820() {
        let plan = KvmHypervisor::boot_plan(&VmConfig {
            memory_mib: 64,
            ..VmConfig::default()
        })
        .expect("boot plan");
        let cmdline_size = plan.cmdline_size().expect("cmdline size");
        configure_linux_boot(
            &plan.guest_mem,
            Some(setup_header::default()),
            GuestAddress(CMDLINE_START),
            cmdline_size,
            None,
        )
        .expect("boot params");
        let params: boot_params = plan
            .guest_mem
            .read_obj(GuestAddress(ZERO_PAGE_START))
            .expect("read zero page");
        let boot_flag = params.hdr.boot_flag;
        let header = params.hdr.header;
        let cmd_line_ptr = params.hdr.cmd_line_ptr;
        let header_cmdline_size = params.hdr.cmdline_size;
        let e820_entries = params.e820_entries;
        let entry0_addr = params.e820_table[0].addr;
        let entry0_type = params.e820_table[0].type_;
        let entry1_addr = params.e820_table[1].addr;
        let entry1_type = params.e820_table[1].type_;
        let entry2_addr = params.e820_table[2].addr;
        let entry2_type = params.e820_table[2].type_;
        assert_eq!(boot_flag, KERNEL_BOOT_FLAG_MAGIC);
        assert_eq!(header, KERNEL_HDR_MAGIC);
        assert_eq!(cmd_line_ptr, CMDLINE_START as u32);
        assert_eq!(header_cmdline_size, cmdline_size as u32);
        assert_eq!(e820_entries, 3);
        assert_eq!(entry0_addr, 0);
        assert_eq!(entry0_type, E820_RAM);
        assert_eq!(entry1_addr, SYSTEM_MEM_START);
        assert_eq!(entry1_type, E820_RESERVED);
        assert_eq!(entry2_addr, HIMEM_START);
        assert_eq!(entry2_type, E820_RAM);
    }

    #[cfg(feature = "kvm")]
    #[test]
    fn create_vm_requires_kernel_path() {
        let hv = match KvmHypervisor::new() {
            Ok(hv) => hv,
            Err(err) => {
                assert!(err.to_string().contains("/dev/kvm"));
                return;
            }
        };
        let err = hv.create_vm(&VmConfig::default()).unwrap_err();
        assert!(err.to_string().contains("kernel path is required"));
    }
}
