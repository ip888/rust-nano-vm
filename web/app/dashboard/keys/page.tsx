"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";

import {
  ApiError,
  issueKey,
  listKeys,
  revokeKey,
  type IssueKeyResponse,
  type KeyEntry,
} from "@/lib/api";
import { clearSession, getSession, type Session } from "@/lib/session";

/**
 * `/dashboard/keys` — list, mint, and revoke runtime API keys.
 *
 * The freshly-minted bearer is shown ONCE at the top of the page
 * (with copy-to-clipboard). Anything shorter than a hard reload
 * discards it — the server never returns it again. If the user
 * misses the copy window they mint another.
 *
 * Revoke is destructive; guarded by a confirm() dialog. After revocation
 * the key list is re-fetched; a 401 means the current bearer was just
 * revoked, so the session is cleared and the user is redirected to login.
 */
export default function KeysPage() {
  const router = useRouter();
  const [session, setSess] = useState<Session | null>(null);
  const [keys, setKeys] = useState<KeyEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [newlyIssued, setNewlyIssued] = useState<IssueKeyResponse | null>(null);
  const [minting, setMinting] = useState(false);
  const [revokingId, setRevokingId] = useState<string | null>(null);

  useEffect(() => {
    const s = getSession();
    if (!s) {
      router.replace("/login");
      return;
    }
    setSess(s);
    (async () => {
      try {
        const resp = await listKeys(s.apiKey);
        setKeys(resp.keys);
      } catch (err) {
        if (err instanceof ApiError && err.status === 401) {
          clearSession();
          router.replace("/login");
          return;
        }
        setError(
          err instanceof ApiError ? err.message : "Couldn't load your keys.",
        );
      }
    })();
  }, [router]);

  const currentKeyId: string | null = null;

  async function mint() {
    if (!session || minting) return;
    setMinting(true);
    setError(null);
    try {
      const resp = await issueKey(session.apiKey);
      setNewlyIssued(resp);
      const updated = await listKeys(session.apiKey);
      setKeys(updated.keys);
    } catch (err) {
      setError(
        err instanceof ApiError ? err.message : "Couldn't mint a new key.",
      );
    } finally {
      setMinting(false);
    }
  }

  async function revoke(id: string) {
    if (!session || revokingId) return;
    if (
      !window.confirm(`Revoke key ${id}? Anything using it will break.`)
    ) {
      return;
    }
    setRevokingId(id);
    setError(null);
    try {
      await revokeKey(session.apiKey, id);
      // Re-fetch the key list. A 401 here means the key we just revoked
      // was our own bearer — clear the session and bounce to login.
      const resp = await listKeys(session.apiKey);
      setKeys(resp.keys);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        clearSession();
        router.replace("/login");
        return;
      }
      setError(
        err instanceof ApiError ? err.message : "Couldn't revoke that key.",
      );
    } finally {
      setRevokingId(null);
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
          <h1 className="text-2xl font-semibold">API keys</h1>
          <p className="text-sm text-gray-500">
            Signed in as <span className="font-mono">{session.org}</span>. All
            keys below belong to this org.
          </p>
        </div>
        <button
          onClick={mint}
          disabled={minting}
          className="rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600 disabled:opacity-60"
        >
          {minting ? "Minting…" : "New key"}
        </button>
      </header>

      {error && (
        <div className="mb-6 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          {error}
        </div>
      )}

      {newlyIssued && (
        <NewKeyCallout
          issued={newlyIssued}
          onDismiss={() => setNewlyIssued(null)}
        />
      )}

      <section className="rounded-lg border border-gray-200 bg-white shadow-sm dark:border-gray-800 dark:bg-gray-900">
        {keys === null ? (
          <div className="p-6 text-sm text-gray-500">Loading…</div>
        ) : keys.length === 0 ? (
          <div className="p-6 text-sm text-gray-500">
            No keys yet. Click <strong>New key</strong> to mint one.
          </div>
        ) : (
          <ul className="divide-y divide-gray-100 dark:divide-gray-800">
            {keys.map((k) => (
              <li
                key={k.id}
                className="flex items-center justify-between gap-4 p-4"
              >
                <div className="min-w-0">
                  <div className="truncate font-mono text-sm">
                    {k.id}
                    {k.id === currentKeyId && (
                      <span className="ml-2 rounded bg-brand-50 px-2 py-0.5 text-xs text-brand-700 dark:bg-brand-500/10 dark:text-brand-500">
                        current
                      </span>
                    )}
                  </div>
                  <div className="text-xs text-gray-500">
                    created {k.created_at}
                  </div>
                </div>
                <button
                  onClick={() => revoke(k.id)}
                  disabled={revokingId === k.id}
                  className="rounded-md border border-gray-300 px-3 py-1 text-sm text-red-600 hover:bg-red-50 disabled:opacity-60 dark:border-gray-700 dark:hover:bg-red-950"
                >
                  {revokingId === k.id ? "Revoking…" : "Revoke"}
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      <p className="mt-6 text-xs text-gray-500">
        Revoking a key immediately invalidates every session using it. Anything
        with the old key hard-coded (CI, agent bots) will start returning 401
        until you rotate.
      </p>
    </main>
  );
}

function NewKeyCallout({
  issued,
  onDismiss,
}: {
  issued: IssueKeyResponse;
  onDismiss: () => void;
}) {
  const [copyState, setCopyState] = useState<"idle" | "copied" | "failed">(
    "idle",
  );
  return (
    <div className="mb-6 rounded-lg border border-brand-500/40 bg-brand-50/50 p-6 dark:border-brand-500/40 dark:bg-brand-500/10">
      <div className="mb-2 flex items-start justify-between gap-4">
        <div>
          <h2 className="mb-1 text-lg font-semibold">
            New key — copy it now
          </h2>
          <p className="text-sm text-gray-700 dark:text-gray-300">
            This is the only time we'll show it. If you lose it, mint another.
          </p>
        </div>
        <button
          onClick={onDismiss}
          className="text-sm text-gray-500 hover:text-gray-700"
          aria-label="Dismiss"
        >
          ✕
        </button>
      </div>
      <pre className="mt-3 overflow-x-auto rounded-md bg-gray-900 p-4">
        <code className="whitespace-pre-wrap break-all font-mono text-sm text-gray-100">
          {issued.token}
        </code>
      </pre>
      <button
        onClick={() => {
          const clip = navigator.clipboard;
          if (!clip) {
            setCopyState("failed");
            return;
          }
          clip.writeText(issued.token).then(
            () => setCopyState("copied"),
            () => setCopyState("failed"),
          );
        }}
        className="mt-3 rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-white dark:border-gray-700 dark:hover:bg-gray-800"
      >
        {copyState === "copied"
          ? "Copied ✓"
          : copyState === "failed"
            ? "Couldn't copy — select and copy manually"
            : "Copy to clipboard"}
      </button>
    </div>
  );
}

