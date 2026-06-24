//! Seccomp-BPF sandbox for the VMM process.
//!
//! `rust-nano-vm` runs untrusted guest code by design. The host-side
//! VMM that owns `/dev/kvm` is the highest-value attack surface in the
//! whole stack: if a guest finds a KVM vulnerability that escapes
//! through `ioctl(KVM_RUN)`, what they get is whatever the VMM
//! process can do. Seccomp-BPF narrows that envelope by killing the
//! process if it tries to make a syscall on a deny-list, hardening
//! the blast radius without changing any of the VMM's hot path.
//!
//! Scope: this is a **deny-list** — we keep the rest of the syscall
//! surface open (KVM ioctls, mmap, futex, vsock, …) and just refuse
//! the categories of syscall a healthy steady-state VMM has no
//! reason to make. That's the same shape Firecracker calls its
//! "default" profile and the same crate (`seccompiler`) does the
//! BPF compilation.
//!
//! Denied syscalls (per [`DENIED_SYSCALLS`]):
//!
//! - `execve`, `execveat` — process spawning is a textbook escape
//!   pivot. The VMM has no reason to exec anything.
//! - `ptrace` — debugger-based escape into adjacent processes.
//! - `mount`, `umount2` — host filesystem manipulation.
//! - `kexec_load`, `kexec_file_load` — kernel replacement.
//! - `init_module`, `finit_module`, `delete_module` — kernel module
//!   manipulation.
//! - `reboot` — should never be reachable from a sandbox.
//! - `setns`, `unshare` — namespace pivoting.
//! - `chroot`, `pivot_root` — root-fs manipulation.
//!
//! Match action is [`SeccompAction::KillProcess`]: the kernel
//! delivers `SIGSYS` and reaps the entire process group. Crash-loud
//! is correct here — we want operators to notice that the VMM
//! attempted something forbidden.
//!
//! Opt-in: [`install_default_filter`] is called by
//! `KvmHypervisor::new` when `NANOVM_SECCOMP=1` is set in the
//! environment. The default is OFF so existing deployments (and the
//! `cargo test --features kvm` matrix) don't change behaviour on
//! upgrade.

use std::collections::BTreeMap;

use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
use vm_core::{VmError, VmResult};

/// Syscall numbers (per the current target architecture) that the
/// VMM has no legitimate reason to invoke and which carry meaningful
/// escape risk. Sourced from [`libc::SYS_*`] constants so the list
/// re-resolves correctly on x86_64 and aarch64.
fn denied_syscalls() -> &'static [libc::c_long] {
    &[
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_chroot,
        libc::SYS_pivot_root,
    ]
}

/// Pick the `seccompiler::TargetArch` matching the build target. We
/// resolve at compile time so the filter compiles to the right opcode
/// stream — installing an x86_64 filter on aarch64 (or vice-versa)
/// would be a soundness bug, not just a runtime mistake.
fn target_arch() -> VmResult<TargetArch> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(VmError::Unsupported(
            "vm-kvm: seccomp filter not implemented for this architecture",
        ))
    }
}

/// Build and apply the default deny-list filter to the current
/// process. Returns `Ok(())` once the filter is loaded; the kernel
/// keeps the filter for the lifetime of the process (it's
/// inherited across `clone`).
///
/// Calling this twice is harmless — the second filter is layered
/// underneath the first per the kernel's seccomp stacking semantics.
///
/// Errors:
/// - [`VmError::Backend`] wrapping a `seccompiler` parse / load
///   failure. The most common cause in practice is the running
///   kernel lacking `CONFIG_SECCOMP=y` (`apply_filter` returns
///   `EINVAL`), which means seccomp itself isn't available on this
///   host. The function does NOT call `prctl(PR_SET_NO_NEW_PRIVS)`
///   on the caller's behalf; running unprivileged generally already
///   has the bit, and the `seccompiler` crate sets it as part of
///   `apply_filter`.
pub fn install_default_filter() -> VmResult<()> {
    // Build the rules map: every denied syscall maps to an empty
    // rules vec, which seccompiler interprets as "this syscall is
    // unconditionally matched" (no argument predicates to apply).
    // The match action is then KillProcess.
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for sysno in denied_syscalls() {
        rules.insert(*sysno, Vec::new());
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow, // mismatch: anything not in the map is allowed
        SeccompAction::KillProcess, // match: kill the whole process on offence
        target_arch()?,
    )
    .map_err(|e| VmError::Backend(format!("seccomp: build filter: {e}")))?;

    let program: BpfProgram = filter
        .try_into()
        .map_err(|e| VmError::Backend(format!("seccomp: compile bpf: {e}")))?;
    seccompiler::apply_filter(&program)
        .map_err(|e| VmError::Backend(format!("seccomp: apply filter: {e}")))?;
    Ok(())
}

/// True when the env var `NANOVM_SECCOMP=1` opts this process in to
/// the default filter. Any value other than `1` (including unset,
/// `0`, `true`, `yes`, anything else) leaves seccomp disabled. We
/// keep the on-bit narrow on purpose — operators should have to
/// explicitly turn the sandbox on rather than wander into it.
pub fn env_opts_in() -> bool {
    matches!(std::env::var("NANOVM_SECCOMP").ok().as_deref(), Some("1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_arch_resolves_on_this_host() {
        // If we built for x86_64 or aarch64, target_arch() succeeds.
        // Anywhere else it errors loudly — both shapes are fine, we
        // just want to know the helper doesn't panic.
        let _ = target_arch();
    }

    #[test]
    fn denied_syscalls_list_is_nonempty_and_unique() {
        let list = denied_syscalls();
        assert!(!list.is_empty());
        let mut seen = std::collections::HashSet::new();
        for sysno in list {
            assert!(
                seen.insert(sysno),
                "duplicate syscall in deny list: {sysno}"
            );
        }
    }

    #[test]
    fn filter_compiles_to_bpf_program() {
        // Don't actually apply the filter inside the test process —
        // a successful apply would kill any further test invocation
        // that ran a child via std::process. Just prove the BPF
        // builds.
        let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
        for sysno in denied_syscalls() {
            rules.insert(*sysno, Vec::new());
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::KillProcess,
            target_arch().expect("target arch"),
        )
        .expect("build filter");
        let program: Result<BpfProgram, _> = filter.try_into();
        assert!(program.is_ok(), "BPF compile failed: {:?}", program.err());
    }
}
