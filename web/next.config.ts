import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // Next.js "standalone" output — builds a self-contained
  // `.next/standalone/` tree with only the runtime files needed to
  // `node server.js`. The Dockerfile.web multi-stage build copies
  // that + `.next/static` + `public/` into a distroless-node image,
  // yielding a ~200 MB final image vs the ~1 GB you'd get from
  // shipping node_modules straight through.
  //
  // Zero effect on `next dev` and `next start`; standalone only
  // changes what `next build` writes under `.next/`.
  //
  // For a CDN-only deploy (no Node runtime), set `output: "export"`
  // here instead and re-run `npm run build`. That path currently
  // requires dropping the `useSearchParams` client hook wrapping in
  // `web/app/signup/verify/page.tsx` (already Suspense-wrapped, so
  // the export will work) — verify the built `out/` renders before
  // uploading.
  output: "standalone",
  reactStrictMode: true,
};

export default nextConfig;
