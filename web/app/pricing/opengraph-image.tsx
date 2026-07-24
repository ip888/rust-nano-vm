import { OG_CONTENT_TYPE, OG_SIZE, nanovmOgResponse } from "@/lib/og";

export const alt = "nanovm pricing — free tier + Pro from $29/mo";
export const size = OG_SIZE;
export const contentType = OG_CONTENT_TYPE;

export default function Image() {
  return nanovmOgResponse({
    title: "Pricing that ends in a checkout, not a call.",
    subtitle:
      "Free tier forever. Pro from $29/mo for unlimited monthly forks. Team from $199/mo. Enterprise for SSO, RBAC, air-gap, and on-prem.",
    routeLabel: "/pricing",
  });
}
