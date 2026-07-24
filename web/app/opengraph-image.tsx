import { OG_CONTENT_TYPE, OG_SIZE, nanovmOgResponse } from "@/lib/og";

// Landing-page OG image. Rendered on demand at
// `/opengraph-image.png` by Next.js's file convention; the same
// URL fills the `og:image` slot Twitter / LinkedIn / HN unfurl.
export const alt = "nanovm — sub-second microVMs for AI agents";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return nanovmOgResponse({
    title: "Sub-second microVMs for AI agents.",
    subtitle:
      "Fork a real KVM microVM in ~12 ms. Give your agent a sandbox its tool calls can actually run in — Python, shell, arbitrary binaries — with hardware isolation.",
  });
}
