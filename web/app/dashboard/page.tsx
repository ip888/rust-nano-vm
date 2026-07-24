"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useMemo, useState } from "react";

import {
  ApiError,
  getBillingPortalUrl,
  getPlan,
  getUsage,
  getUsageByOrg,
  type PlanResponse,
  type UsageResponseDto,
} from "@/lib/api";
import { clearSession, getSession, type Session } from "@/lib/session";

/**
 * The dashboard. All data comes from the API — no server components
 * touch the customer's key. On mount we pull the session out of
 * localStorage; if it's absent we bounce to /login.
 *
 * Three tiles: plan, usage, quick-start snippet. A "Billing portal"
 * button opens Stripe's hosted management UI. Sign-out clears the
 * session and returns to the landing page.
 */
export default function DashboardPage() {
  const router = useRouter();
  const [session, setSess] = useState<Session | null>(null);
  const [plan, setPlan] = useState<PlanResponse | null>(null);
  const [usage, setUsage] = useState<UsageResponseDto | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const s = getSession();
    if (!s) {
      router.replace("/login");
      return;
    }
    setSess(s);
    (async () => {
      try {
        const [p, u] = await Promise.all([getPlan(s.apiKey), getUsage(s.apiKey)]);
        setPlan(p);
        setUsage(u);
      } catch (err) {
        if (err instanceof ApiError && err.status === 401) {
          clearSession();
          router.replace("/login");
          return;
        }
        setError(
          err instanceof ApiError
            ? err.message
            : "Couldn't load your dashboard.",
        );
      }
    })();
  }, [router]);

  async function openBillingPortal() {
    if (!session) return;
    try {
      const { url } = await getBillingPortalUrl(session.apiKey);
      window.location.href = url;
    } catch (err) {
      const msg =
        err instanceof ApiError
          ? err.message
          : "Couldn't open the billing portal.";
      setError(msg);
    }
  }

  function signOut() {
    clearSession();
    router.replace("/");
  }

  if (!session) {
    return null; // Router.replace is in flight.
  }

  return (
    <main className="mx-auto max-w-5xl px-6 py-12">
      <header className="mb-10 flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold">Dashboard</h1>
          <p className="text-sm text-gray-500">
            Signed in as{" "}
            <span className="font-mono">{session.org}</span>
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <Link
            href="/dashboard/playground"
            className="rounded-md bg-brand-500 px-4 py-2 text-sm font-medium text-white hover:bg-brand-600"
          >
            Playground
          </Link>
          <Link
            href="/dashboard/vms"
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            VMs
          </Link>
          <Link
            href="/dashboard/snapshots"
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            Snapshots
          </Link>
          <Link
            href="/dashboard/keys"
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            API keys
          </Link>
          <button
            onClick={signOut}
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            Sign out
          </button>
        </div>
      </header>

      {error && (
        <div className="mb-6 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          {error}
        </div>
      )}

      <OnboardingChecklist apiKey={session.apiKey} usage={usage} />

      <div className="grid gap-6 md:grid-cols-2">
        <Tile title="Plan">
          {plan ? <PlanBody plan={plan} onOpenPortal={openBillingPortal} /> : (
            <Skeleton />
          )}
        </Tile>

        <Tile title="Usage this session">
          {usage ? <UsageBody usage={usage} /> : <Skeleton />}
        </Tile>

        <Tile title="Fork activity (last 5 min)" className="md:col-span-2">
          <ForkActivity apiKey={session.apiKey} orgId={session.org} plan={plan} />
        </Tile>

        <Tile title="Quick start" className="md:col-span-2">
          <QuickStart apiKey={session.apiKey} />
        </Tile>
      </div>
    </main>
  );
}

function Tile({
  title,
  className,
  children,
}: {
  title: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <section
      className={`rounded-lg border border-gray-200 bg-white p-6 shadow-sm dark:border-gray-800 dark:bg-gray-900 ${className ?? ""}`}
    >
      <h2 className="mb-4 text-sm font-medium uppercase tracking-wide text-gray-500">
        {title}
      </h2>
      {children}
    </section>
  );
}

function Skeleton() {
  return (
    <div className="space-y-3">
      <div className="h-4 w-1/3 animate-pulse rounded bg-gray-200 dark:bg-gray-800" />
      <div className="h-4 w-2/3 animate-pulse rounded bg-gray-200 dark:bg-gray-800" />
    </div>
  );
}

function PlanBody({
  plan,
  onOpenPortal,
}: {
  plan: PlanResponse;
  onOpenPortal: () => void;
}) {
  return (
    <div className="space-y-3">
      <div>
        <div className="text-3xl font-semibold">
          {plan.plan?.name ?? "Free"}
        </div>
        <div className="text-sm text-gray-500">
          {plan.plan
            ? `${plan.plan.rps.toLocaleString()} forks/second`
            : "No active subscription"}
        </div>
      </div>
      <div className="text-sm">
        Status:{" "}
        <span className="font-mono">
          {plan.subscription_status ?? "none"}
        </span>
      </div>
      <button
        onClick={onOpenPortal}
        className="rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600"
      >
        Manage billing →
      </button>
    </div>
  );
}

function UsageBody({ usage }: { usage: UsageResponseDto }) {
  const avgMs =
    usage.fork_count > 0
      ? Math.round(usage.fork_total_ms / usage.fork_count)
      : 0;
  return (
    <div className="space-y-3">
      <div>
        <div className="text-3xl font-semibold">
          {usage.fork_count.toLocaleString()}
        </div>
        <div className="text-sm text-gray-500">forks by this API key</div>
      </div>
      <dl className="text-sm">
        <div className="flex justify-between border-t border-gray-100 py-2 dark:border-gray-800">
          <dt className="text-gray-500">Total wall-time</dt>
          <dd className="font-mono">{usage.fork_total_ms.toLocaleString()} ms</dd>
        </div>
        <div className="flex justify-between border-t border-gray-100 py-2 dark:border-gray-800">
          <dt className="text-gray-500">Average fork</dt>
          <dd className="font-mono">{avgMs} ms</dd>
        </div>
      </dl>
    </div>
  );
}

/**
 * Rolling-window fork-rate view. Polls `/v1/usage/by-org` every
 * `POLL_MS`, keeps the last `WINDOW` samples, and renders a sparkline
 * of the derived per-interval rate alongside a `current rate / plan
 * cap` readout.
 *
 * MVP is intentionally poll-based: the existing `nanovm_forks_total_by_org`
 * counter surfaces cheaply and the extra round trip is trivial next to
 * the wall-clock latency of a fork itself. A follow-up (PR-D1v2) swaps
 * to `GET /v1/events` SSE so the browser gets pushed deltas instead of
 * pulling snapshots.
 */
const POLL_MS = 5_000;
const WINDOW = 60; // 60 × 5 s = 5 min of history.

interface Sample {
  /** ms since epoch when the sample landed on the client. */
  t: number;
  /** Cumulative fork count reported by the server at that moment. */
  total: number;
}

function ForkActivity({
  apiKey,
  orgId,
  plan,
}: {
  apiKey: string;
  orgId: string;
  plan: PlanResponse | null;
}) {
  const [samples, setSamples] = useState<Sample[]>([]);
  const [pollError, setPollError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timeoutHandle: ReturnType<typeof setTimeout> | null = null;

    async function tick() {
      try {
        const resp = await getUsageByOrg(apiKey);
        const row = resp.orgs.find((o) => o.org_id === orgId);
        if (cancelled) return;
        if (!row) {
          // No forks recorded yet — the counter is lazy and only
          // appears on first fork. Render an empty sparkline; not an
          // error.
          setSamples((prev) => appendSample(prev, { t: Date.now(), total: 0 }));
        } else {
          setSamples((prev) => appendSample(prev, { t: Date.now(), total: row.fork_count }));
          setPollError(null);
        }
      } catch (err) {
        if (cancelled) return;
        // Show a stable status/code only. `ApiError.message` can carry
        // raw response text (proxy HTML, upstream 5xx bodies) that we
        // shouldn't paint straight into the UI.
        setPollError(describePollError(err));
      } finally {
        // Self-schedule the next tick so a slow response never
        // overlaps with the next one — `setInterval` fires on wall
        // clock regardless of in-flight work and can queue calls under
        // tab-throttling / slow network. `setTimeout` chained in
        // `finally` bounds concurrency to at most one poll at a time.
        if (!cancelled) {
          timeoutHandle = setTimeout(tick, POLL_MS);
        }
      }
    }

    tick(); // eager first sample so the UI populates immediately
    return () => {
      cancelled = true;
      if (timeoutHandle !== null) {
        clearTimeout(timeoutHandle);
      }
    };
  }, [apiKey, orgId]);

  const rates = deriveRates(samples);
  const currentRate = rates.length > 0 ? rates[rates.length - 1] : 0;
  const capRps = plan?.plan?.rps ?? null;
  const utilization = capRps ? Math.min(1, currentRate / capRps) : null;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-baseline justify-between gap-4">
        <div>
          <div className="text-3xl font-semibold">
            {currentRate.toFixed(2)}
            <span className="ml-1 text-base font-normal text-gray-500">
              forks/s
            </span>
          </div>
          <div className="text-sm text-gray-500">
            {capRps
              ? `${(utilization! * 100).toFixed(0)}% of ${capRps.toLocaleString()} forks/s plan cap`
              : "no active plan — showing rate only"}
          </div>
        </div>
        <div className="text-xs text-gray-400">
          Updates every {POLL_MS / 1000} s · window {WINDOW * POLL_MS / 60_000} min
        </div>
      </div>

      <Sparkline rates={rates} capRps={capRps} />

      {utilization !== null && (
        <div className="h-2 w-full overflow-hidden rounded bg-gray-200 dark:bg-gray-800">
          <div
            className={`h-full transition-all ${
              utilization > 0.9
                ? "bg-red-500"
                : utilization > 0.75
                  ? "bg-amber-500"
                  : "bg-brand-500"
            }`}
            style={{ width: `${utilization * 100}%` }}
          />
        </div>
      )}

      {pollError && (
        <p className="text-xs text-amber-600 dark:text-amber-400">
          Live rate paused — {pollError}
        </p>
      )}
    </div>
  );
}

/** Render a poll failure as a small, stable string. Deliberately does
 *  NOT interpolate `ApiError.message`, which can carry arbitrary body
 *  text (proxy HTML, upstream 5xx dumps) via the `request()` fallback.
 *  Special-cases 401 as the common key-rotation signal so the user
 *  gets an actionable hint. */
function describePollError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.status === 401) {
      return "session expired — sign in again";
    }
    return `HTTP ${err.status} (${err.code})`;
  }
  return "network error";
}

/** Append `next` to the rolling window, capping length at `WINDOW`. */
function appendSample(prev: Sample[], next: Sample): Sample[] {
  const merged = [...prev, next];
  return merged.length <= WINDOW ? merged : merged.slice(merged.length - WINDOW);
}

/** Diff consecutive cumulative counts into per-interval forks/second.
 *  Never negative (a restart can zero the counter) and never emits the
 *  final undefined-partner sample. */
function deriveRates(samples: Sample[]): number[] {
  const out: number[] = [];
  for (let i = 1; i < samples.length; i++) {
    const a = samples[i - 1]!;
    const b = samples[i]!;
    const dt = (b.t - a.t) / 1000;
    if (dt <= 0) continue;
    const dv = Math.max(0, b.total - a.total);
    out.push(dv / dt);
  }
  return out;
}

/**
 * Renders the rate series as an inline SVG polyline. Uses a fixed
 * viewBox so the parent controls sizing (Tailwind width). Empty /
 * flat series still draws a baseline so the tile doesn't visually
 * collapse.
 */
function Sparkline({
  rates,
  capRps,
}: {
  rates: number[];
  capRps: number | null;
}) {
  const width = 600;
  const height = 80;
  // Baseline scale: max of (observed max, plan cap) so the cap line
  // stays inside the frame even during idle periods.
  const dataMax = rates.length > 0 ? Math.max(...rates) : 0;
  const scaleMax = Math.max(dataMax, capRps ?? 0, 1);
  const stepX = rates.length > 1 ? width / (rates.length - 1) : 0;
  const points = rates
    .map((r, i) => {
      const x = i * stepX;
      const y = height - (r / scaleMax) * height;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  const capY = capRps ? height - (capRps / scaleMax) * height : null;

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      className="h-20 w-full"
      preserveAspectRatio="none"
      role="img"
      aria-label={`Fork rate sparkline, ${rates.length} samples`}
    >
      {capY !== null && (
        <line
          x1={0}
          y1={capY}
          x2={width}
          y2={capY}
          stroke="currentColor"
          strokeDasharray="4 4"
          strokeWidth={1}
          className="text-amber-500"
        />
      )}
      {rates.length > 1 ? (
        <polyline
          points={points}
          fill="none"
          strokeWidth={2}
          className="stroke-brand-500"
        />
      ) : (
        <line
          x1={0}
          y1={height - 1}
          x2={width}
          y2={height - 1}
          strokeWidth={1}
          className="stroke-gray-300 dark:stroke-gray-700"
        />
      )}
    </svg>
  );
}

function QuickStart({ apiKey }: { apiKey: string }) {
  const masked = apiKey.length > 12
    ? apiKey.slice(0, 8) + "…" + apiKey.slice(-4)
    : apiKey;
  return (
    <div className="space-y-4">
      <p className="text-sm text-gray-600 dark:text-gray-400">
        Run this in your terminal — the SDK is on PyPI.
      </p>
      <pre className="overflow-x-auto rounded-md bg-gray-900 p-4 text-sm text-gray-100">
{`pip install nanovm

python -c "
from nanovm import Client
c = Client(api_key='${masked}')
print(c.execute_python('print(1+1)'))
"`}
      </pre>
      <p className="text-xs text-gray-500 dark:text-gray-400">
        The key above is masked; use your real one in the shell.
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Onboarding checklist
// ---------------------------------------------------------------------------

/**
 * Post-signup "get started" checklist. Shows above the dashboard
 * tiles until every step is checked OR the user dismisses it.
 *
 * State model (all localStorage):
 * - Per-step `done` bit, keyed by step id — flipped by clicking the
 *   checkbox OR (for the "run first code" step) auto-detected from
 *   `usage.fork_count >= 1` on every mount.
 * - A separate `dismissed` bit that hides the widget outright, in
 *   case the user knows what they're doing and wants the tiles
 *   uncluttered.
 *
 * The auto-detected steps update whenever fresh usage lands from
 * `/v1/usage`, so a user who runs a fork via the SDK (not the
 * playground) still gets credit.
 */
const CHECKLIST_STORAGE_KEY = "nanovm.onboarding.v1";

interface ChecklistState {
  done: Record<string, boolean>;
  dismissed: boolean;
}

interface ChecklistStep {
  id: string;
  label: string;
  description: string;
  action:
    | { kind: "link"; href: string; cta: string }
    | { kind: "copy"; cta: string; payload: string };
  /** When true, this step auto-completes from live signals rather
   *  than a click. */
  autoDetected?: boolean;
}

function loadChecklistState(): ChecklistState {
  if (typeof window === "undefined") {
    return { done: {}, dismissed: false };
  }
  try {
    const raw = window.localStorage.getItem(CHECKLIST_STORAGE_KEY);
    if (!raw) return { done: {}, dismissed: false };
    const parsed = JSON.parse(raw) as Partial<ChecklistState>;
    return {
      done: parsed.done && typeof parsed.done === "object" ? parsed.done : {},
      dismissed: parsed.dismissed === true,
    };
  } catch {
    return { done: {}, dismissed: false };
  }
}

function persistChecklistState(state: ChecklistState) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(CHECKLIST_STORAGE_KEY, JSON.stringify(state));
  } catch {
    // localStorage full or blocked (private-window Safari, iframe
    // sandbox); silent — the state just doesn't persist this
    // session.
  }
}

function OnboardingChecklist({
  apiKey,
  usage,
}: {
  apiKey: string;
  usage: UsageResponseDto | null;
}) {
  const [state, setState] = useState<ChecklistState>({ done: {}, dismissed: false });
  const [ready, setReady] = useState(false);

  // Hydrate from localStorage on mount so a returning user sees their
  // prior progress. `ready` gates the initial render so SSR/CSR
  // don't clash on the tick marks.
  useEffect(() => {
    setState(loadChecklistState());
    setReady(true);
  }, []);

  // Auto-detect the "run first code" step from usage.fork_count. This
  // means a user who ran their first fork via the SDK (not the
  // playground) still gets credit here — the whole point is
  // celebrating first-success regardless of surface.
  useEffect(() => {
    if (!ready) return;
    if (usage && usage.fork_count >= 1 && !state.done["run-first"]) {
      const next = {
        ...state,
        done: { ...state.done, "run-first": true },
      };
      setState(next);
      persistChecklistState(next);
    }
  }, [ready, usage, state]);

  const steps: ChecklistStep[] = useMemo(
    () => [
      {
        id: "copy-key",
        label: "Copy your API key",
        description:
          "You'll paste it into the SDK or CLI as the `Authorization: Bearer` header.",
        action: { kind: "copy", cta: "Copy key", payload: apiKey },
      },
      {
        id: "run-first",
        label: "Run your first Python call",
        description:
          "Open the in-browser Playground and hit Run — every call is a real ~12 ms KVM fork.",
        action: { kind: "link", href: "/dashboard/playground", cta: "Open Playground" },
        autoDetected: true,
      },
      {
        id: "browse-marketplace",
        label: "Browse pre-built snapshots",
        description:
          "The marketplace ships ready-to-fork Python, Node, and shell images so you don't build one yourself.",
        action: { kind: "link", href: "/marketplace", cta: "Open Marketplace" },
      },
      {
        id: "review-billing",
        label: "See your plan and billing",
        description:
          "Free tier is 5 forks/sec + 10K forks/month. Upgrade from the billing portal when you outgrow it.",
        action: { kind: "link", href: "/pricing", cta: "See pricing" },
      },
    ],
    [apiKey],
  );

  function markDone(id: string) {
    const next = { ...state, done: { ...state.done, [id]: true } };
    setState(next);
    persistChecklistState(next);
  }

  function dismiss() {
    const next = { ...state, dismissed: true };
    setState(next);
    persistChecklistState(next);
  }

  if (!ready || state.dismissed) return null;
  const remaining = steps.filter((s) => !state.done[s.id]).length;
  const allDone = remaining === 0;

  return (
    <section className="mb-6 rounded-lg border border-brand-500/40 bg-brand-50/50 p-6 dark:border-brand-500/40 dark:bg-brand-500/10">
      <div className="mb-4 flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold">
            {allDone
              ? "🎉 You're all set."
              : `Get started — ${steps.length - remaining} of ${steps.length} done`}
          </h2>
          <p className="mt-1 text-sm text-gray-700 dark:text-gray-300">
            {allDone
              ? "Nice work. Hide this to reclaim the space."
              : "Take these four one-click steps to hit your first successful sandbox call."}
          </p>
        </div>
        <button
          onClick={dismiss}
          className="text-xs text-gray-500 hover:text-gray-700 dark:hover:text-gray-300"
        >
          {allDone ? "Hide" : "Hide checklist"}
        </button>
      </div>

      <ol className="space-y-3">
        {steps.map((step, i) => (
          <ChecklistRow
            key={step.id}
            index={i + 1}
            step={step}
            done={!!state.done[step.id]}
            onMarkDone={() => markDone(step.id)}
          />
        ))}
      </ol>
    </section>
  );
}

function ChecklistRow({
  index,
  step,
  done,
  onMarkDone,
}: {
  index: number;
  step: ChecklistStep;
  done: boolean;
  onMarkDone: () => void;
}) {
  const [copyState, setCopyState] = useState<"idle" | "copied" | "failed">("idle");

  function handleCopy(payload: string) {
    const clip = typeof navigator !== "undefined" ? navigator.clipboard : null;
    if (!clip) {
      setCopyState("failed");
      return;
    }
    clip.writeText(payload).then(
      () => {
        setCopyState("copied");
        onMarkDone();
      },
      () => setCopyState("failed"),
    );
  }

  return (
    <li className="flex items-start gap-3">
      <span
        className={`mt-0.5 flex h-6 w-6 flex-shrink-0 items-center justify-center rounded-full text-xs font-medium ${
          done
            ? "bg-brand-500 text-white"
            : "border border-gray-300 bg-white text-gray-600 dark:border-gray-700 dark:bg-gray-900 dark:text-gray-400"
        }`}
        aria-hidden
      >
        {done ? "✓" : index}
      </span>
      <div className="min-w-0 flex-1">
        <div className={`text-sm font-medium ${done ? "line-through opacity-70" : ""}`}>
          {step.label}
          {step.autoDetected && !done && (
            <span
              className="ml-2 rounded bg-gray-200 px-1.5 py-0.5 text-xs font-normal text-gray-600 dark:bg-gray-800 dark:text-gray-400"
              title="Auto-detected from your fork count"
            >
              auto
            </span>
          )}
        </div>
        <div className="mt-1 text-xs text-gray-600 dark:text-gray-400">
          {step.description}
        </div>
        {!done && (
          <div className="mt-2">
            {step.action.kind === "link" ? (
              <Link
                href={step.action.href}
                onClick={onMarkDone}
                className="inline-block rounded-md bg-brand-500 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-600"
              >
                {step.action.cta} →
              </Link>
            ) : (
              <button
                onClick={() => handleCopy((step.action as { payload: string }).payload)}
                className="rounded-md bg-brand-500 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-600"
              >
                {copyState === "copied"
                  ? "Copied ✓"
                  : copyState === "failed"
                    ? "Couldn't copy — select the key from a code block"
                    : step.action.cta}
              </button>
            )}
          </div>
        )}
      </div>
    </li>
  );
}
