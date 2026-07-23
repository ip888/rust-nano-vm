//! `nanovm-api-bench` — reproducible REST-API fork-latency harness.
//!
//! Complements `crates/bench`'s `nanovm-fork-bench` (KVM host-side —
//! measures the internal `restore()` syscall). This binary measures
//! what a customer actually experiences: HTTP `POST /v1/snapshots/:id/fork`
//! from wherever the caller lives to wherever the control plane runs,
//! then reads the server-reported `fork_ms` out of the response body so
//! the reported number excludes network RTT.
//!
//! It's the tool behind the "~12 ms fork" claim on the landing page:
//! any operator can point it at their own deployment and produce the
//! same shape of markdown table.
//!
//! ## Usage
//!
//! ```sh
//! cargo run -p api-bench --release -- \
//!     --api-url https://api.your-saas.com \
//!     --token   nv_your-throwaway-key \
//!     --marketplace-name python-3.12-minimal \
//!     --n 100 --warmup 10
//! ```
//!
//! Reads env fallbacks `NANOVM_BENCH_URL` and `NANOVM_BENCH_TOKEN`.
//!
//! After each measured fork the harness `DELETE`s the returned VM
//! (VMs are cheap but unbounded VM accumulation exhausts the operator's
//! per-org VM budget). The destroy round-trip is NOT included in the
//! reported per-fork latency.

use std::io::{self, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, Parser};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Fork-latency benchmark against a running nanovm control plane",
    long_about = "Sequential POSTs to /v1/snapshots/:id/fork or /v1/marketplace/snapshots/:name/fork; \
                  reads server-reported `fork_ms` from each response; \
                  destroys the returned VM after each measured fork; \
                  prints p50/p90/p95/p99/min/max/mean/stddev + text histogram \
                  + copy-pasteable markdown table."
)]
#[command(group(
    ArgGroup::new("target").required(true).args(&["snapshot_id", "marketplace_name"])
))]
struct Args {
    /// Base URL of the control plane. e.g. `https://api.your-saas.com`.
    #[arg(long, env = "NANOVM_BENCH_URL")]
    api_url: String,

    /// Bearer token to authenticate with. Provisioned per-org via
    /// `POST /v1/keys` or the env `NANOVM_API_TOKENS` on the server.
    #[arg(long, env = "NANOVM_BENCH_TOKEN")]
    token: String,

    /// Snapshot id to fork. Mutually exclusive with `--marketplace-name`.
    #[arg(long, group = "target")]
    snapshot_id: Option<u64>,

    /// Marketplace entry name to fork. Mutually exclusive with
    /// `--snapshot-id`.
    #[arg(long, group = "target")]
    marketplace_name: Option<String>,

    /// Number of measured forks. Reported statistics are over these.
    #[arg(long, default_value_t = 100)]
    n: usize,

    /// Warmup forks that are discarded from statistics. First-fork
    /// latency for a marketplace snapshot includes a tarball download
    /// (seconds); this filter isolates the steady-state warm-pool
    /// number the marketing surface cites.
    #[arg(long, default_value_t = 10)]
    warmup: usize,

    /// Emit JSON (samples + summary) instead of markdown. Useful for
    /// programmatic consumption / CI regression detection.
    #[arg(long)]
    json: bool,

    /// Timeout for a single fork request (seconds). Also applied to
    /// the follow-up destroy.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,

    /// Skip destroying the returned VM after each fork. Useful when
    /// benchmarking against a mock backend where destroy is a no-op
    /// anyway. Off by default; leaving VMs behind on a real deployment
    /// exhausts the org's VM budget.
    #[arg(long)]
    no_destroy: bool,
}

/// Ceiling on `--warmup + --n`. Everything is buffered in RAM (the
/// Vec of measured samples + the intermediate per-iteration state),
/// so an unbounded value would let a typo turn a benchmark run into
/// an OOM. Chosen high enough to cover every realistic benchmark
/// (100k forks × 4 bytes/sample ≈ 400 KB) while still catching the
/// `--n 100000000` fat-finger.
const MAX_TOTAL_ITERATIONS: usize = 100_000;

fn main() -> Result<()> {
    let args = Args::parse();
    if args.n == 0 {
        bail!("--n must be >= 1");
    }
    let total = args
        .warmup
        .checked_add(args.n)
        .ok_or_else(|| anyhow!("--warmup + --n overflowed usize"))?;
    if total > MAX_TOTAL_ITERATIONS {
        bail!(
            "--warmup + --n = {total} exceeds MAX_TOTAL_ITERATIONS ({MAX_TOTAL_ITERATIONS}); \
             the harness records every measured sample in memory. Split into multiple runs \
             or raise the cap."
        );
    }

    let client = build_client(&args)?;
    let path = fork_path(&args);
    let url = format!("{}{}", args.api_url.trim_end_matches('/'), path);

    // Reserve for the MEASURED window only — warmup samples are
    // dropped before landing in `samples`, so `args.n` is the actual
    // capacity we need.
    let mut samples: Vec<u32> = Vec::with_capacity(args.n);

    for i in 0..total {
        let (ms, vm_id) = one_fork(&client, &url, &args.token, args.timeout_secs)?;
        if !args.no_destroy {
            // Best-effort — a destroy that 404s (already-gone VM) or
            // 5xxs shouldn't fail the whole run.
            let _ = destroy_vm(
                &client,
                &args.api_url,
                &args.token,
                vm_id,
                args.timeout_secs,
            );
        }
        if i >= args.warmup {
            samples.push(ms);
        }
        if !args.json && (i + 1) % 10 == 0 {
            let _ = writeln!(io::stderr(), "  {:>4}/{} {} ms", i + 1, total, ms,);
        }
    }

    let summary = summarise(&samples);
    if args.json {
        let out = JsonOut {
            api_url: &args.api_url,
            target: describe_target(&args),
            samples_ms: &samples,
            summary: &summary,
            warmup: args.warmup,
        };
        serde_json::to_writer_pretty(io::stdout().lock(), &out)?;
        println!();
    } else {
        print_markdown(&args, &summary, &samples);
    }
    Ok(())
}

fn build_client(args: &Args) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", args.token))
            .context("token contained non-ASCII characters")?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(args.timeout_secs))
        .build()
        .context("build reqwest client")
}

fn fork_path(args: &Args) -> String {
    if let Some(id) = args.snapshot_id {
        return format!("/v1/snapshots/{id}/fork");
    }
    // clap enforces exactly-one via ArgGroup, so this branch means the
    // marketplace name is present.
    let name = args
        .marketplace_name
        .as_ref()
        .expect("clap arg group ensures one target is set");
    format!(
        "/v1/marketplace/snapshots/{}/fork",
        url_encode_path_segment(name)
    )
}

fn describe_target(args: &Args) -> String {
    if let Some(id) = args.snapshot_id {
        format!("snapshot id {id}")
    } else {
        format!(
            "marketplace/{}",
            args.marketplace_name.as_deref().unwrap_or("?")
        )
    }
}

/// Do one fork request. Returns `(fork_ms, vm_id)`. Uses the
/// server-reported `fork_ms` if present; falls back to client wall-clock.
/// Retries 429 up to 3 times honoring `Retry-After`.
fn one_fork(client: &Client, url: &str, token: &str, timeout_secs: u64) -> Result<(u32, u64)> {
    let mut attempt = 0u32;
    loop {
        let t0 = Instant::now();
        let resp = client.post(url).body("{}").send().context("POST /fork")?;
        let status = resp.status();
        if status.is_success() {
            let elapsed_ms = t0.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let body: serde_json::Value = resp.json().context("parse fork response body")?;
            let ms = body
                .get("fork_ms")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(u32::MAX as u64) as u32)
                .unwrap_or(elapsed_ms);
            let vm_id = body
                .get("vm")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("fork response missing vm.id"))?;
            return Ok((ms, vm_id));
        }
        if status == StatusCode::TOO_MANY_REQUESTS && attempt < 3 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1)
                .min(timeout_secs);
            let _ = writeln!(
                io::stderr(),
                "  429 rate-limited; sleeping {retry_after}s (attempt {})",
                attempt + 1
            );
            sleep(Duration::from_secs(retry_after));
            attempt += 1;
            continue;
        }
        // Anything else is a hard failure; surface the server envelope
        // so the caller sees "auth invalid", "quota exhausted", etc.
        let body = resp.text().unwrap_or_default();
        // Use the token in a scoped debug print rather than tricking the
        // reader with a truncated bearer.
        let _ = token; // token is embedded in the client's default headers
        bail!("fork request failed: HTTP {status}: {body}");
    }
}

fn destroy_vm(
    client: &Client,
    api_url: &str,
    _token: &str,
    id: u64,
    timeout_secs: u64,
) -> Result<()> {
    let url = format!("{}/v1/vms/{}", api_url.trim_end_matches('/'), id);
    let resp = client
        .delete(&url)
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .context("DELETE /v1/vms/:id")?;
    // 204 or 404 (already-gone) both acceptable. Anything else surfaces.
    if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(anyhow!("destroy of vm {id} failed: HTTP {}", resp.status()))
    }
}

/// Percent-encode a marketplace entry name so any `/`, `?`, `&`, etc.
/// stays inside one path segment. Handrolled to avoid pulling in
/// `url`/`percent-encoding` as an extra workspace dep — the harness
/// is otherwise <5 direct deps.
fn url_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Summary statistics over a sample vector. All values in ms, u32-sized.
#[derive(Debug, Serialize)]
struct Summary {
    n: usize,
    p50: u32,
    p90: u32,
    p95: u32,
    p99: u32,
    min: u32,
    max: u32,
    mean: u32,
    stddev: u32,
}

fn summarise(samples: &[u32]) -> Summary {
    if samples.is_empty() {
        return Summary {
            n: 0,
            p50: 0,
            p90: 0,
            p95: 0,
            p99: 0,
            min: 0,
            max: 0,
            mean: 0,
            stddev: 0,
        };
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let sum: u64 = sorted.iter().map(|v| u64::from(*v)).sum();
    let mean = (sum / sorted.len() as u64) as u32;
    // Population variance (N — the sample IS the population here).
    let var_num: u64 = sorted
        .iter()
        .map(|v| {
            let d = i64::from(*v) - i64::from(mean);
            (d * d) as u64
        })
        .sum();
    let variance = var_num / sorted.len() as u64;
    let stddev = (variance as f64).sqrt() as u32;
    Summary {
        n: sorted.len(),
        p50: percentile(&sorted, 50),
        p90: percentile(&sorted, 90),
        p95: percentile(&sorted, 95),
        p99: percentile(&sorted, 99),
        min: sorted[0],
        max: sorted[sorted.len() - 1],
        mean,
        stddev,
    }
}

/// Inclusive-index percentile over a pre-sorted vector: `idx =
/// floor((n - 1) * p / 100)`. This is NOT the classic nearest-rank
/// definition (`ceil(n * p / 100) - 1`); the difference only matters
/// at small `n` and at the top end (nearest-rank returns
/// `sorted[n-1]` for p=99 at n=20; this returns `sorted[18]`).
/// Chose the inclusive-index form because it stays inside
/// `[0, n-1]` without a `saturating_sub`, and because it lines up
/// with what most percentile helpers in the Prometheus /
/// perf-tools world produce for the same input.
fn percentile(sorted: &[u32], p: u32) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as u64 * u64::from(p) / 100) as usize;
    sorted[idx]
}

#[derive(Debug, Serialize)]
struct JsonOut<'a> {
    api_url: &'a str,
    target: String,
    warmup: usize,
    samples_ms: &'a [u32],
    summary: &'a Summary,
}

fn print_markdown(args: &Args, s: &Summary, samples: &[u32]) {
    println!("# nanovm-api-bench");
    println!();
    println!("| Field           | Value                    |");
    println!("|-----------------|--------------------------|");
    println!("| API             | `{}` |", args.api_url);
    println!("| Target          | `{}` |", describe_target(args));
    println!("| Measured forks  | {} |", s.n);
    println!("| Warmup (discarded) | {} |", args.warmup);
    println!();
    println!("## Summary (ms)");
    println!();
    println!("| p50 | p90 | p95 | p99 | min | max | mean | stddev |");
    println!("|-----|-----|-----|-----|-----|-----|------|--------|");
    println!(
        "| {}  | {}  | {}  | {}  | {}  | {}  | {}   | {}     |",
        s.p50, s.p90, s.p95, s.p99, s.min, s.max, s.mean, s.stddev,
    );
    println!();
    println!("## Distribution");
    println!();
    print_histogram(samples, s.max.max(1));
}

/// 10-bucket text histogram over [0, cap] inclusive. `cap` should
/// be `max` — this keeps the last non-zero bucket flush with the max
/// sample.
fn print_histogram(samples: &[u32], cap: u32) {
    if samples.is_empty() || cap == 0 {
        return;
    }
    let buckets = 10usize;
    let mut counts = vec![0usize; buckets];
    let bucket_of = |v: u32| -> usize {
        // ceil-inclusive at the top edge.
        let scaled = (u64::from(v) * buckets as u64) / u64::from(cap);
        (scaled as usize).min(buckets - 1)
    };
    for v in samples {
        counts[bucket_of(*v)] += 1;
    }
    let peak = *counts.iter().max().unwrap_or(&1) as f64;
    let bar_max = 40u32;
    println!("```");
    for (i, count) in counts.iter().enumerate() {
        let lo = (u64::from(cap) * i as u64) / buckets as u64;
        let hi = (u64::from(cap) * (i + 1) as u64) / buckets as u64;
        let bar_len = ((*count as f64) / peak * bar_max as f64).round() as u32;
        let bar: String = std::iter::repeat_n('█', bar_len as usize).collect();
        println!("{:>4} – {:>4} ms │ {:>4}  {}", lo, hi, count, bar);
    }
    println!("```");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_boundary_cases() {
        let s: Vec<u32> = (1..=100).collect(); // 1..=100
        assert_eq!(percentile(&s, 0), 1);
        assert_eq!(percentile(&s, 50), 50);
        assert_eq!(percentile(&s, 99), 99);
        assert_eq!(percentile(&s, 100), 100);
    }

    #[test]
    fn summarise_uniform_input() {
        // 20 samples: 12 twelves + 3 elevens + 2 thirteens + 3 in the tail.
        // Explicit values so p50/p95 are predictable.
        let samples: Vec<u32> = vec![
            12, 11, 12, 13, 12, 11, 12, 28, 12, 11, 13, 12, 12, 14, 12, 11, 25, 12, 12, 13,
        ];
        let s = summarise(&samples);
        assert_eq!(s.n, 20);
        assert_eq!(s.min, 11);
        assert_eq!(s.max, 28);
        assert!(s.p50 == 12, "expected p50=12 got {}", s.p50);
        // Nearest-rank with 20 samples: idx = 19 * p / 100.
        //   p95 → idx 18 → sorted[18] = 25
        //   p99 → idx 18 (1881/100 = 18 too) → sorted[18] = 25.
        // A larger sample size (>=100) separates p95 and p99; at n=20
        // the two collapse onto the same nearest-rank index by
        // definition. The `max` field surfaces the 28.
        assert_eq!(s.p95, 25);
        assert_eq!(s.p99, 25);
        assert_eq!(s.max, 28);
        // Mean is around 13.
        assert!((12..=14).contains(&s.mean), "mean = {}", s.mean);
    }

    #[test]
    fn summarise_empty_is_zeroed() {
        let s = summarise(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.p50, 0);
        assert_eq!(s.mean, 0);
        assert_eq!(s.stddev, 0);
    }

    #[test]
    fn url_encode_reserved_chars_stay_in_one_segment() {
        // `/`, `?`, `&`, `%`, space — all must percent-encode.
        assert_eq!(
            url_encode_path_segment("weird/name?with&chars"),
            "weird%2Fname%3Fwith%26chars"
        );
        assert_eq!(url_encode_path_segment("has space"), "has%20space");
        // Unreserved chars pass through.
        assert_eq!(
            url_encode_path_segment("python-3.12-minimal"),
            "python-3.12-minimal"
        );
        assert_eq!(url_encode_path_segment("a_b.c-d~e"), "a_b.c-d~e");
    }
}
