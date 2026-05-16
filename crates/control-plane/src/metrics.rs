//! Prometheus exposition endpoint for `nanovm-control-plane`.
//!
//! Three metrics, hand-rolled in the text format to avoid pulling in
//! the `prometheus` crate (and its ~15 transitive deps) for what is
//! ultimately a few atomic increments and a string format:
//!
//! - `nanovm_http_requests_total` — counter, total HTTP requests served
//!   by the router (including `/healthz` and `/metrics` itself).
//! - `nanovm_http_requests_by_class_total{class="2xx"|"3xx"|"4xx"|"5xx"}`
//!   — same counter, partitioned by HTTP status class. Five-cardinality
//!   on purpose: per-route labels are tempting but explode cardinality
//!   the moment we accept user-controlled path segments.
//! - `nanovm_http_inflight` — gauge, requests currently in flight.
//!
//! The `/metrics` endpoint is mounted on the outer router with no auth
//! so scrapers don't need to hold a bearer token. Operators who don't
//! want it publicly reachable should bind the control plane to
//! `127.0.0.1` or block `/metrics` at their reverse proxy. Closes
//! tracked gap G1 from `docs/threat-model.md`.
//!
//! Future PRs will add: a `nanovm_rate_limit_throttled_total` counter
//! once the rate-limit middleware lands (PR #38), and a request
//! duration histogram once we have a real backend with non-trivial
//! latency to observe.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Extension, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Process-wide counters surfaced through `/metrics`. Cheap to clone
/// via `Arc`; all updates are lock-free.
#[derive(Debug, Default)]
pub struct Metrics {
    requests_total: AtomicU64,
    /// Indexed by status-class bucket: 0=1xx, 1=2xx, 2=3xx, 3=4xx, 4=5xx.
    requests_by_class: [AtomicU64; 5],
    inflight: AtomicI64,
}

impl Metrics {
    /// Construct a fresh, zeroed metrics registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the current counter values in Prometheus 0.0.4 text
    /// exposition format (the same wire format `prometheus_client`
    /// would emit). Cheap — just a handful of atomic loads.
    pub fn render(&self) -> String {
        let total = self.requests_total.load(Ordering::Relaxed);
        let inflight = self.inflight.load(Ordering::Relaxed);
        let mut out = String::with_capacity(512);
        let _ = writeln!(
            out,
            "# HELP nanovm_http_requests_total Total HTTP requests served."
        );
        let _ = writeln!(out, "# TYPE nanovm_http_requests_total counter");
        let _ = writeln!(out, "nanovm_http_requests_total {total}");
        let _ = writeln!(
            out,
            "# HELP nanovm_http_requests_by_class_total HTTP requests partitioned by status class."
        );
        let _ = writeln!(out, "# TYPE nanovm_http_requests_by_class_total counter");
        for (idx, label) in ["1xx", "2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
            let v = self.requests_by_class[idx].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "nanovm_http_requests_by_class_total{{class=\"{label}\"}} {v}",
            );
        }
        let _ = writeln!(
            out,
            "# HELP nanovm_http_inflight HTTP requests currently in flight."
        );
        let _ = writeln!(out, "# TYPE nanovm_http_inflight gauge");
        let _ = writeln!(out, "nanovm_http_inflight {inflight}");
        out
    }

    fn record(&self, status: StatusCode) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        let idx = match status.as_u16() {
            100..=199 => 0,
            200..=299 => 1,
            300..=399 => 2,
            400..=499 => 3,
            _ => 4, // 5xx + any out-of-range future status code
        };
        self.requests_by_class[idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// Axum middleware that increments `inflight` for the duration of the
/// request and records the final status on the way out. Install with
/// `.layer(middleware::from_fn(metrics::track_request))` at the
/// outermost level so it observes every route, including `/healthz`
/// and `/metrics` itself.
///
/// Tolerant of a missing `Metrics` extension: library consumers that
/// don't care about metrics pay zero cost.
pub async fn track_request(
    metrics: Option<Extension<Arc<Metrics>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(Extension(metrics)) = metrics else {
        return next.run(req).await;
    };
    metrics.inflight.fetch_add(1, Ordering::Relaxed);
    let response = next.run(req).await;
    metrics.inflight.fetch_sub(1, Ordering::Relaxed);
    metrics.record(response.status());
    response
}

/// Handler for `GET /metrics`. Returns the Prometheus text exposition
/// with the canonical `text/plain; version=0.0.4` content-type.
pub async fn metrics_handler(metrics: Option<Extension<Arc<Metrics>>>) -> Response {
    let Some(Extension(metrics)) = metrics else {
        // Extension wasn't installed — return an empty 200 rather than
        // a 500. Mirrors the degrade-gracefully shape used elsewhere
        // (auth, rate-limit).
        return (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4")],
            String::new(),
        )
            .into_response();
    };
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        metrics.render(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_metrics_render_all_zero() {
        let m = Metrics::new();
        let text = m.render();
        assert!(text.contains("nanovm_http_requests_total 0"));
        assert!(text.contains("nanovm_http_inflight 0"));
        for class in ["1xx", "2xx", "3xx", "4xx", "5xx"] {
            assert!(
                text.contains(&format!("class=\"{class}\"}} 0")),
                "missing zeroed class {class} in:\n{text}"
            );
        }
    }

    #[test]
    fn record_bumps_total_and_correct_class() {
        let m = Metrics::new();
        m.record(StatusCode::OK);
        m.record(StatusCode::CREATED);
        m.record(StatusCode::NOT_FOUND);
        m.record(StatusCode::INTERNAL_SERVER_ERROR);
        let text = m.render();
        assert!(text.contains("nanovm_http_requests_total 4"));
        assert!(text.contains("class=\"2xx\"} 2"));
        assert!(text.contains("class=\"4xx\"} 1"));
        assert!(text.contains("class=\"5xx\"} 1"));
        assert!(text.contains("class=\"3xx\"} 0"));
    }

    #[test]
    fn render_includes_help_and_type_lines() {
        // Prometheus parsers tolerate missing HELP/TYPE but tools
        // (Grafana label discovery, prom2json) lean on them. Keep them.
        let text = Metrics::new().render();
        assert!(text.contains("# HELP nanovm_http_requests_total"));
        assert!(text.contains("# TYPE nanovm_http_requests_total counter"));
        assert!(text.contains("# TYPE nanovm_http_inflight gauge"));
    }
}
