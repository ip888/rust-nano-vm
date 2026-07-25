import type { MetadataRoute } from "next";

/**
 * `/sitemap.xml` — the crawler-facing index of every page a search
 * engine should consider for ranking.
 *
 * Next.js's file-convention `sitemap.ts` runs at build time (or at
 * request time for dynamic sitemaps) and serves the result at the
 * conventional `/sitemap.xml` URL. We enumerate only public
 * marketing routes: `/dashboard`, `/login`, `/signup`,
 * `/signup/verify` are per-user surfaces that would waste
 * Googlebot's crawl budget and shouldn't appear in the SERP
 * anyway (see `robots.ts` for the disallow list).
 *
 * `metadataBase` from `app/layout.tsx` resolves the relative paths
 * below to absolute URLs against `NEXT_PUBLIC_NANOVM_WEB_ORIGIN`,
 * so an operator re-brand does the right thing without editing
 * this file.
 *
 * `lastModified` is deploy-time — a rebuild after a doc/feature
 * change bumps every entry. Search engines use this as a hint,
 * not a mandate; they'll refetch on their own cadence regardless.
 * `changeFrequency` and `priority` are similarly hints; we set
 * conservative values.
 */
export default function sitemap(): MetadataRoute.Sitemap {
  const lastModified = new Date();
  return [
    {
      url: "/",
      lastModified,
      changeFrequency: "weekly",
      priority: 1.0,
    },
    {
      url: "/pricing",
      lastModified,
      changeFrequency: "weekly",
      priority: 0.9,
    },
    {
      url: "/why-nanovm",
      lastModified,
      changeFrequency: "monthly",
      priority: 0.8,
    },
    {
      url: "/marketplace",
      lastModified,
      changeFrequency: "weekly",
      priority: 0.7,
    },
  ];
}
