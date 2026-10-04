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
      allowedOrigins: ["nanovm.app", "*.nanovm.app", "*.fly.dev"],
    },
  },

  // Keep source maps in prod for meaningful stack traces from the
  // handful of client interactions we ship (a "Try live" button in
  // week 3, essentially — nothing before that).
  productionBrowserSourceMaps: true,

  // Canonicalise on https://nanovm.app. The same deployment is also
  // reachable at nanovm-web.fly.dev (Fly's built-in hostname) — if we
  // let both URLs serve the same content, Google indexes duplicates
  // and may pick the ugly one as canonical. A 301 at the host level
  // leaves the brand URL as the sole indexable surface and keeps
  // link equity flowing to it.
  async redirects() {
    return [
      {
        source: "/:path*",
        has: [{ type: "host", value: "nanovm-web.fly.dev" }],
        destination: "https://nanovm.app/:path*",
        permanent: true,
      },
    ];
  },
};

export default nextConfig;
