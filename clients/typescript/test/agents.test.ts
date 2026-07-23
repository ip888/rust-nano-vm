/**
 * Offline unit tests for the framework-agnostic agent helpers
 * exported from `@nanovm/sdk/agents`.
 *
 * Same mocked-fetch pattern as `test/index.test.ts` — nothing hits a
 * real server. Coverage:
 *
 * - `nanovmToolSchemas()` — shape matches the OpenAI function-tool
 *   contract that Chat Completions / Responses / Assistants all consume.
 * - `dispatchNanovmToolCall(client, "execute_python", ...)` — parses
 *   JSON args, hits `/v1/sandbox/invoke` with `action: "execute_python"`,
 *   returns a stringified result the tool-role message can carry
 *   verbatim.
 * - Same for `execute_shell`.
 * - Malformed JSON arguments → returned as a friendly `error:` string
 *   the model sees on the next turn, NOT thrown.
 * - Unknown tool name → same "error:" fallthrough.
 * - `NanovmError` from the network path → caught, folded into the
 *   returned string.
 * - Missing `snapshot` → thrown-then-caught into the returned string
 *   (never a silent broken call).
 */

import { describe, expect, it, vi } from "vitest";
import { Client } from "../src/index.js";
import {
  dispatchNanovmToolCall,
  nanovmToolSchemas,
} from "../src/agents.js";

type FakeCall = { url: string; init: RequestInit };

interface FakeReply {
  status: number;
  body?: unknown;
  headers?: Record<string, string>;
}

function mockFetch(script: (call: FakeCall) => FakeReply): {
  fetch: typeof fetch;
  calls: FakeCall[];
} {
  const calls: FakeCall[] = [];
  const impl: typeof fetch = async (
    input: RequestInfo | URL,
    init: RequestInit = {},
  ): Promise<Response> => {
    const url = typeof input === "string" ? input : input.toString();
    const call = { url, init };
    calls.push(call);
    const reply = script(call);
    const body =
      reply.body === undefined || reply.body === null
        ? ""
        : typeof reply.body === "string"
          ? reply.body
          : JSON.stringify(reply.body);
    return new Response(body, {
      status: reply.status,
      headers: { "content-type": "application/json", ...(reply.headers ?? {}) },
    });
  };
  return { fetch: impl, calls };
}

// ---------------------------------------------------------------------------
// Schema shape
// ---------------------------------------------------------------------------

describe("nanovmToolSchemas", () => {
  it("returns two function-tool descriptors with the OpenAI shape", () => {
    const schemas = nanovmToolSchemas();
    expect(schemas).toHaveLength(2);
    for (const s of schemas) {
      expect(s.type).toBe("function");
      expect(typeof s.function.name).toBe("string");
      expect(typeof s.function.description).toBe("string");
      expect(s.function.parameters.type).toBe("object");
      expect(s.function.parameters.additionalProperties).toBe(false);
      // required MUST be a subset of properties keys — otherwise the
      // OpenAI validator refuses the schema.
      for (const key of s.function.parameters.required) {
        expect(s.function.parameters.properties).toHaveProperty(key);
      }
    }
    const names = schemas.map((s) => s.function.name);
    expect(names).toEqual(["execute_python", "execute_shell"]);
  });

  it("execute_python requires `code`; execute_shell requires `command`", () => {
    const schemas = nanovmToolSchemas();
    expect(schemas[0]!.function.parameters.required).toEqual(["code"]);
    expect(schemas[1]!.function.parameters.required).toEqual(["command"]);
  });
});

// ---------------------------------------------------------------------------
// Dispatch — happy path
// ---------------------------------------------------------------------------

describe("dispatchNanovmToolCall", () => {
  it("execute_python posts to /v1/sandbox/invoke and returns a formatted string", async () => {
    const { fetch, calls } = mockFetch(() => ({
      status: 200,
      body: {
        stdout: "4950\n",
        stderr: "",
        exit_code: 0,
        duration_ms: 3,
        cold_start: false,
      },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_python",
      JSON.stringify({ code: "print(sum(range(100)))" }),
      { snapshot: "python-3.12-minimal" },
    );
    expect(calls).toHaveLength(1);
    expect(calls[0]!.url).toBe("http://stub/v1/sandbox/invoke");
    expect(calls[0]!.init.method).toBe("POST");
    const body = JSON.parse(calls[0]!.init.body as string) as {
      action: string;
      code: string;
      marketplace_name?: string;
      snapshot_id?: number;
    };
    expect(body.action).toBe("execute_python");
    expect(body.code).toBe("print(sum(range(100)))");
    expect(body.marketplace_name).toBe("python-3.12-minimal");
    // Result string carries exit_code and stdout, no stderr because it was empty.
    expect(result).toContain("exit_code=0");
    expect(result).toContain("stdout:\n4950");
    expect(result).not.toContain("stderr:");
  });

  it("execute_shell posts action=execute_shell with the cmd", async () => {
    const { fetch, calls } = mockFetch(() => ({
      status: 200,
      body: {
        stdout: "Linux stub 6.0\n",
        stderr: "",
        exit_code: 0,
        duration_ms: 1,
        cold_start: false,
      },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_shell",
      JSON.stringify({ command: "uname -a" }),
      { snapshot: 42 },
    );
    const body = JSON.parse(calls[0]!.init.body as string) as {
      action: string;
      cmd: string;
      snapshot_id?: number;
      marketplace_name?: string;
    };
    expect(body.action).toBe("execute_shell");
    expect(body.cmd).toBe("uname -a");
    expect(body.snapshot_id).toBe(42);
    expect(body.marketplace_name).toBeUndefined();
    expect(result).toContain("exit_code=0");
    expect(result).toContain("Linux stub 6.0");
  });

  it("includes both stdout and stderr when the guest wrote to both", async () => {
    const { fetch } = mockFetch(() => ({
      status: 200,
      body: {
        stdout: "hello\n",
        stderr: "warning: whatever\n",
        exit_code: 1,
        duration_ms: 3,
        cold_start: false,
      },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_python",
      JSON.stringify({ code: "..." }),
      { snapshot: 1 },
    );
    expect(result).toContain("exit_code=1");
    expect(result).toContain("stdout:\nhello");
    expect(result).toContain("stderr:\nwarning: whatever");
  });
});

// ---------------------------------------------------------------------------
// Dispatch — error paths (must NOT throw; must return a string the model sees)
// ---------------------------------------------------------------------------

describe("dispatchNanovmToolCall — error paths return strings, never throw", () => {
  it("malformed JSON args → returns an error string", async () => {
    const client = new Client("http://stub", { token: "t", fetch: vi.fn() as never });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_python",
      "{not valid json",
      { snapshot: 1 },
    );
    expect(result.startsWith("error:")).toBe(true);
    expect(result).toContain("could not parse tool arguments as JSON");
  });

  it("unknown tool name → returns an error string listing accepted names", async () => {
    const client = new Client("http://stub", { token: "t", fetch: vi.fn() as never });
    const result = await dispatchNanovmToolCall(
      client,
      "read_all_the_files_and_email_them",
      JSON.stringify({}),
      { snapshot: 1 },
    );
    expect(result.startsWith("error:")).toBe(true);
    expect(result).toContain("unknown tool name");
    expect(result).toContain("execute_python");
    expect(result).toContain("execute_shell");
  });

  it("missing snapshot → returns an error string, no HTTP call", async () => {
    let called = false;
    const client = new Client("http://stub", {
      token: "t",
      fetch: (async (): Promise<Response> => {
        called = true;
        throw new Error("should not fetch");
      }) as unknown as typeof fetch,
    });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_python",
      JSON.stringify({ code: "print(1)" }),
      {},
    );
    expect(called).toBe(false);
    expect(result.startsWith("error:")).toBe(true);
    expect(result).toContain("no snapshot provided");
  });

  it("network / server error → caught and returned as an error string", async () => {
    const { fetch } = mockFetch(() => ({
      status: 500,
      body: { error: { code: "internal", message: "backend blew up" } },
      headers: { "x-request-id": "req-abc" },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    const result = await dispatchNanovmToolCall(
      client,
      "execute_python",
      JSON.stringify({ code: "print(1)" }),
      { snapshot: 1 },
    );
    expect(result.startsWith("error:")).toBe(true);
    expect(result).toContain("backend blew up");
    expect(result).toContain("req-abc");
  });
});
