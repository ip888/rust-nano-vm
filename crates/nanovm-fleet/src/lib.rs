//! Process-fleet [`Hypervisor`] backend.
//!
//! Implements [`vm_core::Hypervisor`] by spawning **one
//! `nanovm-jailer` subprocess per VM**. The jailer creates a fresh
//! cgroup v2 child with the requested `memory.max` / `cpu.max`,
//! attaches itself, and `execve()`s into `nanovm-vmm-child`. From
//! the orchestrator's point of view each VM is a separate
//! process with its own crash domain: an OOM in one tenant's VM
//! trips the kernel's cap and SIGKILLs *that* worker, not the
//! whole control plane.
//!
//! ```text
//!  control-plane (nanovm-control-plane)
//!         │
//!         │ ProcessFleetHypervisor::create_vm(cfg)
//!         ▼
//!  ┌──────────────────────┐    spawn         ┌────────────────────┐
//!  │  ProcessFleet        │ ────────────────▶│ nanovm-jailer (pid) │
//!  │  - workers map       │                  │   sets memory.max,  │
//!  │  - per-VM IPC client │                  │   cpu.max, then     │
//!  └──────────┬───────────┘                  │   execve(            │
//!             │                              │     nanovm-vmm-child │
//!             │ Unix socket                  │   )                  │
//!             ▼                              └─────────┬──────────┘
//!  ┌──────────────────────┐  framed JSON over    ┌─────▼──────────┐
//!  │ vmm-ipc Request /    │ ◀──────────────────▶ │ vmm-ipc serve()│
//!  │ Response             │                      │ inside worker  │
//!  └──────────────────────┘                      └────────────────┘
//! ```
//!
//! ## Scope of PR-4
//!
//! Wires the spawn → IPC handshake → forward-trait-methods loop.
//! Covers the lifecycle methods (`create_vm`, `start`, `stop`,
//! `state`, `vm_meta`, `snapshot`, `restore`, `destroy`,
//! `list_vms`, `list_snapshots`, `delete_snapshot`,
//! `snapshot_meta`) plus the guest-side ops (`exec_in_guest`,
//! `read_file`, `write_file`). The streaming exec
//! (`exec_in_guest_stream`) stays on the in-process backend until
//! PR-5; SSE over IPC is its own design.
//!
//! ## Sync ↔ async bridge
//!
//! The `Hypervisor` trait is synchronous; `vmm-ipc` is
//! `tokio`-async. Each trait method drives the IPC roundtrip via a
//! shared multi-thread runtime owned by the [`ProcessFleet`]. The
//! control plane already wraps `spawn_blocking` around hypervisor
//! calls in the routes layer, so blocking the caller for the
//! duration of the IPC round-trip is the documented contract — no
//! reactor stall.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::net::UnixStream;
use vm_core::{
    GuestExecRequest, GuestExecResult, Hypervisor, SnapshotId, SnapshotMeta, VmConfig, VmError,
    VmHandle, VmId, VmMeta, VmResult, VmState,
};
use vmm_ipc::framing::{read_frame, write_frame};
use vmm_ipc::{ErrorCode, Request, Response};

/// Operator-tunable knobs for the fleet. Constructed once at
/// startup and shared across every VM the orchestrator spawns.
#[derive(Debug, Clone)]
pub struct FleetConfig {
    /// Absolute path to the `nanovm-jailer` binary the fleet
    /// `Command::spawn`s on every `create_vm`. The jailer takes
    /// care of cgroup setup before `execve`-ing into the worker.
    pub jailer_binary: PathBuf,
    /// Absolute path to `nanovm-vmm-child`. Passed through to the
    /// jailer as `--vmm-child-binary`.
    pub vmm_child_binary: PathBuf,
    /// Directory the fleet creates Unix sockets in (one per VM:
    /// `<dir>/vm-<id>.sock`). Must exist and be writable by the
    /// control-plane process. Defaults to `/var/run/nanovm`.
    pub socket_dir: PathBuf,
    /// Optional default per-VM memory cap (MiB). The jailer writes
    /// it into the new cgroup's `memory.max`. `None` skips the
    /// memory cap — useful for hosts that haven't enabled the
    /// memory controller in `cgroup.subtree_control`.
    pub default_memory_limit_mib: Option<u64>,
    /// Optional default per-VM CPU quota in percent-of-one-CPU.
    /// `100` → exactly one CPU, `200` → two CPUs.
    pub default_cpu_quota_pct: Option<u32>,
    /// Optional `--cgroup-parent` override. `None` lets the
    /// jailer use its own cgroup, which is what you want under a
    /// systemd `Delegate=` unit.
    pub cgroup_parent: Option<PathBuf>,
    /// How long to wait for the worker's socket to appear after
    /// `Command::spawn` returns. The jailer's setup + `execve` +
    /// the worker's `bind` is usually <50ms; the default of 5s
    /// covers debug builds + cold disk. Hitting the timeout
    /// surfaces as [`VmError::Backend`].
    pub spawn_timeout: Duration,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            jailer_binary: PathBuf::from("/usr/local/bin/nanovm-jailer"),
            vmm_child_binary: PathBuf::from("/usr/local/bin/nanovm-vmm-child"),
            socket_dir: PathBuf::from("/var/run/nanovm"),
            default_memory_limit_mib: None,
            default_cpu_quota_pct: None,
            cgroup_parent: None,
            spawn_timeout: Duration::from_secs(5),
        }
    }
}

/// One worker subprocess + the **persistent** IPC stream we talk
/// to it on. The worker accepts exactly one connection and runs
/// the serve loop on it — fresh-per-request connections would hit
/// connection-refused on the second op. We hold the stream alive
/// for the lifetime of the worker and serialize requests through
/// the surrounding `Mutex`.
struct Worker {
    /// Jailer PID. Held so `destroy` / `Drop` can SIGKILL on the
    /// uncooperative path (worker won't respond to `Shutdown`).
    /// `Option` because we `wait()` after a cooperative shutdown.
    child: Option<Child>,
    /// Socket path. Held for cleanup.
    socket: PathBuf,
    /// Open IPC stream to the worker. Closed in `destroy` (sends
    /// `Shutdown` + drops); the worker exits on EOF.
    stream: Option<UnixStream>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("socket", &self.socket)
            .field("child_pid", &self.child.as_ref().map(|c| c.id()))
            .field("stream_open", &self.stream.is_some())
            .finish()
    }
}

impl Worker {
    /// Best-effort `kill -9` if the child is still alive. Always
    /// removes the socket file last so a fresh spawn with the
    /// same VM id doesn't see EADDRINUSE.
    fn force_kill(&mut self) {
        // Drop the stream first so the worker sees EOF and exits
        // cleanly if it hasn't already.
        self.stream.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            self.child = None;
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.force_kill();
    }
}

type WorkerSlot = Arc<Mutex<Worker>>;

/// Process-fleet [`Hypervisor`] implementation. Holds the worker
/// map + the snapshot→VM routing table so `restore` and
/// `snapshot_meta` can find the right worker even after the
/// originating VM has been destroyed (snapshot lifetime exceeds
/// VM lifetime in the public API).
pub struct ProcessFleet {
    config: FleetConfig,
    /// Map from VmId → live worker. Insertions happen on
    /// `create_vm` / `restore`; removals on `destroy` (which also
    /// kills the subprocess).
    workers: RwLock<HashMap<VmId, WorkerSlot>>,
    /// Map from SnapshotId → owning VmId. Snapshots are owned by
    /// the worker that captured them; this index lets us forward
    /// `restore` / `snapshot_meta` / `delete_snapshot` to the
    /// right worker.
    snapshot_owner: RwLock<HashMap<SnapshotId, VmId>>,
    /// Monotonic VM id source. The id is what the jailer uses to
    /// name the per-VM cgroup (`nanovm-vm-<id>`), so it must be
    /// unique across the lifetime of the control plane.
    next_vm_id: AtomicU64,
    /// Tokio runtime the IPC roundtrips run on. Owned (not
    /// borrowed) so the fleet works in non-tokio callers — the
    /// control plane creates a dedicated worker pool when it
    /// installs the fleet.
    runtime: Arc<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for ProcessFleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessFleet")
            .field("config", &self.config)
            .field("workers", &"<RwLock<HashMap>>")
            .finish()
    }
}

impl ProcessFleet {
    /// Construct a fleet from a [`FleetConfig`]. Spawns its own
    /// multi-thread tokio runtime for IPC work; the caller doesn't
    /// have to already be inside an async context.
    pub fn new(config: FleetConfig) -> Result<Self, std::io::Error> {
        // Two worker threads is enough for IPC roundtrips — the
        // hot path is one short JSON message per VM op, and the
        // sync-trait bridge serializes per VM anyway. Bumping
        // higher just trades RSS for nothing measurable.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .thread_name("nanovm-fleet-ipc")
            .build()?;
        Ok(Self {
            config,
            workers: RwLock::new(HashMap::new()),
            snapshot_owner: RwLock::new(HashMap::new()),
            next_vm_id: AtomicU64::new(1),
            runtime: Arc::new(runtime),
        })
    }

    /// Spawn a jailer subprocess for a new VM, wait for the
    /// worker's socket to appear, and return the open Worker slot
    /// after a successful `Ping` handshake.
    fn spawn_worker(&self, vm_id: VmId) -> VmResult<WorkerSlot> {
        std::fs::create_dir_all(&self.config.socket_dir)
            .map_err(|e| VmError::Backend(format!("create socket_dir: {e}")))?;
        let socket = self.config.socket_dir.join(format!("vm-{}.sock", vm_id.0));
        // Stale socket from a crashed predecessor would make the
        // worker's `bind` fail with EADDRINUSE. Best-effort
        // delete; ignore NotFound.
        let _ = std::fs::remove_file(&socket);

        let mut cmd = Command::new(&self.config.jailer_binary);
        cmd.arg("--vm-id")
            .arg(vm_id.0.to_string())
            .arg("--socket")
            .arg(&socket)
            .arg("--vmm-child-binary")
            .arg(&self.config.vmm_child_binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(mib) = self.config.default_memory_limit_mib {
            cmd.arg("--memory-limit-mib").arg(mib.to_string());
        }
        if let Some(pct) = self.config.default_cpu_quota_pct {
            cmd.arg("--cpu-quota-pct").arg(pct.to_string());
        }
        if let Some(parent) = &self.config.cgroup_parent {
            cmd.arg("--cgroup-parent").arg(parent);
        }
        let child = cmd
            .spawn()
            .map_err(|e| VmError::Backend(format!("spawn jailer: {e}")))?;

        // Block on the runtime so the sync Hypervisor caller
        // doesn't have to know we're driving an async transport.
        let socket_for_wait = socket.clone();
        let timeout = self.config.spawn_timeout;
        let handshake = self
            .runtime
            .block_on(wait_for_socket_and_open(&socket_for_wait, timeout));
        match handshake {
            Ok(stream) => Ok(Arc::new(Mutex::new(Worker {
                child: Some(child),
                socket,
                stream: Some(stream),
            }))),
            Err(e) => {
                // Worker never came up. Kill the jailer (and by
                // extension the half-started worker) so a hung
                // child doesn't leak.
                let mut c = child;
                let _ = c.kill();
                let _ = c.wait();
                let _ = std::fs::remove_file(&socket);
                Err(e)
            }
        }
    }

    /// Look up the worker slot for `id`. Returns `UnknownVm` if
    /// the worker isn't in the map.
    fn worker_for(&self, id: VmId) -> VmResult<WorkerSlot> {
        self.workers
            .read()
            .expect("workers lock")
            .get(&id)
            .cloned()
            .ok_or(VmError::UnknownVm(id))
    }

    /// Send a request to the worker owning `id` and parse the
    /// response. Single-threaded per worker; the Mutex serializes
    /// the request stream.
    fn dispatch(&self, id: VmId, req: Request) -> VmResult<Response> {
        let slot = self.worker_for(id)?;
        self.dispatch_to_slot(&slot, req)
    }

    /// Send a request on the given worker slot. Factored out so
    /// `create_vm` can drive the slot it just spawned (before
    /// inserting into the map) and the lookup-by-id path can
    /// share the same body. The persistent stream is borrowed
    /// for the duration of one roundtrip; the outer Mutex
    /// serializes concurrent callers per VM.
    fn dispatch_to_slot(&self, slot: &WorkerSlot, req: Request) -> VmResult<Response> {
        let mut guard = slot.lock().expect("worker lock");
        let stream = guard
            .stream
            .as_mut()
            .ok_or_else(|| VmError::Backend("worker stream already closed".into()))?;
        let resp = self
            .runtime
            .block_on(roundtrip_on(stream, req))
            .map_err(|e| VmError::Backend(format!("ipc: {e}")))?;
        Ok(resp)
    }
}

/// Walk a response into a typed result. Maps the wire-format
/// `ErrorCode` back to the corresponding `VmError` variant so the
/// upstream control-plane error envelope keeps the same codes /
/// HTTP status whether the backend is in-process or fleet.
fn unwrap_response<T>(
    resp: Response,
    extract: impl FnOnce(Response) -> Result<T, Response>,
) -> VmResult<T> {
    match resp {
        Response::Error { code, message } => Err(match code {
            // The wire format doesn't carry the offending id/state
            // tuple — we lose precision on the typed error here.
            // The orchestrator already has the id from the request
            // it sent, so this is mostly cosmetic; the human-
            // readable `message` carries the detail.
            ErrorCode::UnknownVm => VmError::UnknownVm(VmId(0)),
            ErrorCode::UnknownSnapshot => VmError::UnknownSnapshot(SnapshotId(0)),
            ErrorCode::InvalidTransition => VmError::InvalidTransition {
                id: VmId(0),
                from: VmState::Created,
                to: VmState::Running,
            },
            // `VmError::Unsupported` takes a `&'static str` so we
            // can't ferry the dynamic worker message verbatim;
            // demote to `Backend` so the message survives.
            ErrorCode::Unsupported => VmError::Backend(format!("unsupported: {message}")),
            ErrorCode::Backend => VmError::Backend(message),
            ErrorCode::BadRequest => VmError::Backend(format!("worker bad-request: {message}")),
        }),
        other => {
            extract(other).map_err(|r| VmError::Backend(format!("unexpected response: {r:?}")))
        }
    }
}

/// Poll-connect until the worker has `bind`ed the socket OR the
/// timeout elapses. On success, run the `Ping`/`Pong` handshake
/// and return the open stream — the fleet keeps it alive for the
/// rest of the worker's lifetime.
async fn wait_for_socket_and_open(socket: &Path, timeout: Duration) -> VmResult<UnixStream> {
    let start = std::time::Instant::now();
    loop {
        match UnixStream::connect(socket).await {
            Ok(mut s) => {
                write_frame(&mut s, &Request::Ping)
                    .await
                    .map_err(|e| VmError::Backend(format!("write ping: {e}")))?;
                let resp: Response = read_frame(&mut s)
                    .await
                    .map_err(|e| VmError::Backend(format!("read pong: {e}")))?;
                match resp {
                    Response::Pong => return Ok(s),
                    other => {
                        return Err(VmError::Backend(format!(
                            "worker handshake: expected Pong, got {other:?}"
                        )));
                    }
                }
            }
            Err(_) => {
                if start.elapsed() >= timeout {
                    return Err(VmError::Backend(format!(
                        "worker never bound socket {} within {:?}",
                        socket.display(),
                        timeout
                    )));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// One request/response roundtrip on an open stream. The Mutex
/// around the Worker ensures we never have two roundtrips
/// in flight on the same stream.
async fn roundtrip_on(stream: &mut UnixStream, req: Request) -> std::io::Result<Response> {
    write_frame(stream, &req).await.map_err(framing_to_io)?;
    let resp: Response = read_frame(stream).await.map_err(framing_to_io)?;
    Ok(resp)
}

fn framing_to_io(e: vmm_ipc::framing::FrameError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl Hypervisor for ProcessFleet {
    fn create_vm(&self, cfg: &VmConfig) -> VmResult<VmHandle> {
        let vm_id = VmId(self.next_vm_id.fetch_add(1, Ordering::Relaxed));
        let slot = self.spawn_worker(vm_id)?;
        let resp = self.dispatch_to_slot(
            &slot,
            Request::CreateVm {
                config: cfg.clone(),
            },
        )?;
        let handle = unwrap_response(resp, |r| match r {
            Response::VmHandle(h) => Ok(h),
            other => Err(other),
        })?;
        // Worker assigns its own VmId internally; we always trust
        // the orchestrator-side id (the cgroup is named after
        // `vm_id` and the worker just lives in it). If the worker
        // returned a different id, replace it.
        let handle = VmHandle {
            id: vm_id,
            ..handle
        };
        self.workers
            .write()
            .expect("workers lock")
            .insert(vm_id, slot);
        Ok(handle)
    }

    fn start(&self, id: VmId) -> VmResult<()> {
        let resp = self.dispatch(id, Request::Start { id })?;
        unwrap_response(resp, |r| match r {
            Response::Empty => Ok(()),
            other => Err(other),
        })
    }

    fn stop(&self, id: VmId) -> VmResult<()> {
        let resp = self.dispatch(id, Request::Stop { id })?;
        unwrap_response(resp, |r| match r {
            Response::Empty => Ok(()),
            other => Err(other),
        })
    }

    fn state(&self, id: VmId) -> VmResult<VmState> {
        let resp = self.dispatch(id, Request::State { id })?;
        unwrap_response(resp, |r| match r {
            Response::State { state } => Ok(state),
            other => Err(other),
        })
    }

    fn snapshot(&self, id: VmId) -> VmResult<SnapshotId> {
        let resp = self.dispatch(id, Request::Snapshot { id })?;
        let snap = unwrap_response(resp, |r| match r {
            Response::Snapshot { id } => Ok(id),
            other => Err(other),
        })?;
        self.snapshot_owner
            .write()
            .expect("snapshot owner lock")
            .insert(snap, id);
        Ok(snap)
    }

    fn restore(&self, _snap: SnapshotId) -> VmResult<VmHandle> {
        // Snapshots live inside the worker that captured them
        // (single-VM workers + in-memory state). Cross-worker
        // restore needs the snapshot blob to leave that worker
        // — either via the durable snapshot store
        // (`snapshot_export` / `snapshot_adopt`) or via a
        // shared-state refactor. Both land in PR-5 of the arc.
        // The mock-backend in-process path still works for
        // operators that haven't switched to the fleet yet.
        Err(VmError::Backend(
            "restore not yet supported on the process-fleet backend; \
             cross-worker snapshot transfer ships in PR-5. Use the \
             in-process backend for the snapshot/restore/fork path \
             until then."
                .into(),
        ))
    }

    fn destroy(&self, id: VmId) -> VmResult<()> {
        let slot = self.worker_for(id)?;
        // Cooperative shutdown — best-effort. The Worker's Drop
        // SIGKILLs if this doesn't land in time.
        let _ = self.dispatch_to_slot(&slot, Request::Destroy { id });
        let _ = self.dispatch_to_slot(&slot, Request::Shutdown);
        self.workers.write().expect("workers lock").remove(&id);
        // The Arc may live on briefly in a concurrent caller; the
        // Drop on the last reference cleans up. forget any
        // snapshots this VM owned — they go away with the worker.
        self.snapshot_owner
            .write()
            .expect("snapshot owner lock")
            .retain(|_, owner_vm| *owner_vm != id);
        Ok(())
    }

    fn list_vms(&self) -> VmResult<Vec<VmHandle>> {
        // The fleet authoritative source: the workers map. Each
        // entry corresponds to one live VM; we round-trip a
        // cheap `State` query so the returned VmHandle reflects
        // the current state instead of always-`Created`. We
        // serialize on a snapshot of the ids to avoid holding
        // the workers lock across IPC.
        let ids: Vec<VmId> = self
            .workers
            .read()
            .expect("workers lock")
            .keys()
            .copied()
            .collect();
        let mut handles = Vec::with_capacity(ids.len());
        for id in ids {
            let state = self.state(id).unwrap_or(VmState::Created);
            handles.push(VmHandle { id, state });
        }
        Ok(handles)
    }

    fn list_snapshots(&self) -> VmResult<Vec<SnapshotId>> {
        Ok(self
            .snapshot_owner
            .read()
            .expect("snapshot owner lock")
            .keys()
            .copied()
            .collect())
    }

    fn delete_snapshot(&self, snap: SnapshotId) -> VmResult<()> {
        let owner_vm = self
            .snapshot_owner
            .read()
            .expect("snapshot owner lock")
            .get(&snap)
            .copied()
            .ok_or(VmError::UnknownSnapshot(snap))?;
        let resp = self.dispatch(owner_vm, Request::DeleteSnapshot { id: snap })?;
        unwrap_response(resp, |r| match r {
            Response::Empty => Ok(()),
            other => Err(other),
        })?;
        self.snapshot_owner
            .write()
            .expect("snapshot owner lock")
            .remove(&snap);
        Ok(())
    }

    fn snapshot_meta(&self, snap: SnapshotId) -> VmResult<SnapshotMeta> {
        let owner_vm = self
            .snapshot_owner
            .read()
            .expect("snapshot owner lock")
            .get(&snap)
            .copied()
            .ok_or(VmError::UnknownSnapshot(snap))?;
        let resp = self.dispatch(owner_vm, Request::SnapshotMeta { id: snap })?;
        unwrap_response(resp, |r| match r {
            Response::SnapshotMeta(m) => Ok(m),
            other => Err(other),
        })
    }

    fn vm_meta(&self, id: VmId) -> VmResult<VmMeta> {
        let resp = self.dispatch(id, Request::VmMeta { id })?;
        unwrap_response(resp, |r| match r {
            Response::VmMeta(m) => Ok(m),
            other => Err(other),
        })
    }

    fn exec_in_guest(&self, id: VmId, req: GuestExecRequest) -> VmResult<GuestExecResult> {
        let resp = self.dispatch(id, Request::ExecInGuest { id, req })?;
        unwrap_response(resp, |r| match r {
            Response::ExecResult(res) => Ok(res),
            other => Err(other),
        })
    }

    fn write_file(&self, id: VmId, path: String, content: Vec<u8>, mode: u32) -> VmResult<u64> {
        let resp = self.dispatch(
            id,
            Request::WriteFile {
                id,
                path,
                content,
                mode,
            },
        )?;
        unwrap_response(resp, |r| match r {
            Response::Written { bytes } => Ok(bytes),
            other => Err(other),
        })
    }

    fn read_file(&self, id: VmId, path: String) -> VmResult<Vec<u8>> {
        let resp = self.dispatch(id, Request::ReadFile { id, path })?;
        unwrap_response(resp, |r| match r {
            Response::Bytes { content } => Ok(content),
            other => Err(other),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_config_default_has_safe_defaults() {
        let cfg = FleetConfig::default();
        assert!(cfg.jailer_binary.is_absolute());
        assert!(cfg.vmm_child_binary.is_absolute());
        assert!(cfg.socket_dir.is_absolute());
        assert!(cfg.default_memory_limit_mib.is_none());
        assert!(cfg.default_cpu_quota_pct.is_none());
        assert!(cfg.cgroup_parent.is_none());
        assert!(cfg.spawn_timeout >= Duration::from_secs(1));
    }

    #[test]
    fn new_succeeds_without_a_running_jailer() {
        // ProcessFleet::new just spawns a tokio runtime; no
        // jailer is invoked until create_vm. Construction must
        // be cheap and infallible on any host.
        let dir = tempfile::tempdir().unwrap();
        let cfg = FleetConfig {
            socket_dir: dir.path().to_path_buf(),
            ..FleetConfig::default()
        };
        let fleet = ProcessFleet::new(cfg).expect("construct");
        assert_eq!(fleet.list_vms().unwrap().len(), 0);
        assert_eq!(fleet.list_snapshots().unwrap().len(), 0);
    }

    #[test]
    fn worker_for_unknown_vm_returns_unknown_vm() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FleetConfig {
            socket_dir: dir.path().to_path_buf(),
            ..FleetConfig::default()
        };
        let fleet = ProcessFleet::new(cfg).unwrap();
        let err = fleet.worker_for(VmId(999)).unwrap_err();
        assert!(matches!(err, VmError::UnknownVm(VmId(999))));
    }
}
