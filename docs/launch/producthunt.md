# Product Hunt submission

PH's launch timezone is midnight Pacific — schedule the day
before Show HN if you want same-week compounding, or a week after
if you want a clean isolated day (PH's rank algorithm rewards
"launches without inbound traffic momentum" more, oddly).

## Product

- **Name**: `nanovm`
- **Tagline** (60 char max): `Sub-second KVM microVMs for AI-agent code execution`
- **Topics**: `Developer Tools`, `Artificial Intelligence`, `SaaS`
- **First launch**: Yes.
- **Maker(s)**: You.

## Gallery

Six slots, in this order. Reuse assets already shipped — no
new design needed.

1. **`opengraph-image.png`** from `/` — the landing hero card.
2. **The LiveForkBenchmark GIF** (same as Twitter tweet 1).
3. **Screenshot of `/dashboard/playground`** with the "Hello
   world" preset visible and output populated. Grab from the
   [visual walkthrough artifact](https://claude.ai/code/artifact/140f236a-da55-4a4a-bbea-45a41bec3bfc)
   step 10.
4. **Screenshot of `/pricing`** — the four-tier card row.
5. **Comparison-table screenshot** from `/pricing` — the "vs
   E2B / Modal / AWS Lambda / Docker" table.
6. **Code screenshot** of the three-line LangChain.js example
   from blog post 06. Terminal font, dark background.

## Description (260 chars)

```
Fork a real KVM microVM in ~12 ms. Give your LLM agent
(Claude Code, LangChain, OpenAI, Cursor) a sandbox its
tool calls can actually run in — Python, shell, anything
— with hardware isolation and a free tier. Apache 2.0.
Self-hostable.
```

## First comment (post yourself, immediately after launch fires)

```
Hi PH — I made nanovm.

Why: every AI agent that runs code (Claude Code, Cursor
Agent, Devin, LangChain, aider) needs to `execute_python` /
`execute_shell` somewhere safe. Docker exec is 100-500 ms
per call and shares a kernel with your laptop. E2B / Modal
Sandbox are proprietary and 150-400 ms per call. Just
trusting the model isn't a plan.

nanovm's snapshot + fork primitive collapses that to ~12 ms
per call. Same MAP_PRIVATE trick a Firecracker snapshot
restore does, but exposed as a first-class REST endpoint
with SDKs.

There's an in-browser playground behind the free-tier signup
(no credit card): paste Python, hit Run, watch it execute in
a real KVM microVM in under a second. That's usually the
"oh, this actually works" moment.

Happy to answer questions here or in DMs. And thanks to
whoever hunted this if it wasn't me — appreciate you.
```

## Hunter selection

If you can get a well-known PH hunter to submit (rather than
self-hunting), the launch gets a ~2× rank boost on average.
Reach out ~1 week ahead. Otherwise self-hunt — nothing wrong
with it.

## The 4 things to do on launch day

1. **Upvote from every device you legitimately have access to
   in the first hour** — no coordinated brigading, but your own
   phone / laptop / tablet each count.
2. **Reply to every comment inside 10 minutes**. PH's rank
   algorithm weights comment thread engagement heavily.
3. **Cross-post the PH URL** into the Twitter thread + the
   LinkedIn post as a "we're #N on Product Hunt today, would
   love your upvote" quote-tweet ~2 hours in.
4. **Don't ask for upvotes on HN** — the two audiences are
   overlapping enough that HN readers WILL vote, but the ask
   itself is a downvote magnet on HN.

## Ranking targets

- **Top 5 of the day**: realistic with the assets above + the
  Twitter/LinkedIn crossover audience.
- **Top 1**: needs either a genuinely enormous inbound bump
  (Twitter viral) or a hunter with a large following.

Aim for top 5 and reset expectations from there.
