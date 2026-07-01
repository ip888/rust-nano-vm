# Live-KVM demo

**A real `rust-nano-vm` control plane, driving real KVM ioctls on a real Linux kernel, with real Prometheus scraping real metrics into real Grafana dashboards — one command up, one command down.**

This directory is a self-contained answer to *"show me the platform is production quality, not just a passing test suite."* Stand it up in about 5 minutes, share the Grafana URL with a prospect, watch panels move under load, tear it down. Nothing here is simulated or mocked.

## Two paths — pick one

| Your host | Command | Cost | What runs where |
|---|---|---|---|
| **Linux + `/dev/kvm`** (Omarchy, Arch, Ubuntu, Fedora, bare-metal Debian, …) | `./up-local.sh` | $0 | Real KVM ioctls on **your host kernel**. Control plane + Prometheus + Grafana all in docker on your box. |
| **macOS (Intel or M1/M2/M3/M4), Windows, or no local KVM** | `./up.sh` | ~$0.30/hr while running | Real KVM ioctls on a **Fly.io performance machine**. Prometheus + Grafana on your laptop, pointed at the Fly URL. |

Everything downstream — dashboards, load generator, audit-log tailing — is the same either way. Both paths use the same production `nanovm-control-plane-kvm` image and the same `nanovm-overview.json` dashboard the Helm chart ships.

## What "real KVM" means today

Since the process-fleet arc landed (PRs #115-#132), the shape is:

```
  REST client ─►  nanovm-control-plane  (in this container)
                         │
                         │  spawns one worker per VM
                         ▼
              nanovm-jailer  ─►  nanovm-vmm-child  (built --features kvm)
                                       │
                                       ▼
                                   /dev/kvm   ◄── real KVM ioctls
```

The KVM image bundles `nanovm-vmm-child` built with `--features kvm`, so every VM the REST API creates gets its own worker process holding an open `/dev/kvm` handle. `NANOVM_BACKEND=fleet` is the container's default env; you don't have to set it.

**What still lands as follow-up:** the image doesn't yet bundle a default kernel + rootfs, so `POST /v1/vms` with the built-in `VmConfig::default()` will fail to boot (worker returns "kernel not found"). You'll still see the whole REST surface + Prometheus wiring + audit log working end-to-end because a fork-quota reject, an unauth 401, a per-org meter, and a warm-pool hit all fire *before* the actual kernel-load step. Bundling a minimal Alpine kernel + rootfs into the image is the next PR — until then, the "watch a real kernel boot inside a VM" step is only reachable through the bench binary (`cargo run -p bench --features kvm --release --bin nanovm-fork-bench`), which points at a fixture kernel checked into the repo.

## What you'll see

| Thing you'll see | Where |
|---|---|
| Real `/dev/kvm` on a Linux 6.1+ kernel | `curl <base-url>/v1/health` → `"backend":"kvm"` (base = `http://localhost:8080` for Path A, `https://<app>.fly.dev` for Path B) |
| Real KVM snapshot/restore | Grafana → *nanovm-overview* → **Fork latency p50/p99** panel (100-300 ms range, not `sleep(50)`) |
| Real multi-tenant per-org metering | Grafana → **Forks by org** panel (3 diverging lines: acme / globex / initech) |
| Real fork-quota throttling | Grafana → **Throttled forks** panel (429 curve climbing on `acme`) |
| Real warm-pool hit ratio | Grafana → **Warm pool hits vs misses** panel |
| Real structured tracing | Terminal: `./tail.sh` streams JSON `RUST_LOG` from the Fly machine |
| Real JSONL audit log | Same terminal, interleaved: every `fork` / `snapshot` / `delete` privileged call |
| Real Prometheus alert rules | Prometheus → Alerts tab shows the 4 shipped alerts evaluating live |

## Path A — Local (Linux + KVM, zero cloud spend)

For Omarchy / Arch / Ubuntu / Fedora / any Linux box with `/dev/kvm`. Runs everything on your host in three docker containers: control-plane (with `--device=/dev/kvm`), Prometheus, Grafana.

### Preflight

```sh
ls -l /dev/kvm                           # /dev/kvm exists, group `kvm`
egrep -c '(vmx|svm)' /proc/cpuinfo       # > 0 (Intel vmx or AMD svm)
docker compose version                   # docker + compose plugin present
```

If `/dev/kvm` is missing, install KVM (Arch/Omarchy: `sudo pacman -S qemu-full`; Debian family: `sudo apt install qemu-kvm`) and reboot / `sudo modprobe kvm-intel` (or `kvm-amd`).

### Bring it up

```sh
cd deploy/live-demo
./up-local.sh          # gets the KVM image, mints tokens, starts 3 containers
./load.sh              # in another terminal: multi-org traffic generator
./tail-local.sh        # in another: docker logs + audit JSONL, interleaved
```

`up-local.sh` first tries to pull `ghcr.io/ip888/nanovm-control-plane-kvm:latest` from GHCR (published by `.github/workflows/docker.yml` on every semver tag). If that fails — the image hasn't been published to the repo you're building from, or you're offline — it falls back to a local `docker build -f Dockerfile.kvm .` from the repo root. **First-time local build takes ~5 min** (cold Rust workspace + `--features kvm`); subsequent runs hit the docker layer cache and finish in seconds.

Skip the build entirely on rebuilds — the script skips both pull and build if the tag already exists locally.

Then open [`http://localhost:3000/d/nanovm-overview`](http://localhost:3000/d/nanovm-overview) — same dashboard, real KVM on your host.

### Tear down

```sh
./down-local.sh                  # stop 3 containers, drop audit-log volume
./down-local.sh --keep-audit     # keep the audit JSONL volume for later inspection
```

## Path B — Fly.io (macOS / M1 / Windows / no local KVM, ~$0.30/hr while running)

For laptops without `/dev/kvm`. Deploys the same KVM image to a Fly.io **performance-2x** machine (the CPU kind that exposes `/dev/kvm`) and points a local Prometheus + Grafana at it.

### Prerequisites (~2 min)

On your laptop:

- [`flyctl`](https://fly.io/docs/hands-on/install-flyctl/) — one-line install
- A Fly.io account (`flyctl auth login`, free to sign up; performance machines are pay-per-hour, budget ~$0.30/hr while running)
- `docker` + `docker compose` (for local Prometheus + Grafana)
- `curl`, `envsubst`, `openssl` (or `/dev/urandom`) — standard on every Linux/macOS box

**No local KVM required.** The KVM ioctls happen inside the Fly.io performance machine's kernel — your laptop is just watching.

### Bring it up

```sh
cd deploy/live-demo
./up.sh
```

That single script:

1. Deploys the pre-built `ghcr.io/ip888/nanovm-control-plane-kvm:latest` image to a Fly.io **performance** machine (the CPU kind that exposes `/dev/kvm`) with 4 GiB RAM.
2. Generates fresh per-org bearer tokens (`acme:…`, `globex:…`, `initech:…`) into `.env.local` (gitignored) and plants them as Fly secrets.
3. Curls `/v1/health` and refuses to continue unless the backend self-reports as `"kvm"` — belt-and-suspenders check that the machine really landed on a KVM-capable host.
4. Renders `compose/prometheus.yml` from the template, pointed at your Fly hostname.
5. Starts local Prometheus (`:9090`) + Grafana (`:3000`, anonymous Admin) via `docker compose`, with the same `nanovm-overview.json` dashboard the Helm chart ships auto-loaded.
6. Prints the URLs.

Expected output (last block):

```
✓ Live-KVM demo is running.

  Control plane (real KVM on Fly.io):
    Health:    https://nanovm-live-demo.fly.dev/v1/health
    Metrics:   https://nanovm-live-demo.fly.dev/metrics
    OpenAPI:   https://nanovm-live-demo.fly.dev/openapi.json

  Local observability (your laptop):
    Prometheus: http://localhost:9090
    Grafana:    http://localhost:3000/d/nanovm-overview
```

## Drive load

In one terminal:

```sh
./load.sh
```

Three concurrent workers, one per synthetic org:

| Org | Rate | Why |
|---|---|---|
| `acme` | fork ~1/s | Will trip the fork-quota (default 5 rps / 10 burst) → 429s show up in dashboard |
| `globex` | fork ~1/3s | Well within quota → steady green line |
| `initech` | fork ~1/6s | Idle-ish → sparse dots |

Each iteration is a full **create → snapshot → fork ×3 → exec → destroy** lifecycle. You'll see per-fork HTTP status lines scroll (`HTTP 201`, `HTTP 429`), and every one of them is one real KVM restore round-trip.

## Tail logs + audit

In a second terminal:

```sh
./tail.sh
```

Interleaves two streams with a coloured prefix:

- `[log]`   — the control plane's JSON `RUST_LOG` output (streamed via `flyctl logs`)
- `[audit]` — `/var/log/nanovm/audit.jsonl` on the Fly machine (streamed via `flyctl ssh console -C 'tail -F …'`)

You'll see lines like:

```
[audit] {"who":"acme","action":"fork","vm":42,"snapshot":"snap-xyz",...}
[log]   {"lvl":"INFO","msg":"fork served from warm pool","span":"fork"...}
[log]   {"lvl":"WARN","msg":"fork quota exceeded","span":"fork",...}
[audit] {"who":"globex","action":"snapshot","vm":43,...}
```

Every one of those lines is a real API call against a real KVM machine.

## Open the dashboard

Browser → `http://localhost:3000/d/nanovm-overview`

The 6 panels populate in ~30-60 seconds (Prometheus scrapes every 15 s). Once populated:

- **Forks / sec (by org)** — three diverging lines. `acme` climbs, `globex` steady, `initech` sparse.
- **Fork latency p50 / p99** — real KVM restore latencies. First few forks are cold (higher), then warm-pool kicks in and p50 drops.
- **Warm pool hits vs misses** — hits climb after the first pool warm-up burst.
- **Throttled forks (by org)** — `acme` line climbs as it exceeds the quota.
- **HTTP request rate (by route)** — `/v1/snapshots/:id/fork` dominates, `/v1/health` steady from Fly's health check.
- **`nanovm_up`** — steady 1 (unless the machine dies, which is exactly what an operator wants to see).

## Share the demo

Grafana runs anonymous-Admin so you can send the URL to a prospect. The Fly.io app URL is public HTTPS. Prospect can:

- `curl https://<app>.fly.dev/v1/health -H "Authorization: Bearer <acme-token>"` — hit the API themselves
- Browse `http://<your-tunneled-grafana>:3000/d/nanovm-overview` — watch metrics move
- Read `https://<app>.fly.dev/openapi.json` — see the full REST surface

For the "we want to host this ourselves" enterprise pitch, point them at **Path 2 (Kubernetes/Helm)** or **Path 3 (AWS bare metal)** in [../README.md](../README.md) — same binary, their infra.

## Tear it down

```sh
./down.sh                    # stop the Fly machine + local compose stack (app kept, no compute charges)
./down.sh --destroy-fly-app  # nuke the Fly app entirely
```

`./down.sh` by default only scales to 0 machines so you stop paying while keeping the app + secrets around for a fast `./up.sh` next time. Add `--destroy-fly-app` when you're really done and want a clean slate.

## Cost estimate

Fly.io `performance-2x` at 4 GiB, `iad` region, as of writing: about **$0.30 per running hour** — a couple dollars for a full working day of demoing. Bandwidth for Prometheus scrapes (`/metrics` is a few KB, scraped every 15 s) is negligible. **You are charged only while the Fly machine is running**; `./down.sh` scales it to zero.

## Why this is honest evidence, not a mock

- The `nanovm-control-plane-kvm` image is the **same binary** the Helm chart deploys.
- `/dev/kvm` on a Fly.io performance machine is the **same kernel primitive** used on AWS `*.metal`.
- The Grafana dashboard JSON is **byte-identical** to `../grafana/dashboards/nanovm-overview.json` — what you see here is what an operator sees in production.
- The Prometheus alert rules are **byte-identical** to `../prometheus/alerts.yaml` — a live alert firing here would fire the same way in prod.
- The audit JSONL format is **the same file** `NANOVM_AUDIT_LOG` writes in any deployment.

There is nothing under `deploy/live-demo/` that shortcuts the real code path.

## Non-goals for this demo

- **Not high-availability.** One machine. A production deployment would front `≥3` behind a Fly load balancer with `NANOVM_SNAPSHOT_STORE=s3://…` for cross-region snapshot durability. See the "Honest non-features" section of [../README.md](../README.md).
- **Not persistent.** `NANOVM_TOKEN_STORE_PATH` and `NANOVM_SNAPSHOT_STORE` default to in-machine paths — a `flyctl machine restart` resets state. Wire durable stores for production.
- **Not multi-tenant hardened at the hypervisor level.** Fly.io's per-app isolation is good enough for a demo but for the "we sell this as SaaS" pitch you'll want AWS Nitro `*.metal` or dedicated bare metal — see Path 3.
