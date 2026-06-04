# Launch day — minute-by-minute runbook

A script. Not a playbook. Follow start to finish.

**Total active time:** ~3 hours (with longer monitoring tail).
**Day:** Tuesday, Wednesday, or Thursday. *Not* Mon / Fri / weekend.
**Start time:** 08:00 ET (~ 13:00 UTC, ~ 15:00 in Kyiv/EU CET).
This catches both EU afternoon and US morning on the upvote curve.

Have these tabs open before you start:

- The repo: https://github.com/ip888/rust-nano-vm
- HN submit: https://news.ycombinator.com/submit
- HN frontpage: https://news.ycombinator.com/newest
- /r/rust submit: https://www.reddit.com/r/rust/submit
- Your X compose page
- This file (`docs/launch/launch-day.md`)
- The HN content: [`hn-show.md`](hn-show.md)
- The Reddit content: [`reddit-rust.md`](reddit-rust.md)
- The X thread: [`twitter-thread.md`](twitter-thread.md)
- The response templates: [`response-templates.md`](response-templates.md)

## T-30 min — preflight

- [ ] Pull `main` locally; verify `cargo test --workspace` is green.
- [ ] If you have `/dev/kvm`: run `cargo run -p bench --features kvm --release -- --count 100 --alive 50`. Save the output to a text file — you'll need a screenshot/paste of it.
- [ ] Open `docs/landing.html` in a browser. It renders.
- [ ] Mute Slack/email notifications. Phone on silent. You will need to focus.
- [ ] Eat. Drink water. You'll be at the keyboard for 3 hours.

## T-00 — submit HN (the critical moment)

1. Go to https://news.ycombinator.com/submit
2. **Title:**
   ```
   Show HN: Rust-nano-vm – 12 ms cold-start microVMs for AI agents
   ```
3. **URL:**
   ```
   https://github.com/ip888/rust-nano-vm
   ```
4. **Text:** leave empty.
5. Click **submit**.
6. You're redirected to your post. **Copy the URL of your post immediately** (looks like `https://news.ycombinator.com/item?id=NNNNNNN`). Paste it into a sticky note — you'll need it three times.

## T+0 to T+5 — paste the first comment

1. On your HN post page, click into the comment box at the bottom.
2. Paste the **"First comment"** block from [`hn-show.md`](hn-show.md) verbatim.
3. Click **add comment**.
4. Verify the comment renders correctly (HN preserves line breaks; check the URLs are clickable).

## T+5 to T+10 — post the X thread

1. Open X compose.
2. Paste **Tweet 1** from [`twitter-thread.md`](twitter-thread.md).
3. Click the **+** to add the next tweet in the same thread.
4. Repeat through tweet 7.
5. Click **Post all**.
6. Open the first tweet, click "Add another post" (quote-tweet your own first tweet), paste:
   ```
   Also on HN: <paste your HN URL>
   ```
   Post.

## T+10 to T+60 — the upvote hour (don't leave the keyboard)

This is the hour where HN decides whether you front-page or not. The
algorithm cares about: upvote velocity + comment velocity + author
responsiveness.

**Your job for the next 60 minutes:**

- [ ] Refresh your HN post every 2–3 minutes.
- [ ] **Reply to every top-level comment.** Even "thanks for trying it" is fine. Engagement is the ranker signal.
- [ ] Use [`response-templates.md`](response-templates.md) for the 5 questions you'll get most.
- [ ] Reply to the most useful comments under tweets that are getting traction.
- [ ] **DO NOT** ask friends to upvote. HN detects vote rings; it kills the post invisibly.
- [ ] **DO NOT** argue with critics. If someone says "this is just X", drop a code link and move on.
- [ ] **DO NOT** edit your post or the first comment after submitting (it gets re-flagged as new and resets the ranker signal).

**Healthy signs at T+30 min:**
- 10–20 points and 3–5 comments → you're on track for front page.
- 30+ points → you're already front page.
- 0–3 points → not landing. Don't panic; go to "if it bombs" section below.

## T+60 to T+120 — Reddit + watch

1. Open /r/rust submit: https://www.reddit.com/r/rust/submit
2. Choose **Text post** (NOT link post; reddit's algorithm punishes link posts hard).
3. **Title** from [`reddit-rust.md`](reddit-rust.md).
4. **Body** from [`reddit-rust.md`](reddit-rust.md).
5. Add flair: pick `project` or `release` (whichever the subreddit offers).
6. Post.
7. Back to HN. Keep replying to new comments.
8. Star your own repo so the star count isn't 0 (don't be embarrassed; everyone does this).

## T+120 to T+180 — keep replying, monitor inbound

By now:
- GitHub stars are coming in.
- New issues will appear. Triage:
  - **Bug report** → thank them, ask for a repro, label as `bug`
  - **Feature request that aligns with roadmap** → thank them, label as `enhancement`, link to the relevant milestone
  - **Feature request out of scope** → thank them, explain politely why it's out of scope, close with `wontfix`
  - **Question** → answer, label as `question`
- Cold emails from recruiters / AI infra companies will start arriving. **Reply to all of them within 24 hours.** Even "thanks, not looking right now" is fine. The conversation graph compounds.

## T+180+ — wind down (but don't disappear)

After 3 hours of constant attention:

- Take a 30-minute break.
- Then check back every hour for the next 6 hours.
- Reply to anything new.
- Stop responding around 22:00 your local time. People expect maintainers to sleep.

## Day 2

- [ ] In the morning: look at the metrics. How many stars? How many issues? How many inbound emails?
- [ ] Reply to anything that arrived overnight.
- [ ] **Write down the top 3 questions you got** in a scratch file. They'll become [`docs/faq.md`](../faq.md) next week.
- [ ] Star and follow anyone who starred your repo and looks relevant (AI infra engineers, Rust systems people). You're building the graph.

## If it lands (>200 points / >1000 stars in week 1)

- **Do not promise things in HN comments.** Don't say "I'll add containerd integration next week" unless you've already started it. Promises in launch threads age badly.
- **Triage the issues hard.** Most will be variations of 5 themes. Group them, close duplicates, write 5 README sections that pre-empt the next wave.
- **Take the recruiter calls.** Even ones that sound off-target. The information value of those conversations (what they ask, what they offer) is massive at this stage.
- **Plan the Phase 2 (containerd-shim) PR.** Now you have signal that the niche exists.

## If it does moderately (50–200 points / 100–1000 stars)

- You got eyeballs but not viral lift. That's *fine* — most useful infra projects launch this way.
- The follow-up matters more than the launch itself: ship Phase 2 in 2 weeks and re-launch with a "now drops into your k8s cluster" angle.
- Keep replying to issues; the long-tail traffic from "rust microVM" searches will compound for months.

## If it bombs (<50 points / <100 stars)

- Take 24 hours off the project. Seriously.
- Then write ONE post-mortem note for yourself: *what didn't land?* (Was it the headline number? The framing? The day of the week?)
- Don't relaunch the same content. **Pick one specific angle** ("measuring CoW: why Pss not RSS") and write a 1500-word technical post on the project's GitHub Pages. Post *that* to HN in 2 weeks.
- Most projects "bomb" their first launch. The repo is still public; the work is still good. The window opens again every time you ship something interesting.

## What success actually looks like

Not virality. **One real conversation** that turns into a contract, a
job offer, or a substantive collaboration. The HN/Reddit/X traffic
exists to *generate* those conversations. Even a "modest" launch
(150 stars, 8 issues, 3 cold emails) is a success if one of those 3
emails leads somewhere.

You only need one.
