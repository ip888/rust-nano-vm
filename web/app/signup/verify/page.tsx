"use client";

import Link from "next/link";
import { useSearchParams, useRouter } from "next/navigation";
import { Suspense, useEffect, useState } from "react";

import { ApiError, verifySignup } from "@/lib/api";
import { setSession } from "@/lib/session";

type CopyState = "idle" | "copied" | "failed";

/**
 * Signup step 2: user clicked the magic link. The token is in
 * `?token=…`; we POST to `/v1/signup/verify` and receive the fresh
 * API key. The key is shown ONCE — subsequent visits to the same URL
 * fail (the token is consumed-on-read server-side).
 */
export default function VerifyPage() {
  return (
    // useSearchParams needs a Suspense boundary in the App Router.
    <Suspense fallback={<Loading />}>
      <VerifyInner />
    </Suspense>
  );
}

function VerifyInner() {
  const params = useSearchParams();
  const router = useRouter();
  const token = params?.get("token") ?? "";
  const [state, setState] = useState<
    | { kind: "loading" }
    | { kind: "success"; org: string; apiKey: string }
    | { kind: "error"; message: string }
  >(() => (token ? { kind: "loading" } : { kind: "error", message: "Missing token — the magic link didn't include one." }));
  const [copyState, setCopyState] = useState<CopyState>("idle");

  useEffect(() => {
    if (state.kind !== "loading") return;
    (async () => {
      try {
        const resp = await verifySignup(token);
        setSession({ apiKey: resp.api_key, org: resp.org });
        setState({ kind: "success", org: resp.org, apiKey: resp.api_key });
      } catch (err) {
        const message =
          err instanceof ApiError
            ? err.message
            : "Verification failed — try requesting a fresh link.";
        setState({ kind: "error", message });
      }
    })();
  }, [state.kind, token]);

  if (state.kind === "loading") {
    return <Loading />;
  }

  if (state.kind === "error") {
    return (
      <Wrapper title="Verification failed">
        <p className="text-red-600 dark:text-red-400">{state.message}</p>
        <div className="mt-6 flex gap-3">
          <Link
            href="/signup"
            className="rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600"
          >
            Request a fresh link
          </Link>
          <Link
            href="/"
            className="rounded-md border border-gray-300 px-4 py-2 hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            Home
          </Link>
        </div>
      </Wrapper>
    );
  }

  return (
    <Wrapper title={`Welcome, ${state.org}`}>
      <p className="mb-4 text-gray-700 dark:text-gray-300">
        This is your API key. <strong>Copy it now</strong> — for security,
        we never show it again. If you lose it, mint a new one from{" "}
        <span className="font-mono text-sm">POST /v1/keys</span> or the
        dashboard.
      </p>
      <div className="mb-4 overflow-x-auto rounded-md bg-gray-900 p-4">
        <code className="whitespace-pre-wrap break-all font-mono text-sm text-gray-100">
          {state.apiKey}
        </code>
      </div>
      <button
        onClick={() => {
          // `navigator.clipboard` is undefined on insecure origins
          // (http://…, unless it's localhost) and writeText can
          // reject if the tab lacks focus / permission. Handle both
          // so the console stays clean and the user gets a visible
          // failure state.
          const clip = navigator.clipboard;
          if (!clip) {
            setCopyState("failed");
            return;
          }
          clip.writeText(state.apiKey).then(
            () => setCopyState("copied"),
            () => setCopyState("failed"),
          );
        }}
        className="mb-6 rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
      >
        {copyState === "copied"
          ? "Copied ✓"
          : copyState === "failed"
            ? "Couldn't copy — select and copy manually"
            : "Copy to clipboard"}
      </button>
      <button
        onClick={() => router.push("/dashboard")}
        className="w-full rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600"
      >
        Continue to dashboard →
      </button>
    </Wrapper>
  );
}

function Loading() {
  return (
    <Wrapper title="Verifying…">
      <p className="text-gray-500 dark:text-gray-400">
        Checking your magic link.
      </p>
    </Wrapper>
  );
}

function Wrapper({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <main className="mx-auto flex min-h-screen max-w-lg items-center px-6">
      <div className="w-full rounded-lg border border-gray-200 bg-white p-8 shadow-sm dark:border-gray-800 dark:bg-gray-900">
        <h1 className="mb-4 text-2xl font-semibold">{title}</h1>
        {children}
      </div>
    </main>
  );
}
