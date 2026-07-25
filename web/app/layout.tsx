import type { Metadata } from "next";
import type { ReactNode } from "react";

import "./globals.css";

/**
 * Canonical origin used for `metadataBase`. Every relative OG /
 * Twitter image URL Next.js generates is resolved against this — so
 * a share of `/pricing` on Twitter fetches `${WEB_ORIGIN}/pricing/opengraph-image.png`.
 *
 * Env override matches the `NEXT_PUBLIC_*` convention already used by
 * `NEXT_PUBLIC_NANOVM_API_URL` etc, so operators re-brand this by
 * setting one env var at build time. Fallback to the demo origin so
 * a fresh clone renders correctly without env config.
 *
 * We resolve the URL here and swallow parse failures rather than
 * letting `new URL(...)` throw at module init: a malformed env value
 * would otherwise crash the whole web app on first request instead
 * of just breaking OG previews.
 */
const FALLBACK_ORIGIN = "https://nanovm.example.com";

function resolveMetadataBase(): URL {
  const raw = process.env.NEXT_PUBLIC_NANOVM_WEB_ORIGIN?.trim();
  const candidates = [raw, FALLBACK_ORIGIN].filter(
    (v): v is string => typeof v === "string" && v.length > 0,
  );
  for (const c of candidates) {
    const trimmed = c.replace(/\/+$/, "");
    try {
      return new URL(trimmed);
    } catch {
      // `NEXT_PUBLIC_NANOVM_WEB_ORIGIN` is missing the scheme or is
      // otherwise malformed — fall through to the demo origin, which
      // is a compile-time constant and cannot throw.
      if (typeof console !== "undefined") {
        console.warn(
          `[nanovm-web] Ignoring invalid NEXT_PUBLIC_NANOVM_WEB_ORIGIN=${JSON.stringify(
            trimmed,
          )}; falling back to ${FALLBACK_ORIGIN}.`,
        );
      }
    }
  }
  // Unreachable — FALLBACK_ORIGIN is a valid absolute URL. Kept as
  // a defensive last resort so the function's return type stays
  // non-nullable.
  return new URL(FALLBACK_ORIGIN);
}

const METADATA_BASE = resolveMetadataBase();
const WEB_ORIGIN = METADATA_BASE.origin;

const TITLE = "nanovm — sub-second microVMs for AI agents";
const DESCRIPTION =
  "Fork a real KVM microVM in ~12 ms. Give your AI agent a sandbox its tool calls can actually run in.";

export const metadata: Metadata = {
  metadataBase: METADATA_BASE,
  title: {
    default: TITLE,
    // Per-page `metadata.title` values become `<value> — nanovm`, so
    // a share of `/pricing` renders as "Pricing — nanovm" without
    // every page having to spell it out.
    template: "%s — nanovm",
  },
  description: DESCRIPTION,
  applicationName: "nanovm",
  openGraph: {
    type: "website",
    siteName: "nanovm",
    title: TITLE,
    description: DESCRIPTION,
    url: WEB_ORIGIN,
    locale: "en_US",
    // Next.js file-convention `opengraph-image.tsx` files auto-fill
    // the actual image URL per route — leaving this block minimal
    // keeps the per-page files in charge of the specific image.
  },
  twitter: {
    card: "summary_large_image",
    title: TITLE,
    description: DESCRIPTION,
  },
};

/**
 * Root layout — kept minimal so page-level components own their own
 * chrome. Just wires the tailwind stylesheet + a system font stack.
 */
export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" className="antialiased">
      <body className="min-h-screen font-sans">{children}</body>
    </html>
  );
}
