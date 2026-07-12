import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // No custom server needed — the dashboard is entirely client-side
  // against the control-plane API. Prod deploy = `next start` on any
  // Node runtime.
  //
  // For a CDN-only deploy (no Node runtime), set `output: "export"`
  // here and re-run `npm run build`. That path currently requires
  // dropping the `useSearchParams` client hook wrapping in
  // `web/app/signup/verify/page.tsx` (already Suspense-wrapped, so
  // the export will work) — verify the built `out/` renders before
  // uploading.
  reactStrictMode: true,
};

export default nextConfig;
