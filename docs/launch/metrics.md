# First 24 hours — what to watch, what to act on

The launch content in this folder generates the traffic. The
list here tells you what to look at while the traffic is
landing, and what the concrete "do X" is when each signal fires.

Read this ONCE before firing anything. Print it. Have it open on
a second monitor for the 6 hours after Show HN goes up.

## Instrumentation (already shipped)

Every one of these already exists on the control plane. Nothing
in this doc requires new server code.

| Signal | Source | Where |
|---|---|---|
| Fork RPS by org | Prometheus `nanovm_forks_total_by_org` counter | `/metrics` on the control plane |
| Fork wall-clock (server-reported) | Prometheus `nanovm_fork_duration_ms` histogram | `/metrics` |
| Signup rate | Prometheus `nanovm_signups_total` counter | `/metrics` |
| 429 rate | Prometheus `nanovm_fork_rate_limited_total` counter | `/metrics` |
| Dunning blocks | Prometheus `nanovm_dunning_blocked_total` counter | `/metrics` |
| Web page views | Vercel Analytics (built-in) | Vercel dashboard |
| Stripe subscription events | Stripe dashboard | stripe.com |

The Grafana dashboard the Helm chart ships (`deploy/helm/nanovm/grafana/nanovm-overview.json`) covers all of these on one screen.

## The two Prometheus queries that predict trouble

Set both as alerts before launch. Alerts fire faster than eyes on a Grafana panel.

### Alert 1 — demo tenant hit hard enough to matter

```promql
rate(nanovm_forks_total_by_org{org="demo"}[1m]) > 20
```

**When it fires**: the landing page's LiveForkBenchmark is getting real click-through. HN or Twitter or LinkedIn is actually driving traffic. Not a problem, just a heads-up — check that traffic is landing on `/signup` and not just bouncing.

**Do**: post the "we're on HN #N right now" quote-tweet if you haven't already. Reply-cadence on HN goes to 10-min instead of 15-min.

### Alert 2 — Stripe webhook processing lag

```promql
rate(nanovm_stripe_webhook_processed_total[5m]) < 0.1
  AND ON () rate(nanovm_signups_total[5m]) > 0.5
```

**When it fires**: signups are landing but Stripe events aren't being processed — subscriptions won't activate, dunning won't fire, revenue is at risk. Almost always a webhook signing-secret mismatch or a network path issue between Stripe and your control plane.

**Do**: verify `STRIPE_WEBHOOK_SIGNING_SECRET` matches Stripe's configured value; check control-plane logs for `stripe_webhook_signature_verification_failed`; if broken, ship a hotfix and manually resync subscriptions from the Stripe dashboard once fixed.

## The five things to eyeball every 30 minutes

1. **HN rank** on `https://news.ycombinator.com/show`. Top-3 is a great day. Top-10 pays for the launch's ops setup week.
2. **Vercel Analytics `/`**. The pageview count divided by `nanovm_signups_total` growth gives you the conversion rate. > 1 % on Day 1 is healthy for a technical product; < 0.3 % means the landing page isn't closing.
3. **`/pricing` bounce rate**. If pricing is where people leave, the tier numbers or the CTAs aren't landing.
4. **First `active` subscription** in Stripe dashboard. Screenshot it the moment it lands — that's tomorrow's tweet.
5. **`SDK install` requests on npmjs.com stats for `@nanovm/sdk`**. Rising means real integrations, not just tire-kicking.

## HN comment triage — when to reply, when to skip

| Comment shape | Response |
|---|---|
| Genuine technical question | Reply within 15 min. Use the prepared answers from `show-hn.md` verbatim if applicable. |
| Comparison ("how vs X") | Reply with the `/why-nanovm` link + one honest sentence about where X wins. |
| Feature request | Acknowledge, add to a followup GitHub issue, don't over-promise ship dates. |
| "You could do this with N lines of $existing_tool" | Reply once, honestly, agree if right, stop. |
| Downvote-baiting rants | Skip. Engaging them attracts more of the same. |
| "Not novel" | Reply once with the specific novel bit (MAP_PRIVATE fork + snapshot as REST primitive) + a link to blog post 01. |

## When to declare victory or defeat

### 24 hours in — victory conditions

- ≥ 200 signups
- ≥ 3 paying conversions (any tier)
- HN top-page for ≥ 4 hours
- ≥ 500 npm installs of `@nanovm/sdk`
- ≥ 50 GitHub stars

Hit any two of five → launch worked. Send the "we're launched" email to whoever should know.

### 24 hours in — defeat conditions

- < 20 signups
- Zero paying conversions
- HN post never crossed 20 upvotes
- Twitter thread < 1,000 impressions

Hit any two → the launch content or the audience-fit is off. Don't fire the Product Hunt post yet. Read the HN comments carefully for what specifically didn't land, revise the pitch, re-launch in 2 weeks with a corrected story.

## After Day 1

- Post a first-day retrospective on Twitter with the actual numbers ("first 24h: N signups, M paying, HN top-page for X hours") — engineers rewire this kind of transparency into follow-on reach.
- Reply to any HN comment thread that's still going.
- Ship a blog post 07 within 7 days answering the single most-asked HN question. This locks in the second wave of inbound.
