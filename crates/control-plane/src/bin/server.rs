//! `nanovm-control-plane` — REST server binary.
//!
//! Wires a `MockHypervisor` by default so this binary is runnable on any
//! machine without `/dev/kvm`.
//!
//! Environment:
//! - `NANOVM_CONTROL_PLANE_ADDR` — bind address (default `127.0.0.1:8080`).
//! - `NANOVM_API_TOKENS` — comma-separated bearer tokens. **Empty disables
//!   auth** and emits a `WARN` log line on startup.
//! - `NANOVM_SHUTDOWN_GRACE_SECS` — bound on the graceful-drain window
//!   after `SIGTERM`/`SIGINT`. Default `30`. Once the budget elapses,
//!   any still-inflight requests are dropped and the process exits.
//!   `0` keeps the legacy unbounded behaviour (NOT recommended — a
//!   wedged handler will pin the process forever).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use control_plane::{router, ApiTokens, AppState};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::Notify;
use tracing::{error, info, warn};
use vm_mock::MockHypervisor;

const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr =
        std::env::var("NANOVM_CONTROL_PLANE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let tokens = Arc::new(ApiTokens::from_env());
    if tokens.is_empty() {
        warn!(
            "NANOVM_API_TOKENS is empty — /v1/* is unauthenticated. \
             Set this env var to a comma-separated list of bearer tokens \
             before exposing this service to the network."
        );
    } else {
        info!(count = tokens.len(), "bearer-token auth enabled");
    }

    let hypervisor: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app = router()
        .layer(Extension(tokens))
        .with_state(AppState::new(hypervisor));

    let grace = std::env::var("NANOVM_SHUTDOWN_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECS);

    let listener = TcpListener::bind(&addr).await?;
    info!(%addr, grace_secs = grace, "nanovm-control-plane listening");

    // `shutdown_started` separates the two phases the bounded-drain
    // logic cares about:
    //   1. pre-signal: the server is accepting connections; the drain
    //      timer is parked on `Notify::notified()`.
    //   2. post-signal: the signal future fires, notifies the timer,
    //      and the server begins refusing new connections + draining.
    //   3. after `grace` seconds in phase 2 the timer fires and we
    //      force-exit, logging an ERROR if any handlers were still
    //      in flight.
    let shutdown_started = Arc::new(Notify::new());
    let signal_notify = shutdown_started.clone();
    let signal_fut = async move {
        shutdown_signal().await;
        signal_notify.notify_waiters();
    };

    let serve = axum::serve(listener, app).with_graceful_shutdown(signal_fut);
    // `WithGracefulShutdown` doesn't implement `Future` directly —
    // it implements `IntoFuture`. Convert here so `bound_drain`'s
    // `F: Future` bound is satisfied.
    let serve = std::future::IntoFuture::into_future(serve);
    bound_drain(serve, shutdown_started, Duration::from_secs(grace)).await?;
    Ok(())
}

/// Await `serve` to completion, but if `shutdown_started` fires,
/// give the inner future at most `grace` seconds to drain before
/// returning anyway. Logs an `ERROR` if we exit on the timer with
/// requests still in flight.
///
/// `grace == 0` disables the timer — `serve` is awaited
/// unconditionally (legacy behaviour, retained as an escape hatch).
async fn bound_drain<F, E>(
    serve: F,
    shutdown_started: Arc<Notify>,
    grace: Duration,
) -> Result<(), E>
where
    F: std::future::Future<Output = Result<(), E>>,
{
    if grace.is_zero() {
        return serve.await;
    }
    let timer = async move {
        // Park until the shutdown signal fires; only then does the
        // grace clock start ticking.
        shutdown_started.notified().await;
        tokio::time::sleep(grace).await;
    };
    tokio::pin!(serve);
    tokio::pin!(timer);
    tokio::select! {
        result = &mut serve => result,
        () = &mut timer => {
            error!(
                grace_secs = grace.as_secs(),
                "graceful drain budget exceeded; forcing exit with requests still in flight"
            );
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received, draining");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::future::pending;

    /// When `grace == 0` the timer is disabled and the serve future
    /// is awaited unconditionally.
    #[tokio::test]
    async fn grace_zero_awaits_serve_to_completion() {
        let serve = async { Ok::<(), Infallible>(()) };
        let n = Arc::new(Notify::new());
        bound_drain(serve, n, Duration::ZERO).await.unwrap();
    }

    /// Pre-signal, the timer never fires — the serve future drives
    /// completion. Verifies we never force-exit while the service
    /// is still healthy.
    #[tokio::test]
    async fn timer_does_not_fire_until_shutdown_starts() {
        let serve = async { Ok::<(), Infallible>(()) };
        let n = Arc::new(Notify::new());
        // No `n.notify_waiters()` here — the timer must stay parked.
        tokio::time::timeout(
            Duration::from_millis(50),
            bound_drain(serve, n, Duration::from_secs(60 * 60)),
        )
        .await
        .expect("serve resolved before timeout, even though grace is 1h — confirms the timer didn't trip")
        .unwrap();
    }

    /// After the signal fires, the timer enforces the grace budget
    /// even if the serve future hangs forever. Uses a short
    /// real-time grace so the test completes quickly without
    /// needing tokio's `test-util` feature.
    #[tokio::test]
    async fn timer_force_exits_after_grace_when_serve_hangs() {
        let serve = async { pending::<Result<(), Infallible>>().await };
        let n = Arc::new(Notify::new());
        let n2 = n.clone();
        // Fire the shutdown signal "from the OS" 10ms in.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            n2.notify_waiters();
        });
        // Grace of 50ms — the bounded drain must return after ~60ms
        // total, not hang forever on the pending serve future.
        let start = std::time::Instant::now();
        bound_drain(serve, n, Duration::from_millis(50))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(55) && elapsed < Duration::from_secs(1),
            "elapsed was {elapsed:?}"
        );
    }
}
