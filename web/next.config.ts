import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // No custom server needed — the dashboard is entirely client-side
  // against the control-plane API. `next start` on any Node runtime,
  // or a static export for CDN-only deploys.
  reactStrictMode: true,
};

export default nextConfig;
