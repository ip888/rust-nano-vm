# LinkedIn launch post

Single post — LinkedIn's algorithm penalises threads, and its
audience skews further from the Twitter/HN crowd (more founders,
more procurement, more "does this fit our stack?" than "show me
the syscalls"). Reshape the same story with that in mind.

Fire 3 hours after the Show HN. LinkedIn reach peaks 3 hours after
posting; stacking the launch on top of HN referrals compounds.

---

## Post body

```
We shipped nanovm today — a sandbox designed specifically for AI
agents that run code as tool calls.

The problem: every current option forces a bad trade-off.

• Docker exec: works, but 100-500 ms per call. An agent that
  makes 50 tool calls per task pays 5-25 seconds of pure sandbox
  overhead you can't get back.
• E2B / Modal Sandbox: purpose-built for this shape, but
  proprietary managed services with per-second billing.
• Just trusting the model: the honest industry default,
  terrifying in production.

nanovm collapses cold-start to ~12 ms by making snapshot + fork
a first-class primitive: one "golden" agent VM, snapshotted
once, then each tool call maps the snapshot's memory file
MAP_PRIVATE and lets KVM restore from it. Real KVM microVM with
its own kernel + rootfs; hardware isolation from the host, not
just namespaces.

What that adds up to for the agent teams we've been talking to:

→ 100-call agent tasks pay ~1.2 s total sandbox overhead
  (vs 15-40 s on managed alternatives)
→ Self-hostable on your own KVM host — Apache 2.0 / MIT
→ Single binary; Helm chart for k8s
→ Enterprise plumbing already in the box: SIEM audit sink,
  RBAC, air-gap install docs, Stripe-metered billing
→ Python + TypeScript SDKs with one-import LangChain.js /
  Vercel AI SDK / OpenAI Assistants adapters

Free tier is a real production plan (5 forks/sec, 10K/mo,
no card). Pro is $29/mo for unlimited monthly forks. Team +
Enterprise scale from there.

Try it in your browser (there's an in-app playground behind
signup that runs Python in a real KVM microVM in under a
second):

https://nanovm.example.com

Would especially love feedback from anyone shipping AI agents
to a regulated industry — the "prove hardware isolation" part
of a security review has been the loudest ask.
```

## Media

Attach the same landing-page LiveForkBenchmark GIF from the Twitter thread. LinkedIn's autoplay preview loops it in the feed; users don't have to click to see the ~12 ms hit rate.

## First-comment reply (post yourself, 10 min after)

```
Happy to answer specific questions — I'm most curious about
which agent framework you're wiring this into.

If it helps: the framework adapters ship in one SDK import.
Both LangChain.js's bindTools and Vercel AI SDK's
streamText({tools}) accept the OpenAI-shape descriptors we
return, so wire-up is three lines regardless.

Details + code:
https://nanovm.example.com/blog/06-langchain-js-execute-python
```

## Second-comment reply

If someone asks about self-host or enterprise:

```
Both live at the same code path — the "hosted" plan is us
running the same binary you'd `helm install`. Enterprise
add-ons (SSO, custom RBAC, dedicated support) sit on top;
core auth + audit + rate-limiting are in the OSS build.

Deploy walkthrough:
https://github.com/ip888/rust-nano-vm/tree/main/deploy

Air-gap install (compliance shops):
https://github.com/ip888/rust-nano-vm/tree/main/deploy/enterprise
```

## Voice notes

- LinkedIn rewards paragraph structure with a blank line between
  each — the algorithm reads "structured post" as higher quality.
- The `→` bullets travel further than `-` or `•` on this
  platform. They render as decorative accents in the feed.
- Never open a LinkedIn launch with "Excited to announce". Two
  years ago that was the meta; today it reads as AI-generated
  and gets buried.
