import { OG_CONTENT_TYPE, OG_SIZE, nanovmOgResponse } from "@/lib/og";

export const alt = "Why nanovm — an honest comparison with E2B, Modal, Docker, and AWS Lambda MicroVMs";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return nanovmOgResponse({
    title: "Honest comparison with E2B, Modal, Docker.",
    subtitle:
      "Side-by-side on cold start, isolation, self-host, and pricing. We recommend the other guy when they're the right pick.",
    routeLabel: "/why-nanovm",
  });
}
