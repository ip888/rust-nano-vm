//! `nanovm-bench` — regression harness for the control-plane lifecycle loop.
//!
//! Scripts the canonical agent workload through the REST API:
//!
//! ```text
//!   POST   /v1/vms            → create
//!   POST   /v1/vms/:id/start  → start
//!   POST   /v1/vms/:id/snapshot → snapshot
//!   POST   /v1/snapshots/:id/restore → fork
//!   DELETE /v1/vms/:id        → destroy
//! ```
//!
//! Reports per-stage p50 / p95 / p99 + the wall-clock end-to-end.
//! Drives `vm-mock` by default (works in CI) and gives a hard lower
//! bound on the *control-plane* overhead before any real backend
//! latency is layered in.
//!
//! Once `vm-kvm` ships (M2+), wiring this loop against a real
//! hypervisor is a one-line change — the script doesn't care which
//! backend is behind the API.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde_json::{json, Value};

#[derive(Parser, Debug)]
#[command(
    name = "nanovm-bench",
    about = "Perf regression harness for nanovm-control-plane.",
    version
)]
struct Args {
    /// Base URL of the running control plane.
    #[arg(
        long,
        env = "NANOVM_BENCH_BASE",
        default_value = "http://127.0.0.1:8080"
    )]
    base: String,

    /// Bearer token; defaults to the `NANOVM_TOKEN` env var or none.
    #[arg(long, env = "NANOVM_TOKEN")]
    token: Option<String>,

    /// Number of full create→start→snapshot→fork→destroy iterations.
    #[arg(long, default_value_t = 100)]
    iterations: u32,

    /// Concurrent workers (each runs `iterations / workers` loops).
    /// Set higher to stress-test under load; left low by default
    /// because we want clean per-stage latency, not throughput.
    #[arg(long, default_value_t = 1)]
    workers: u32,

    /// Print one JSON object per iteration in addition to the
    /// summary table. Useful when piping into `jq` or a dashboard.
    #[arg(long)]
    jsonl: bool,
}

/// Stages in the order they execute. The string is what shows up
/// in the summary table.
const STAGES: &[&str] = &["create", "start", "snapshot", "fork", "destroy"];

fn main() -> Result<()> {
    let args = Args::parse();
    let per_worker = args.iterations.div_ceil(args.workers.max(1));
    let total = per_worker * args.workers;

    eprintln!(
        "nanovm-bench: {} iterations × {} worker(s) → {} loops total against {}",
        per_worker, args.workers, total, args.base,
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build http client")?;

    // Quick reachability check so we fail fast instead of N
    // iterations of confusing 5xx noise.
    preflight(&client, &args.base)?;

    let wall = Instant::now();
    let samples = std::thread::scope(|scope| -> Result<Vec<Sample>> {
        let mut handles = Vec::with_capacity(args.workers as usize);
        for worker_id in 0..args.workers {
            let client = client.clone();
            let base = args.base.clone();
            let token = args.token.clone();
            let jsonl = args.jsonl;
            handles.push(scope.spawn(move || {
                let mut local: Vec<Sample> = Vec::with_capacity(per_worker as usize);
                for iter in 0..per_worker {
                    match run_one(&client, &base, token.as_deref()) {
                        Ok(sample) => {
                            if jsonl {
                                println!("{}", sample.as_jsonl(worker_id, iter));
                            }
                            local.push(sample);
                        }
                        Err(e) => {
                            eprintln!("worker {worker_id} iter {iter}: {e:#}");
                        }
                    }
                }
                local
            }));
        }
        let mut all = Vec::with_capacity(total as usize);
        for h in handles {
            all.extend(h.join().expect("worker panicked"));
        }
        Ok(all)
    })?;
    let elapsed = wall.elapsed();

    if samples.is_empty() {
        return Err(anyhow!("all iterations failed — nothing to report"));
    }

    print_summary(&samples, elapsed);
    Ok(())
}

/// One end-to-end loop. Returns per-stage durations.
fn run_one(client: &reqwest::blocking::Client, base: &str, token: Option<&str>) -> Result<Sample> {
    let mut stages = HashMap::new();

    let t = Instant::now();
    let vm = post(client, base, token, "/v1/vms", &json!({}))?;
    stages.insert("create".to_string(), t.elapsed());
    let vm_id = vm["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("create response missing numeric id: {vm}"))?;

    let t = Instant::now();
    post(
        client,
        base,
        token,
        &format!("/v1/vms/{vm_id}/start"),
        &Value::Null,
    )?;
    stages.insert("start".to_string(), t.elapsed());

    let t = Instant::now();
    let snap = post(
        client,
        base,
        token,
        &format!("/v1/vms/{vm_id}/snapshot"),
        &Value::Null,
    )?;
    stages.insert("snapshot".to_string(), t.elapsed());
    let snap_id = snap["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("snapshot response missing numeric id: {snap}"))?;

    let t = Instant::now();
    let forked = post(
        client,
        base,
        token,
        &format!("/v1/snapshots/{snap_id}/restore"),
        &Value::Null,
    )?;
    stages.insert("fork".to_string(), t.elapsed());
    let forked_id = forked["id"]
        .as_u64()
        .ok_or_else(|| anyhow!("restore response missing numeric id: {forked}"))?;

    let t = Instant::now();
    delete(client, base, token, &format!("/v1/vms/{vm_id}"))?;
    delete(client, base, token, &format!("/v1/vms/{forked_id}"))?;
    stages.insert("destroy".to_string(), t.elapsed());

    Ok(Sample { stages })
}

/// One iteration's per-stage durations.
#[derive(Debug)]
struct Sample {
    stages: HashMap<String, Duration>,
}

impl Sample {
    fn as_jsonl(&self, worker: u32, iter: u32) -> String {
        let mut map = serde_json::Map::new();
        map.insert("worker".into(), worker.into());
        map.insert("iter".into(), iter.into());
        for stage in STAGES {
            let micros = self
                .stages
                .get(*stage)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            map.insert(format!("{stage}_us"), micros.into());
        }
        serde_json::to_string(&Value::Object(map)).expect("serialize sample")
    }
}

/// Fail fast if the base URL is wrong or the server is down: a
/// healthz round-trip should be cheap and unauthenticated.
fn preflight(client: &reqwest::blocking::Client, base: &str) -> Result<()> {
    let url = format!("{base}/healthz");
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("preflight GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "preflight GET {url} returned {}; bench target unreachable",
            resp.status()
        ));
    }
    Ok(())
}

fn post(
    client: &reqwest::blocking::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
    body: &Value,
) -> Result<Value> {
    let url = format!("{base}{path}");
    let mut req = client.post(&url);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    if !body.is_null() {
        req = req.json(body);
    }
    let resp = req.send().with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("POST {url} → {status}: {text}"));
    }
    // `start`/`stop`/`destroy` return 204 with no body; surface that
    // as `Value::Null` rather than failing JSON parsing.
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("parse response from POST {url}: {text}"))
}

fn delete(
    client: &reqwest::blocking::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
) -> Result<()> {
    let url = format!("{base}{path}");
    let mut req = client.delete(&url);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!(
            "DELETE {url} → {status}: {}",
            resp.text().unwrap_or_default()
        ));
    }
    Ok(())
}

fn print_summary(samples: &[Sample], wall: Duration) {
    let n = samples.len();
    println!();
    println!(
        "samples: {n}, wall: {:.3}s, throughput: {:.1} loop/s",
        wall.as_secs_f64(),
        n as f64 / wall.as_secs_f64(),
    );
    println!();
    println!(
        "{:<10} {:>10} {:>10} {:>10} {:>10}",
        "stage", "p50 µs", "p95 µs", "p99 µs", "max µs"
    );
    println!("{}", "-".repeat(54));
    for stage in STAGES {
        let mut micros: Vec<u64> = samples
            .iter()
            .filter_map(|s| s.stages.get(*stage).map(|d| d.as_micros() as u64))
            .collect();
        micros.sort_unstable();
        let p50 = percentile(&micros, 0.50);
        let p95 = percentile(&micros, 0.95);
        let p99 = percentile(&micros, 0.99);
        let max = micros.last().copied().unwrap_or(0);
        println!("{stage:<10} {p50:>10} {p95:>10} {p99:>10} {max:>10}");
    }
}

/// Nearest-rank percentile. `q` in `[0.0, 1.0]`. Empty input → 0.
fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    // Nearest-rank with 1-based ceil: rank = ceil(q * n).
    // Saturate to the highest index so q=1.0 lands on the max.
    let rank = ((q * n as f64).ceil() as usize).clamp(1, n);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_handles_empty_input() {
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn percentile_nearest_rank_matches_canonical_table() {
        // [15, 20, 35, 40, 50] — Wikipedia nearest-rank examples.
        let v = vec![15, 20, 35, 40, 50];
        assert_eq!(percentile(&v, 0.30), 20);
        assert_eq!(percentile(&v, 0.40), 20);
        assert_eq!(percentile(&v, 0.50), 35);
        assert_eq!(percentile(&v, 0.95), 50);
        assert_eq!(percentile(&v, 1.00), 50);
    }

    #[test]
    fn percentile_single_element() {
        assert_eq!(percentile(&[42], 0.5), 42);
        assert_eq!(percentile(&[42], 0.99), 42);
    }
}
