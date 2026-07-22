"use client";

import { useCallback, useMemo, useState } from "react";

/**
 * Live fork-latency benchmark, meant to slot into the landing page.
 *
 * Two modes:
 *
 * - **Live**  — when both `NEXT_PUBLIC_NANOVM_DEMO_URL` and
 *   `NEXT_PUBLIC_NANOVM_DEMO_TOKEN` are set at build time (typically a
 *   throwaway public demo tenant on the operator's own control plane),
 *   the "Run 20 forks" button hits the real API and renders the
 *   real per-fork wall-clock. `NEXT_PUBLIC_NANOVM_DEMO_SNAPSHOT_ID` is
 *   the snapshot to fork; empty → use the marketplace fork endpoint
 *   with `NEXT_PUBLIC_NANOVM_DEMO_MARKETPLACE_NAME`
 *   (default `python-3.12-minimal`).
 * - **Seeded** — when the env vars aren't set (default), the button
 *   plays back a plausible latency distribution so the visualisation
 *   isn't blank. A small "seeded — not live" pill makes the mode
 *   obvious so a visitor doesn't mistake it for real numbers.
 *
 * The point of the seeded mode: even without a public demo tenant,
 * the landing page shows the SHAPE of the argument (~12 ms warm-pool
 * hits, ~28 ms occasional cold, tight distribution) rather than an
 * empty chart. The moment an operator wires the env vars, real
 * numbers show up.
 */

const N_FORKS = 20;

/** Plausible latency dataset used in seeded mode. Chosen to match
 *  the numbers on the marketing surface: 17/20 warm-pool hits around
 *  12 ms + a few colds spread up to ~30. Deterministic so the
 *  chart renders identically for every visitor. */
const SEEDED_LATENCIES_MS = [
  12, 11, 12, 13, 12, 11, 12, 28, 12, 11,
  13, 12, 12, 14, 12, 11, 25, 12, 12, 13,
];

interface DemoConfig {
  apiUrl: string;
  token: string;
  snapshotId: string | null;
  marketplaceName: string;
}

function demoConfig(): DemoConfig | null {
  const apiUrl = process.env.NEXT_PUBLIC_NANOVM_DEMO_URL?.trim();
  const token = process.env.NEXT_PUBLIC_NANOVM_DEMO_TOKEN?.trim();
  if (!apiUrl || !token) return null;
  const snapshotId = process.env.NEXT_PUBLIC_NANOVM_DEMO_SNAPSHOT_ID?.trim() || null;
  const marketplaceName =
    process.env.NEXT_PUBLIC_NANOVM_DEMO_MARKETPLACE_NAME?.trim() ||
    "python-3.12-minimal";
  return { apiUrl, token, snapshotId, marketplaceName };
}

type Result =
  | { kind: "idle" }
  | { kind: "loading"; done: number; latencies: number[] }
  | { kind: "done"; latencies: number[]; live: boolean }
  | { kind: "error"; message: string };

export default function LiveForkBenchmark() {
  const [result, setResult] = useState<Result>({ kind: "idle" });
  const config = useMemo(() => demoConfig(), []);
  const liveMode = config !== null;

  const run = useCallback(async () => {
    if (!config) {
      // Seeded mode: play back the fixed dataset with a short
      // per-sample delay so the "running…" state is visible.
      const latencies: number[] = [];
      setResult({ kind: "loading", done: 0, latencies: [] });
      for (let i = 0; i < SEEDED_LATENCIES_MS.length; i++) {
        latencies.push(SEEDED_LATENCIES_MS[i]!);
        setResult({ kind: "loading", done: i + 1, latencies: [...latencies] });
        await new Promise((r) => setTimeout(r, 40));
      }
      setResult({ kind: "done", latencies, live: false });
      return;
    }

    // Live mode: hit the demo tenant's fork endpoint N_FORKS times.
    const latencies: number[] = [];
    setResult({ kind: "loading", done: 0, latencies: [] });
    const path = config.snapshotId
      ? `/v1/snapshots/${encodeURIComponent(config.snapshotId)}/fork`
      : `/v1/marketplace/snapshots/${encodeURIComponent(config.marketplaceName)}/fork`;
    try {
      for (let i = 0; i < N_FORKS; i++) {
        const t0 =
          typeof performance !== "undefined" ? performance.now() : Date.now();
        const resp = await fetch(`${config.apiUrl}${path}`, {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            Authorization: `Bearer ${config.token}`,
          },
          body: "{}",
        });
        if (!resp.ok) {
          throw new Error(await formatHttpError(resp));
        }
        const body = await resp.json();
        // Prefer server-reported fork_ms (accurate, excludes network
        // RTT); fall back to client-measured wall clock if the
        // response omits it.
        const t1 =
          typeof performance !== "undefined" ? performance.now() : Date.now();
        const ms =
          typeof body?.fork_ms === "number"
            ? Math.round(body.fork_ms)
            : Math.round(t1 - t0);
        latencies.push(ms);
        setResult({ kind: "loading", done: i + 1, latencies: [...latencies] });
      }
      setResult({ kind: "done", latencies, live: true });
    } catch (err) {
      setResult({
        kind: "error",
        message:
          err instanceof Error
            ? err.message
            : "Fork request failed. Check the browser console.",
      });
    }
  }, [config]);

  return (
    <section className="mb-16 rounded-lg border border-gray-200 p-6 dark:border-gray-800">
      <div className="mb-4 flex flex-wrap items-baseline justify-between gap-3">
        <div>
          <h2 className="text-2xl font-semibold">See it fork, live</h2>
          <p className="mt-1 text-sm text-gray-600 dark:text-gray-400">
            {N_FORKS} sequential fork requests against a public demo
            tenant. Server reports per-fork wall-clock;{" "}
            {liveMode ? (
              <span>
                you&apos;re seeing real numbers from{" "}
                <code className="font-mono text-xs">{config.apiUrl}</code>.
              </span>
            ) : (
              <>
                the numbers below are a seeded dataset while a public
                demo tenant isn&apos;t configured — the shape mirrors
                what a warm-pool fork actually looks like.
              </>
            )}
          </p>
        </div>
        <ModePill live={liveMode} />
      </div>

      <div className="mb-4 flex items-center gap-3">
        <button
          onClick={run}
          disabled={result.kind === "loading"}
          className="rounded-md bg-brand-500 px-4 py-2 text-sm font-medium text-white hover:bg-brand-600 disabled:cursor-not-allowed disabled:bg-brand-300"
        >
          {result.kind === "loading"
            ? `Forking… ${result.done}/${N_FORKS}`
            : result.kind === "done"
              ? "Run again"
              : `Run ${N_FORKS} forks`}
        </button>
        {result.kind === "error" && (
          <span className="text-sm text-red-600 dark:text-red-400">
            {result.message}
          </span>
        )}
      </div>

      {"latencies" in result && result.latencies.length > 0 && (
        <LatencyChart
          latencies={result.latencies}
          inFlight={result.kind === "loading" ? result.done : -1}
          total={N_FORKS}
        />
      )}
    </section>
  );
}

/**
 * Turn a non-2xx response into an actionable message. Prefers the
 * server's structured error envelope (`{error: {message, code}}`)
 * over a bare status code so "402 dunning", "401 bad token", "404
 * unknown snapshot" surface on-page rather than "HTTP 402".
 */
async function formatHttpError(resp: Response): Promise<string> {
  try {
    const body = await resp.json();
    const message =
      typeof body?.error?.message === "string"
        ? body.error.message
        : typeof body?.message === "string"
          ? body.message
          : null;
    const code =
      typeof body?.error?.code === "string" ? body.error.code : null;
    if (message && code) return `HTTP ${resp.status} [${code}]: ${message}`;
    if (message) return `HTTP ${resp.status}: ${message}`;
  } catch {
    // fall through — response wasn't JSON, use the status alone.
  }
  return `HTTP ${resp.status}`;
}

function ModePill({ live }: { live: boolean }) {
  return live ? (
    <span className="inline-flex items-center gap-1.5 rounded-full bg-green-100 px-2.5 py-1 text-xs font-medium text-green-800 dark:bg-green-950 dark:text-green-200">
      <span className="h-1.5 w-1.5 rounded-full bg-green-500" aria-hidden />
      Live
    </span>
  ) : (
    <span className="inline-flex items-center gap-1.5 rounded-full bg-amber-100 px-2.5 py-1 text-xs font-medium text-amber-800 dark:bg-amber-950 dark:text-amber-200">
      <span className="h-1.5 w-1.5 rounded-full bg-amber-500" aria-hidden />
      Seeded — not live
    </span>
  );
}

function LatencyChart({
  latencies,
  inFlight,
  total,
}: {
  latencies: number[];
  inFlight: number;
  total: number;
}) {
  // Scale bars to the widest observed sample (or 40 ms as a floor so
  // the chart doesn't zoom in on tiny numbers).
  const max = Math.max(40, ...latencies);
  const p50 = pctile(latencies, 50);
  const p95 = pctile(latencies, 95);
  const min = latencies.length > 0 ? Math.min(...latencies) : 0;
  const worst = latencies.length > 0 ? Math.max(...latencies) : 0;

  return (
    <div className="space-y-4">
      {/* Summary tiles */}
      {latencies.length > 0 && (
        <div className="grid grid-cols-4 gap-3">
          <Stat label="p50" value={p50 ?? 0} />
          <Stat label="p95" value={p95 ?? 0} />
          <Stat label="min" value={min} />
          <Stat label="max" value={worst} />
        </div>
      )}

      {/* Bar chart */}
      <div className="space-y-1">
        {Array.from({ length: total }).map((_, i) => {
          const v = latencies[i];
          // `inFlight` is the count of completed rows; the currently
          // running row is the NEXT index. `-1` (terminal states)
          // pulses nothing.
          const running = i === inFlight;
          return (
            <div key={i} className="flex items-center gap-2 text-xs">
              <span className="w-6 text-right text-gray-400 tabular-nums">
                {i + 1}
              </span>
              <div className="relative h-4 flex-1 rounded bg-gray-100 dark:bg-gray-800">
                {v !== undefined && (
                  <div
                    className="h-full rounded bg-brand-500 transition-all"
                    style={{
                      width: `${Math.min(100, (v / max) * 100)}%`,
                    }}
                  />
                )}
                {v === undefined && running && (
                  <div className="h-full w-2 animate-pulse rounded bg-brand-300" />
                )}
              </div>
              <span className="w-14 text-right font-mono tabular-nums text-gray-600 dark:text-gray-400">
                {v !== undefined ? `${v} ms` : running ? "…" : "—"}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded border border-gray-200 p-3 text-center dark:border-gray-800">
      <div className="text-xs uppercase tracking-wide text-gray-500">
        {label}
      </div>
      <div className="mt-1 font-mono text-lg tabular-nums">
        {value} <span className="text-xs text-gray-500">ms</span>
      </div>
    </div>
  );
}

/** Exact percentile of a sample set. Empty → undefined. */
function pctile(samples: number[], p: number): number | undefined {
  if (samples.length === 0) return undefined;
  const sorted = [...samples].sort((a, b) => a - b);
  const idx = Math.min(
    sorted.length - 1,
    Math.floor(((sorted.length - 1) * p) / 100),
  );
  return sorted[idx];
}
