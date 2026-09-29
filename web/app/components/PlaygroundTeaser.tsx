// Coming-soon panel for the interactive playground. Replaced with a
// working spawn button in weeks 3–4 once the vm-kvm virtio-net PR +
// host bridge + playground backend are in place. Deliberately static
// (Server Component) so it renders identically for every visitor —
// no hydration cost for a not-yet-functional feature.

const roadmap: { week: string; label: string; done: boolean }[] = [
  {
    week: "Week 1",
    label: "Landing page + Fly.io deployment pipeline",
    done: true,
  },
  {
    week: "Week 2",
    label: "virtio-net + host bridge (real network into each guest)",
    done: false,
  },
  {
    week: "Week 3",
    label: "First live spawn — one Petclinic per visitor, cold boot",
    done: false,
  },
  {
    week: "Week 4",
    label: "Warm pool + snapshot/fork. Sub-second spawn",
    done: false,
  },
  {
    week: "Week 5",
    label: "Microservices variant: full 7-service stack per visitor",
    done: false,
  },
  {
    week: "Week 6",
    label: "Live metrics dashboard + enterprise trial contact form",
    done: false,
  },
];

export default function PlaygroundTeaser() {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elev)] p-6">
      <p className="mb-6 text-[var(--color-fg-dim)]">
        Each visitor will click a button and get their own real Spring
        Petclinic instance, running inside a nanovm-forked KVM guest,
        reachable at a personal URL for ~10 minutes. Real HTTP, real
        Eureka lookup, real database queries. Not a screencast, not a
        recording.
      </p>
      <ol className="space-y-3">
        {roadmap.map(({ week, label, done }) => (
          <li
            key={week}
            className="flex items-center gap-3 font-mono text-sm"
          >
            <span
              className={
                done
                  ? "inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-[var(--color-accent)] text-black"
                  : "inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-full border border-[var(--color-border)] text-[var(--color-fg-faint)]"
              }
              aria-hidden
            >
              {done ? "✓" : ""}
            </span>
            <span
              className={
                done
                  ? "text-[var(--color-fg-dim)] line-through"
                  : "text-[var(--color-fg)]"
              }
            >
              {/*
                sr-only status text so a screen reader announces
                completion. The visual line-through + filled
                checkmark alone don't reach assistive-tech users.
              */}
              <span className="sr-only">
                {done ? "Completed: " : "Upcoming: "}
              </span>
              <span className="mr-3 inline-block w-14 text-[var(--color-fg-faint)]">
                {week}
              </span>
              {label}
            </span>
          </li>
        ))}
      </ol>
    </div>
  );
}
