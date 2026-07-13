"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";

import {
  ApiError,
  getBillingPortalUrl,
  getPlan,
  getUsage,
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
        <div className="flex items-center gap-3">
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

      <div className="grid gap-6 md:grid-cols-2">
        <Tile title="Plan">
          {plan ? <PlanBody plan={plan} onOpenPortal={openBillingPortal} /> : (
            <Skeleton />
          )}
        </Tile>

        <Tile title="Usage this session">
          {usage ? <UsageBody usage={usage} /> : <Skeleton />}
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
