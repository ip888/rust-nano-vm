import GitHubStats from "./components/GitHubStats";
import PlaygroundTeaser from "./components/PlaygroundTeaser";

// Revalidate top-level page every 5 min so a `git push → merge` shows
// up on the site within that window without a rebuild. The GitHub
// stats fetch inside <GitHubStats/> has its own tighter ISR (60 s).
export const revalidate = 300;

export default function Home() {
  return (
    <main className="mx-auto min-h-screen max-w-4xl px-6 py-16 md:py-24">
      <header className="mb-16 md:mb-24">
        <div className="mb-4 inline-flex items-center gap-2 rounded-full border border-[var(--color-border)] bg-[var(--color-bg-elev)] px-3 py-1 text-xs font-medium text-[var(--color-fg-dim)]">
          <span className="h-1.5 w-1.5 rounded-full bg-[var(--color-accent)]" />
          Week 1 — landing scaffold. Live playground: weeks 3–4.
        </div>
        <h1 className="mb-6 text-5xl font-semibold leading-[1.05] tracking-tight md:text-6xl lg:text-7xl">
          Sub-second JVM cold-start
          <br />
          <span className="text-[var(--color-fg-dim)]">
            for enterprise Java.
          </span>
        </h1>
        <p className="max-w-2xl text-lg leading-relaxed text-[var(--color-fg-dim)] md:text-xl">
          KVM microVM snapshot/restore plus MAP_PRIVATE fork-many. Boot
          Spring Boot once, fork thousands of ready-to-serve replicas
          in <span className="text-[var(--color-fg)]">~200 ms</span>{" "}
          each. No code changes. Any JDK. Your database wherever it
          lives.
        </p>
      </header>

      <section className="mb-16 grid grid-cols-1 gap-8 md:grid-cols-3">
        <Metric
          label="Fork latency, p50"
          value="~200 ms"
          detail="Spring Boot single-jar, warmed snapshot"
        />
        <Metric
          label="Per-fork memory"
          value="~0.5 MiB"
          detail="Pss, thanks to MAP_PRIVATE CoW"
        />
        <Metric
          label="Cold-boot baseline"
          value="8–30 s"
          detail="What snapshot/fork replaces"
        />
      </section>

      <section className="mb-16">
        <h2 className="mb-6 text-2xl font-semibold tracking-tight">
          Progress
        </h2>
        <GitHubStats
          owner="ip888"
          repo="rust-nano-vm"
          fallbackStars={0}
          fallbackForks={0}
        />
      </section>

      <section className="mb-16">
        <h2 className="mb-6 text-2xl font-semibold tracking-tight">
          Try it live
        </h2>
        <PlaygroundTeaser />
      </section>

      <footer className="mt-24 border-t border-[var(--color-border)] pt-8 text-sm text-[var(--color-fg-faint)]">
        <div className="flex flex-wrap gap-x-6 gap-y-2">
          <a
            href="https://github.com/ip888/rust-nano-vm"
            className="transition hover:text-[var(--color-fg)]"
          >
            GitHub
          </a>
          <a
            href="https://github.com/ip888/rust-nano-vm/tree/main/docs/prototypes"
            className="transition hover:text-[var(--color-fg)]"
          >
            Docs
          </a>
          <span>Licensed Apache-2.0 OR MIT.</span>
        </div>
      </footer>
    </main>
  );
}

function Metric({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elev)] p-6">
      <div className="mb-1 text-xs uppercase tracking-wider text-[var(--color-fg-faint)]">
        {label}
      </div>
      <div className="mb-2 text-3xl font-semibold tracking-tight">
        {value}
      </div>
      <div className="text-sm text-[var(--color-fg-dim)]">{detail}</div>
    </div>
  );
}
