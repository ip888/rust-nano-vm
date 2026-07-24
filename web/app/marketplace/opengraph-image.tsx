import { OG_CONTENT_TYPE, OG_SIZE, nanovmOgResponse } from "@/lib/og";

export const alt = "nanovm marketplace — pre-built microVM snapshots";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return nanovmOgResponse({
    title: "Pre-built snapshots. Fork in one call.",
    subtitle:
      "Python 3.12, Node 20 + Playwright, Alpine shell, and more — ready to fork in ~12 ms. No image-build, no Dockerfile.",
    routeLabel: "/marketplace",
  });
}
