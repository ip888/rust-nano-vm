"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";

import {
  ApiError,
  destroyVm,
  listVms,
  type VmListEntry,
  type VmListResponse,
} from "@/lib/api";
import { clearSession, getSession, type Session } from "@/lib/session";

/**
 * `/dashboard/vms` — list the caller org's VMs, destroy them individually.
 *
 * Cursor-paginated on `next`. First render fetches page 1 (50 rows);
 * "Load more" appends the next page until the server stops returning
 * a cursor. Destroy is guarded by a `confirm()` dialog and refreshes
 * the current view on success.
 *
 * A 401 anywhere → clear the session and bounce to /login. Every
 * other error surfaces the server envelope from `ApiError.message` in
 * a top-of-page banner.
 */
export default function VmsPage() {
  const router = useRouter();
  const [session, setSess] = useState<Session | null>(null);
  const [vms, setVms] = useState<VmListEntry[] | null>(null);
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
        const resp: VmListResponse = await listVms(apiKey, { limit: 50 });
        setVms(resp.vms);
        setNextCursor(resp.next ?? null);
      } catch (err) {
        handleAuthOrShow(err, "Couldn't load your VMs.");
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
      const resp = await listVms(session.apiKey, {
        limit: 50,
        after: nextCursor,
      });
      setVms((prev) => [...(prev ?? []), ...resp.vms]);
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
      !window.confirm(`Destroy VM ${id}? This is irreversible.`)
    ) {
      return;
    }
    setDestroyingId(id);
    setError(null);
    try {
      await destroyVm(session.apiKey, id);
      // Optimistically remove from the current view — cheaper than a
      // full re-fetch, and consistent with `/dashboard/keys`.
      setVms((prev) => prev?.filter((v) => v.id !== id) ?? null);
    } catch (err) {
      handleAuthOrShow(err, `Couldn't destroy VM ${id}.`);
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
          <h1 className="text-2xl font-semibold">VMs</h1>
          <p className="text-sm text-gray-500">
            Signed in as <span className="font-mono">{session.org}</span>. Every
            VM below belongs to this org.
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
        {vms === null ? (
          <div className="p-6 text-sm text-gray-500">Loading…</div>
        ) : vms.length === 0 ? (
          <EmptyState />
        ) : (
          <ul className="divide-y divide-gray-100 dark:divide-gray-800">
            {vms.map((v) => (
              <VmRow
                key={v.id}
                vm={v}
                destroying={destroyingId === v.id}
                onDestroy={() => destroy(v.id)}
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

      <p className="mt-6 text-xs text-gray-500">
        Destroying a VM releases its memory + storage immediately. Its
        snapshots stay — destroy those separately from{" "}
        <Link href="/dashboard/snapshots" className="hover:text-brand-600">
          Snapshots
        </Link>
        .
      </p>
    </main>
  );
}

function VmRow({
  vm,
  destroying,
  onDestroy,
}: {
  vm: VmListEntry;
  destroying: boolean;
  onDestroy: () => void;
}) {
  return (
    <li className="flex items-center justify-between gap-4 p-4">
      <div className="min-w-0">
        <div className="flex items-center gap-3">
          <span className="font-mono text-sm">{vm.display}</span>
          <StateBadge state={vm.state} />
        </div>
        <div className="mt-1 text-xs text-gray-500">
          id <span className="font-mono">{vm.id}</span>
          {vm.vcpus !== undefined && (
            <>
              {" · "}
              {vm.vcpus} vCPU
            </>
          )}
          {vm.memory_mib !== undefined && (
            <>
              {" · "}
              {vm.memory_mib} MiB
            </>
          )}
          {vm.snapshot_dir && (
            <>
              {" · from "}
              <span className="font-mono">{vm.snapshot_dir}</span>
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

function StateBadge({ state }: { state: VmListEntry["state"] }) {
  const styles: Record<VmListEntry["state"], string> = {
    running:
      "bg-green-100 text-green-800 dark:bg-green-950 dark:text-green-200",
    stopped: "bg-gray-100 text-gray-700 dark:bg-gray-800 dark:text-gray-300",
    created:
      "bg-blue-100 text-blue-800 dark:bg-blue-950 dark:text-blue-200",
  };
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium ${styles[state]}`}
    >
      <span
        className="h-1.5 w-1.5 rounded-full bg-current opacity-70"
        aria-hidden
      />
      {state}
    </span>
  );
}

function EmptyState() {
  return (
    <div className="space-y-3 p-8 text-sm text-gray-500">
      <p>No VMs yet.</p>
      <p>
        Fork a snapshot from the{" "}
        <Link href="/marketplace" className="text-brand-600 hover:underline">
          marketplace
        </Link>{" "}
        to get started, or drive the API directly with the{" "}
        <a
          href="https://pypi.org/project/nanovm/"
          className="text-brand-600 hover:underline"
        >
          Python SDK
        </a>
        .
      </p>
    </div>
  );
}
