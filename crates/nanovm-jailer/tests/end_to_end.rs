//! End-to-end integration test for `nanovm-jailer`.
//!
//! Runs the real `nanovm-jailer` binary as a subprocess against a
//! tiny stand-in for `nanovm-vmm-child` (a `bash -c` one-liner the
//! jailer execs into). The stand-in writes its own cgroup path
//! back to the parent over a Unix socket, then sleeps. The parent
//! reads the kernel-recorded `memory.max` and `cpu.max` for that
//! cgroup and asserts they match the request.
//!
//! Auto-skips when:
//!
//! - Not on Linux.
//! - cgroup v2 isn't mounted at `/sys/fs/cgroup`.
//! - The test process's parent cgroup doesn't delegate `memory` +
//!   `cpu` (i.e. we're not under a systemd `Delegate=` unit and not
//!   running as root in an interactive shell with the controllers
//!   already enabled). This is the common case on dev hosts and on
//!   the GitHub-hosted CI runner.
//! - The test process can't `create_dir` a sentinel sub-cgroup
//!   (delegation might be subtree-only / rmdir-only).
//!
//! Skipped runs print a single `eprintln!` line explaining which
//! precondition failed; the test still PASSES, so the suite stays
//! green on hosts that can't exercise the real cgroup wiring. The
//! kernel-recorded value is the only thing we trust — the library
//! unit tests in `src/lib.rs` already cover the pure logic.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// True iff the host has cgroup v2 + memory/cpu delegated + we can
/// create a child cgroup. Returns the parent path on success.
fn host_supports_test() -> Option<PathBuf> {
    if !Path::new(CGROUP_ROOT).is_dir() {
        eprintln!("SKIP: {CGROUP_ROOT} is not a directory");
        return None;
    }
    let own = fs::read_to_string("/proc/self/cgroup").ok()?;
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
        eprintln!(
            "SKIP: parent cgroup {} does not delegate memory+cpu \
             (subtree_control: {:?})",
            parent.display(),
            tokens
        );
        return None;
    }
    let probe = parent.join("nanovm-jailer-probe");
    match fs::create_dir(&probe) {
        Ok(_) => {
            let _ = fs::remove_dir(&probe);
        }
        Err(e) => {
            eprintln!(
                "SKIP: cannot create sub-cgroup under {}: {e}",
                parent.display()
            );
            return None;
        }
    }
    Some(parent)
}

/// Resolve the freshly-built `nanovm-jailer` binary in the cargo
/// target dir. cargo sets `CARGO_BIN_EXE_<name>` for any binary in
/// the package under test, so we don't have to guess the profile.
fn jailer_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nanovm-jailer"))
}

/// Build a tiny shell stand-in for `nanovm-vmm-child` and return
/// its path. We can't reach for the real worker here: it spawns
/// vsock listeners and other backend-specific machinery that the
/// jailer test isn't responsible for. The shell stub does exactly
/// what we need to assert correctness: report its cgroup path to
/// the parent, then park.
fn make_worker_stub(dir: &Path, signal_path: &Path) -> PathBuf {
    let script = dir.join("worker.sh");
    // The stub reads its own cgroup path from /proc/self/cgroup
    // and writes it (plus the PID) to the signal file the test
    // watches. Then it parks for up to 30 seconds (the test kills
    // it long before that).
    let body = format!(
        "#!/bin/sh\n\
         set -e\n\
         awk -F: '/^0::/ {{ print $3 }}' /proc/self/cgroup > {signal}.path\n\
         echo $$ > {signal}.pid\n\
         touch {signal}.ready\n\
         sleep 30\n",
        signal = signal_path.display(),
    );
    fs::write(&script, body).expect("write worker stub");
    let mut perm = fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&script, perm).unwrap();
    script
}

/// Poll for a file to appear, up to `timeout`. Returns true if it
/// showed up, false on timeout.
fn wait_for(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn jailer_applies_caps_and_execs_into_worker() {
    let Some(parent) = host_supports_test() else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let signal = dir.path().join("worker-signal");
    let worker = make_worker_stub(dir.path(), &signal);

    // Pick a VM id that's unlikely to collide with anything else.
    let vm_id: u64 = (std::process::id() as u64) * 1_000_000 + 42;
    let cgroup_dir = parent.join(format!("nanovm-vm-{vm_id}"));
    // Best-effort cleanup from a prior failed run.
    let _ = fs::remove_dir(&cgroup_dir);

    let memory_mib: u64 = 64;
    let cpu_quota_pct: u32 = 25;
    let socket = dir.path().join("vm.sock");

    let mut child = Command::new(jailer_binary())
        .arg("--vm-id")
        .arg(vm_id.to_string())
        .arg("--memory-limit-mib")
        .arg(memory_mib.to_string())
        .arg("--cpu-quota-pct")
        .arg(cpu_quota_pct.to_string())
        .arg("--vmm-child-binary")
        .arg(&worker)
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn jailer");

    let ready = signal.with_extension("ready");
    let ok = wait_for(&ready, Duration::from_secs(5));
    if !ok {
        let _ = child.kill();
        let _ = fs::remove_dir(&cgroup_dir);
        panic!("worker stub never signalled ready (timeout)");
    }

    // Read kernel-recorded caps from the cgroup the jailer created.
    let mem_path = cgroup_dir.join("memory.max");
    let cpu_path = cgroup_dir.join("cpu.max");
    let mem_raw = fs::read_to_string(&mem_path).expect("read memory.max");
    let cpu_raw = fs::read_to_string(&cpu_path).expect("read cpu.max");

    // memory.max should be exactly memory_mib * 1 MiB.
    let want_bytes = memory_mib * 1024 * 1024;
    assert_eq!(
        mem_raw.trim(),
        want_bytes.to_string(),
        "kernel-recorded memory.max ({mem_raw:?}) != requested {want_bytes}"
    );

    // cpu.max format: "<quota_us> <period_us>". Period is 100_000
    // (the kernel default the jailer hard-codes).
    let want_quota_us = (u64::from(cpu_quota_pct) * 100_000) / 100;
    assert_eq!(
        cpu_raw.trim(),
        format!("{want_quota_us} 100000"),
        "kernel-recorded cpu.max ({cpu_raw:?}) != requested"
    );

    // Worker's own cgroup path should end with nanovm-vm-<id>.
    let worker_cgroup =
        fs::read_to_string(signal.with_extension("path")).expect("read worker cgroup");
    assert!(
        worker_cgroup
            .trim()
            .ends_with(&format!("nanovm-vm-{vm_id}")),
        "worker landed in unexpected cgroup: {worker_cgroup:?}"
    );

    // Cleanup: kill the stub, then drop the cgroup.
    let _ = child.kill();
    let _ = child.wait();
    // The cgroup can't be rmdir'd while a process is still in it;
    // small delay + retry covers the reaper race.
    let mut tries = 0;
    while tries < 20 && fs::remove_dir(&cgroup_dir).is_err() {
        std::thread::sleep(Duration::from_millis(50));
        tries += 1;
    }
}
