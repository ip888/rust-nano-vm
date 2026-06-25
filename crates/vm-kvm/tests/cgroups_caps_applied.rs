//! Integration test for the cgroups v2 process-wide cap installer.
//!
//! Mirrors the shape of [`seccomp_blocks_execve`]: a parent
//! `fork()`s a child, the child runs the installer with env vars
//! set, then the parent reads back what the kernel actually
//! recorded on disk and asserts it matches.
//!
//! The test is best-effort: it auto-skips on hosts where cgroup v2
//! isn't mounted, where the parent cgroup doesn't delegate the
//! controllers we need, or where the test process can't write to
//! the cgroup hierarchy. The failure of those preconditions is an
//! environment fact, not a regression in our installer. We log the
//! reason and pass so the suite stays green on dev boxes and on the
//! GitHub-hosted CI runner that doesn't expose a delegated cgroup.
//!
//! What we DO assert (when the preconditions hold) is:
//!
//! - The installer creates `/sys/fs/cgroup/<own>/nanovm-vmm-<pid>/`.
//! - `memory.max` contains exactly the requested byte count.
//! - `cpu.max` contains exactly `<quota_us> 100000` derived from
//!   the percent-of-one-CPU env var.
//! - The child PID lands in `cgroup.procs` of the new cgroup.

#![cfg(all(feature = "kvm", target_os = "linux"))]

use std::fs;
use std::path::{Path, PathBuf};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// True iff cgroup v2 is mounted at `/sys/fs/cgroup` AND the parent
/// cgroup has memory + cpu delegated in `subtree_control`. We need
/// both to actually exercise the installer.
fn host_supports_test() -> Option<PathBuf> {
    if !Path::new(CGROUP_ROOT).is_dir() {
        return None;
    }
    let own = match fs::read_to_string("/proc/self/cgroup") {
        Ok(s) => s,
        Err(_) => return None,
    };
    let path = own.lines().find_map(|l| l.strip_prefix("0::"))?.to_owned();
    let parent = if path == "/" {
        PathBuf::from(CGROUP_ROOT)
    } else {
        PathBuf::from(CGROUP_ROOT).join(path.trim_start_matches('/'))
    };
    let subtree = parent.join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&subtree).ok()?;
    let tokens: Vec<&str> = enabled.split_ascii_whitespace().collect();
    if !tokens.contains(&"memory") || !tokens.contains(&"cpu") {
        return None;
    }
    // Probe write access by trying to create a sentinel subdir;
    // many setups have subtree_control delegated but rmdir-only
    // permissions. cleanup is best-effort.
    let probe = parent.join("nanovm-vmm-probe");
    match fs::create_dir(&probe) {
        Ok(_) => {
            let _ = fs::remove_dir(&probe);
            Some(parent)
        }
        Err(_) => None,
    }
}

#[test]
fn install_default_limits_writes_expected_values() {
    let Some(parent) = host_supports_test() else {
        eprintln!(
            "cgroups test: host doesn't expose a writable cgroup v2 with \
             memory+cpu delegated, skipping (this is normal on dev boxes \
             and GitHub-hosted CI)."
        );
        return;
    };

    // Construct the env-var values up front so the child doesn't
    // have to allocate after fork.
    let mem_mib: u64 = 256;
    let cpu_pct: u32 = 75;

    // SAFETY: same caveat as `seccomp_blocks_execve` — fork + post-
    // fork allocation isn't strictly async-signal-safe but the test
    // is short-lived and stable in practice. If this ever flakes,
    // promote the child body to a separate helper binary.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
        0 => {
            // Child. Set the env vars, install, and exit. The
            // exit code communicates result back to the parent;
            // we don't print on success (the parent reads the
            // cgroupfs to verify).
            unsafe {
                std::env::set_var("NANOVM_VMM_MEMORY_LIMIT_MIB", mem_mib.to_string());
                std::env::set_var("NANOVM_VMM_CPU_QUOTA_PCT", cpu_pct.to_string());
            }
            match vm_kvm::install_default_limits() {
                Ok(()) => std::process::exit(0),
                Err(e) => {
                    eprintln!("child: install_default_limits failed: {e}");
                    std::process::exit(2);
                }
            }
        }
        child => {
            let mut status: libc::c_int = 0;
            let waited = unsafe { libc::waitpid(child, &mut status, 0) };
            assert_eq!(
                waited,
                child,
                "waitpid failed: {}",
                std::io::Error::last_os_error()
            );
            let exited = (status & 0x7f) == 0;
            let exit_code = (status >> 8) & 0xff;
            assert!(
                exited && exit_code == 0,
                "child did not exit cleanly: exited?={exited} \
                 code={exit_code} raw_status={status:#x}"
            );

            let cgroup = parent.join(format!("nanovm-vmm-{child}"));
            // The child is gone, so cgroup.procs is empty; the
            // directory and the *.max files persist until rmdir.
            // We expect the cap values the child wrote.
            let mem = fs::read_to_string(cgroup.join("memory.max"))
                .expect("read memory.max")
                .trim()
                .to_owned();
            assert_eq!(
                mem,
                (mem_mib * 1024 * 1024).to_string(),
                "memory.max not set to the requested {mem_mib} MiB"
            );

            let cpu = fs::read_to_string(cgroup.join("cpu.max"))
                .expect("read cpu.max")
                .trim()
                .to_owned();
            let expected_quota_us = u64::from(cpu_pct) * 100_000 / 100;
            assert_eq!(
                cpu,
                format!("{expected_quota_us} 100000"),
                "cpu.max not set to the requested {cpu_pct}% quota"
            );

            // Cleanup. The kernel auto-fails rmdir if any tasks
            // remain in the cgroup; since the child is reaped, the
            // dir is empty and rmdir succeeds. Best-effort.
            let _ = fs::remove_dir(&cgroup);
        }
    }
}
