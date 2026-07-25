import type { MetadataRoute } from "next";

/**
 * `/robots.txt` — the crawler-facing policy file.
 *
 * The rules are conservative:
 *
 * - **Marketing routes**: allow all user-agents (`/`, `/pricing`,
 *   `/why-nanovm`, `/marketplace`) — these are the pages we WANT
 *   Google to rank.
 * - **Per-user surfaces**: disallow. `/dashboard/*` needs auth,
 *   `/login` / `/signup` / `/signup/verify` are form entry points
 *   Google can't do anything useful with. Indexing them would
 *   waste crawl budget and, in the case of `/signup/verify`, leak
 *   an example magic-link token into search results.
 * - **API base**: `/opengraph-image` PNGs are useful when linked
 *   from `og:image` tags but not as standalone SERP results —
 *   they'd render as an image search hit with no context. Left
 *   allowed since blocking them would also block social crawlers
 *   from unfurling.
 *
 * `sitemap` points at the file-convention route from `sitemap.ts`.
 * Both live at absolute URLs Next.js resolves against
 * `metadataBase` (`NEXT_PUBLIC_NANOVM_WEB_ORIGIN`), so an operator
 * re-brand does the right thing without editing this file.
 */
export default function robots(): MetadataRoute.Robots {
  return {
    rules: [
      {
        userAgent: "*",
        allow: "/",
        disallow: [
          "/dashboard",
          "/dashboard/",
          "/login",
          "/signup",
          "/signup/",
        ],
      },
    ],
    sitemap: "/sitemap.xml",
  };
}
