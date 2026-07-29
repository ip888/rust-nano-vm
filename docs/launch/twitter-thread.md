# Twitter / X launch thread

Fire 15 minutes after the Show HN goes up. Total: 6 tweets.

Each tweet's `[MEDIA]` line names the asset that should be attached — record + trim these once ahead of launch. The GIFs are the reach amplifiers here; images without motion get 2–3× less impression volume on this shape of thread.

---

## Tweet 1 — the hook

```
i built a KVM microVM that forks in ~12 ms so LLM agents can
run their tool calls somewhere safe without eating 100-500 ms
of docker cold start per call

live playground, free tier, no card:
nanovm.example.com

demo GIF ↓
```

`[MEDIA]` 8-second screen recording of the landing-page LiveForkBenchmark button being clicked and the 20 forks completing (they all land in ~250 ms). Loop it. This IS the tweet — the video does more work than the copy.

---

## Tweet 2 — the shape of the argument

```
100 tool calls per agent task:

docker exec:      5-20 sec of pure sandbox overhead
E2B:              15-40 sec
modal sandbox:    ~20 sec
nanovm (~12 ms):  ~1.2 sec

your LLM roundtrip is 100× longer than the sandbox — that's the
right ratio
```

No media. Numbers-in-monospace-tweets travel farther without a visual competing with them.

---

## Tweet 3 — how

```
the trick is snapshot + fork as a first-class primitive: one
prepared "golden" agent VM, snapshot it once, then each fork
maps the snapshot memory file MAP_PRIVATE and lets KVM restore
from it

~50 lines of unsafe. the kernel does the rest

writeup: nanovm.example.com/blog/01-mmap-private
```

`[MEDIA]` Screenshot of the annotated `MAP_PRIVATE` section from `crates/vm-kvm/src/vmstate.rs` — the actual load-bearing code, syntax-highlighted, cropped to the 20 relevant lines.

---

## Tweet 4 — the SDK story

```
claude code / cursor / langchain / vercel AI SDK all take a tool
descriptor + a callback. `@nanovm/sdk/agents` ships both as one
import:

  import { nanovmToolSchemas, dispatchNanovmToolCall } from
    "@nanovm/sdk/agents";

  const tools = nanovmToolSchemas();
  llm.bindTools(tools);  // langchain: works

zero peer deps. same schema, every framework
```

`[MEDIA]` Screenshot of the 8-line code block. Terminal font, dark background.

---

## Tweet 5 — self-host escape hatch

```
if you don't want to run on the hosted plan:

- apache 2.0 / MIT dual license
- single rust binary, single helm chart
- byo snapshot store (S3 or filesystem)
- SIEM webhook audit sink, RBAC, air-gap install docs

the hosted tiers buy support + throughput + an SLA on top of
infra you don't have to run
```

No media. This is the enterprise-buyer signal — they need to see "self-host is real" before they'll trust the hosted product.

---

## Tweet 6 — the CTA

```
free tier is 5 forks/sec + 10K/mo, no card. pro is $29/mo for
unlimited monthly + 20× the burst rate.

sign up, hit the in-browser playground, paste `print(1+1)`,
watch a real KVM microVM fork in under a second:

nanovm.example.com/pricing
```

`[MEDIA]` OG image from `/pricing` (Next.js auto-generated `opengraph-image.tsx`) — the four-tier card grid.

---

## After the thread

- **Quote-tweet blog post 05** at T+2h — same audience, second impression.
- **Quote-tweet blog post 06** at T+4h — same shape, LangChain.js audience.
- Reply to every reply within 30 min for the first 6 hours. The reply/impression ratio on this shape of thread predicts second-day reach.

## Language notes

- Lowercase-first tweets travel further on this platform than sentence-case ones today. It reads as engineer-vernacular rather than marketing.
- Never lead with the URL. Lead with the hook, land the URL in the third line, keep the last line clean.
- No hashtags. #AI / #LLM / #Rust each drop reach 15–25 % on technical launches.
