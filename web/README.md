# nanovm-web

Public site for [rust-nano-vm](https://github.com/ip888/rust-nano-vm) —
landing page today, interactive playground in weeks 3–4.

## Local development

Requires Node 20+.

```sh
cd web
npm install
npm run dev        # http://localhost:3000
```

## Deployment

Fly.io. The root `Dockerfile.web` and `fly.toml` at the repo root
build a distroless-node runtime image from this directory. See
`.github/workflows/web-deploy.yml` for the auto-deploy pipeline that
runs on every merge to `main` touching `web/**`.

## Structure

```
web/
├── app/
│   ├── components/
│   │   ├── GitHubStats.tsx    live repo stats (ISR, 60 s)
│   │   └── PlaygroundTeaser.tsx  week-by-week rollout tracker
│   ├── globals.css            design tokens + Tailwind v4
│   ├── layout.tsx             root layout + metadata
│   └── page.tsx               landing page
├── package.json
├── next.config.ts             output: "standalone" for Fly deploy
├── postcss.config.js          @tailwindcss/postcss
└── tsconfig.json
```

Deliberately minimal — every week ships one visible change so
contributors can watch functionality land rather than get a big
launch at the end.
