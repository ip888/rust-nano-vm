//! Per-VM cgroup v2 setup + `execve()`.
//!
//! The jailer is a small privileged shim that runs once per VM,
//! does the cgroup work the worker can't do for itself (because
//! the worker has already cleared the privileges needed to create
//! a sibling cgroup), then `execve()`s into `nanovm-vmm-child`.
//! From the kernel's point of view the resulting process is in
//! the new cgroup with the requested `memory.max` and `cpu.max`
//! applied — and *every subprocess the worker spawns* inherits
//! the same cap. A fork-bomb inside the mock backend, an OOM
//! allocator loop inside a real-KVM guest, a busy-spin vCPU
//! thread — they all trip the kernel's per-VM cap and the rest
//! of the host stays up.
//!
//! ## Design
//!
//! - **Single binary, no daemon.** The jailer is invoked once per
//!   VM, finishes its setup, and `exec()`s into the worker. There's
//!   nothing to keep running.
//! - **Caller picks the parent cgroup.** Defaults to whatever
//!   cgroup we landed in (`/proc/self/cgroup`), so under systemd
//!   `Delegate=yes` the parent is the service's own slice; under a
//!   user session it's the user slice. Override with
//!   `--cgroup-parent` when the operator is doing something fancier
//!   (e.g. carving out a `nanovm.slice/tenant-a` per-tenant slice).
//! - **Fail loud on misconfig.** A garbage env var, a missing
//!   controller delegation, a leftover cgroup from a crashed
//!   predecessor — every one of these is an actionable diagnostic.
//!   We don't silently drop a cap.
//! - **No unsafe code.** `std::os::unix::process::CommandExt::exec`
//!   is safe Rust; we call it through `Command::exec`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};

/// Default cpu.max period in microseconds. 100 ms is the kernel
/// default and what systemd uses; keeping it matches operator
/// expectations and avoids surprising p99 jitter from a shorter
/// window.
const CPU_PERIOD_US: u64 = 100_000;

/// Root of the cgroup v2 unified hierarchy on every modern distro.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Per-VM jailer configuration. Constructed from CLI args in
/// `main.rs`; exposed as a struct so the unit tests can build it
/// without going through `clap`.
#[derive(Debug, Clone)]
pub struct JailerConfig {
    /// Numeric VM id. Used to name the per-VM cgroup directory
    /// (`nanovm-vm-<id>`) so siblings are clearly distinguishable
    /// in the cgroupfs.
    pub vm_id: u64,
    /// Memory cap in MiB. `None` skips the memory.max write.
    pub memory_limit_mib: Option<u64>,
    /// CPU quota in percent-of-one-CPU. `None` skips the
    /// cpu.max write. 100 → exactly one CPU; 200 → two CPUs.
    pub cpu_quota_pct: Option<u32>,
    /// Path the worker will bind to. We pass it through as
    /// `--socket` on the exec.
    pub socket: PathBuf,
    /// Absolute path to the `nanovm-vmm-child` binary the jailer
    /// will `exec()` into. We don't search `$PATH`: explicit is
    /// safer for a privileged helper.
    pub vmm_child_binary: PathBuf,
    /// Optional override for the parent cgroup directory. When
    /// `None`, the jailer uses its own cgroup as the parent.
    pub cgroup_parent: Option<PathBuf>,
}

/// Errors the jailer can produce. Distinct from
/// `vm_core::VmError`: jailer failures happen before any VM
/// exists, so the orchestrator handles them as
/// process-spawn-failed rather than VM-side errors.
#[derive(Debug, thiserror::Error)]
pub enum JailerError {
    /// Reading or writing a cgroup file failed.
    #[error("cgroup I/O at {path}: {source}")]
    Io {
        /// The file we were trying to read or write.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// `/proc/self/cgroup` either didn't exist or didn't contain a
    /// cgroup v2 line. Indicates a legacy cgroup v1 host or a
    /// hybrid that hides v2 from the unified-hierarchy view.
    #[error("no cgroup v2 line in /proc/self/cgroup (legacy cgroup v1 host?)")]
    NoCgroupV2,
    /// Requested controllers aren't in the parent's
    /// `cgroup.subtree_control`. The most common cause is running
    /// outside a systemd `Delegate=` service.
    #[error(
        "parent cgroup {parent} does not delegate controllers {missing:?} \
         (enable them in {parent}/cgroup.subtree_control, or run under a \
         systemd unit with `Delegate=memory cpu`)"
    )]
    ControllersMissing {
        /// Path of the parent cgroup that's missing delegations.
        parent: PathBuf,
        /// Which controllers were missing.
        missing: Vec<String>,
    },
    /// The child cgroup directory already exists. A crashed
    /// predecessor with the same VM id left it behind, or the
    /// orchestrator double-spawned. Either way, we won't clobber.
    #[error("child cgroup {path} already exists (rmdir + retry)")]
    AlreadyExists {
        /// The conflicting directory.
        path: PathBuf,
    },
    /// `Command::exec()` returned (it shouldn't if the exec
    /// succeeded; on Unix it only returns on failure).
    #[error("exec into {binary}: {source}")]
    Exec {
        /// The binary we tried to exec into.
        binary: PathBuf,
        /// The OS error from execve(2).
        #[source]
        source: std::io::Error,
    },
}

/// Read `/proc/self/cgroup` and return the v2 path
/// (everything after the `0::` prefix). Pure: just file I/O +
/// string parsing.
pub fn own_cgroup_path() -> Result<String, JailerError> {
    let path = PathBuf::from("/proc/self/cgroup");
    let txt = fs::read_to_string(&path).map_err(|source| JailerError::Io { path, source })?;
    parse_own_cgroup_line(&txt)
}

/// Pure parser used by [`own_cgroup_path`] and the tests.
fn parse_own_cgroup_line(proc_self_cgroup: &str) -> Result<String, JailerError> {
    for line in proc_self_cgroup.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Ok(rest.to_owned());
        }
    }
    Err(JailerError::NoCgroupV2)
}

/// Compute the child cgroup directory we'll create for this VM.
/// Returns `<CGROUP_ROOT>/<parent>/nanovm-vm-<vm_id>` — joining
/// the parent under the unified-hierarchy mount point and
/// suffixing the VM id.
pub fn child_cgroup_path(parent: &Path, vm_id: u64) -> PathBuf {
    let mut path = PathBuf::from(CGROUP_ROOT);
    let parent_str = parent.to_string_lossy();
    let trimmed = parent_str.trim_start_matches('/');
    if !trimmed.is_empty() {
        path.push(trimmed);
    }
    path.push(format!("nanovm-vm-{vm_id}"));
    path
}

/// Verify the parent has the listed controllers enabled in its
/// `cgroup.subtree_control`. Mirrors the
/// `vm-kvm::cgroups::check_controllers_delegated` shape.
pub fn check_controllers(parent: &Path, needed: &[&str]) -> Result<(), JailerError> {
    let path = parent.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&path).map_err(|source| JailerError::Io {
        path: path.clone(),
        source,
    })?;
    let tokens: Vec<&str> = enabled.split_ascii_whitespace().collect();
    let mut missing: Vec<String> = Vec::new();
    for &ctl in needed {
        if !tokens.contains(&ctl) {
            missing.push(ctl.to_owned());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(JailerError::ControllersMissing {
            parent: parent.to_path_buf(),
            missing,
        })
    }
}

/// Determine which controllers we need delegated based on the
/// configured caps. `None` for both = empty list (caller can skip
/// the delegation check entirely, but still needs cgroup.procs).
pub fn required_controllers(cfg: &JailerConfig) -> Vec<&'static str> {
    let mut out = Vec::with_capacity(2);
    if cfg.memory_limit_mib.is_some() {
        out.push("memory");
    }
    if cfg.cpu_quota_pct.is_some() {
        out.push("cpu");
    }
    out
}

/// Apply the jailer's configuration to the current process and
/// then `execve()` into the worker binary. Returns `Infallible`
/// on success because the running program has been replaced; the
/// `Err` arm covers every pre-exec failure mode.
///
/// Order matters:
/// 1. Resolve the parent cgroup.
/// 2. Verify the parent delegates the needed controllers.
/// 3. `create_dir` the child cgroup (no `_all`: EEXIST is loud).
/// 4. Write `memory.max` and/or `cpu.max`.
/// 5. Move self into the child via `cgroup.procs`.
/// 6. `exec()` into the worker, passing `--socket` through.
///
/// Steps 4 and 5 are intentionally last because moving into a
/// cgroup before writing caps means the kernel briefly accounts
/// our jailer process against the new cgroup. That's harmless
/// (the jailer immediately execs), but the canonical Firecracker
/// order is "cap first, attach last," so we follow it.
pub fn apply_isolation_and_exec(cfg: JailerConfig) -> Result<Infallible, JailerError> {
    let parent = match &cfg.cgroup_parent {
        Some(p) => p.clone(),
        None => {
            let own = own_cgroup_path()?;
            let trimmed = own.trim_start_matches('/');
            let mut p = PathBuf::from(CGROUP_ROOT);
            if !trimmed.is_empty() {
                p.push(trimmed);
            }
            p
        }
    };

    let needed = required_controllers(&cfg);
    if !needed.is_empty() {
        check_controllers(&parent, &needed)?;
    }

    let child = child_cgroup_path(&parent, cfg.vm_id);
    tracing::info!(
        vm_id = cfg.vm_id,
        cgroup = %child.display(),
        "creating per-VM cgroup"
    );
    fs::create_dir(&child).map_err(|source| match source.kind() {
        std::io::ErrorKind::AlreadyExists => JailerError::AlreadyExists {
            path: child.clone(),
        },
        _ => JailerError::Io {
            path: child.clone(),
            source,
        },
    })?;

    if let Some(mib) = cfg.memory_limit_mib {
        let bytes = mib.saturating_mul(1024 * 1024);
        let path = child.join("memory.max");
        fs::write(&path, bytes.to_string()).map_err(|source| JailerError::Io {
            path: path.clone(),
            source,
        })?;
        tracing::debug!(memory_max_bytes = bytes, "applied memory.max");
    }

    if let Some(pct) = cfg.cpu_quota_pct {
        let quota_us = u64::from(pct).saturating_mul(CPU_PERIOD_US) / 100;
        let path = child.join("cpu.max");
        fs::write(&path, format!("{quota_us} {CPU_PERIOD_US}")).map_err(|source| {
            JailerError::Io {
                path: path.clone(),
                source,
            }
        })?;
        tracing::debug!(cpu_max_quota_us = quota_us, "applied cpu.max");
    }

    let procs = child.join("cgroup.procs");
    fs::write(&procs, std::process::id().to_string()).map_err(|source| JailerError::Io {
        path: procs.clone(),
        source,
    })?;
    tracing::debug!("attached self to child cgroup");

    // exec into the worker. From here, errors only return on
    // failure; success replaces the process.
    exec_into_worker(&cfg)
}

#[cfg(unix)]
fn exec_into_worker(cfg: &JailerConfig) -> Result<Infallible, JailerError> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    let err = Command::new(&cfg.vmm_child_binary)
        .arg("--socket")
        .arg(&cfg.socket)
        .exec();
    Err(JailerError::Exec {
        binary: cfg.vmm_child_binary.clone(),
        source: err,
    })
}

#[cfg(not(unix))]
fn exec_into_worker(cfg: &JailerConfig) -> Result<Infallible, JailerError> {
    // The jailer is fundamentally Linux-only (it manipulates the
    // cgroup v2 hierarchy), so this branch only exists so the
    // library half compiles on non-Unix dev hosts for unit tests.
    Err(JailerError::Exec {
        binary: cfg.vmm_child_binary.clone(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exec() is Unix-only; jailer cannot run on this platform",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> JailerConfig {
        JailerConfig {
            vm_id: 7,
            memory_limit_mib: None,
            cpu_quota_pct: None,
            socket: PathBuf::from("/tmp/vm-7.sock"),
            vmm_child_binary: PathBuf::from("/usr/local/bin/nanovm-vmm-child"),
            cgroup_parent: None,
        }
    }

    #[test]
    fn parse_proc_self_cgroup_finds_v2_line() {
        let txt = "0::/user.slice/user-1000.slice\n";
        assert_eq!(
            parse_own_cgroup_line(txt).unwrap(),
            "/user.slice/user-1000.slice"
        );
    }

    #[test]
    fn parse_proc_self_cgroup_skips_v1_lines_and_finds_v2() {
        let txt = "\
12:memory:/some/v1/cgroup
11:cpuacct,cpu:/another
0::/the/v2/cgroup
";
        assert_eq!(parse_own_cgroup_line(txt).unwrap(), "/the/v2/cgroup");
    }

    #[test]
    fn parse_proc_self_cgroup_errors_when_no_v2() {
        let txt = "12:memory:/v1\n11:cpu:/v1\n";
        assert!(matches!(
            parse_own_cgroup_line(txt).unwrap_err(),
            JailerError::NoCgroupV2
        ));
    }

    #[test]
    fn child_cgroup_path_joins_root_parent_and_vm_id() {
        let p = child_cgroup_path(Path::new("/user.slice"), 42);
        assert_eq!(p, PathBuf::from("/sys/fs/cgroup/user.slice/nanovm-vm-42"));
    }

    #[test]
    fn child_cgroup_path_handles_root_parent() {
        // Edge case: parent is `/` (root cgroup). We must avoid
        // joining a leading `/` because Path::push("/x") clobbers
        // the prefix.
        let p = child_cgroup_path(Path::new("/"), 3);
        assert_eq!(p, PathBuf::from("/sys/fs/cgroup/nanovm-vm-3"));
    }

    #[test]
    fn required_controllers_reflects_caps() {
        let mut c = cfg();
        assert_eq!(required_controllers(&c), Vec::<&str>::new());
        c.memory_limit_mib = Some(128);
        assert_eq!(required_controllers(&c), vec!["memory"]);
        c.cpu_quota_pct = Some(50);
        assert_eq!(required_controllers(&c), vec!["memory", "cpu"]);
        c.memory_limit_mib = None;
        assert_eq!(required_controllers(&c), vec!["cpu"]);
    }

    #[test]
    fn check_controllers_passes_when_all_present() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().to_path_buf();
        fs::write(parent.join("cgroup.subtree_control"), "memory cpu io\n").unwrap();
        assert!(check_controllers(&parent, &["memory", "cpu"]).is_ok());
    }

    #[test]
    fn check_controllers_reports_missing_set() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().to_path_buf();
        fs::write(parent.join("cgroup.subtree_control"), "io\n").unwrap();
        match check_controllers(&parent, &["memory", "cpu"]) {
            Err(JailerError::ControllersMissing { missing, .. }) => {
                assert_eq!(missing, vec!["memory".to_owned(), "cpu".to_owned()]);
            }
            other => panic!("expected ControllersMissing, got {other:?}"),
        }
    }

    #[test]
    fn check_controllers_io_error_when_subtree_control_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = check_controllers(tmp.path(), &["memory"]).unwrap_err();
        assert!(matches!(err, JailerError::Io { .. }));
    }
}
