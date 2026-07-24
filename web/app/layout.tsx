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
 */
const WEB_ORIGIN = (
  process.env.NEXT_PUBLIC_NANOVM_WEB_ORIGIN?.trim() ||
  "https://nanovm.example.com"
).replace(/\/+$/, "");

const TITLE = "nanovm — sub-second microVMs for AI agents";
const DESCRIPTION =
  "Fork a real KVM microVM in ~12 ms. Give your AI agent a sandbox its tool calls can actually run in.";

export const metadata: Metadata = {
  metadataBase: new URL(WEB_ORIGIN),
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
