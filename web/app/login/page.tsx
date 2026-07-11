"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useState } from "react";

import { ApiError, getPlan } from "@/lib/api";
import { setSession } from "@/lib/session";

/**
 * "Log in" here means "paste your existing API key". The dashboard is
 * fully client-side; the key IS the session (see lib/session.ts). We
 * do one round-trip against `/v1/billing/plan` to confirm the key is
 * live before persisting.
 *
 * The org name is inferred from the key: the auth layer's issued
 * tokens are formatted `<org>:<secret>`; we take the prefix. Non-standard
 * keys (older, or minted with a different format) get `"?"` as the org
 * placeholder — the dashboard's plan query still works.
 */
export default function LoginPage() {
  const router = useRouter();
  const [apiKey, setApiKey] = useState("");
  const [status, setStatus] = useState<
    { kind: "idle" } | { kind: "loading" } | { kind: "error"; message: string }
  >({ kind: "idle" });

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (status.kind === "loading") return;
    const key = apiKey.trim();
    if (!key) return;
    setStatus({ kind: "loading" });
    try {
      // Round-trip against /v1/billing/plan; a 401 tells us the key's
      // dead before we cache it and confuse the user with a broken
      // dashboard.
      await getPlan(key);
      const orgFromKey = key.includes(":") ? key.split(":")[0] : "?";
      setSession({ apiKey: key, org: orgFromKey });
      router.push("/dashboard");
    } catch (err) {
      const message =
        err instanceof ApiError
          ? err.message
          : "Couldn't reach the API — is the server running?";
      setStatus({ kind: "error", message });
    }
  }

  return (
    <main className="mx-auto flex min-h-screen max-w-md items-center px-6">
      <div className="w-full">
        <Link
          href="/"
          className="mb-8 inline-flex items-center gap-2 text-sm text-gray-500 hover:text-brand-600"
        >
          ← Home
        </Link>
        <div className="rounded-lg border border-gray-200 bg-white p-8 shadow-sm dark:border-gray-800 dark:bg-gray-900">
          <h1 className="mb-6 text-2xl font-semibold">Log in</h1>
          <form onSubmit={submit} className="space-y-4">
            <label className="block">
              <span className="mb-1 block text-sm font-medium">API key</span>
              <input
                type="password"
                autoFocus
                required
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder="acme:… (from your signup email)"
                className="w-full rounded-md border border-gray-300 px-3 py-2 font-mono text-sm focus:border-brand-500 focus:outline-none focus:ring-2 focus:ring-brand-500 dark:border-gray-700 dark:bg-gray-900"
              />
            </label>
            <button
              type="submit"
              disabled={status.kind === "loading"}
              className="w-full rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600 disabled:opacity-60"
            >
              {status.kind === "loading" ? "Signing in…" : "Sign in"}
            </button>
            {status.kind === "error" && (
              <p className="text-sm text-red-600 dark:text-red-400">
                {status.message}
              </p>
            )}
          </form>
          <p className="mt-6 text-sm text-gray-500 dark:text-gray-400">
            Don&apos;t have a key yet?{" "}
            <Link href="/signup" className="text-brand-600 hover:underline">
              Start free
            </Link>
            .
          </p>
        </div>
      </div>
    </main>
  );
}
