import Link from "next/link";

/**
 * Landing page. Structure:
 *   1. Hero + big number + primary CTA.
 *   2. "Three lines to try it" — the moment of hooked-attention →
 *      shortest possible code path.
 *   3. Three-panel "why" — one paragraph each on isolation, speed,
 *      metered billing.
 *   4. Comparison strip — the ~12ms fork against the alternatives
 *      that show up in a real bakeoff (Lambda MicroVMs, E2B, Modal
 *      Sandbox, Docker).
 *   5. "Made for agents" — LangChain / OpenAI Assistants snippet.
 *   6. Deep-dive link to /why-nanovm.
 *   7. Footer.
 *
 * Kept single-page — everything more than a mid-funnel prospect
 * needs lives at /why-nanovm or in the docs.
 */
export default function LandingPage() {
  return (
    <main className="mx-auto max-w-5xl px-6 py-12">
      <SiteHeader />

      <Hero />
      <QuickStart />
      <WhyPanels />
      <ComparisonStrip />
      <MadeForAgents />
      <DeeperDive />

      <Footer />
    </main>
  );
}

function SiteHeader() {
  return (
    <header className="mb-14 flex items-center justify-between">
      <div className="flex items-center gap-3">
        <div className="h-8 w-8 rounded-lg bg-brand-500" aria-hidden />
        <span className="text-lg font-semibold">nanovm</span>
      </div>
      <nav className="flex items-center gap-5 text-sm">
        <Link href="/marketplace" className="hover:text-brand-600">
          Marketplace
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

function Hero() {
  return (
    <section className="mb-16">
      <div className="mb-4 inline-flex items-center gap-2 rounded-full bg-brand-50 px-3 py-1 text-xs font-medium text-brand-700 dark:bg-brand-950 dark:text-brand-300">
        <span className="h-2 w-2 rounded-full bg-brand-500" aria-hidden />
        Open source. Runs on your own K8s or ours.
      </div>
      <h1 className="mb-6 text-5xl font-bold tracking-tight md:text-6xl">
        Fork a real microVM in{" "}
        <span className="text-brand-600 dark:text-brand-400">~12 ms</span>.
      </h1>
      <p className="mb-8 max-w-2xl text-xl text-gray-700 dark:text-gray-300">
        Give your AI agent a sandbox its tool calls can actually run in
        — Python, shell, arbitrary binaries — with KVM-grade process
        isolation and a per-fork rate limit that scales with your plan.
      </p>
      <div className="flex flex-wrap gap-4">
        <Link
          href="/signup"
          className="rounded-md bg-brand-500 px-6 py-3 text-white hover:bg-brand-600"
        >
          Start free
        </Link>
        <Link
          href="/marketplace"
          className="rounded-md border border-gray-300 px-6 py-3 hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
        >
          Browse marketplace
        </Link>
        <a
          href="https://github.com/ip888/rust-nano-vm"
          className="rounded-md border border-gray-300 px-6 py-3 hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
        >
          View source
        </a>
      </div>
    </section>
  );
}

function QuickStart() {
  return (
    <section className="mb-16">
      <h2 className="mb-4 text-2xl font-semibold">Three lines to try it</h2>
      <pre className="overflow-x-auto rounded-lg bg-gray-900 p-4 text-sm text-gray-100">
        {`pip install nanovm

from nanovm import Client
client = Client("https://api.nanovm.io", token="<your key>")
print(client.execute_python("print(sum(range(100)))").stdout)  # "4950\\n"`}
      </pre>
      <p className="mt-3 text-sm text-gray-500 dark:text-gray-400">
        Or reuse one sandbox across many calls — fork once, pay ~12 ms
        of overhead per <em>session</em>, not per <em>call</em>:
      </p>
      <pre className="mt-2 overflow-x-auto rounded-lg bg-gray-900 p-4 text-sm text-gray-100">
        {`with client.sandbox(snapshot="python-3.12-ds") as sb:
    sb.execute_python("import pandas as pd")                # ~12 ms fork
    sb.execute_python("df = pd.DataFrame({'x': [1,2,3]})")  # same VM, sub-ms
    print(sb.execute_python("print(df.sum().to_dict())").stdout)`}
      </pre>
    </section>
  );
}

function WhyPanels() {
  return (
    <section className="mb-16 grid gap-6 md:grid-cols-3">
      <Feature
        title="Real isolation"
        body="Each fork is a KVM microVM with its own kernel + rootfs. seccomp filter on the vmm, cgroups on the jailer, no shared syscall surface with the host."
      />
      <Feature
        title="Fast enough for agent loops"
        body="~12 ms fork against the warm pool. Under 30 ms cold. Your LangChain / OpenAI / CrewAI agent doesn't wait on infrastructure — the LLM roundtrip is 100× longer than the sandbox."
      />
      <Feature
        title="Metered billing built-in"
        body="Signup, Stripe subscription, tier-based rate limits, dunning enforcement, and usage_records reporting — all in the same Rust binary. No sidecars."
      />
    </section>
  );
}

function ComparisonStrip() {
  return (
    <section className="mb-16">
      <h2 className="mb-4 text-2xl font-semibold">
        Cold-fork p50, side-by-side
      </h2>
      <p className="mb-6 max-w-2xl text-sm text-gray-600 dark:text-gray-400">
        Same workload (drop into a warm Python 3.12 sandbox, run
        <code className="mx-1 rounded bg-gray-100 px-1.5 py-0.5 font-mono text-xs dark:bg-gray-800">
          print(1+1)
        </code>
        , close) across the sandbox layers an AI-agent stack usually
        picks between. Lower is better; nanovm's number is warm-pool
        fork against a pre-captured snapshot.
      </p>
      <div className="overflow-x-auto rounded-lg border border-gray-200 dark:border-gray-800">
        <table className="w-full text-sm">
          <thead className="bg-gray-50 dark:bg-gray-900">
            <tr>
              <th className="px-4 py-3 text-left font-semibold">Sandbox</th>
              <th className="px-4 py-3 text-right font-semibold">Cold-fork p50</th>
              <th className="px-4 py-3 text-left font-semibold">Isolation</th>
              <th className="px-4 py-3 text-left font-semibold">Self-host</th>
            </tr>
          </thead>
          <tbody>
            <ComparisonRow
              name="nanovm"
              speed="~12 ms"
              speedBar={0.03}
              isolation="KVM microVM"
              selfHost="Yes (Apache 2.0)"
              highlight
            />
            <ComparisonRow
              name="AWS Lambda MicroVMs"
              speed="~100 ms*"
              speedBar={0.25}
              isolation="Firecracker microVM"
              selfHost="No"
            />
            <ComparisonRow
              name="E2B"
              speed="~150–400 ms"
              speedBar={1.0}
              isolation="Firecracker microVM"
              selfHost="Partial"
            />
            <ComparisonRow
              name="Modal Sandbox"
              speed="~200 ms"
              speedBar={0.5}
              isolation="gVisor + microVM"
              selfHost="No"
            />
            <ComparisonRow
              name="Docker exec"
              speed="~50 ms"
              speedBar={0.12}
              isolation="Namespaces (container)"
              selfHost="Yes"
            />
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-xs text-gray-500 dark:text-gray-400">
        * AWS Lambda MicroVMs, launched July 2026, fork from a
        per-account image built via Dockerfile — no arbitrary-snapshot
        fork. Numbers are provider-published p50 for warm forks; your
        milage varies with region + payload size.{" "}
        <Link href="/why-nanovm" className="text-brand-600 hover:underline">
          Full comparison →
        </Link>
      </p>
    </section>
  );
}

function ComparisonRow({
  name,
  speed,
  speedBar,
  isolation,
  selfHost,
  highlight,
}: {
  name: string;
  speed: string;
  /** Fraction of the widest bar, 0..1 — the visual weight. */
  speedBar: number;
  isolation: string;
  selfHost: string;
  highlight?: boolean;
}) {
  const barPct = Math.min(100, Math.max(4, speedBar * 100));
  return (
    <tr
      className={`border-t border-gray-100 dark:border-gray-800 ${
        highlight
          ? "bg-brand-50 font-medium dark:bg-brand-950/40"
          : ""
      }`}
    >
      <td className="px-4 py-3">{name}</td>
      <td className="px-4 py-3 text-right">
        <span className="mr-2 font-mono text-xs">{speed}</span>
        <span
          className={`inline-block h-1.5 rounded-full align-middle ${
            highlight ? "bg-brand-500" : "bg-gray-300 dark:bg-gray-600"
          }`}
          style={{ width: `${barPct}px` }}
          aria-hidden
        />
      </td>
      <td className="px-4 py-3">{isolation}</td>
      <td className="px-4 py-3">{selfHost}</td>
    </tr>
  );
}

function MadeForAgents() {
  return (
    <section className="mb-16">
      <h2 className="mb-4 text-2xl font-semibold">Made for agent tool loops</h2>
      <div className="grid gap-6 md:grid-cols-2">
        <div>
          <h3 className="mb-2 font-semibold">LangChain / LangGraph</h3>
          <pre className="overflow-x-auto rounded-md bg-gray-900 p-3 text-xs text-gray-100">
{`from nanovm.agents.langchain import NanoVMTool
from langgraph.prebuilt import create_react_agent

agent = create_react_agent(
    llm=ChatOpenAI(model="gpt-4o"),
    tools=[NanoVMTool(client, snapshot="python-3.12-ds")],
)`}
          </pre>
        </div>
        <div>
          <h3 className="mb-2 font-semibold">OpenAI Assistants / Responses</h3>
          <pre className="overflow-x-auto rounded-md bg-gray-900 p-3 text-xs text-gray-100">
{`from nanovm.agents.openai import tool_schemas, dispatch_tool_call

tools = tool_schemas()
rsp = openai.chat.completions.create(..., tools=tools)
for call in rsp.choices[0].message.tool_calls:
    result = dispatch_tool_call(client, call.function.name,
                                 call.function.arguments)`}
          </pre>
        </div>
      </div>
      <p className="mt-4 text-sm text-gray-600 dark:text-gray-400">
        Every tool call is a fresh microVM fork — <b>~12 ms</b> on real
        KVM. An agent that hits its tool 100× per task pays ~1.2 s of
        sandbox overhead total. Compare to E2B (30–40 s) or Modal
        Sandbox (~20 s) at the same call count.
      </p>
    </section>
  );
}

function DeeperDive() {
  return (
    <section className="mb-16 rounded-lg border border-gray-200 bg-gray-50 p-6 dark:border-gray-800 dark:bg-gray-900">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold">
            Not sure if nanovm fits your stack?
          </h2>
          <p className="mt-1 text-sm text-gray-600 dark:text-gray-400">
            The full comparison — sandbox layer, pricing curve, on-prem
            story, security posture — for every vendor above.
          </p>
        </div>
        <Link
          href="/why-nanovm"
          className="rounded-md bg-brand-500 px-4 py-2 text-sm text-white hover:bg-brand-600"
        >
          Read the deep dive →
        </Link>
      </div>
    </section>
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

function Feature({ title, body }: { title: string; body: string }) {
  return (
    <div className="rounded-lg border border-gray-200 p-6 dark:border-gray-800">
      <h3 className="mb-2 font-semibold">{title}</h3>
      <p className="text-sm text-gray-600 dark:text-gray-400">{body}</p>
    </div>
  );
}
