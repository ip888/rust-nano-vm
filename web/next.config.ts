import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // `standalone` output lets us build a slim Docker image that copies
  // just `.next/standalone` + `.next/static` + `public/` (~60 MiB
  // total) instead of shipping the full `node_modules`. Required for
  // the distroless-node runtime the Fly.io deploy uses.
  output: "standalone",

  // The site fetches GitHub API for live repo stats at build time and
  // at runtime (Server Component fetch with ISR). Domain must be in
  // the allow-list so Next.js's URL restrictions don't block it.
  experimental: {
    serverActions: {
      // Fly.io HTTPS terminates at their edge; the app receives HTTP
      // internally. Trust the forwarded-proto header from Fly's edge.
      allowedOrigins: ["nanovm.io", "*.nanovm.io", "*.fly.dev"],
    },
  },

  // Keep source maps in prod for meaningful stack traces from the
  // handful of client interactions we ship (a "Try live" button in
  // week 3, essentially — nothing before that).
  productionBrowserSourceMaps: true,
};

export default nextConfig;
