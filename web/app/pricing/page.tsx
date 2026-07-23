import Link from "next/link";
import type { Metadata } from "next";

/**
 * `/pricing` — public pricing page for self-serve conversion.
 *
 * The concrete numbers below are the operator's default pricing —
 * the same shape `NANOVM_PLAN_TIERS` accepts (`stripe_price_id=name:rps`).
 * When an operator re-brands nanovm-web for their own SaaS, they
 * override the numbers here and the matching `NANOVM_PLAN_TIERS`
 * entries on the control plane. The dashboard's `/v1/billing/plan`
 * endpoint then resolves the caller's subscription against those
 * price ids at runtime.
 *
 * Static (no client component) — every render is cache-friendly and
 * search-indexable. No API calls from this route.
 */

export const metadata: Metadata = {
  title: "Pricing — nanovm",
  description:
    "Simple, usage-based pricing for sub-second microVMs. Free tier for hobby " +
    "projects; Pro from $29/mo; Team from $199/mo; Enterprise with SSO / audit " +
    "sink / on-prem.",
};

interface Tier {
  name: string;
  price: string;
  priceCadence: string;
  tagline: string;
  cta: { label: string; href: string };
  featured?: boolean;
  bullets: string[];
  fine?: string;
}

const TIERS: Tier[] = [
  {
    name: "Free",
    price: "$0",
    priceCadence: "forever",
    tagline: "Hobby projects, evals, learning.",
    cta: { label: "Start free", href: "/signup" },
    bullets: [
      "5 forks/second sustained",
      "10,000 forks / month",
      "Community support (GitHub issues)",
      "1 GB snapshot storage",
      "Public marketplace snapshots",
      "No credit card required",
    ],
  },
  {
    name: "Pro",
    price: "$29",
    priceCadence: "/ month",
    tagline: "Solo developers shipping agents in production.",
    cta: { label: "Start Pro", href: "/signup" },
    featured: true,
    bullets: [
      "100 forks/second sustained",
      "Unlimited monthly forks",
      "Email support (business days)",
      "50 GB snapshot storage",
      "Private marketplace snapshots",
      "SLA: 99.5% control-plane uptime",
    ],
    fine: "First 14 days free — cancel anytime from the dashboard.",
  },
  {
    name: "Team",
    price: "$199",
    priceCadence: "/ month",
    tagline: "Small teams behind a shared control plane.",
    cta: { label: "Start Team", href: "/signup" },
    bullets: [
      "500 forks/second sustained",
      "Unlimited monthly forks",
      "Priority email + Slack support",
      "500 GB snapshot storage",
      "5 organization seats included",
      "SLA: 99.9% control-plane uptime",
      "Audit-log JSONL access",
    ],
    fine: "Additional seats $19/mo each.",
  },
  {
    name: "Enterprise",
    price: "Custom",
    priceCadence: "annual",
    tagline: "Regulated industries, on-prem, custom controls.",
    cta: { label: "Talk to us", href: "mailto:sales@nanovm.example.com" },
    bullets: [
      "Unlimited forks/second",
      "SSO (SAML / OIDC) + SCIM",
      "RBAC with custom roles",
      "SIEM webhook audit sink",
      "Air-gapped self-host",
      "On-prem or your VPC deploy",
      "Uptime SLA + dedicated support",
      "Contractual DPA + custom terms",
    ],
    fine: "Typical Enterprise contract lands in 2–4 weeks.",
  },
];

interface CompetitorRow {
  name: string;
  coldStart: string;
  costPer100Calls: string;
  isolation: string;
  selfHost: string;
  highlight?: boolean;
}

const COMPETITOR_ROWS: CompetitorRow[] = [
  {
    name: "nanovm (Pro)",
    coldStart: "~12 ms",
    costPer100Calls: "$0.00 (unlimited)",
    isolation: "KVM microVM",
    selfHost: "Yes",
    highlight: true,
  },
  {
    name: "E2B",
    coldStart: "150–400 ms",
    costPer100Calls: "$0.05–0.15",
    isolation: "Firecracker",
    selfHost: "Partial",
  },
  {
    name: "Modal Sandbox",
    coldStart: "~200 ms",
    costPer100Calls: "$0.02–0.10",
    isolation: "gVisor + microVM",
    selfHost: "No",
  },
  {
    name: "AWS Lambda MicroVMs",
    coldStart: "~100 ms*",
    costPer100Calls: "$0.02",
    isolation: "Firecracker",
    selfHost: "No",
  },
  {
    name: "Docker exec",
    coldStart: "~50 ms",
    costPer100Calls: "$0.00 (self-host)",
    isolation: "Namespaces",
    selfHost: "Yes",
  },
];

export default function PricingPage() {
  return (
    <main className="mx-auto max-w-6xl px-6 py-12">
      <SiteHeader />

      <section className="mb-14 text-center">
        <h1 className="mb-4 text-4xl font-bold tracking-tight md:text-5xl">
          Simple, usage-based pricing.
        </h1>
        <p className="mx-auto max-w-2xl text-lg text-gray-700 dark:text-gray-300">
          Every plan runs on the same ~12 ms fork engine. Pay for
          throughput and support, not for the sandbox itself. Self-hosting
          is always free (Apache 2.0).
        </p>
      </section>

      <section className="mb-16 grid gap-6 md:grid-cols-2 lg:grid-cols-4">
        {TIERS.map((t) => (
          <PricingCard key={t.name} tier={t} />
        ))}
      </section>

      <section className="mb-16">
        <h2 className="mb-4 text-2xl font-semibold">
          Cold-fork p50 and cost per 100 tool calls
        </h2>
        <p className="mb-6 max-w-2xl text-sm text-gray-600 dark:text-gray-400">
          Same workload (drop into a warm Python 3.12 sandbox, run{" "}
          <code className="rounded bg-gray-100 px-1.5 py-0.5 font-mono text-xs dark:bg-gray-800">
            print(1+1)
          </code>
          , close) across the sandbox layers an AI-agent stack picks
          between. Nanovm at Pro pricing is unlimited-per-month, so the
          per-100-calls number is a wash.
        </p>
        <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 dark:bg-gray-900">
              <tr>
                <th className="px-4 py-3 text-left font-semibold">Sandbox</th>
                <th className="px-4 py-3 text-right font-semibold">
                  Cold-fork p50
                </th>
                <th className="px-4 py-3 text-right font-semibold">
                  Cost / 100 calls
                </th>
                <th className="px-4 py-3 text-left font-semibold">Isolation</th>
                <th className="px-4 py-3 text-left font-semibold">Self-host</th>
              </tr>
            </thead>
            <tbody>
              {COMPETITOR_ROWS.map((r) => (
                <tr
                  key={r.name}
                  className={`border-t border-gray-100 dark:border-gray-800 ${
                    r.highlight
                      ? "bg-brand-50 font-medium dark:bg-brand-950/40"
                      : ""
                  }`}
                >
                  <td className="px-4 py-3">{r.name}</td>
                  <td className="px-4 py-3 text-right font-mono text-xs">
                    {r.coldStart}
                  </td>
                  <td className="px-4 py-3 text-right font-mono text-xs">
                    {r.costPer100Calls}
                  </td>
                  <td className="px-4 py-3">{r.isolation}</td>
                  <td className="px-4 py-3">{r.selfHost}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className="mt-3 text-xs text-gray-500 dark:text-gray-400">
          * AWS Lambda MicroVMs cost excludes egress + underlying invoke
          fees. Cost numbers approximate provider-published rates as of
          publication; verify directly for procurement.
        </p>
      </section>

      <section className="mb-16">
        <h2 className="mb-6 text-2xl font-semibold">FAQ</h2>
        <div className="space-y-6">
          <Faq q="What counts as a fork?">
            One fork = one <code>POST /v1/snapshots/:id/fork</code> or{" "}
            <code>/v1/marketplace/snapshots/:name/fork</code> that
            successfully returns a VM handle. Fork attempts that hit the
            rate limit (429) or fail before restore don&apos;t count.
          </Faq>
          <Faq q="Do I need a credit card to start?">
            No. The Free tier requires only a signup email — no credit
            card, no expiring trial. Upgrade whenever you outgrow the
            5 forks/second cap.
          </Faq>
          <Faq q="Can I self-host?">
            Yes. The entire stack is dual-licensed Apache 2.0 OR MIT. The
            hosted plans buy throughput, support, and an SLA on top of
            infrastructure you don&apos;t have to run. See the{" "}
            <Link href="/why-nanovm" className="text-brand-600 hover:underline">
              deep dive
            </Link>{" "}
            for the trade-offs.
          </Faq>
          <Faq q="What happens when I hit my plan's fork/sec cap?">
            The API returns <code>429 Too Many Requests</code> with a{" "}
            <code>Retry-After</code> header. The Python and TypeScript
            SDKs raise a typed <code>RateLimited</code> exception so
            callers can back off. Upgrade or wait — no forks are dropped
            silently.
          </Faq>
          <Faq q="How is billing metered?">
            Every plan is a fixed monthly Stripe subscription for the
            included forks/sec headroom. Above-tier usage isn&apos;t
            billed today — the fork cap enforces the limit instead of a
            surprise invoice. Usage-based add-ons are a follow-up.
          </Faq>
          <Faq q="How do I upgrade from Free to Pro (or Team)?">
            Sign up for the Free tier, then hit{" "}
            <Link href="/dashboard" className="text-brand-600 hover:underline">
              Manage billing
            </Link>{" "}
            on the dashboard — the Stripe customer portal lets you pick
            or change your plan there. Every plan uses the same API key,
            same dashboard, same SDK; you never need to re-onboard.
          </Faq>
          <Faq q="Can I cancel any time?">
            Yes. Cancel from the dashboard&apos;s{" "}
            <Link href="/dashboard" className="text-brand-600 hover:underline">
              Manage billing
            </Link>{" "}
            button, which opens the Stripe customer portal. The
            subscription runs through the current billing period; no
            pro-rated refunds.
          </Faq>
          <Faq q="What SSO does Enterprise support?">
            SAML and OIDC (Okta, Entra ID, Google Workspace, generic
            identity providers). SCIM 2.0 for user provisioning.
            Integration typically completes in one working session with
            an Okta / Entra admin on your side.
          </Faq>
        </div>
      </section>

      <section className="mb-16 rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-800 dark:bg-gray-900">
        <h2 className="mb-3 text-2xl font-semibold">
          Still deciding? Try it free.
        </h2>
        <p className="mx-auto mb-6 max-w-xl text-sm text-gray-600 dark:text-gray-400">
          The Free tier is a real production plan with real rate limits —
          not a demo timer. Upgrade only when you outgrow it.
        </p>
        <div className="flex flex-wrap justify-center gap-4">
          <Link
            href="/signup"
            className="rounded-md bg-brand-500 px-6 py-3 text-white hover:bg-brand-600"
          >
            Start free
          </Link>
          <Link
            href="/why-nanovm"
            className="rounded-md border border-gray-300 px-6 py-3 hover:bg-gray-100 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            How it compares
          </Link>
        </div>
      </section>

      <Footer />
    </main>
  );
}

function PricingCard({ tier }: { tier: Tier }) {
  const wrapper = tier.featured
    ? "relative rounded-lg border-2 border-brand-500 bg-white p-6 shadow-md dark:bg-gray-900"
    : "relative rounded-lg border border-gray-200 bg-white p-6 dark:border-gray-800 dark:bg-gray-900";
  const button = tier.featured
    ? "block w-full rounded-md bg-brand-500 px-4 py-2 text-center text-white hover:bg-brand-600"
    : "block w-full rounded-md border border-gray-300 px-4 py-2 text-center hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800";
  return (
    <div className={wrapper}>
      {tier.featured && (
        <span className="absolute -top-3 right-4 rounded-full bg-brand-500 px-3 py-0.5 text-xs font-medium text-white">
          Most popular
        </span>
      )}
      <h3 className="text-lg font-semibold">{tier.name}</h3>
      <p className="mt-1 text-sm text-gray-600 dark:text-gray-400">
        {tier.tagline}
      </p>
      <div className="my-6">
        <span className="text-4xl font-bold">{tier.price}</span>
        <span className="ml-1 text-sm text-gray-500">{tier.priceCadence}</span>
      </div>
      {tier.cta.href.startsWith("mailto:") ? (
        <a href={tier.cta.href} className={button}>
          {tier.cta.label}
        </a>
      ) : (
        <Link href={tier.cta.href} className={button}>
          {tier.cta.label}
        </Link>
      )}
      <ul className="mt-6 space-y-2 text-sm">
        {tier.bullets.map((b) => (
          <li key={b} className="flex items-start gap-2">
            <span
              className="mt-0.5 text-brand-500"
              aria-hidden
            >
              ✓
            </span>
            <span>{b}</span>
          </li>
        ))}
      </ul>
      {tier.fine && (
        <p className="mt-4 text-xs text-gray-500 dark:text-gray-500">
          {tier.fine}
        </p>
      )}
    </div>
  );
}

function Faq({ q, children }: { q: string; children: React.ReactNode }) {
  return (
    <details className="group rounded-lg border border-gray-200 p-4 dark:border-gray-800">
      <summary className="cursor-pointer list-none font-semibold marker:hidden">
        <span className="mr-2 inline-block text-brand-500 transition-transform group-open:rotate-90">
          ▸
        </span>
        {q}
      </summary>
      <div className="mt-3 pl-6 text-sm text-gray-700 dark:text-gray-300">
        {children}
      </div>
    </details>
  );
}

function SiteHeader() {
  return (
    <header className="mb-14 flex items-center justify-between">
      <Link href="/" className="flex items-center gap-3 hover:text-brand-600">
        <div className="h-8 w-8 rounded-lg bg-brand-500" aria-hidden />
        <span className="text-lg font-semibold">nanovm</span>
      </Link>
      <nav className="flex items-center gap-5 text-sm">
        <Link href="/marketplace" className="hover:text-brand-600">
          Marketplace
        </Link>
        <Link href="/pricing" className="hover:text-brand-600">
          Pricing
        </Link>
        <Link href="/why-nanovm" className="hover:text-brand-600">
          Why nanovm
        </Link>
        <a
          href="https://github.com/ip888/rust-nano-vm"
          className="hover:text-brand-600"
        >
          GitHub
        </a>
        <Link href="/login" className="hover:text-brand-600">
          Log in
        </Link>
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

function Footer() {
  return (
    <footer className="border-t border-gray-200 pt-8 text-sm text-gray-500 dark:border-gray-800 dark:text-gray-400">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <p>
          Open source (Apache 2.0).{" "}
          <a
            href="https://github.com/ip888/rust-nano-vm"
            className="hover:text-brand-600"
          >
            github.com/ip888/rust-nano-vm
          </a>
        </p>
        <div className="flex gap-4">
          <Link href="/marketplace" className="hover:text-brand-600">
            Marketplace
          </Link>
          <Link href="/pricing" className="hover:text-brand-600">
            Pricing
          </Link>
          <Link href="/why-nanovm" className="hover:text-brand-600">
            Why nanovm
          </Link>
          <a
            href="https://github.com/ip888/rust-nano-vm/tree/main/docs"
            className="hover:text-brand-600"
          >
            Docs
          </a>
        </div>
      </div>
    </footer>
  );
}
