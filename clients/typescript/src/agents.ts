/**
 * Framework-agnostic agent-tool helpers for `@nanovm/sdk`.
 *
 * Mirrors [`nanovm.agents.openai`](../../python/src/nanovm/agents/openai.py)
 * from the Python SDK: a JSON-Schema tool descriptor list plus a
 * dispatcher. The OpenAI function-tool shape is the lingua franca of
 * every current agent framework — it drops straight into:
 *
 * - **OpenAI Chat Completions / Responses / Assistants** — pass as
 *   `tools=[...]` verbatim.
 * - **LangChain.js** — `llm.bindTools(nanovmToolSchemas())` accepts
 *   the OpenAI shape directly.
 * - **Vercel AI SDK** — wrap with `jsonSchema()` from `ai` when
 *   passing to `streamText({tools: {...}})`.
 * - **Anthropic tool use** — `input_schema` field is `parameters`
 *   under a different name; a two-line remap adapts.
 *
 * Zero peer deps: the schema is plain objects and the dispatcher
 * takes a string. Users bring their own agent framework.
 *
 * @example OpenAI Chat Completions
 * ```ts
 * import OpenAI from "openai";
 * import { Client } from "@nanovm/sdk";
 * import { nanovmToolSchemas, dispatchNanovmToolCall } from "@nanovm/sdk/agents";
 *
 * const llm = new OpenAI();
 * const sandbox = new Client("http://localhost:8080", { token: "dev-token" });
 * const tools = nanovmToolSchemas();
 *
 * const messages: unknown[] = [
 *   { role: "user", content: "Compute pi to 40 digits" },
 * ];
 * while (true) {
 *   const rsp = await llm.chat.completions.create({
 *     model: "gpt-4o", messages: messages as never, tools,
 *   });
 *   const msg = rsp.choices[0].message;
 *   messages.push(msg);
 *   if (!msg.tool_calls?.length) { console.log(msg.content); break; }
 *   for (const call of msg.tool_calls) {
 *     const content = await dispatchNanovmToolCall(
 *       sandbox, call.function.name, call.function.arguments,
 *       { snapshot: "python-3.12-minimal" },
 *     );
 *     messages.push({ role: "tool", tool_call_id: call.id, content });
 *   }
 * }
 * ```
 */

import type { Client, SandboxResult } from "./index.js";

/**
 * The two nanovm sandbox actions we surface to the model, packaged as
 * OpenAI function-tool descriptors. Copy the object into any
 * `tools=[...]` array the framework expects.
 *
 * Adding a new action is a two-step change: append its descriptor
 * here, then handle it in {@link dispatchNanovmToolCall}. Keep the
 * two lists byte-for-byte in sync — the dispatcher returns an
 * `error: unknown tool name '<name>'…` string for names that aren't
 * in the schema list (the "never throw" contract that keeps the
 * agent loop alive on a mismatch).
 */
export interface NanovmToolSchema {
  type: "function";
  function: {
    name: string;
    description: string;
    parameters: {
      type: "object";
      properties: Record<string, { type: string; description?: string }>;
      required: string[];
      additionalProperties: false;
    };
  };
}

/** Return the OpenAI `tools=[...]` list for the nanovm sandbox actions. */
export function nanovmToolSchemas(): NanovmToolSchema[] {
  return [
    {
      type: "function",
      function: {
        name: "execute_python",
        description:
          "Run a complete Python 3 program inside a fresh microVM sandbox. " +
          "Returns exit_code, stdout, and stderr. Use whenever the task " +
          "benefits from actually executing code — computation, hypothesis " +
          "testing, generating output that depends on runtime values.",
        parameters: {
          type: "object",
          properties: {
            code: {
              type: "string",
              description:
                "A complete Python 3 program. stdout is captured and " +
                "returned. Each call is fully isolated — no shared state " +
                "between calls.",
            },
          },
          required: ["code"],
          additionalProperties: false,
        },
      },
    },
    {
      type: "function",
      function: {
        name: "execute_shell",
        description:
          "Run a shell command (`sh -c <command>`) inside a fresh microVM " +
          "sandbox. Returns exit_code, stdout, and stderr. Prefer this when " +
          "you need a system binary (curl, git, grep, apt, …).",
        parameters: {
          type: "object",
          properties: {
            command: {
              type: "string",
              description: "The shell command to run.",
            },
          },
          required: ["command"],
          additionalProperties: false,
        },
      },
    },
  ];
}

/**
 * Options for {@link dispatchNanovmToolCall}. `snapshot` is required
 * either here or via the server's `NANOVM_SANDBOX_SNAPSHOT_ID` env
 * (the server falls back to that when the client omits it).
 */
export interface DispatchOptions {
  /** Numeric snapshot id OR marketplace entry name to fork before
   *  running the action. When omitted, the server's env-var default
   *  applies. */
  snapshot?: number | string;
  /** Per-call timeout override (ms). */
  timeoutMs?: number;
}

/**
 * Dispatch one OpenAI-shape tool call to the nanovm sandbox.
 *
 * `argumentsJson` is the JSON string OpenAI hands you on
 * `ToolCall.function.arguments` (Chat Completions / Responses /
 * Assistants — same field in all three). Parses it, invokes the
 * right client action, and returns a compact string suitable for
 * the tool-role message body:
 *
 *   `{"role": "tool", "tool_call_id": call.id, "content": <return>}`
 *
 * Every error — bad JSON, unknown tool name, network / auth / quota
 * failure — is caught and rendered as the tool result string. The
 * model sees the error and can self-correct rather than blowing up
 * the whole agent loop.
 */
export async function dispatchNanovmToolCall(
  client: Client,
  name: string,
  argumentsJson: string,
  opts: DispatchOptions = {},
): Promise<string> {
  const parsed = parseArgs(argumentsJson);
  if (parsed.kind === "error") return parsed.message;
  const args = parsed.value;

  try {
    let result: SandboxResult;
    if (name === "execute_python") {
      if (typeof args.code !== "string" || args.code === "") {
        return `error: tool 'execute_python' requires a non-empty string 'code' argument.`;
      }
      result = await callExecute(
        (snapshot) =>
          client.executePython(args.code as string, {
            snapshot,
            ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
          }),
        opts.snapshot,
      );
    } else if (name === "execute_shell") {
      if (typeof args.command !== "string" || args.command === "") {
        return `error: tool 'execute_shell' requires a non-empty string 'command' argument.`;
      }
      result = await callExecute(
        (snapshot) =>
          client.executeShell(args.command as string, {
            snapshot,
            ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
          }),
        opts.snapshot,
      );
    } else {
      return `error: unknown tool name '${name}'. Expected 'execute_python' or 'execute_shell'.`;
    }
    return formatResult(result);
  } catch (e) {
    const err = e as { name?: string; message?: string };
    return `error: ${err.name ?? "Error"}: ${err.message ?? String(e)}`;
  }
}

/**
 * Parse and validate an OpenAI tool-call `arguments` string into an
 * object we can index into. OpenAI's field is nominally a JSON string
 * of the schema's `parameters` object, but callers have been seen
 * emitting `null`, arrays, or plain literals (Anthropic tool use is
 * looser about this too). Reject anything that isn't a JSON object
 * with an actionable error string — the alternative is silently
 * executing an empty program because `args.code` would be `undefined`.
 */
function parseArgs(
  argumentsJson: string,
): { kind: "ok"; value: Record<string, unknown> } | { kind: "error"; message: string } {
  const raw = argumentsJson === "" ? "{}" : argumentsJson;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    return {
      kind: "error",
      message: `error: could not parse tool arguments as JSON: ${
        e instanceof Error ? e.message : String(e)
      }`,
    };
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return {
      kind: "error",
      message: `error: tool arguments must be a JSON object; got ${describeJsonKind(parsed)}.`,
    };
  }
  return { kind: "ok", value: parsed as Record<string, unknown> };
}

function describeJsonKind(v: unknown): string {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  return typeof v;
}

/**
 * Invoke `fn(snapshot)` — pushing `snapshot` in as required by the
 * `Client` action signatures — but treat "no snapshot passed" as a
 * hard error. The one-shot sandbox actions' `snapshot` is a required
 * parameter on the client, so if the caller forgot it we surface a
 * targeted string instead of a `TypeError` deep inside `fetch`.
 */
async function callExecute(
  fn: (snapshot: number | string) => Promise<SandboxResult>,
  snapshot: number | string | undefined,
): Promise<SandboxResult> {
  if (snapshot === undefined) {
    throw new Error(
      "dispatchNanovmToolCall: no snapshot provided. Pass " +
        "`{ snapshot: <id | marketplace-name> }` in the 4th argument " +
        "(the DispatchOptions object). If you want the server's " +
        "`NANOVM_SANDBOX_SNAPSHOT_ID` env fallback, call " +
        "`Client.executePython/executeShell` directly instead of going " +
        "through this dispatcher.",
    );
  }
  return fn(snapshot);
}

/**
 * Compact but complete rendering — the model sees exit_code plus
 * both output streams. Matches the Python SDK's `_format_result`
 * shape so a prompt authored against one runs against the other.
 */
function formatResult(result: SandboxResult): string {
  const parts = [`exit_code=${result.exit_code}`];
  if (result.stdout) parts.push(`stdout:\n${result.stdout.replace(/\s+$/, "")}`);
  if (result.stderr) parts.push(`stderr:\n${result.stderr.replace(/\s+$/, "")}`);
  return parts.join("\n");
}
