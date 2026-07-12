"use client";

import Link from "next/link";
import { useState } from "react";

import { ApiError, requestSignup } from "@/lib/api";

/**
 * Signup step 1: enter email + org, POST to /v1/signup/request. The
 * server sends a magic link; we show a "check your email" confirmation.
 *
 * The response body is deliberately opaque server-side (same message
 * regardless of outcome — see `signup_request` in the Rust module) so
 * this page renders the same success screen even if the address was
 * already taken. That's the point: no address enumeration.
 */
export default function SignupPage() {
  const [email, setEmail] = useState("");
  const [org, setOrg] = useState("");
  const [status, setStatus] = useState<
    { kind: "idle" }
    | { kind: "loading" }
    | { kind: "sent"; message: string }
    | { kind: "error"; message: string }
  >({ kind: "idle" });

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (status.kind === "loading") return;
    setStatus({ kind: "loading" });
    try {
      const resp = await requestSignup({ email, org });
      setStatus({ kind: "sent", message: resp.message });
    } catch (err) {
      const message =
        err instanceof ApiError
          ? err.message
          : "Something went wrong — try again in a moment.";
      setStatus({ kind: "error", message });
    }
  }

  if (status.kind === "sent") {
    return (
      <CenteredCard title="Check your email">
        <p className="text-gray-700 dark:text-gray-300">{status.message}</p>
        <p className="mt-4 text-sm text-gray-500 dark:text-gray-400">
          The link expires in 15 minutes.
        </p>
      </CenteredCard>
    );
  }

  return (
    <CenteredCard title="Start free">
      <form onSubmit={submit} className="space-y-4">
        <Field
          label="Email"
          type="email"
          required
          value={email}
          onChange={setEmail}
          placeholder="you@example.com"
        />
        <Field
          label="Org name"
          required
          value={org}
          onChange={setOrg}
          placeholder="Acme Inc"
        />
        <button
          type="submit"
          disabled={status.kind === "loading"}
          className="w-full rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600 disabled:opacity-60"
        >
          {status.kind === "loading" ? "Sending link…" : "Send magic link"}
        </button>
        {status.kind === "error" && (
          <p className="text-sm text-red-600 dark:text-red-400">
            {status.message}
          </p>
        )}
      </form>
      <p className="mt-6 text-sm text-gray-500 dark:text-gray-400">
        Already have an API key?{" "}
        <Link href="/login" className="text-brand-600 hover:underline">
          Log in
        </Link>
        .
      </p>
    </CenteredCard>
  );
}

function CenteredCard({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
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
          <h1 className="mb-6 text-2xl font-semibold">{title}</h1>
          {children}
        </div>
      </div>
    </main>
  );
}

function Field({
  label,
  type = "text",
  value,
  onChange,
  placeholder,
  required,
}: {
  label: string;
  type?: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  required?: boolean;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-sm font-medium">{label}</span>
      <input
        type={type}
        required={required}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="w-full rounded-md border border-gray-300 px-3 py-2 focus:border-brand-500 focus:outline-none focus:ring-2 focus:ring-brand-500 dark:border-gray-700 dark:bg-gray-900"
      />
    </label>
  );
}
