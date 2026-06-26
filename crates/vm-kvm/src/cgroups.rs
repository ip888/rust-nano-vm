//! Cgroups v2 resource caps on the VMM process.
//!
//! This is the v1 of resource isolation for `rust-nano-vm`: the VMM
//! process itself is moved into a fresh cgroup v2 child and capped on
//! memory and CPU. The intent is **host self-protection** — a runaway
//! VMM (fork bomb in a guest, runaway allocation, leaked thread) is
//! killed by the kernel via `memory.max` / `cpu.max` instead of
//! bringing the box down.
//!
//! ## What this is NOT
//!
//! These caps are *process-wide*, not per-VM. Every VM hosted by a
//! single VMM shares the same memory and CPU budget. That's a real
//! limitation: in a multi-tenant deployment, tenant A can starve
//! tenant B inside one VMM. Doing it right (per-VM proper caps)
//! requires a one-VMM-process-per-VM rearchitecture; that's a future
//! milestone documented in the README. For the v1 monolithic VMM,
//! per-process caps are still meaningfully useful — they bound the
//! VMM's blast radius on the host without changing the threading
//! model.
//!
//! ## Opt-in
//!
//! Setting either `NANOVM_VMM_MEMORY_LIMIT_MIB` or
//! `NANOVM_VMM_CPU_QUOTA_PCT` in the environment opts in. Neither set
//! → [`install_default_limits`] is a no-op. There's no separate "off"
//! switch on purpose: if you set the env var you want the cap.
//!
//! ## Requirements
//!
//! - Linux with cgroup v2 (unified hierarchy) mounted at
//!   `/sys/fs/cgroup`.
//! - The VMM's parent cgroup must delegate the controllers you
//!   actually request (`memory` for `NANOVM_VMM_MEMORY_LIMIT_MIB`,
//!   `cpu` for `NANOVM_VMM_CPU_QUOTA_PCT`). A memory-only config
//!   does not require `cpu` delegation, and vice versa. Under
//!   systemd this means `Delegate=memory`, `Delegate=cpu`, or
//!   `Delegate=memory cpu`. If a requested controller isn't
//!   delegated, [`install_default_limits`] returns
//!   [`VmError::Backend`] with a diagnostic — we fail loudly rather
//!   than silently dropping the cap.
//!
//! ## What the function does
//!
//! 1. Reads the calling process's own cgroup path from
//!    `/proc/self/cgroup` (cgroup v2 has exactly one line, prefixed
//!    `0::`).
//! 2. Creates a child cgroup at
//!    `/sys/fs/cgroup/<own-path>/nanovm-vmm-<pid>/`.
//! 3. Writes `memory.max` (bytes) and / or `cpu.max`
//!    (`<quota_us> 100000`) if configured.
//! 4. Writes its own PID into the child's `cgroup.procs`, atomically
//!    moving the whole process (and all its threads) under the cap.

use std::fs;
use std::path::{Path, PathBuf};

use vm_core::{VmError, VmResult};

/// Default cpu.max period in microseconds. 100 ms is the kernel
/// default and what systemd uses; keeping it matches operator
/// expectations and avoids surprising p99 jitter from a shorter
/// window.
const CPU_PERIOD_US: u64 = 100_000;

/// Root of the cgroup v2 unified hierarchy. The kernel mounts cgroup
/// v2 here on every modern distro; we don't try to discover an
/// alternate mount point because none of our supported platforms uses
/// one.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Memory cap in MiB and CPU quota in percent-of-one-CPU, parsed from
/// the environment. Either field may be `None`; if both are `None`,
/// [`install_default_limits`] is a no-op.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LimitsFromEnv {
    memory_mib: Option<u64>,
    cpu_quota_pct: Option<u32>,
}

impl LimitsFromEnv {
    /// Read both env vars and fail loudly on garbage. A *missing* var
    /// is fine — it just means that knob isn't requested. An *empty
    /// or unparsable* var means the operator typed something they
    /// expected to apply, and we'd be silently dropping the cap if
    /// we returned `None`. That's a security footgun: the host would
    /// look protected from a config audit but actually run without
    /// the limit.
    fn from_env() -> VmResult<Self> {
        Ok(Self {
            memory_mib: parse_env_u64("NANOVM_VMM_MEMORY_LIMIT_MIB")?,
            cpu_quota_pct: parse_env_u32("NANOVM_VMM_CPU_QUOTA_PCT")?,
        })
    }

    fn any_set(&self) -> bool {
        self.memory_mib.is_some() || self.cpu_quota_pct.is_some()
    }
}

/// `Ok(None)` when the env var is unset, `Ok(Some(n))` when present
/// and parsable, `Err(Backend)` when present-but-unparsable. The
/// failure path is deliberately loud — see `LimitsFromEnv::from_env`.
fn parse_env_u64(key: &str) -> VmResult<Option<u64>> {
    match std::env::var(key) {
        Err(_) => Ok(None),
        Ok(raw) => raw.trim().parse::<u64>().map(Some).map_err(|e| {
            VmError::Backend(format!(
                "cgroups: {key}={raw:?} is not a non-negative integer ({e}); \
                 unset or fix the value, don't leave a garbage cap request"
            ))
        }),
    }
}

fn parse_env_u32(key: &str) -> VmResult<Option<u32>> {
    match std::env::var(key) {
        Err(_) => Ok(None),
        Ok(raw) => raw.trim().parse::<u32>().map(Some).map_err(|e| {
            VmError::Backend(format!(
                "cgroups: {key}={raw:?} is not a non-negative integer ({e}); \
                 unset or fix the value, don't leave a garbage cap request"
            ))
        }),
    }
}

/// True when at least one of the cgroup limit env vars is set,
/// regardless of whether its value parses. Used by `KvmHypervisor::new`
/// to decide whether to call into this module at all — a present-
/// but-garbage value still gates entry, so the parse failure in
/// `install_default_limits` surfaces to the operator instead of being
/// silently skipped.
pub fn env_opts_in() -> bool {
    std::env::var_os("NANOVM_VMM_MEMORY_LIMIT_MIB").is_some()
        || std::env::var_os("NANOVM_VMM_CPU_QUOTA_PCT").is_some()
}

/// Read `/proc/self/cgroup` and return the v2 path
/// (everything after the `0::` prefix). Errors if the file is
/// missing, malformed, or doesn't contain a v2 line — the latter
/// indicates a legacy cgroup v1 host that we don't support.
fn own_cgroup_path() -> VmResult<String> {
    let txt = fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| VmError::Backend(format!("cgroups: read /proc/self/cgroup: {e}")))?;
    // cgroup v2 line shape: `0::/some/path`. Hybrid hosts also
    // expose v1 lines like `12:memory:/...`; we ignore those and
    // require a v2 line to be present.
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Ok(rest.to_owned());
        }
    }
    Err(VmError::Backend(
        "cgroups: no cgroup v2 line in /proc/self/cgroup (host is legacy cgroup v1?)".into(),
    ))
}

/// Verify that `memory` and `cpu` are listed in the parent's
/// `cgroup.subtree_control` so that the child we're about to create
/// will actually accept the limits we write. Returns an actionable
/// diagnostic if not — the most common cause is running outside a
/// systemd `Delegate=yes` service.
fn check_controllers_delegated(parent: &Path, needed: &[&str]) -> VmResult<()> {
    let path = parent.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&path).map_err(|e| {
        VmError::Backend(format!(
            "cgroups: read {}: {e} (is cgroup v2 mounted at {CGROUP_ROOT}?)",
            path.display()
        ))
    })?;
    let tokens: Vec<&str> = enabled.split_ascii_whitespace().collect();
    let mut missing: Vec<&str> = Vec::new();
    for &ctl in needed {
        if !tokens.contains(&ctl) {
            missing.push(ctl);
        }
    }
    if !missing.is_empty() {
        return Err(VmError::Backend(format!(
            "cgroups: parent {} does not delegate controllers {missing:?} \
             (enable them in {}/cgroup.subtree_control, or run under a \
             systemd unit with `Delegate=memory cpu`)",
            parent.display(),
            parent.display(),
        )));
    }
    Ok(())
}

/// Resolve the absolute filesystem path of the child cgroup we'll
/// create for this process. Returns
/// `/sys/fs/cgroup/<own-cgroup>/nanovm-vmm-<pid>`.
fn child_cgroup_path() -> VmResult<PathBuf> {
    let mut path = PathBuf::from(CGROUP_ROOT);
    let own = own_cgroup_path()?;
    // own_cgroup_path returns a leading "/" for the root cgroup,
    // which when joined to CGROUP_ROOT would clobber the prefix.
    // Strip it so Path::join appends instead of replacing.
    let trimmed = own.trim_start_matches('/');
    if !trimmed.is_empty() {
        path.push(trimmed);
    }
    path.push(format!("nanovm-vmm-{}", std::process::id()));
    Ok(path)
}

/// Apply the configured limits to the current process by creating a
/// fresh cgroup v2 child under our own cgroup, writing the limit
/// knobs, and moving the process in.
///
/// No-op when neither `NANOVM_VMM_MEMORY_LIMIT_MIB` nor
/// `NANOVM_VMM_CPU_QUOTA_PCT` is set.
///
/// Errors return [`VmError::Backend`] wrapping the underlying I/O
/// problem or a controller-not-delegated diagnostic. We fail loud
/// rather than silently skipping the cap — if an operator asked for
/// a 512 MiB limit and we couldn't apply it, they need to know
/// before the VMM is exposed to traffic.
pub fn install_default_limits() -> VmResult<()> {
    let limits = LimitsFromEnv::from_env()?;
    if !limits.any_set() {
        return Ok(());
    }
    let child = child_cgroup_path()?;
    // The parent is whatever cgroup we're currently in. We need
    // only the controllers actually requested to be delegated —
    // memory-only configs shouldn't fail just because cpu isn't
    // available, and vice versa.
    let parent = child
        .parent()
        .ok_or_else(|| VmError::Backend("cgroups: child path has no parent (impossible)".into()))?;
    let mut needed: Vec<&str> = Vec::with_capacity(2);
    if limits.memory_mib.is_some() {
        needed.push("memory");
    }
    if limits.cpu_quota_pct.is_some() {
        needed.push("cpu");
    }
    check_controllers_delegated(parent, &needed)?;

    // `create_dir` (NOT `create_dir_all`) errors loudly when the
    // target already exists, which catches the edge case where a
    // previous VMM with our same PID was killed without cleaning up
    // its cgroup. Reusing that directory would silently inherit
    // whatever caps the prior process wrote — the operator's intent
    // here is "fresh child," so we surface the conflict with an
    // actionable diagnostic instead of clobbering.
    fs::create_dir(&child).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => VmError::Backend(format!(
            "cgroups: child {} already exists — a previous VMM with the \
             same PID likely crashed; rmdir the leftover directory and \
             retry",
            child.display()
        )),
        _ => VmError::Backend(format!("cgroups: create {}: {e}", child.display())),
    })?;

    if let Some(mib) = limits.memory_mib {
        let bytes = mib.saturating_mul(1024 * 1024);
        let path = child.join("memory.max");
        fs::write(&path, bytes.to_string())
            .map_err(|e| VmError::Backend(format!("cgroups: write {}: {e}", path.display())))?;
    }

    if let Some(pct) = limits.cpu_quota_pct {
        // Percent-of-one-CPU → microseconds of runtime per
        // CPU_PERIOD_US window. 100 → 100_000us / 100_000us =
        // exactly one CPU. 200 → two CPUs. 50 → half a CPU.
        let quota_us = u64::from(pct).saturating_mul(CPU_PERIOD_US) / 100;
        let path = child.join("cpu.max");
        fs::write(&path, format!("{quota_us} {CPU_PERIOD_US}"))
            .map_err(|e| VmError::Backend(format!("cgroups: write {}: {e}", path.display())))?;
    }

    // Atomically move the whole process (all threads) into the
    // child cgroup. cgroup v2 `cgroup.procs` accepts a PID and
    // migrates every TID sharing that thread group.
    let procs = child.join("cgroup.procs");
    fs::write(&procs, std::process::id().to_string()).map_err(|e| {
        VmError::Backend(format!("cgroups: attach pid to {}: {e}", procs.display()))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `from_env()` reads process-global state, so the three env
    // tests below share a mutex to stay deterministic when the
    // suite runs threaded.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_parser_handles_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("NANOVM_VMM_MEMORY_LIMIT_MIB");
            std::env::remove_var("NANOVM_VMM_CPU_QUOTA_PCT");
        }
        assert_eq!(LimitsFromEnv::from_env().unwrap(), LimitsFromEnv::default());
    }

    #[test]
    fn env_parser_fails_loudly_on_garbage() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("NANOVM_VMM_MEMORY_LIMIT_MIB", "garbage");
            std::env::remove_var("NANOVM_VMM_CPU_QUOTA_PCT");
        }
        // Unparsable env values are a config-audit footgun: an
        // operator who set the var expects the cap to apply.
        // Returning Ok(None) here would silently drop the cap and
        // leave the host unprotected.
        let err = LimitsFromEnv::from_env().expect_err("garbage must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("NANOVM_VMM_MEMORY_LIMIT_MIB"),
            "diagnostic should name the env var; got: {msg}"
        );
        unsafe {
            std::env::remove_var("NANOVM_VMM_MEMORY_LIMIT_MIB");
        }
    }

    #[test]
    fn env_parser_accepts_valid_values() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("NANOVM_VMM_MEMORY_LIMIT_MIB", "512");
            std::env::set_var("NANOVM_VMM_CPU_QUOTA_PCT", "150");
        }
        let parsed = LimitsFromEnv::from_env().unwrap();
        assert_eq!(parsed.memory_mib, Some(512));
        assert_eq!(parsed.cpu_quota_pct, Some(150));
        assert!(parsed.any_set());
        unsafe {
            std::env::remove_var("NANOVM_VMM_MEMORY_LIMIT_MIB");
            std::env::remove_var("NANOVM_VMM_CPU_QUOTA_PCT");
        }
    }

    #[test]
    fn env_opts_in_reflects_any_set_even_when_unparsable() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("NANOVM_VMM_MEMORY_LIMIT_MIB");
            std::env::remove_var("NANOVM_VMM_CPU_QUOTA_PCT");
        }
        assert!(!env_opts_in());
        // Even a garbage value gates entry — that way the parse
        // failure is surfaced loudly inside install_default_limits
        // instead of being silently skipped at the caller.
        unsafe {
            std::env::set_var("NANOVM_VMM_CPU_QUOTA_PCT", "not-a-number");
        }
        assert!(env_opts_in(), "garbage value must still gate entry");
        unsafe {
            std::env::set_var("NANOVM_VMM_CPU_QUOTA_PCT", "50");
        }
        assert!(env_opts_in());
        unsafe {
            std::env::remove_var("NANOVM_VMM_CPU_QUOTA_PCT");
        }
    }

    #[test]
    fn child_cgroup_path_ends_with_pid() {
        // Skip if /proc/self/cgroup isn't readable — e.g. running
        // on a non-Linux dev box or inside a sandbox that hides
        // /proc. The function is exercised end-to-end by the
        // integration test on a real cgroup v2 host.
        let Ok(path) = child_cgroup_path() else {
            return;
        };
        let pid = std::process::id();
        let last = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        assert!(
            last.starts_with("nanovm-vmm-") && last.ends_with(&pid.to_string()),
            "unexpected child cgroup name: {last}"
        );
        assert!(
            path.starts_with(CGROUP_ROOT),
            "child path should be under {CGROUP_ROOT}: {}",
            path.display()
        );
    }
}
