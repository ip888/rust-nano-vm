//! `nanovm-jailer` — per-VM cgroup setup + `execve()` into the
//! `nanovm-vmm-child` worker.
//!
//! Invocation:
//! ```text
//! nanovm-jailer \
//!   --vm-id 1 \
//!   --memory-limit-mib 256 \
//!   --cpu-quota-pct 50 \
//!   --vmm-child-binary /usr/local/bin/nanovm-vmm-child \
//!   --socket /var/run/nanovm/vm-1.sock
//! ```
//!
//! On success the process is replaced by `nanovm-vmm-child` with
//! the per-VM cgroup already in place. On failure (cgroup not
//! delegated, leftover dir, EACCES, etc.) the jailer exits non-zero
//! with an actionable diagnostic on stderr.
//!
//! See the crate README for the full architecture and demo recipe.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use clap::Parser;

use nanovm_jailer::{apply_isolation_and_exec, JailerConfig};

#[derive(Debug, Parser)]
#[command(
    name = "nanovm-jailer",
    version,
    about = "Set up a per-VM cgroup v2 and exec into nanovm-vmm-child."
)]
struct Args {
    /// Numeric VM id. Used to name the per-VM cgroup directory.
    #[arg(long)]
    vm_id: u64,

    /// Memory cap in MiB. Omit to skip `memory.max`.
    #[arg(long)]
    memory_limit_mib: Option<u64>,

    /// CPU quota in percent-of-one-CPU. Omit to skip `cpu.max`.
    /// 100 = one CPU, 50 = half a CPU, 200 = two CPUs.
    #[arg(long)]
    cpu_quota_pct: Option<u32>,

    /// Absolute path to the `nanovm-vmm-child` binary. We don't
    /// search `$PATH`: explicit is safer for a privileged helper.
    #[arg(long)]
    vmm_child_binary: PathBuf,

    /// Unix socket path passed through to the worker as `--socket`.
    #[arg(long)]
    socket: PathBuf,

    /// Override the parent cgroup directory. Defaults to whichever
    /// cgroup we landed in (read from `/proc/self/cgroup`).
    #[arg(long)]
    cgroup_parent: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let cfg = JailerConfig {
        vm_id: args.vm_id,
        memory_limit_mib: args.memory_limit_mib,
        cpu_quota_pct: args.cpu_quota_pct,
        socket: args.socket,
        vmm_child_binary: args.vmm_child_binary,
        cgroup_parent: args.cgroup_parent,
    };
    // apply_isolation_and_exec returns Infallible on success
    // (process replaced). Anything reaching us here is an error.
    match apply_isolation_and_exec(cfg) {
        Ok(infallible) => match infallible {},
        Err(e) => Err(e.into()),
    }
}
