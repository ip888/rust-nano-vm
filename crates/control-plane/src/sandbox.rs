//! `POST /v1/sandbox/invoke` — ephemeral fork → action → destroy.
//!
//! Single endpoint with an `action` discriminator, mirroring the
//! contract used by nanolambda's MCP-facing API and any downstream
//! agent runner that expects a "give me a sandboxed result, I don't
//! care about VM lifecycle" shape.
//!
//! Lifecycle:
//!
//! 1. Resolve a snapshot id — caller-supplied `snapshot` wins,
//!    otherwise falls back to `NANOVM_SANDBOX_SNAPSHOT_ID`.
//! 2. Pop a pre-warmed fork from the warm pool if one is available
//!    (sub-millisecond), else cold-restore from the snapshot.
//! 3. Dispatch the action against the resulting VM.
//! 4. Destroy the VM unconditionally — sandbox VMs never leak.
//! 5. Return the result envelope, including `cold_start` (true iff
//!    the VM was cold-restored) and total wall-clock `duration_ms`.
//!
//! Per-VM resource caps are handled at the VMM process level by the
//! cgroups wiring in `vm-kvm`; this endpoint inherits those caps for
//! free without any extra plumbing.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{rejection::JsonRejection, State},
    Json,
};
use serde::{Deserialize, Serialize};
use vm_core::{GuestExecRequest, Hypervisor, SnapshotId, VmId};

use crate::error::ApiError;
use crate::routes::AppState;

/// Per-action payload. `#[serde(tag = "action")]` makes the request
/// body a tagged union — the discriminator is a sibling field of the
/// action's parameters, matching nanolambda's flat-JSON shape.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum SandboxAction {
    /// Run a Python program. Equivalent to `python3 -c <code>`
    /// inside the guest.
    ExecutePython {
        code: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Run a shell command. Equivalent to `sh -c <command>`
    /// inside the guest.
    ExecuteShell {
        command: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Read a file from the guest filesystem. The file content
    /// is returned (UTF-8 lossy) in `stdout`.
    ReadFile { path: String },
    /// Write a file to the guest filesystem. `content` is the file
    /// body as a UTF-8 string; `mode` defaults to `0o644`.
    WriteFile {
        path: String,
        content: String,
        #[serde(default)]
        mode: Option<u32>,
    },
    /// List directory entries. Returns one path per line in
    /// `stdout`, equivalent to `ls -1 <path>` inside the guest.
    ListFiles { path: String },
}

/// Top-level request body. `snapshot` is optional; when omitted the
/// server falls back to `NANOVM_SANDBOX_SNAPSHOT_ID`.
#[derive(Debug, Deserialize)]
pub(crate) struct SandboxInvokeRequest {
    #[serde(default)]
    pub snapshot: Option<u64>,
    #[serde(flatten)]
    pub action: SandboxAction,
}

/// Flat result envelope. `exit_code` follows POSIX convention: a
/// signal-killed process is reported as `128 + signal`. File-op
/// actions report `exit_code = 0` on success; failures surface as
/// an `ApiError` 4xx/5xx instead.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct SandboxResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub cold_start: bool,
}

/// Env var holding the default snapshot id when the caller doesn't
/// pass one. Exposed as a constant so tests can flip it safely.
pub(crate) const DEFAULT_SNAPSHOT_ENV: &str = "NANOVM_SANDBOX_SNAPSHOT_ID";

/// Resolve the snapshot id for an invoke call given a body-supplied
/// value and the raw env-var string. Pure function — the env read
/// happens in [`resolve_snapshot`] so tests can exercise the
/// precedence rules without touching process env.
fn resolve_snapshot_with(
    req_snapshot: Option<u64>,
    env_value: Option<&str>,
) -> Result<SnapshotId, ApiError> {
    if let Some(id) = req_snapshot {
        return Ok(SnapshotId(id));
    }
    match env_value {
        Some(s) => s
            .parse::<u64>()
            .map(SnapshotId)
            .map_err(|_| ApiError::Bad(format!("{DEFAULT_SNAPSHOT_ENV} must be a u64; got {s:?}"))),
        None => Err(ApiError::Bad(format!(
            "no snapshot id: pass `snapshot` in body or set {DEFAULT_SNAPSHOT_ENV}"
        ))),
    }
}

/// Resolve the snapshot id for an invoke call: caller-supplied wins,
/// else `NANOVM_SANDBOX_SNAPSHOT_ID`, else a 400.
fn resolve_snapshot(req_snapshot: Option<u64>) -> Result<SnapshotId, ApiError> {
    let env_value = std::env::var(DEFAULT_SNAPSHOT_ENV).ok();
    resolve_snapshot_with(req_snapshot, env_value.as_deref())
}

/// Run an action against an already-allocated VM and produce a
/// result envelope (without the `duration_ms` / `cold_start` fields,
/// which the caller stamps after destroy).
fn dispatch(
    hv: &Arc<dyn Hypervisor>,
    vm: VmId,
    action: SandboxAction,
) -> Result<DispatchOutcome, ApiError> {
    match action {
        SandboxAction::ExecutePython { code, timeout_ms } => {
            let req = GuestExecRequest {
                program: "python3".to_owned(),
                args: vec!["-c".to_owned(), code],
                cwd: None,
                env: Vec::new(),
                timeout_ms,
            };
            let r = hv.exec_in_guest(vm, req)?;
            Ok(DispatchOutcome::from_exec(r))
        }
        SandboxAction::ExecuteShell {
            command,
            timeout_ms,
        } => {
            let req = GuestExecRequest {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), command],
                cwd: None,
                env: Vec::new(),
                timeout_ms,
            };
            let r = hv.exec_in_guest(vm, req)?;
            Ok(DispatchOutcome::from_exec(r))
        }
        SandboxAction::ReadFile { path } => {
            let bytes = hv.read_file(vm, path)?;
            Ok(DispatchOutcome {
                stdout: String::from_utf8_lossy(&bytes).into_owned(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
        SandboxAction::WriteFile {
            path,
            content,
            mode,
        } => {
            let mode = mode.unwrap_or(0o644);
            let bytes = hv.write_file(vm, path, content.into_bytes(), mode)?;
            Ok(DispatchOutcome {
                stdout: format!("bytes_written={bytes}"),
                stderr: String::new(),
                exit_code: 0,
            })
        }
        SandboxAction::ListFiles { path } => {
            // `--` terminates option parsing, so a caller-supplied
            // `path` starting with `-` is treated as the directory
            // operand instead of an `ls` flag.
            let req = GuestExecRequest {
                program: "ls".to_owned(),
                args: vec!["-1".to_owned(), "--".to_owned(), path],
                cwd: None,
                env: Vec::new(),
                timeout_ms: None,
            };
            let r = hv.exec_in_guest(vm, req)?;
            Ok(DispatchOutcome::from_exec(r))
        }
    }
}

struct DispatchOutcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

impl DispatchOutcome {
    fn from_exec(r: vm_core::GuestExecResult) -> Self {
        // Flatten exit-code/signal into a single integer matching the
        // POSIX shell convention. Signal-killed → 128 + signal.
        let exit_code = match (r.exit_code, r.signal) {
            (Some(code), _) => code,
            (None, Some(sig)) => 128 + sig,
            (None, None) => -1,
        };
        Self {
            stdout: String::from_utf8_lossy(&r.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
            exit_code,
        }
    }
}

pub(crate) async fn sandbox_invoke(
    State(state): State<AppState>,
    body: Result<Json<SandboxInvokeRequest>, JsonRejection>,
) -> Result<Json<SandboxResult>, ApiError> {
    let Json(req) = body?;
    let snapshot_id = resolve_snapshot(req.snapshot)?;

    let started = Instant::now();
    let (handle, cold_start) = match state.warm_pool().take(snapshot_id) {
        Some(h) => {
            state.metrics().record_warm_hit();
            (h, false)
        }
        None => {
            state.metrics().record_warm_miss();
            (state.hypervisor().restore(snapshot_id)?, true)
        }
    };
    let vm_id = handle.id;

    let outcome = dispatch(state.hypervisor(), vm_id, req.action);

    // Ephemeral semantics: VM is always destroyed, even on error.
    // The action result still goes back to the caller; a destroy
    // failure is an operator concern (potential VM leak / cgroup
    // leftover), so we surface it via `tracing` instead of bubbling
    // a 5xx that would mask the action outcome.
    if let Err(err) = state.hypervisor().destroy(vm_id) {
        tracing::warn!(vm_id = vm_id.0, ?err, "sandbox: VM destroy failed");
    }

    let outcome = outcome?;
    Ok(Json(SandboxResult {
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        exit_code: outcome.exit_code,
        duration_ms: started.elapsed().as_millis() as u64,
        cold_start,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_execute_python_action() {
        let json = r#"{"action": "execute_python", "code": "print(1)"}"#;
        let req: SandboxInvokeRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            req.action,
            SandboxAction::ExecutePython { ref code, timeout_ms: None } if code == "print(1)"
        ));
        assert_eq!(req.snapshot, None);
    }

    #[test]
    fn parses_execute_shell_with_snapshot_and_timeout() {
        let json =
            r#"{"snapshot": 7, "action": "execute_shell", "command": "ls", "timeout_ms": 500}"#;
        let req: SandboxInvokeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.snapshot, Some(7));
        assert!(matches!(
            req.action,
            SandboxAction::ExecuteShell { ref command, timeout_ms: Some(500) } if command == "ls"
        ));
    }

    #[test]
    fn parses_file_actions() {
        let read: SandboxInvokeRequest =
            serde_json::from_str(r#"{"action":"read_file","path":"/etc/hostname"}"#).unwrap();
        assert!(
            matches!(read.action, SandboxAction::ReadFile { ref path } if path == "/etc/hostname")
        );

        let write: SandboxInvokeRequest = serde_json::from_str(
            r#"{"action":"write_file","path":"/tmp/x","content":"hi","mode":420}"#,
        )
        .unwrap();
        assert!(matches!(write.action,
            SandboxAction::WriteFile { ref path, ref content, mode: Some(420) }
            if path == "/tmp/x" && content == "hi"));

        let list: SandboxInvokeRequest =
            serde_json::from_str(r#"{"action":"list_files","path":"/tmp"}"#).unwrap();
        assert!(matches!(list.action, SandboxAction::ListFiles { ref path } if path == "/tmp"));
    }

    #[test]
    fn rejects_unknown_action() {
        let json = r#"{"action": "rm_rf", "path": "/"}"#;
        assert!(serde_json::from_str::<SandboxInvokeRequest>(json).is_err());
    }

    #[test]
    fn snapshot_from_request_wins_over_env() {
        let resolved = resolve_snapshot_with(Some(7), Some("99")).unwrap();
        assert_eq!(resolved, SnapshotId(7));
    }

    #[test]
    fn snapshot_falls_back_to_env() {
        let resolved = resolve_snapshot_with(None, Some("42")).unwrap();
        assert_eq!(resolved, SnapshotId(42));
    }

    #[test]
    fn snapshot_400s_when_unset() {
        let err = resolve_snapshot_with(None, None).unwrap_err();
        assert!(matches!(err, ApiError::Bad(_)));
    }

    #[test]
    fn snapshot_400s_when_env_is_garbage() {
        let err = resolve_snapshot_with(None, Some("not-a-number")).unwrap_err();
        assert!(matches!(err, ApiError::Bad(_)));
    }
}
