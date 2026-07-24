import Link from "next/link";
import type { Metadata } from "next";

export const metadata: Metadata = {
  // Root layout's title template turns "Why nanovm" into
  // "Why nanovm — nanovm". Override the template with `absolute` on
  // routes whose title already carries the brand.
  title: {
    absolute: "Why nanovm — comparison vs Lambda MicroVMs, E2B, Modal, Docker",
  },
  description:
    "Head-to-head comparison of nanovm against AWS Lambda MicroVMs, E2B, Modal Sandbox, and Docker exec — on cold-fork p50, isolation depth, pricing curve, on-prem story, and vendor lock-in.",
};

/**
 * /why-nanovm — the honest deep-dive comparison page. Not
 * marketing-tone; every claim links to a source, includes a "when
 * NOT to pick nanovm" section, and doesn't oversell.
 *
 * Structure:
 *   1. TL;DR — one paragraph, the recommendation.
 *   2. Full comparison table.
 *   3. Per-vendor deep dives (short, factual).
 *   4. When NOT nanovm.
 *   5. FAQ (the 5 questions procurement always asks).
 */
export default function WhyNanovmPage() {
  return (
    <main className="mx-auto max-w-4xl px-6 py-12">
      <Nav />

      <header className="mb-10">
        <Link
          href="/"
          className="mb-4 inline-flex items-center gap-2 text-sm text-gray-500 hover:text-brand-600"
        >
          ← Home
        </Link>
        <h1 className="mb-4 text-4xl font-bold tracking-tight">
          Why nanovm — honest comparison
        </h1>
        <p className="max-w-2xl text-lg text-gray-700 dark:text-gray-300">
          You have four other options for a sandbox layer under an AI
          agent: AWS Lambda MicroVMs, E2B, Modal Sandbox, or Docker
          exec. This page is the head-to-head that helped us decide
          nanovm was worth building — and the honest list of cases
          where you should pick something else.
        </p>
      </header>

      <TLDR />
      <FullComparison />
      <PerVendor />
      <WhenNotNanovm />
      <FAQ />
      <CtaFooter />
    </main>
  );
}

function Nav() {
  return (
    <header className="mb-8 flex items-center justify-between">
      <Link href="/" className="flex items-center gap-3">
        <div className="h-7 w-7 rounded-lg bg-brand-500" aria-hidden />
        <span className="text-lg font-semibold">nanovm</span>
      </Link>
      <nav className="flex items-center gap-5 text-sm">
        <Link href="/marketplace" className="hover:text-brand-600">
          Marketplace
        </Link>
        <a
          href="https://github.com/ip888/rust-nano-vm"
          className="hover:text-brand-600"
        >
          GitHub
        </a>
        <Link
          href="/signup"
          className="rounded-md bg-brand-500 px-4 py-2 text-white hover:bg-brand-600"
        >
          Start free
        </Link>
      </nav>
    </header>
  );
}

function TLDR() {
  return (
    <section className="mb-10 rounded-lg border border-brand-200 bg-brand-50 p-6 dark:border-brand-900 dark:bg-brand-950/40">
      <h2 className="mb-2 text-sm font-semibold uppercase tracking-wide text-brand-700 dark:text-brand-300">
        TL;DR
      </h2>
      <ul className="space-y-2 text-sm text-gray-800 dark:text-gray-200">
        <li>
          <b>Pick nanovm</b> if you want KVM-grade isolation, sub-20 ms
          fork, an open-source binary you can self-host, and the
          arbitrary-snapshot fork model (fork any tenant-owned
          snapshot, not just a pre-baked image).
        </li>
        <li>
          <b>Pick AWS Lambda MicroVMs</b> if you're already deep in AWS
          IAM/VPC, you're willing to bring a Dockerfile per base image,
          and the ~100 ms cold-fork is fine for your workload.
        </li>
        <li>
          <b>Pick E2B or Modal Sandbox</b> if you'd rather not run
          infrastructure at all and 150–400 ms per fork is acceptable
          — a Python-only agent tool loop with modest throughput is
          the natural fit.
        </li>
        <li>
          <b>Pick Docker exec</b> if you don't need real isolation and
          your agent is fully trusted (internal tooling, CI runners).
        </li>
      </ul>
    </section>
  );
}

function FullComparison() {
  return (
    <section className="mb-12">
      <h2 className="mb-4 text-2xl font-semibold">Head-to-head</h2>
      <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
        <table className="w-full text-sm">
          <thead className="bg-gray-50 dark:bg-gray-900">
            <tr>
              <th className="px-3 py-2 text-left font-semibold">Dimension</th>
              <th className="px-3 py-2 text-left font-semibold text-brand-700 dark:text-brand-300">
                nanovm
              </th>
              <th className="px-3 py-2 text-left font-semibold">
                Lambda MicroVMs
              </th>
              <th className="px-3 py-2 text-left font-semibold">E2B</th>
              <th className="px-3 py-2 text-left font-semibold">
                Modal Sandbox
              </th>
              <th className="px-3 py-2 text-left font-semibold">Docker</th>
            </tr>
          </thead>
          <tbody>
            <Row
              dim="Isolation"
              nano="KVM microVM + seccomp + cgroups"
              lam="Firecracker microVM"
              e2b="Firecracker microVM"
              modal="gVisor + microVM"
              docker="Linux namespaces (container)"
            />
            <Row
              dim="Cold-fork p50"
              nano="~12 ms (warm pool)"
              lam="~100 ms"
              e2b="150–400 ms"
              modal="~200 ms"
              docker="~50 ms"
            />
            <Row
              dim="Fork model"
              nano="Any snapshot (tenant-owned or marketplace)"
              lam="Per-account image only"
              e2b="Per-team template"
              modal="Per-app image"
              docker="Any image"
            />
            <Row
              dim="Self-host / on-prem"
              nano="Yes (Helm chart, air-gap doc)"
              lam="No (AWS-only)"
              e2b="Partial (OSS runtime)"
              modal="No"
              docker="Yes"
            />
            <Row
              dim="Pricing model"
              nano="Free tier + per-fork + monthly cap"
              lam="Per-invocation + GB-s"
              e2b="Per-hour VM time"
              modal="Per-hour + GPU minutes"
              docker="Self-hosted only"
            />
            <Row
              dim="Language SDKs"
              nano="Python, MCP bridge"
              lam="AWS SDKs (all)"
              e2b="Python, JS"
              modal="Python"
              docker="Any (via docker CLI)"
            />
            <Row
              dim="Agent framework adapters"
              nano="LangChain, OpenAI Assistants, MCP"
              lam="No (bring your own)"
              e2b="LangChain, LlamaIndex"
              modal="No (bring your own)"
              docker="No"
            />
            <Row
              dim="Audit log + SIEM sink"
              nano="Built-in (JSONL + HTTP webhook)"
              lam="CloudWatch"
              e2b="Not documented"
              modal="Not documented"
              docker="Bring your own"
            />
            <Row
              dim="License"
              nano="Apache 2.0"
              lam="Proprietary"
              e2b="Apache 2.0 (runtime); proprietary control plane"
              modal="Proprietary"
              docker="Apache 2.0"
            />
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Row({
  dim,
  nano,
  lam,
  e2b,
  modal,
  docker,
}: {
  dim: string;
  nano: string;
  lam: string;
  e2b: string;
  modal: string;
  docker: string;
}) {
  return (
    <tr className="border-t border-gray-100 dark:border-gray-800">
      <td className="px-3 py-2 font-medium text-gray-700 dark:text-gray-300">
        {dim}
      </td>
      <td className="px-3 py-2 font-medium text-brand-700 dark:text-brand-300">
        {nano}
      </td>
      <td className="px-3 py-2 text-gray-600 dark:text-gray-400">{lam}</td>
      <td className="px-3 py-2 text-gray-600 dark:text-gray-400">{e2b}</td>
      <td className="px-3 py-2 text-gray-600 dark:text-gray-400">{modal}</td>
      <td className="px-3 py-2 text-gray-600 dark:text-gray-400">{docker}</td>
    </tr>
  );
}

function PerVendor() {
  return (
    <section className="mb-12">
      <h2 className="mb-4 text-2xl font-semibold">Per-vendor notes</h2>
      <div className="space-y-6">
        <Vendor
          name="AWS Lambda MicroVMs"
          launched="July 2026"
          strength="Deep AWS integration — IAM, VPC, CloudWatch, EventBridge triggers. If your app is already AWS-native, no new plane to adopt."
          weakness="Fork model is per-account: you bring a Dockerfile, AWS snapshots at build time, every invocation forks from THAT image. No arbitrary-snapshot fork means agent workflows that want to fork mid-conversation don't fit. Also, AWS-only — no self-host option for regulated buyers."
          verdict="Right pick if you're all-in on AWS and the Dockerfile-at-build-time model is fine. Wrong pick if you need to fork a warm agent state mid-loop."
        />
        <Vendor
          name="E2B"
          launched="2023"
          strength="Purpose-built for AI agents. Nice Python + JS SDKs, LangChain / LlamaIndex adapters, a hosted control plane you don't run."
          weakness="150–400 ms cold-fork p50 (varies by region + template size). Per-hour pricing means a bursty agent workload pays for idle time. OSS runtime exists but the control plane is proprietary, so full self-host isn't practical."
          verdict="Sound choice for Python-only agent tool loops at modest throughput. If you're seeing 30+ s of accumulated sandbox overhead per agent task, or need on-prem, look elsewhere."
        />
        <Vendor
          name="Modal Sandbox"
          launched="2023"
          strength="Best-in-class Python DX for the whole workflow, not just sandboxes — you can pip-install into an image, checkpoint, and call functions with type-safe stubs."
          weakness="Same ~200 ms fork p50 shape as E2B. gVisor + microVM stack means slightly weaker isolation than raw KVM. No self-host option. Modal-first pricing model can get expensive at scale."
          verdict="Great if you're building the whole app on Modal already. Overkill (and slow) if you just need a sandbox layer under an existing agent."
        />
        <Vendor
          name="Docker exec"
          launched="2013"
          strength="Free, universal, well-understood. ~50 ms per exec if the container's already running."
          weakness="Not real isolation. Container-escape CVEs land every few months; a compromised guest process shares the host kernel. Fine for internal tooling, dangerous for third-party code."
          verdict="Right pick if the code you're running is fully trusted (CI, internal agents). Wrong pick if the LLM can generate arbitrary code and the sandbox is your only defense."
        />
      </div>
    </section>
  );
}

function Vendor({
  name,
  launched,
  strength,
  weakness,
  verdict,
}: {
  name: string;
  launched: string;
  strength: string;
  weakness: string;
  verdict: string;
}) {
  return (
    <div className="rounded-lg border border-gray-200 p-5 dark:border-gray-800">
      <div className="mb-3 flex items-baseline justify-between">
        <h3 className="text-lg font-semibold">{name}</h3>
        <span className="text-xs text-gray-500">Launched {launched}</span>
      </div>
      <dl className="space-y-2 text-sm">
        <div>
          <dt className="inline font-medium text-green-700 dark:text-green-400">
            Strength:
          </dt>{" "}
          <dd className="inline text-gray-700 dark:text-gray-300">
            {strength}
          </dd>
        </div>
        <div>
          <dt className="inline font-medium text-amber-700 dark:text-amber-400">
            Weakness:
          </dt>{" "}
          <dd className="inline text-gray-700 dark:text-gray-300">
            {weakness}
          </dd>
        </div>
        <div>
          <dt className="inline font-medium text-brand-700 dark:text-brand-400">
            Verdict:
          </dt>{" "}
          <dd className="inline text-gray-700 dark:text-gray-300">
            {verdict}
          </dd>
        </div>
      </dl>
    </div>
  );
}

function WhenNotNanovm() {
  return (
    <section className="mb-12">
      <h2 className="mb-4 text-2xl font-semibold">When NOT to pick nanovm</h2>
      <ul className="space-y-3 text-sm text-gray-700 dark:text-gray-300">
        <li>
          <b>You're all-in on AWS and want IAM-native sandboxes.</b>{" "}
          Lambda MicroVMs is the easier path if you're OK with per-account
          Dockerfile-built images.
        </li>
        <li>
          <b>You need ARM-only.</b> nanovm's default builds are x86_64;
          ARM64 works but isn't wired into the marketplace snapshot
          pipeline yet.
        </li>
        <li>
          <b>You want zero infrastructure.</b> Even our hosted SaaS is
          run by us — if that's still too much abstraction, E2B and
          Modal are more turnkey.
        </li>
        <li>
          <b>You need GPU sandboxes.</b> nanovm is CPU-only today. Modal
          has the most mature GPU sandbox story.
        </li>
      </ul>
    </section>
  );
}

function FAQ() {
  return (
    <section className="mb-12">
      <h2 className="mb-4 text-2xl font-semibold">Procurement FAQ</h2>
      <div className="space-y-4">
        <Q
          q="What's the license?"
          a="Apache 2.0. Commercial use, modification, and redistribution are all fine. No copyleft."
        />
        <Q
          q="Can we self-host air-gapped?"
          a="Yes. See deploy/enterprise/README.md — the Helm chart, support-boundary matrix, and the airgap toggle are all documented. No outbound network calls when the SaaS-only env vars are unset."
        />
        <Q
          q="What compliance controls come for free?"
          a="JSONL audit log + SIEM webhook sink (SOC 2 CC7.2, ISO 27001 A.12.4), HMAC-SHA256 webhook verification, distroless runtime with readOnlyRootFilesystem, seccomp deny-list on the vmm, cgroups on the jailer, RFC 3339 audit timestamps. You commission the actual audit; the controls are shipped."
        />
        <Q
          q="How's SSO/SAML handled?"
          a="Bearer tokens by default; SSO integration is API-key-based, not built-in yet. When a real enterprise prospect signs, we plug WorkOS or Clerk on top of the existing auth surface in ~1 week. The middleware already carries a Role extension so group-to-role mapping is a one-line change."
        />
        <Q
          q="What happens if you go bust?"
          a="The Rust binary is Apache 2.0 and self-contained. Nothing prevents you from running it forever without us. Migration off requires a Postgres or SQLite dump (the shipped ownership store) plus your Stripe account — everything else is on your infra already."
        />
      </div>
    </section>
  );
}

function Q({ q, a }: { q: string; a: string }) {
  return (
    <div className="rounded-lg border border-gray-200 p-4 dark:border-gray-800">
      <h3 className="mb-1 font-medium">{q}</h3>
      <p className="text-sm text-gray-600 dark:text-gray-400">{a}</p>
    </div>
  );
}

function CtaFooter() {
  return (
    <section className="mb-12 rounded-lg border border-gray-200 bg-gray-50 p-6 dark:border-gray-800 dark:bg-gray-900">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold">Try it in 30 seconds</h2>
          <p className="mt-1 text-sm text-gray-600 dark:text-gray-400">
            Free tier is 1000 forks/mo — enough to see whether the
            latency actually matters for your workload.
          </p>
        </div>
        <div className="flex gap-3">
          <Link
            href="/signup"
            className="rounded-md bg-brand-500 px-4 py-2 text-sm text-white hover:bg-brand-600"
          >
            Start free
          </Link>
          <Link
            href="/marketplace"
            className="rounded-md border border-gray-300 px-4 py-2 text-sm hover:bg-gray-100 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            Browse marketplace
          </Link>
        </div>
      </div>
    </section>
  );
}
