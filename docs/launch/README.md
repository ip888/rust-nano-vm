# Launch content kit

Ready-to-paste posts for the day the ops runbook in
[`docs/launch-readiness.md`](../launch-readiness.md) is complete.

Everything here is drafted from the operator's mouth, in the same
voice as the landing page and blog posts. Substitute the placeholder
domain (`nanovm.example.com`) and any per-operator numbers before
posting.

## Contents

| File | Surface | When to fire |
|---|---|---|
| [`show-hn.md`](show-hn.md) | Hacker News `Show HN:` submission + a prepared top-comment technical deep-dive + defenses for the four most likely follow-up questions. | Tuesday–Thursday 6:00–8:00 AM Pacific for max daytime dwell. |
| [`twitter-thread.md`](twitter-thread.md) | 6-tweet launch thread with the LiveForkBenchmark GIF as the hook. | Fire 15 minutes after the HN post to compound. |
| [`linkedin.md`](linkedin.md) | Longer-form single post for LinkedIn's algorithm. | Same day; late morning Pacific. |
| [`producthunt.md`](producthunt.md) | Product Hunt submission — tagline, description, first-comment technical detail. | Day-after HN. PH's own launch timezone is midnight Pacific — schedule accordingly. |
| [`dev-to-crosspost-05.md`](dev-to-crosspost-05.md) | The Claude Code sandbox post ([blog 05](../blog/05-sandbox-for-claude-code.md)) reshaped with dev.to frontmatter + a footer CTA. | 2–3 days after launch, when HN traffic has decayed. |
| [`dev-to-crosspost-06.md`](dev-to-crosspost-06.md) | The LangChain.js post ([blog 06](../blog/06-langchain-js-execute-python.md)) — same treatment. | 24h after post 05 crosspost. |
| [`metrics.md`](metrics.md) | What to watch during the first 24 hours; the two Prometheus queries that predict trouble; when to reply-vs-ignore on HN. | Read before firing anything. |

## Ordering — first 72 hours

1. **T-0**: Show HN goes up. Reply to every comment inside 15 min for the first two hours. That reply cadence is the single biggest driver of top-page dwell time.
2. **T + 15 min**: Twitter/X thread fires with the same GIF the landing-page LiveForkBenchmark shows.
3. **T + 3 h**: LinkedIn post. Reach on LinkedIn peaks 3 hours after posting; going earlier on the same news cycle stacks reach on top of HN referrals.
4. **T + 24 h**: Product Hunt submission (its own launch timezone is midnight PT, so schedule the day before).
5. **T + 48 h**: dev.to crosspost of blog 05.
6. **T + 72 h**: dev.to crosspost of blog 06.

Between launches, watch [`metrics.md`](metrics.md) and act on the two Prometheus queries it names — a fork-RPS spike on the demo tenant is the loudest signal that traffic is real.

## What NOT to do

- Don't post to r/programming or r/rust on Day 1 — those subs downweight self-promotion within 24h of an HN top-page result, and the mods notice.
- Don't chase every negative comment. The prepared defenses in [`show-hn.md`](show-hn.md) cover the four questions that actually recur; below that threshold, engaging invites more of the same.
- Don't announce paid pricing in the HN title. Show HN skews hostile to launch-day monetization. The title lands on the product; pricing is in the third-comment reply.
