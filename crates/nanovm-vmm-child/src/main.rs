//! `nanovm-vmm-child` — single-VM VMM worker binary.
//!
//! Listens on a Unix socket the orchestrator picks for us, accepts
//! exactly one connection, and runs the vmm-ipc request/response
//! loop until the peer sends `Shutdown` or disconnects.
//!
//! Wired into the per-VM cgroup isolation arc (PR-3 lands the
//! jailer that creates the cgroup before exec'ing into this
//! binary). PR-4 wires the control-plane orchestrator into spawning
//! one of us per VM. Today the binary stands on its own and is
//! exercised by integration tests.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::net::UnixListener;
use vm_core::Hypervisor;

#[cfg(feature = "kvm")]
use vm_kvm::KvmHypervisor;
#[cfg(not(feature = "kvm"))]
use vm_mock::MockHypervisor;

#[derive(Debug, Parser)]
#[command(
    name = "nanovm-vmm-child",
    version,
    about = "Single-VM VMM worker. Speaks vmm-ipc on a Unix socket."
)]
struct Args {
    /// Path the orchestrator will connect to. The worker
    /// `bind()`s here; the file is removed before bind so a leftover
    /// socket from a crashed predecessor doesn't block startup. The
    /// orchestrator is responsible for `unlink`ing on shutdown if
    /// it cares about the leftover.
    #[arg(long)]
    socket: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Tracing to stderr — stdout is reserved in case a future
    // milestone uses it for an out-of-band stream (e.g. exec_stream
    // chunks); for now stdout is unused but we keep the discipline.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let _ = std::fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)?;
    tracing::info!(socket = %args.socket.display(), "nanovm-vmm-child listening");

    // Backend selection is a compile-time toggle so the binary is
    // one path — no runtime env var, no dispatch. `Dockerfile.kvm`
    // builds with `--features kvm` for production; dev builds
    // (macOS, CI without /dev/kvm) keep the mock.
    #[cfg(feature = "kvm")]
    let hv: Arc<dyn Hypervisor> = {
        tracing::info!("backend: kvm (KvmHypervisor, opening /dev/kvm)");
        Arc::new(KvmHypervisor::new()?)
    };
    #[cfg(not(feature = "kvm"))]
    let hv: Arc<dyn Hypervisor> = {
        tracing::info!("backend: mock (MockHypervisor, no /dev/kvm)");
        Arc::new(MockHypervisor::new())
    };

    // Wait for exactly one connection — the orchestrator's. Race
    // it against Ctrl-C / SIGTERM so an idle worker doesn't hang
    // around if the operator wants it gone before the orchestrator
    // ever connected.
    let stream = tokio::select! {
        accepted = listener.accept() => {
            let (stream, _addr) = accepted?;
            stream
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C before accept, exiting");
            return Ok(());
        }
    };

    let (reader, writer) = stream.into_split();

    // Drive the serve loop, but bail out cleanly on Ctrl-C / SIGTERM
    // mid-conversation. The transport closing under us is handled
    // inside serve as a clean shutdown.
    tokio::select! {
        result = nanovm_vmm_child::serve(hv, reader, writer) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, exiting serve loop");
        }
    }

    Ok(())
}
