# nanovm-api-bench

Reproducible REST-API fork-latency harness for the `rust-nano-vm`
control plane. Point it at any live deployment and it produces the
same shape of markdown table the landing page cites.

Complements `crates/bench`'s `nanovm-fork-bench`:

| Binary                | Measures                                              | Runs on               |
|-----------------------|-------------------------------------------------------|-----------------------|
| `nanovm-fork-bench`   | KVM host-side `restore()` syscall latency              | The control-plane host (needs `/dev/kvm`) |
| `nanovm-api-bench`    | HTTP `POST /v1/snapshots/:id/fork` client wall-clock, minus network RTT (reads server-reported `fork_ms` out of the body) | Anywhere HTTPS reaches the API |

The API bench is what a customer sees. The host bench is what
Firecracker-benchmark posts compare against.

## 30-second reproduce

1. Provision an org + API key (either via the dashboard `/dashboard/keys`
   or via `NANOVM_API_TOKENS=acme:tok@developer` on the server).
2. Have a snapshot to fork — either a `snapshot_id` you captured
   yourself or a marketplace entry name (`python-3.12-minimal` is the
   canonical one that ships with every marketplace config).
3. Run:

```sh
cargo run -p api-bench --release -- \
    --api-url https://api.your-saas.com \
    --token   nv_<your-key> \
    --marketplace-name python-3.12-minimal \
    --n 100 --warmup 10
```

Or via env vars:

```sh
export NANOVM_BENCH_URL=https://api.your-saas.com
export NANOVM_BENCH_TOKEN=nv_<your-key>
cargo run -p api-bench --release -- \
    --marketplace-name python-3.12-minimal
```

## Sample output (markdown mode)

```
# nanovm-api-bench

| Field           | Value                    |
|-----------------|--------------------------|
| API             | `https://api.your-saas.com` |
| Target          | `marketplace/python-3.12-minimal` |
| Measured forks  | 100 |
| Warmup (discarded) | 10 |

## Summary (ms)

| p50 | p90 | p95 | p99 | min | max | mean | stddev |
|-----|-----|-----|-----|-----|-----|-----|-----|-----|-----|
| 12  | 14  | 25  | 28  | 11  | 30  | 13  | 4   |

## Distribution

   0 –    3 ms │    0
   3 –    6 ms │    0
   6 –    9 ms │    0
   9 –   12 ms │   42  ██████████████████████████████
  12 –   15 ms │   47  █████████████████████████████████
  15 –   18 ms │    2  █▏
  18 –   21 ms │    0
  21 –   24 ms │    2  █▏
  24 –   27 ms │    4  ██▊
  27 –   30 ms │    3  ██
```

## JSON output

For CI regression detection, add `--json`:

```sh
nanovm-api-bench --api-url ... --token ... --snapshot-id 42 --n 100 --json > run.json
```

Emits `{ api_url, target, warmup, samples_ms: [...], summary: {...} }`.
Diff `summary.p95` across runs to detect regressions.

## Server-reported vs client-measured

The `fork_ms` field the server includes in the fork response is the
authoritative number — it's measured inside the control-plane handler
around the `restore()` call, so it excludes network RTT between the
harness and the API. That's exactly the number the landing page cites.

If a response body ever omits `fork_ms` (older server versions), the
harness falls back to `Instant::now()` deltas around the POST — that
one includes network round-trip, so an on-lan sample looks like ~12 ms
but a coast-to-coast sample would report ~50 ms even for the same
underlying fork.

## What the harness does after each fork

Every fork returns a real VM. To keep the run from exhausting the
caller's per-org VM budget, the harness `DELETE`s each returned VM
immediately after recording its `fork_ms`. The destroy round-trip is
NOT included in the reported latency.

Pass `--no-destroy` to skip that step — useful only when benchmarking
against the mock backend where destroy is a no-op anyway.

## Rate limiting

The control plane's per-token fork bucket may respond `429 Too Many
Requests` with a `Retry-After` header. The harness respects it and
retries up to 3 times per fork. If you're routinely hitting the bucket,
either raise the plan's `NANOVM_FORK_RPS` or lower `--n` (warmup +
measured) so the whole run fits inside the bucket.
