// Server Component: fetches public GitHub REST API for repo stats,
// revalidates every 60 s. No auth needed for the read-only endpoints;
// GitHub's anonymous rate limit is 60 req/hr per source IP which is
// plenty for ISR at 60 s cadence (60 revalidations/hr).
//
// A network failure or a rate-limit response falls back to whatever
// the caller passed as `fallback*` props — never blocks page render.
// That's the whole reason this stays a Server Component and not a
// client fetch: even offline, the site loads with sensible defaults.

type GitHubRepo = {
  stargazers_count: number;
  forks_count: number;
  open_issues_count: number;
  pushed_at: string;
  html_url: string;
};

async function fetchRepo(
  owner: string,
  repo: string
): Promise<GitHubRepo | null> {
  // Short abort deadline so a hung GitHub connection can't stall
  // page render / ISR revalidation. 5 s is plenty for a JSON response
  // measured in kilobytes; a slower response is a failure mode we
  // want to surface as a fallback, not a delayed render.
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const res = await fetch(
      `https://api.github.com/repos/${owner}/${repo}`,
      {
        next: { revalidate: 60 },
        signal: controller.signal,
        headers: {
          Accept: "application/vnd.github+json",
          "X-GitHub-Api-Version": "2022-11-28",
        },
      }
    );
    if (!res.ok) return null;
    return (await res.json()) as GitHubRepo;
  } catch {
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

function formatRelative(iso: string): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  const diffSec = Math.max(0, Math.round((now - then) / 1000));
  if (diffSec < 60) return `${diffSec} s ago`;
  const diffMin = Math.round(diffSec / 60);
  if (diffMin < 60) return `${diffMin} min ago`;
  const diffHr = Math.round(diffMin / 60);
  if (diffHr < 24) return `${diffHr} h ago`;
  const diffDay = Math.round(diffHr / 24);
  if (diffDay < 30) return `${diffDay} d ago`;
  const diffMo = Math.round(diffDay / 30);
  return `${diffMo} mo ago`;
}

export default async function GitHubStats({
  owner,
  repo,
  fallbackStars,
  fallbackForks,
}: {
  owner: string;
  repo: string;
  fallbackStars: number;
  fallbackForks: number;
}) {
  const data = await fetchRepo(owner, repo);
  const stars = data?.stargazers_count ?? fallbackStars;
  const forks = data?.forks_count ?? fallbackForks;
  const issues = data?.open_issues_count ?? 0;
  const lastPush = data?.pushed_at
    ? formatRelative(data.pushed_at)
    : "—";

  return (
    <div className="grid grid-cols-2 gap-4 md:grid-cols-4">
      <Stat label="Stars" value={stars.toLocaleString()} />
      <Stat label="Forks" value={forks.toLocaleString()} />
      <Stat label="Open issues" value={issues.toLocaleString()} />
      <Stat label="Last commit" value={lastPush} />
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elev)] p-4">
      <div className="mb-1 text-xs uppercase tracking-wider text-[var(--color-fg-faint)]">
        {label}
      </div>
      <div className="font-mono text-lg font-medium">{value}</div>
    </div>
  );
}
