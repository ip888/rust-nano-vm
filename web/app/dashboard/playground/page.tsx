"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  ApiError,
  destroyVm,
  execVm,
  forkMarketplaceSnapshot,
  listMarketplaceSnapshots,
  type ExecResponse,
  type MarketplaceSnapshot,
} from "@/lib/api";
import { clearSession, getSession, type Session } from "@/lib/session";

/**
 * `/dashboard/playground` — paste code, hit Run, see it execute in a
 * real KVM microVM in <1 s.
 *
 * The lifecycle:
 *
 *   marketplace fork → exec → destroy
 *
 * happens client-side in the caller's session. Every Run is a fresh
 * VM (no state carried across runs). We call the marketplace fork
 * endpoint rather than `/v1/sandbox/invoke` because the invoke
 * endpoint only accepts numeric `snapshot_id`s and a new signup has
 * none — the marketplace fork endpoint accepts entry names and
 * auto-adopts the underlying tarball into the caller's org on first
 * hit.
 *
 * Rate-limited by the caller's `NANOVM_FORK_RPS` plan cap; a 429
 * surfaces via the standard `ApiError` envelope with a hint to check
 * the pricing page or wait.
 *
 * On a fresh signup the user's first Run pulls the marketplace
 * tarball (a few seconds) and every subsequent Run is a ~12 ms
 * warm-pool pop. We show the wall-clock breakdown so the "second run
 * is instant" story is visible in the UI.
 */

type Language = "python" | "shell";

interface Preset {
  label: string;
  language: Language;
  code: string;
}

const PRESETS: Preset[] = [
  {
    label: "Hello world",
    language: "python",
    code: `print("Hello from a KVM microVM!")\nprint(f"1 + 1 = {1 + 1}")\n`,
  },
  {
    label: "Fibonacci",
    language: "python",
    code:
      `def fib(n):\n` +
      `    a, b = 0, 1\n` +
      `    for _ in range(n):\n` +
      `        a, b = b, a + b\n` +
      `    return a\n\n` +
      `print([fib(i) for i in range(15)])\n`,
  },
  {
    label: "HTTP request (urllib)",
    language: "python",
    code:
      `import urllib.request, json\n\n` +
      `with urllib.request.urlopen("https://httpbin.org/uuid", timeout=5) as r:\n` +
      `    body = json.load(r)\n` +
      `print(f"got a fresh uuid: {body['uuid']}")\n`,
  },
  {
    label: "Show kernel + host info",
    language: "shell",
    code: `uname -a\ncat /etc/os-release || true\nnproc\nfree -h\n`,
  },
  {
    label: "Write + read a file",
    language: "shell",
    code:
      `echo "hello from the sandbox" > /tmp/hi.txt\n` +
      `cat /tmp/hi.txt\n` +
      `ls -l /tmp/hi.txt\n`,
  },
];

interface RunOutcome {
  snapshot: string;
  exec: ExecResponse;
  forkMs: number;
  totalMs: number;
}

export default function PlaygroundPage() {
  const router = useRouter();
  const [session, setSess] = useState<Session | null>(null);
  const [snapshots, setSnapshots] = useState<MarketplaceSnapshot[] | null>(
    null,
  );
  const [snapshotName, setSnapshotName] = useState<string>("");
  const [language, setLanguage] = useState<Language>("python");
  const [code, setCode] = useState(PRESETS[0]!.code);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [upgradeUrl, setUpgradeUrl] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<RunOutcome | null>(null);

  const handleAuthOrShow = useCallback(
    (err: unknown, fallback: string) => {
      if (err instanceof ApiError && err.status === 401) {
        clearSession();
        router.replace("/login");
        return;
      }
      if (err instanceof ApiError && err.status === 402 && err.upgradeEndpoint) {
        setUpgradeUrl(err.upgradeEndpoint);
      } else {
        setUpgradeUrl(null);
      }
      setError(err instanceof ApiError ? err.message : fallback);
    },
    [router],
  );

  // Initial: auth guard + marketplace fetch.
  useEffect(() => {
    const s = getSession();
    if (!s) {
      router.replace("/login");
      return;
    }
    setSess(s);
    (async () => {
      try {
        const resp = await listMarketplaceSnapshots();
        setSnapshots(resp.snapshots);
        // Prefer a python entry as the initial pick so the default
        // preset (Python "Hello world") lines up.
        const pyDefault =
          resp.snapshots.find((s) => s.name.toLowerCase().includes("python")) ??
          resp.snapshots[0];
        if (pyDefault) setSnapshotName(pyDefault.name);
      } catch (err) {
        // Marketplace list is a *public* endpoint — a failure here is
        // almost always a network / server outage, not auth. Surface
        // it so the user knows the playground can't start.
        setError(
          err instanceof ApiError ? err.message : "Couldn't load the marketplace.",
        );
      }
    })();
  }, [router]);

  // Currently-selected snapshot's optional description + fork-ability.
  const selectedSnapshot = useMemo(
    () => snapshots?.find((s) => s.name === snapshotName) ?? null,
    [snapshots, snapshotName],
  );
  const canFork = selectedSnapshot?.snapshot_url != null;

  async function run() {
    if (!session || running || !snapshotName) return;
    setRunning(true);
    setError(null);
    setUpgradeUrl(null);
    setOutcome(null);
    const t0 = performance.now();
    let vmId: number | null = null;
    try {
      const fork = await forkMarketplaceSnapshot(session.apiKey, snapshotName);
      vmId = fork.vm.id;
      const exec = await execVm(session.apiKey, vmId, {
        program: language === "python" ? "python3" : "sh",
        args: language === "python" ? ["-c", code] : ["-c", code],
      });
      const totalMs = Math.round(performance.now() - t0);
      setOutcome({
        snapshot: snapshotName,
        exec,
        forkMs: fork.fork_ms,
        totalMs,
      });
    } catch (err) {
      handleAuthOrShow(err, "Run failed. See the browser console for details.");
    } finally {
      // Best-effort destroy on the returned VM so we don't leak one
      // per Run against the caller's quota. Failure here doesn't
      // block the user; the audit log records the leak on the
      // server side.
      if (vmId !== null) {
        destroyVm(session.apiKey, vmId).catch(() => undefined);
      }
      setRunning(false);
    }
  }

  function applyPreset(p: Preset) {
    setLanguage(p.language);
    setCode(p.code);
  }

  if (!session) return null;

  return (
    <main className="mx-auto max-w-6xl px-6 py-12">
      <header className="mb-8">
        <Link
          href="/dashboard"
          className="mb-2 inline-flex items-center gap-2 text-sm text-gray-500 hover:text-brand-600"
        >
          ← Dashboard
        </Link>
        <h1 className="text-2xl font-semibold">Playground</h1>
        <p className="text-sm text-gray-500">
          Paste Python or shell, hit <strong>Run</strong>, watch it execute in a
          fresh KVM microVM. Every run is a fresh fork — no state carries
          across.
        </p>
      </header>

      {error && (
        <div className="mb-6 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">
          <p>{error}</p>
          {upgradeUrl && (
            <p className="mt-2">
              <Link
                href="/pricing"
                className="font-medium text-red-800 underline hover:text-red-900 dark:text-red-200"
              >
                See pricing →
              </Link>
            </p>
          )}
        </div>
      )}

      <div className="grid gap-6 lg:grid-cols-[3fr_2fr]">
        <section className="space-y-4">
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex flex-1 flex-col text-sm">
              <span className="mb-1 text-gray-500">Snapshot</span>
              <select
                value={snapshotName}
                onChange={(e) => setSnapshotName(e.target.value)}
                disabled={snapshots === null || running}
                className="rounded-md border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-700 dark:bg-gray-900"
              >
                {snapshots === null && <option value="">Loading…</option>}
                {snapshots?.length === 0 && (
                  <option value="">(marketplace empty)</option>
                )}
                {snapshots?.map((s) => (
                  <option key={s.name} value={s.name} disabled={!s.snapshot_url}>
                    {s.name}
                    {!s.snapshot_url && " — browse only"}
                  </option>
                ))}
              </select>
            </label>
            <label className="flex flex-col text-sm">
              <span className="mb-1 text-gray-500">Language</span>
              <select
                value={language}
                onChange={(e) => setLanguage(e.target.value as Language)}
                disabled={running}
                className="rounded-md border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-700 dark:bg-gray-900"
              >
                <option value="python">Python 3</option>
                <option value="shell">Shell (sh -c)</option>
              </select>
            </label>
            <button
              onClick={run}
              disabled={running || !canFork || snapshotName === ""}
              className="rounded-md bg-brand-500 px-6 py-2 text-white hover:bg-brand-600 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {running ? "Running…" : "Run"}
            </button>
          </div>

          {selectedSnapshot && !selectedSnapshot.snapshot_url && (
            <p className="rounded-md border border-amber-200 bg-amber-50 p-3 text-xs text-amber-900 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
              This snapshot is browse-only (the marketplace entry has no
              tarball URL). Pick a different snapshot with an&nbsp;
              <code>snapshot_url</code>, or ask your operator to publish one.
            </p>
          )}

          <textarea
            value={code}
            onChange={(e) => setCode(e.target.value)}
            disabled={running}
            spellCheck={false}
            className="h-80 w-full resize-y rounded-md border border-gray-300 bg-gray-900 p-4 font-mono text-sm text-gray-100 dark:border-gray-700"
            placeholder="# Your code here"
          />

          <div className="flex flex-wrap items-center gap-2 text-xs">
            <span className="text-gray-500">Presets:</span>
            {PRESETS.map((p) => (
              <button
                key={p.label}
                onClick={() => applyPreset(p)}
                disabled={running}
                className="rounded-full border border-gray-300 px-3 py-1 hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
              >
                {p.label}
              </button>
            ))}
          </div>
        </section>

        <section className="space-y-3">
          <h2 className="text-sm font-medium uppercase tracking-wide text-gray-500">
            Output
          </h2>
          {outcome ? (
            <OutcomeView outcome={outcome} />
          ) : (
            <div className="rounded-md border border-dashed border-gray-300 p-6 text-sm text-gray-500 dark:border-gray-700">
              {running
                ? "Forking a fresh VM…"
                : "Run something to see stdout/stderr, exit code, and wall-clock timing."}
            </div>
          )}
        </section>
      </div>

      <p className="mt-8 text-xs text-gray-500">
        Every run forks a fresh VM from the selected snapshot, executes your
        code, and destroys the VM. Nothing persists across runs. See{" "}
        <Link href="/dashboard/snapshots" className="hover:text-brand-600">
          Snapshots
        </Link>{" "}
        for the snapshots the server captured on your behalf, and{" "}
        <Link href="/pricing" className="hover:text-brand-600">
          Pricing
        </Link>{" "}
        for the per-tier fork/sec caps that gate this endpoint.
      </p>
    </main>
  );
}

function OutcomeView({ outcome }: { outcome: RunOutcome }) {
  const { exec, forkMs, totalMs, snapshot } = outcome;
  const execMs = Math.max(0, totalMs - forkMs);
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap gap-2 text-xs">
        <Stat label="exit" value={exitLabel(exec)} />
        <Stat label="wall clock" value={`${totalMs} ms`} />
        <Stat label="fork" value={`${forkMs} ms`} />
        <Stat label="exec" value={`${execMs} ms`} />
        <Stat label="snapshot" value={snapshot} />
      </div>
      <OutputBlock label="stdout" content={exec.stdout} />
      {exec.stderr && <OutputBlock label="stderr" content={exec.stderr} />}
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <span className="rounded-full bg-gray-100 px-3 py-1 text-gray-700 dark:bg-gray-800 dark:text-gray-300">
      <span className="text-gray-500">{label}:</span>{" "}
      <span className="font-mono">{value}</span>
    </span>
  );
}

function exitLabel(exec: ExecResponse): string {
  if (exec.exit_code !== null) return String(exec.exit_code);
  if (exec.signal !== null) return `signal ${exec.signal}`;
  return "unknown";
}

function OutputBlock({ label, content }: { label: string; content: string }) {
  const isError = label === "stderr";
  return (
    <div>
      <div
        className={`mb-1 text-xs font-medium uppercase tracking-wide ${
          isError ? "text-red-500" : "text-gray-500"
        }`}
      >
        {label}
      </div>
      <pre className="max-h-64 overflow-auto rounded-md bg-gray-900 p-4 text-xs text-gray-100">
        <code>{content || <span className="opacity-50">(empty)</span>}</code>
      </pre>
    </div>
  );
}
