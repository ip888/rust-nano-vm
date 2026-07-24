/**
 * Shared building blocks for Next.js file-convention Open Graph
 * images (`opengraph-image.tsx`).
 *
 * Every marketing route ships one PNG that renders when the URL is
 * shared on Twitter / LinkedIn / HN / Slack. We generate them
 * programmatically at request time with `next/og`'s `ImageResponse`
 * — no static assets to keep in sync, no design tool round-trip.
 *
 * ## Constraints imposed by `next/og` / satori
 *
 * - No Tailwind. Satori doesn't run PostCSS; every rule is inline
 *   `style={{ ... }}`.
 * - Every element (except leaf text) MUST set `display: 'flex'` or
 *   satori throws at render time. This is the single most common
 *   gotcha; the helpers below apply it defensively.
 * - No `background: linear-gradient(...)` shorthand — satori wants
 *   `backgroundImage: 'linear-gradient(...)'` explicitly.
 * - Fonts default to Noto Sans (bundled). System font families
 *   render as fallbacks. Custom fonts require fetching + passing
 *   into `ImageResponse({ fonts: [...] })`, which we skip for v1
 *   to keep the image cheap to generate.
 *
 * ## Output shape
 *
 * All routes render at the Open Graph canonical 1200×630. Twitter
 * happily reuses that size when no `twitter-image.tsx` is present,
 * so a single file per route is enough for the mainstream surface.
 */

import { ImageResponse } from "next/og";

/** The single canonical OG dimension. Every route uses this. */
export const OG_SIZE = { width: 1200, height: 630 } as const;
/** Content type served for every file-convention OG image. */
export const OG_CONTENT_TYPE = "image/png";

/** Brand palette. Kept in one place so a re-brand is one edit. */
const BRAND = {
  bg: "#0a0a0a",
  fg: "#ffffff",
  accent: "#f97316", // brand-500 — matches Tailwind `bg-brand-500` on the site.
  muted: "#a3a3a3",
  chip: "rgba(249, 115, 22, 0.15)",
} as const;

interface NanovmOgProps {
  /** Big headline (~one short line). */
  title: string;
  /** Wraps under the title. Two lines max at 1200px. */
  subtitle: string;
  /** Optional route path — e.g. "/pricing" — rendered top-right. */
  routeLabel?: string;
}

/**
 * The one-and-done OG card layout. Consistent across every route
 * so a share of `/pricing` and a share of `/why-nanovm` visually
 * belong to the same product.
 */
export function nanovmOgResponse({
  title,
  subtitle,
  routeLabel,
}: NanovmOgProps): ImageResponse {
  return new ImageResponse(
    (
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          width: "100%",
          height: "100%",
          padding: "72px 80px",
          background: BRAND.bg,
          color: BRAND.fg,
          fontFamily: "system-ui, -apple-system, sans-serif",
        }}
      >
        {/* Top row — wordmark + optional route label */}
        <div
          style={{
            display: "flex",
            width: "100%",
            justifyContent: "space-between",
            alignItems: "center",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 16 }}>
            <div
              style={{
                display: "flex",
                width: 44,
                height: 44,
                borderRadius: 10,
                background: BRAND.accent,
              }}
            />
            <span style={{ fontSize: 36, fontWeight: 700, letterSpacing: -0.5 }}>
              nanovm
            </span>
          </div>
          {routeLabel && (
            <span
              style={{
                display: "flex",
                fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
                fontSize: 22,
                color: BRAND.muted,
              }}
            >
              {routeLabel}
            </span>
          )}
        </div>

        {/* Center — title + subtitle */}
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            flex: 1,
            justifyContent: "center",
            paddingRight: 40,
          }}
        >
          <span
            style={{
              fontSize: 84,
              fontWeight: 700,
              lineHeight: 1.05,
              letterSpacing: -2,
              marginBottom: 32,
            }}
          >
            {title}
          </span>
          <span
            style={{
              fontSize: 30,
              lineHeight: 1.35,
              color: BRAND.muted,
              maxWidth: 900,
            }}
          >
            {subtitle}
          </span>
        </div>

        {/* Bottom row — ~12 ms fork badge + tagline */}
        <div
          style={{
            display: "flex",
            width: "100%",
            justifyContent: "space-between",
            alignItems: "center",
            fontSize: 22,
          }}
        >
          <span
            style={{
              display: "flex",
              alignItems: "center",
              gap: 12,
              padding: "10px 20px",
              borderRadius: 999,
              background: BRAND.chip,
              color: BRAND.accent,
              fontWeight: 600,
            }}
          >
            <span
              style={{
                display: "flex",
                width: 10,
                height: 10,
                borderRadius: 999,
                background: BRAND.accent,
              }}
            />
            ~12 ms fork · KVM microVM
          </span>
          <span style={{ display: "flex", color: BRAND.muted }}>
            Open source · Apache 2.0 OR MIT
          </span>
        </div>
      </div>
    ),
    OG_SIZE,
  );
}
