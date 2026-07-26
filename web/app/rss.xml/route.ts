/**
 * `/rss.xml` — RSS 2.0 feed of the nanovm engineering blog.
 *
 * Blog posts live in `docs/blog/` as Markdown files and render on
 * GitHub (they aren't Next.js routes in `web/app`). This feed
 * publishes the same post list at an aggregator-friendly URL so
 * anyone using Feedly / Feedbin / a Slack RSS bot / the HN
 * newsletter aggregator gets notified when a new post lands.
 *
 * Post index is hardcoded below. When post 07+ lands, append one
 * entry and redeploy — that's cheaper than parsing `docs/blog/*.md`
 * across the workspace boundary, and the count is small enough that
 * hand-maintaining the list stays honest.
 *
 * `pubDate` uses the canonical RFC 822 format RSS parsers expect.
 * Dates are the initial-publication dates of each post; a later
 * edit doesn't retroactively become a "new" post the aggregator
 * pushes to subscribers again.
 */

import { NextResponse } from "next/server";

interface Post {
  slug: string;
  title: string;
  /** One-line summary shown in aggregator UIs. */
  description: string;
  /** ISO 8601 initial publication date. */
  publishedIso: string;
}

// Ordered newest-first so the feed sorts correctly when parsers
// don't re-sort by `pubDate`.
const POSTS: Post[] = [
  {
    slug: "06-langchain-js-execute-python",
    title: "Give your LangChain.js agent execute_python in three lines",
    description:
      "Zero-dep TypeScript client + OpenAI-shape tool descriptors — drops into LangChain.js bindTools, Vercel AI SDK, and Anthropic tool use without adapter code.",
    publishedIso: "2026-07-24",
  },
  {
    slug: "05-sandbox-for-claude-code",
    title: "Give Claude Code a real sandbox in three lines",
    description:
      "Every coding-agent CLI needs a shell that won't nuke your $HOME. KVM microVM at ~12 ms per fork; three lines from `pip install` to a working tool.",
    publishedIso: "2026-07-24",
  },
  {
    slug: "04-12ms-eval-fanout",
    title: "12 ms fan-out for AI-agent eval pipelines",
    description:
      "SWE-bench / HumanEval-style fan-out with a snapshot+fork microVM primitive — ~12 ms p50 cold start, ~0.5 MiB Pss per fork.",
    publishedIso: "2026-06-15",
  },
  {
    slug: "03-regulated-ai-sandboxes",
    title: "Regulated-industry AI sandboxes",
    description:
      "HIPAA / SOC 2 / PCI audit narratives for a self-hostable microVM sandbox — real Linux VMs, JSONL audit log, familiar to auditors.",
    publishedIso: "2026-05-20",
  },
  {
    slug: "02-snapshot-restore",
    title: "Faithful KVM snapshot/restore in under 1000 lines",
    description:
      "Full vCPU + LAPIC + FPU + MSR + IRQCHIP + PIT capture via kvm-bindings serde, with guest RAM as a separate backing file.",
    publishedIso: "2026-04-10",
  },
  {
    slug: "01-mmap-private",
    title: "MAP_PRIVATE fork: cold start is an mmap away",
    description:
      "Fork doesn't re-boot a kernel; it maps the snapshot's memory file MAP_PRIVATE. ~50 lines of unsafe under the hood.",
    publishedIso: "2026-03-01",
  },
];

const REPO_BLOB_BASE =
  "https://github.com/ip888/rust-nano-vm/blob/main/docs/blog";

const CHANNEL_TITLE = "nanovm engineering blog";
const CHANNEL_DESCRIPTION =
  "Deep dives on the sub-second microVM primitive powering nanovm: MAP_PRIVATE snapshot fork, KVM restore internals, agent-eval fan-out, and regulated-industry deployment.";

export const dynamic = "force-static";

export function GET() {
  const origin =
    (
      process.env.NEXT_PUBLIC_NANOVM_WEB_ORIGIN?.trim() ||
      "https://nanovm.example.com"
    ).replace(/\/+$/, "");
  const items = POSTS.map((p) => {
    const link = `${REPO_BLOB_BASE}/${p.slug}.md`;
    return [
      "  <item>",
      `    <title>${escapeXml(p.title)}</title>`,
      `    <link>${escapeXml(link)}</link>`,
      // A guid distinct from `link` and marked `isPermaLink="false"`
      // so a future re-hosting of the posts (e.g. a `/blog/[slug]`
      // web route) doesn't retro-fire the feed as "new posts."
      `    <guid isPermaLink="false">nanovm-blog:${p.slug}</guid>`,
      `    <pubDate>${escapeXml(toRfc822(p.publishedIso))}</pubDate>`,
      `    <description>${escapeXml(p.description)}</description>`,
      "  </item>",
    ].join("\n");
  }).join("\n");

  const feed =
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">\n` +
    `<channel>\n` +
    `  <title>${escapeXml(CHANNEL_TITLE)}</title>\n` +
    `  <link>${escapeXml(origin)}</link>\n` +
    `  <description>${escapeXml(CHANNEL_DESCRIPTION)}</description>\n` +
    `  <language>en-us</language>\n` +
    `  <atom:link href="${escapeXml(
      `${origin}/rss.xml`,
    )}" rel="self" type="application/rss+xml" />\n` +
    `${items}\n` +
    `</channel>\n` +
    `</rss>\n`;

  return new NextResponse(feed, {
    status: 200,
    headers: {
      "Content-Type": "application/rss+xml; charset=utf-8",
      // Aggregators typically re-poll every 15–60 min; a 10-min
      // browser-cache window keeps GitHub-style prefetch hits cheap
      // without gluing subscribers to a stale snapshot.
      "Cache-Control": "public, max-age=600, s-maxage=600",
    },
  });
}

/**
 * Minimum XML escape needed for RSS text content — the five named
 * entities. Feed titles / descriptions come from source in this
 * file; no user input path.
 */
function escapeXml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

/**
 * ISO 8601 date-only → RFC 822. RSS parsers accept RFC 822 with
 * either `-0000` or a named zone; we pick GMT explicitly so a
 * cross-timezone re-render doesn't shift a post from "yesterday"
 * to "today" and re-fire subscribers.
 */
function toRfc822(iso: string): string {
  // `new Date(iso)` on a bare date interprets it as midnight UTC.
  // Manually building the components avoids depending on the host's
  // locale.
  const [y, m, d] = iso.split("-").map((v) => Number.parseInt(v, 10));
  const dt = new Date(Date.UTC(y!, m! - 1, d!, 0, 0, 0));
  const day = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][dt.getUTCDay()];
  const month = [
    "Jan",
    "Feb",
    "Mar",
    "Apr",
    "May",
    "Jun",
    "Jul",
    "Aug",
    "Sep",
    "Oct",
    "Nov",
    "Dec",
  ][dt.getUTCMonth()];
  return `${day}, ${String(dt.getUTCDate()).padStart(2, "0")} ${month} ${dt.getUTCFullYear()} 00:00:00 GMT`;
}
