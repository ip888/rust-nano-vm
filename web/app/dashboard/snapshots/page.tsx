"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";

import {
  ApiError,
  destroySnapshot,
  listSnapshots,
  type SnapshotListEntry,
  type SnapshotListResponse,
} from "@/lib/api";
import { clearSession, getSession, type Session } from "@/lib/session";

/**
 * `/dashboard/snapshots` — list the caller org's snapshots, destroy
 * individually.
 *
 * Same cursor-paginated + optimistic-remove pattern as
 * `/dashboard/vms`. Destroying a snapshot drops its warm-pool
 * children server-side (see `WarmPool::drain`), so a delete here does
 * not orphan running forks.
 */
export default function SnapshotsPage() {
  const router = useRouter();
  const [session, setSess] = useState<Session | null>(null);
  const [snapshots, setSnapshots] = useState<SnapshotListEntry[] | null>(null);
  const [nextCursor, setNextCursor] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"loading" | "loadingMore" | null>(null);
  const [destroyingId, setDestroyingId] = useState<number | null>(null);

  const handleAuthOrShow = useCallback(
    (err: unknown, fallback: string) => {
      if (err instanceof ApiError && err.status === 401) {
        clearSession();
        router.replace("/login");
        return;
      }
      setError(err instanceof ApiError ? err.message : fallback);
    },
    [router],
  );

  const load = useCallback(
    async (apiKey: string) => {
      setBusy("loading");
      setError(null);
      try {
        const resp: SnapshotListResponse = await listSnapshots(apiKey, {
          limit: 50,
        });
        setSnapshots(resp.snapshots);
        setNextCursor(resp.next ?? null);
      } catch (err) {
        handleAuthOrShow(err, "Couldn't load your snapshots.");
      } finally {
        setBusy(null);
      }
    },
    [handleAuthOrShow],
  );

  useEffect(() => {
    const s = getSession();
    if (!s) {
      router.replace("/login");
      return;
    }
    setSess(s);
    load(s.apiKey);
  }, [router, load]);

  async function loadMore() {
    if (!session || nextCursor === null || busy) return;
    setBusy("loadingMore");
    setError(null);
    try {
      const resp = await listSnapshots(session.apiKey, {
        limit: 50,
        after: nextCursor,
      });
      setSnapshots((prev) => [...(prev ?? []), ...resp.snapshots]);
      setNextCursor(resp.next ?? null);
    } catch (err) {
      handleAuthOrShow(err, "Couldn't load the next page.");
    } finally {
      setBusy(null);
    }
  }

  async function destroy(id: number) {
    if (!session || destroyingId !== null) return;
    if (
      !window.confirm(
        `Destroy snapshot ${id}? Any warm-pool children are drained; ` +
          `already-running forks keep running until you destroy them.`,
      )
    ) {
      return;
    }
    setDestroyingId(id);
    setError(null);
    try {
      await destroySnapshot(session.apiKey, id);
      setSnapshots((prev) => prev?.filter((s) => s.id !== id) ?? null);
    } catch (err) {
      handleAuthOrShow(err, `Couldn't destroy snapshot ${id}.`);
    } finally {
      setDestroyingId(null);
    }
  }

  if (!session) return null;

  return (
    <main className="mx-auto max-w-5xl px-6 py-12">
      <header className="mb-8 flex items-center justify-between">
        <div>
          <Link
            href="/dashboard"
            className="mb-2 inline-flex items-center gap-2 text-sm text-gray-500 hover:text-brand-600"
          >
            ← Dashboard
          </Link>
          <h1 className="text-2xl font-semibold">Snapshots</h1>
          <p className="text-sm text-gray-500">
            Signed in as <span className="font-mono">{session.org}</span>.
            Snapshots captured by this org.
          </p>
        </div>
        <button
          onClick={() => session && load(session.apiKey)}
          disabled={busy !== null}
          className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 disabled:opacity-60 dark:border-gray-700 dark:hover:bg-gray-800"
        >
          {busy === "loading" ? "Refreshing…" : "Refresh"}
        </button>
      </header>

      {error && (
        <div className="mb-6 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          {error}
        </div>
      )}

      <section className="rounded-lg border border-gray-200 bg-white shadow-sm dark:border-gray-800 dark:bg-gray-900">
        {snapshots === null ? (
          <div className="p-6 text-sm text-gray-500">Loading…</div>
        ) : snapshots.length === 0 ? (
          <EmptyState />
        ) : (
          <ul className="divide-y divide-gray-100 dark:divide-gray-800">
            {snapshots.map((s) => (
              <SnapshotRow
                key={s.id}
                snap={s}
                destroying={destroyingId === s.id}
                onDestroy={() => destroy(s.id)}
              />
            ))}
          </ul>
        )}
      </section>

      {nextCursor !== null && (
        <div className="mt-4 flex justify-center">
          <button
            onClick={loadMore}
            disabled={busy !== null}
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 disabled:opacity-60 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            {busy === "loadingMore" ? "Loading…" : "Load more"}
          </button>
        </div>
      )}
    </main>
  );
}

function SnapshotRow({
  snap,
  destroying,
  onDestroy,
}: {
  snap: SnapshotListEntry;
  destroying: boolean;
  onDestroy: () => void;
}) {
  return (
    <li className="flex items-center justify-between gap-4 p-4">
      <div className="min-w-0">
        <div className="font-mono text-sm">{snap.display}</div>
        <div className="mt-1 text-xs text-gray-500">
          id <span className="font-mono">{snap.id}</span>
          {snap.vcpu_count !== undefined && (
            <>
              {" · "}
              {snap.vcpu_count} vCPU captured
            </>
          )}
          {snap.memory_bytes !== undefined && (
            <>
              {" · "}
              {formatBytes(snap.memory_bytes)}
            </>
          )}
        </div>
      </div>
      <button
        onClick={onDestroy}
        disabled={destroying}
        className="rounded-md border border-gray-300 px-3 py-1 text-sm text-red-600 hover:bg-red-50 disabled:opacity-60 dark:border-gray-700 dark:hover:bg-red-950"
      >
        {destroying ? "Destroying…" : "Destroy"}
      </button>
    </li>
  );
}

function EmptyState() {
  return (
    <div className="space-y-3 p-8 text-sm text-gray-500">
      <p>No snapshots yet.</p>
      <p>
        Capture one via <code>POST /v1/vms/:id/snapshot</code> against a
        running VM, or fork one from the{" "}
        <Link href="/marketplace" className="text-brand-600 hover:underline">
          marketplace
        </Link>
        .
      </p>
    </div>
  );
}

function formatBytes(n: number): string {
  // Snapshot memory is guest RAM — always MiB-to-GiB scale, so the
  // simple divide-and-fixed formatter is enough. No need for a full
  // SI/IEC ladder.
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(0)} MiB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
}
