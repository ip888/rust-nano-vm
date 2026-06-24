//! Integration test for the seccomp-BPF deny-list.
//!
//! Proves the filter actually does what it says — `execve` from
//! inside the sandboxed process gets the kernel-delivered `SIGSYS`
//! signal and the offending process dies, rather than slipping
//! through. This is the contract `install_default_filter` is meant
//! to enforce, so it deserves a real boots-on-the-ground assertion
//! and not just a "filter BPF compiled" unit test.
//!
//! We do this via `fork`: the parent stays clean (no seccomp), the
//! child installs the filter and then immediately calls `execve` on
//! `/bin/true`. The parent `waitpid`s the child and inspects the
//! exit status:
//!
//! - `WIFSIGNALED && WTERMSIG == SIGSYS` → ✅ the filter killed it.
//! - `WIFEXITED && WEXITSTATUS == 0` → ❌ `execve` snuck through.
//! - Anything else → unexpected, fail with details.
//!
//! Linux-only by construction (seccomp). The test is `#[cfg]`'d
//! behind `target_os = "linux"` plus the `kvm` feature flag.

#![cfg(all(feature = "kvm", target_os = "linux"))]

use std::ffi::CString;
use std::process::ExitCode;

#[test]
fn execve_after_filter_is_killed_by_sigsys() {
    // Construct the C strings before forking so OOM / allocator
    // shenanigans don't leave the child in a weird state where it
    // can't proceed but also can't observe the seccomp action.
    let argv0 = CString::new("/bin/true").expect("argv[0]");
    let argv: Vec<*const libc::c_char> = vec![argv0.as_ptr(), std::ptr::null()];
    let envp: Vec<*const libc::c_char> = vec![std::ptr::null()];

    // SAFETY: `fork` is unsafe because in the child a number of host
    // resources (mutexes, allocator state) are in an undefined state
    // until they're explicitly reinitialised. We touch nothing in
    // the child other than the seccomp install + a direct execve
    // syscall — no heap allocations, no Rust stdlib that takes
    // global locks.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
        0 => {
            // Child. Anything other than ExitCode is unreachable —
            // either execve succeeds (filter failed) or seccomp
            // kills us before we get to record the outcome.
            let _ = child_body(&argv, &envp);
            // If we somehow exit cleanly, the parent will see code
            // 99 and report the failure.
            std::process::exit(99);
        }
        child => {
            let mut status: libc::c_int = 0;
            // SAFETY: standard waitpid contract.
            let waited = unsafe { libc::waitpid(child, &mut status, 0) };
            assert_eq!(
                waited,
                child,
                "waitpid failed: {}",
                std::io::Error::last_os_error()
            );
            // WIFSIGNALED / WTERMSIG aren't exposed as functions by
            // libc on Linux — we replicate the macros inline. They're
            // both pure bitwise checks against the `status` int.
            let signalled = (status & 0x7f) != 0 && (status & 0x7f) != 0x7f;
            let signal = status & 0x7f;
            let exited = (status & 0x7f) == 0;
            let exit_code = (status >> 8) & 0xff;
            assert!(
                signalled,
                "expected child to die via signal; exited?={exited} \
                 exit_code={exit_code} raw_status={status:#x}"
            );
            assert_eq!(
                signal,
                libc::SIGSYS,
                "child died from signal {signal}, expected SIGSYS ({}) \
                 — seccomp may not have killed it for the denied syscall",
                libc::SIGSYS,
            );
        }
    }
}

/// Body that runs in the child. Returning normally is a failure — the
/// only legitimate exit is via the SIGSYS the kernel delivers when
/// the filter matches.
fn child_body(argv: &[*const libc::c_char], envp: &[*const libc::c_char]) -> ExitCode {
    if let Err(e) = vm_kvm::install_default_filter() {
        eprintln!("child: install_default_filter failed: {e}");
        return ExitCode::from(2);
    }
    // SAFETY: argv / envp are NUL-terminated arrays of C strings
    // built by the parent. argv[0] outlives this call because the
    // CString lives in the caller's stack frame.
    unsafe {
        libc::execve(argv[0], argv.as_ptr(), envp.as_ptr());
    }
    // If we got here, the filter let execve through.
    eprintln!(
        "child: execve returned (errno={}) — filter failed to kill us",
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    );
    ExitCode::from(3)
}
