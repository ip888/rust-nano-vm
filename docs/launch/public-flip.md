# Going public: the exact clicks

You're on the highest-leverage 20 minutes of the whole launch. Follow
this in order. Do not skip steps.

## 0. Pre-flight (already done — confirmed in chat)

- ✅ `.gitignore` excludes `.env`, `.pem`, `.key`, IDE files, OS files
- ✅ No tracked secrets (verified by grep)
- ✅ No embarrassing TODOs in code
- ✅ License is dual Apache-2.0 / MIT (correct for systems Rust)

## 1. Flip the repo to public (2 minutes)

1. Go to https://github.com/ip888/rust-nano-vm
2. Click **Settings** (top nav)
3. Scroll to the bottom — **Danger Zone**
4. Click **Change repository visibility** → **Make public**
5. GitHub asks you to confirm by typing the full repo path:
   `ip888/rust-nano-vm` → type it → click **I understand, change repository visibility**

The Stars / Forks / Issues counters reset to 0. That's expected — they
never existed publicly. The commit history is preserved.

## 2. Set up branch protection on `main` (5 minutes)

GitHub will nag you with "Your main branch isn't protected." Fix it now.

1. Still in **Settings**, click **Branches** (left sidebar)
2. Click **Add branch ruleset** (newer UI) **or** **Add classic branch protection rule**
   - Either works. The screenshots below match the *classic* form because it's simpler.
3. **Branch name pattern:** `main`
4. Check these boxes:

   - [x] **Require a pull request before merging**
     - **Required approvals:** `0` (you're solo; this still forces PRs, but lets you self-approve)
     - [x] Dismiss stale pull request approvals when new commits are pushed
   - [x] **Require status checks to pass before merging**
     - [x] Require branches to be up to date before merging
     - In the search box, add each of: `rustfmt`, `clippy`, `cargo-deny`, `test`, `GitGuardian Security Checks`
     - (If a check doesn't appear yet, save the rule, push one PR, then come back and add it once GitHub knows about it.)
   - [x] **Require conversation resolution before merging**
   - [x] **Do not allow bypassing the above settings** (even you must follow the rules — this is the *important* one; it stops accidental `git push --force` to main from your laptop)
   - [x] **Restrict who can push to matching branches** — leave it empty (only via PR)
   - [x] **Do not allow force pushes**
   - [x] **Do not allow deletions**

5. Click **Create** (or **Save changes**)

## 3. Set the repo description + topics (3 minutes)

This is what shows up under the repo name on GitHub, and it's what
crawlers / search index, so it matters for discoverability.

1. Back to the main repo page → top right, click the **⚙ (gear)** next to "About"
2. **Description:**
   ```
   Single-binary Rust microVM for AI-agent code execution. ~12 ms cold start, ~0.5 MiB per-fork memory, MAP_PRIVATE CoW fork-many.
   ```
3. **Website:** leave blank for now (or set to the landing page URL once you enable Pages — step 4)
4. **Topics** (add each, one at a time):
   ```
   rust
   microvm
   kvm
   ai-agents
   sandbox
   code-execution
   snapshot
   firecracker-alternative
   serverless
   virtualization
   ```
5. **Releases / Packages / Deployments:** uncheck (nothing to show yet)
6. Click **Save changes**

## 4. (Optional, recommended) Enable GitHub Pages for the landing page (5 minutes)

`docs/landing.html` is a single-file landing page. Hosting it on GitHub
Pages gives you a URL you can put in your bio / tweet / HN profile.

1. **Settings → Pages** (left sidebar)
2. **Source:** Deploy from a branch
3. **Branch:** `main` → `/docs` → **Save**
4. Wait ~1 minute, then the URL appears at the top of the Pages page:
   `https://ip888.github.io/rust-nano-vm/landing.html`
5. Go back to step 3, paste that URL into the "Website" field of the About card.

(If Pages serves `landing.html` from the URL but you want it at the
root, rename `docs/landing.html` to `docs/index.html` in a follow-up
PR. Either works.)

## 5. Verify everything once

Open these in a private/incognito window (so you're not logged in as
yourself — this is what the world sees):

- https://github.com/ip888/rust-nano-vm — should load. About card shows the description + topics.
- https://github.com/ip888/rust-nano-vm/blob/main/README.md — renders with numbers up top.
- https://github.com/ip888/rust-nano-vm/blob/main/docs/blog/01-mmap-private.md — renders.
- https://github.com/ip888/rust-nano-vm/blob/main/docs/blog/02-snapshot-restore.md — renders.
- https://ip888.github.io/rust-nano-vm/landing.html — renders (if you did step 4).

If any of those fail, fix before launching.

## 6. Same flip for nanolambda (when you're ready)

After rust-nano-vm launches successfully, do the same 6 steps for
`nanolambda` with these adjustments:

- **License:** add `LICENSE-APACHE` alongside the existing MIT one;
  change `Cargo.toml` `license = "MIT"` → `license = "Apache-2.0 OR MIT"`.
  (Doing this *before* the public flip avoids "we relicensed it" looking
  awkward in the issue tracker.)
- **Description:** the repositioning from
  [`docs/integration/nanolambda-readme-draft.md`](../integration/nanolambda-readme-draft.md)
  in this repo.
- **Topics:** add `rust-nano-vm` so the two repos are graph-linked.

## What you do NOT need to do today

- Don't add a Code of Conduct yet (do it after first external PR).
- Don't add ISSUE_TEMPLATE / PULL_REQUEST_TEMPLATE yet (premature; do
  it after first 20 issues so the template matches reality).
- Don't add Discussions yet (use Issues; Discussions becomes useful at
  scale).
- Don't add a CONTRIBUTING.md yet (the README already says "file
  issues first").
- Don't tag a `v0.1.0` release yet — release happens *with* the launch,
  not before.

All of the above are "nice to have" and end up looking premature on a
1-day-old public repo. Add them when you actually need them.
