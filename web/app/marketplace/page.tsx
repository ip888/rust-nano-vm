"use client";

import Link from "next/link";
import { useEffect, useMemo, useState } from "react";

import {
  ApiError,
  forkMarketplaceSnapshot,
  listMarketplaceSnapshots,
  type ForkResponse,
  type MarketplaceSnapshot,
} from "@/lib/api";
import { getSession, type Session } from "@/lib/session";

/**
 * `/marketplace` — public browse of the snapshot catalogue with a
 * per-card "Fork this" button.
 *
 * The list itself is unauthenticated (matches the server-side posture:
 * `GET /v1/marketplace/snapshots` needs no bearer). The fork button
 * needs an API key — signed-out visitors see a `Sign in to fork` link
 * on each card instead. This keeps the browse-before-signup path fast
 * while still surfacing the real value prop the moment a visitor has
 * a key.
 */
export default function MarketplacePage() {
  const [state, setState] = useState<
    | { kind: "loading" }
    | { kind: "ok"; snapshots: MarketplaceSnapshot[] }
    | { kind: "error"; message: string }
  >({ kind: "loading" });
  const [query, setQuery] = useState("");
  const [session, setSessionState] = useState<Session | null>(null);

  useEffect(() => {
    setSessionState(getSession());
    (async () => {
      try {
        const resp = await listMarketplaceSnapshots();
        setState({ kind: "ok", snapshots: resp.snapshots });
      } catch (err) {
        setState({
          kind: "error",
          message:
            err instanceof ApiError
              ? err.message
              : "Couldn't reach the API. Is the control plane running?",
        });
      }
    })();
  }, []);

  const filtered = useMemo(() => {
    if (state.kind !== "ok") return [];
    const q = query.trim().toLowerCase();
    if (!q) return state.snapshots;
    return state.snapshots.filter((s) => {
      return (
        s.name.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q) ||
        s.labels.some((l) => l.toLowerCase().includes(q))
      );
    });
  }, [state, query]);

  return (
    <main className="mx-auto max-w-5xl px-6 py-12">
      <header className="mb-8 flex flex-wrap items-center justify-between gap-4">
        <div>
          <Link
            href="/"
            className="mb-2 inline-flex items-center gap-2 text-sm text-gray-500 hover:text-brand-600"
          >
            ← Home
          </Link>
          <h1 className="text-2xl font-semibold">Snapshot marketplace</h1>
          <p className="mt-1 max-w-2xl text-sm text-gray-500 dark:text-gray-400">
            Pre-warmed sandbox environments. Fork any of them in ~12 ms —
            skip the &quot;write a Dockerfile, build an image, wait for the
            registry&quot; loop.
          </p>
        </div>
        <div className="flex gap-3">
          {session ? (
            <Link
              href="/dashboard"
              className="rounded-md bg-brand-500 px-4 py-2 text-sm text-white hover:bg-brand-600"
            >
              Dashboard
            </Link>
          ) : (
            <>
              <Link
                href="/signup"
                className="rounded-md bg-brand-500 px-4 py-2 text-sm text-white hover:bg-brand-600"
              >
                Sign up to fork
              </Link>
              <Link
                href="/login"
                className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
              >
                Log in
              </Link>
            </>
          )}
        </div>
      </header>

      {state.kind === "ok" && state.snapshots.length > 0 && (
        <input
          type="search"
          placeholder="Filter by name, description, or label…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          className="mb-6 w-full rounded-md border border-gray-300 px-3 py-2 focus:border-brand-500 focus:outline-none focus:ring-2 focus:ring-brand-500 dark:border-gray-700 dark:bg-gray-900"
        />
      )}

      {state.kind === "loading" && (
        <div className="rounded-lg border border-gray-200 p-6 text-sm text-gray-500 dark:border-gray-800">
          Loading…
        </div>
      )}

      {state.kind === "error" && (
        <div className="rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          {state.message}
        </div>
      )}

      {state.kind === "ok" && state.snapshots.length === 0 && (
        <EmptyState />
      )}

      {state.kind === "ok" && state.snapshots.length > 0 && (
        <ul className="grid gap-4 md:grid-cols-2">
          {filtered.map((s) => (
            <SnapshotCard
              key={s.name}
              snapshot={s}
              session={session}
            />
          ))}
          {filtered.length === 0 && (
            <li className="col-span-full rounded-lg border border-gray-200 p-6 text-sm text-gray-500 dark:border-gray-800">
              No snapshots match “{query}”.
            </li>
          )}
        </ul>
      )}
    </main>
  );
}

function EmptyState() {
  return (
    <div className="rounded-lg border border-dashed border-gray-300 p-8 text-center dark:border-gray-700">
      <h2 className="mb-2 text-lg font-semibold">No snapshots configured</h2>
      <p className="mx-auto max-w-md text-sm text-gray-500 dark:text-gray-400">
        The operator hasn&apos;t wired{" "}
        <code className="rounded bg-gray-100 px-1 py-0.5 font-mono text-xs dark:bg-gray-800">
          NANOVM_MARKETPLACE_CONFIG
        </code>{" "}
        yet. See{" "}
        <a
          href="https://github.com/ip888/rust-nano-vm/tree/main/deploy/marketplace"
          className="text-brand-600 hover:underline"
        >
          deploy/marketplace/README.md
        </a>{" "}
        for the operator guide.
      </p>
    </div>
  );
}

/**
 * Per-card fork state. Kept card-local so a failure on one entry
 * doesn't taint the others, and so an "in-flight" spinner only
 * disables the button the user clicked.
 */
type ForkState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "success"; result: ForkResponse }
  | { kind: "error"; code: string; message: string };

function SnapshotCard({
  snapshot,
  session,
}: {
  snapshot: MarketplaceSnapshot;
  session: Session | null;
}) {
  const [fork, setFork] = useState<ForkState>({ kind: "idle" });
  const notForkable = !snapshot.snapshot_url;

  async function onFork() {
    if (!session) return;
    setFork({ kind: "loading" });
    try {
      const result = await forkMarketplaceSnapshot(session.apiKey, snapshot.name);
      setFork({ kind: "success", result });
    } catch (err) {
      if (err instanceof ApiError) {
        setFork({ kind: "error", code: err.code, message: err.message });
      } else {
        setFork({
          kind: "error",
          code: "network",
          message: "Couldn't reach the API.",
        });
      }
    }
  }

  return (
    <li className="rounded-lg border border-gray-200 bg-white p-6 shadow-sm transition-shadow hover:shadow-md dark:border-gray-800 dark:bg-gray-900">
      <div className="mb-2 flex items-start justify-between gap-2">
        <h3 className="font-mono text-lg font-semibold">{snapshot.name}</h3>
        <span className="whitespace-nowrap text-xs text-gray-500">
          {formatBytes(snapshot.size_bytes)}
        </span>
      </div>
      <p className="mb-4 text-sm text-gray-600 dark:text-gray-400">
        {snapshot.description}
      </p>
      {snapshot.labels.length > 0 && (
        <div className="mb-4 flex flex-wrap gap-1">
          {snapshot.labels.map((l) => (
            <span
              key={l}
              className="rounded bg-gray-100 px-2 py-0.5 text-xs text-gray-700 dark:bg-gray-800 dark:text-gray-300"
            >
              {l}
            </span>
          ))}
        </div>
      )}

      {/* Fork result surface */}
      {fork.kind === "success" && (
        <div className="mb-4 rounded-md border border-green-200 bg-green-50 p-3 text-xs text-green-800 dark:border-green-900 dark:bg-green-950 dark:text-green-200">
          <p className="font-semibold">
            Forked in {fork.result.fork_ms} ms
          </p>
          <p className="mt-1 font-mono">
            VM #{fork.result.vm.id} ({fork.result.vm.state})
          </p>
          <p className="mt-1 text-green-700 dark:text-green-300">
            You&apos;ve forked {fork.result.fork_count} time
            {fork.result.fork_count === 1 ? "" : "s"} this month.
          </p>
        </div>
      )}
      {fork.kind === "error" && (
        <div className="mb-4 rounded-md border border-red-200 bg-red-50 p-3 text-xs text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200">
          <p className="font-semibold">Fork failed ({fork.code})</p>
          <p className="mt-1">{fork.message}</p>
        </div>
      )}

      <div className="flex items-center justify-between gap-3 text-xs text-gray-500">
        <span>by {snapshot.maintainer}</span>
        {renderForkButton({ session, notForkable, fork, onFork })}
      </div>
    </li>
  );
}

/**
 * Extracted so the card's JSX stays flat. Four states drive the label
 * and the click behavior:
 *   - no session       → link to /login (browse-before-signup path)
 *   - no snapshot_url  → disabled, tooltip explains 501 up front
 *   - loading          → disabled spinner
 *   - idle / done      → primary button, re-fork allowed
 */
function renderForkButton({
  session,
  notForkable,
  fork,
  onFork,
}: {
  session: Session | null;
  notForkable: boolean;
  fork: ForkState;
  onFork: () => void;
}) {
  if (!session) {
    return (
      <Link
        href="/login"
        className="rounded-md border border-gray-300 px-3 py-1.5 text-xs font-medium hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
      >
        Sign in to fork
      </Link>
    );
  }
  if (notForkable) {
    // A `<button disabled title=…>` doesn't reliably surface its
    // tooltip: browsers gate `title` on hover of *focusable* elements
    // and disabled controls aren't focusable. Use a non-disabled
    // `<span>` styled like a chip so the reason is actually
    // discoverable (mouse hover fires; keyboard users get the
    // aria-label read by screen readers).
    return (
      <span
        role="note"
        tabIndex={0}
        title="This entry has no snapshot_url yet — publisher listed for discovery only."
        aria-label="Not forkable: this entry has no snapshot_url yet — publisher listed for discovery only."
        className="cursor-help rounded-md bg-gray-200 px-3 py-1.5 text-xs font-medium text-gray-500 dark:bg-gray-800 dark:text-gray-500"
      >
        Not forkable
      </span>
    );
  }
  const loading = fork.kind === "loading";
  return (
    <button
      type="button"
      onClick={onFork}
      disabled={loading}
      className="rounded-md bg-brand-500 px-3 py-1.5 text-xs font-medium text-white hover:bg-brand-600 disabled:cursor-not-allowed disabled:bg-brand-300"
    >
      {loading ? "Forking…" : fork.kind === "success" ? "Fork again" : "Fork this"}
    </button>
  );
}

/** Human-friendly byte size — no external formatter dep. */
function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "?";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  const digits = v >= 100 || i === 0 ? 0 : v >= 10 ? 1 : 2;
  return `${v.toFixed(digits)} ${units[i]}`;
}
