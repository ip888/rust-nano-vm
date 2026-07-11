import Link from "next/link";

/**
 * Landing page — deliberately short. The pitch is one paragraph,
 * one CTA, one code snippet. Anything longer is the docs' job.
 */
export default function LandingPage() {
  return (
    <main className="mx-auto max-w-4xl px-6 py-16">
      <header className="mb-16 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="h-8 w-8 rounded-lg bg-brand-500" aria-hidden />
          <span className="text-lg font-semibold">nanovm</span>
        </div>
        <nav className="flex items-center gap-6 text-sm">
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

      <section className="mb-20">
        <h1 className="mb-6 text-5xl font-bold tracking-tight">
          Sub-second microVMs for AI agents.
        </h1>
        <p className="mb-8 max-w-2xl text-xl text-gray-700 dark:text-gray-300">
          Fork a real KVM microVM in ~12 ms. Give your agent a sandbox
          its tool calls can actually run in — Python, shell, arbitrary
          binaries — with real process isolation and a per-fork
          rate-limit that scales with your plan.
        </p>
        <div className="flex gap-4">
          <Link
            href="/signup"
            className="rounded-md bg-brand-500 px-6 py-3 text-white hover:bg-brand-600"
          >
            Start free
          </Link>
          <a
            href="https://github.com/ip888/rust-nano-vm"
            className="rounded-md border border-gray-300 px-6 py-3 hover:bg-gray-50 dark:border-gray-700 dark:hover:bg-gray-800"
          >
            View on GitHub
          </a>
        </div>
      </section>

      <section className="mb-16">
        <h2 className="mb-4 text-2xl font-semibold">Three lines to try it</h2>
        <pre className="overflow-x-auto rounded-lg bg-gray-900 p-4 text-sm text-gray-100">
          {`pip install nanovm

from nanovm import Client
client = Client(api_key="<your key>")
print(client.execute_python("print(sum(range(100)))"))`}
        </pre>
      </section>

      <section className="mb-16 grid gap-6 md:grid-cols-3">
        <Feature
          title="Real isolation"
          body="Each fork is a KVM microVM with its own kernel + rootfs. seccomp filter on the vmm, cgroups on the jailer."
        />
        <Feature
          title="Fast enough for agent loops"
          body="~12 ms fork against the warm pool. Under 30 ms cold. Your LangChain / OpenAI agent doesn't wait on infrastructure."
        />
        <Feature
          title="Metered billing built-in"
          body="Signup, Stripe subscription, tier-based rate limits, and usage_records posting — all in the same binary."
        />
      </section>

      <footer className="border-t border-gray-200 pt-8 text-sm text-gray-500 dark:border-gray-800 dark:text-gray-400">
        <p>
          Open source (MIT).{" "}
          <a
            href="https://github.com/ip888/rust-nano-vm"
            className="hover:text-brand-600"
          >
            github.com/ip888/rust-nano-vm
          </a>
        </p>
      </footer>
    </main>
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
