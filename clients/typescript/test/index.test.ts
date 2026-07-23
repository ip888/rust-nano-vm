/**
 * Offline unit tests for @nanovm/sdk. Mocks `globalThis.fetch` so
 * nothing hits a real server. Coverage:
 *
 * - Client wiring: base-URL normalization, Authorization header,
 *   JSON body serialization.
 * - Typed exceptions: 401 → AuthError, 402 → PaymentRequiredError
 *   with upgradeEndpoint, 404 → NotFoundError, 409 → ConflictError,
 *   429 → RateLimitedError with retryAfter.
 * - 5xx surfaces X-Request-Id in the message.
 * - Sandbox reuse: single fork, N execs on the SAME vm id, single
 *   destroy on close.
 * - Sandbox `[Symbol.asyncDispose]` fires close on scope exit.
 * - Marketplace fork with reserved chars in the name URL-encodes.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AuthError,
  Client,
  ConflictError,
  NanovmError,
  NotFoundError,
  PaymentRequiredError,
  RateLimitedError,
  Sandbox,
} from "../src/index.js";

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

afterEach(() => {
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Client wiring
// ---------------------------------------------------------------------------

describe("Client wiring", () => {
  it("strips trailing slashes from baseUrl", () => {
    const c = new Client("http://stub/", { token: "t" });
    expect(c.baseUrl).toBe("http://stub");
    const c2 = new Client("http://stub////", { token: "t" });
    expect(c2.baseUrl).toBe("http://stub");
  });

  it("sends Bearer token + JSON body on POST", async () => {
    const { fetch, calls } = mockFetch(() => ({
      status: 200,
      body: {
        vm: { id: 1, display: "vm-1", state: "running" },
        fork_ms: 12,
        fork_count: 1,
        fork_total_ms: 12,
      },
    }));
    const client = new Client("http://stub", { token: "abc123", fetch });
    await client.forkSnapshot(42);
    expect(calls).toHaveLength(1);
    expect(calls[0]!.url).toBe("http://stub/v1/snapshots/42/fork");
    expect(calls[0]!.init.method).toBe("POST");
    const headers = calls[0]!.init.headers as Record<string, string>;
    expect(headers.Authorization).toBe("Bearer abc123");
    expect(headers["Content-Type"]).toBe("application/json");
    expect(calls[0]!.init.body).toBe("{}");
  });
});

// ---------------------------------------------------------------------------
// Typed exceptions
// ---------------------------------------------------------------------------

describe("Typed exceptions", () => {
  it("401 → AuthError carrying server code", async () => {
    const { fetch } = mockFetch(() => ({
      status: 401,
      body: { error: { code: "unauthorized", message: "bad token" } },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    await expect(client.usage()).rejects.toBeInstanceOf(AuthError);
    await expect(client.usage()).rejects.toMatchObject({
      code: "unauthorized",
      status: 401,
    });
  });

  it("402 → PaymentRequiredError carrying upgradeEndpoint", async () => {
    const { fetch } = mockFetch(() => ({
      status: 402,
      body: {
        error: {
          code: "subscription_dunning_blocked",
          message: "past due",
          upgrade_endpoint: "/v1/billing/portal",
        },
      },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    try {
      await client.usage();
      throw new Error("expected reject");
    } catch (err) {
      expect(err).toBeInstanceOf(PaymentRequiredError);
      const e = err as PaymentRequiredError;
      expect(e.code).toBe("subscription_dunning_blocked");
      expect(e.upgradeEndpoint).toBe("/v1/billing/portal");
    }
  });

  it("404 → NotFoundError", async () => {
    const { fetch } = mockFetch(() => ({
      status: 404,
      body: { error: { code: "unknown_vm", message: "gone" } },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    await expect(client.getVm(999)).rejects.toBeInstanceOf(NotFoundError);
  });

  it("409 → ConflictError", async () => {
    const { fetch } = mockFetch(() => ({
      status: 409,
      body: { error: { code: "invalid_transition", message: "already running" } },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    await expect(
      (await client.getVm(1).catch(() => null)) ??
        Promise.reject(new Error("stub")),
    ).rejects.toBeTruthy();
    // Reconfigure to make sure the second call sees the same 409 shape.
    // (We already reject above from the 404 test's leftover.) Use a
    // fresh mock instead:
    const { fetch: fresh } = mockFetch(() => ({
      status: 409,
      body: { error: { code: "invalid_transition", message: "already running" } },
    }));
    const c2 = new Client("http://stub", { token: "t", fetch: fresh });
    await expect(
      c2._request("POST", "/v1/vms/1/start", {}),
    ).rejects.toBeInstanceOf(ConflictError);
  });

  it("429 → RateLimitedError with retryAfter from header", async () => {
    const { fetch } = mockFetch(() => ({
      status: 429,
      body: { error: { code: "too_many_requests", message: "slow down" } },
      headers: { "retry-after": "7" },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    try {
      await client.forkSnapshot(1);
      throw new Error("expected reject");
    } catch (err) {
      expect(err).toBeInstanceOf(RateLimitedError);
      expect((err as RateLimitedError).retryAfter).toBe(7);
    }
  });

  it("5xx includes X-Request-Id in the message", async () => {
    const { fetch } = mockFetch(() => ({
      status: 503,
      body: { error: { code: "backend_unavailable", message: "no kvm" } },
      headers: { "x-request-id": "req-abc-123" },
    }));
    const client = new Client("http://stub", { token: "t", fetch });
    try {
      await client.usage();
      throw new Error("expected reject");
    } catch (err) {
      expect(err).toBeInstanceOf(NanovmError);
      expect((err as NanovmError).message).toContain("[request_id=req-abc-123]");
      expect((err as NanovmError).status).toBe(503);
    }
  });
});

// ---------------------------------------------------------------------------
// Sandbox fork-once-reuse-N
// ---------------------------------------------------------------------------

describe("Sandbox", () => {
  it("int snapshot: one fork POST, N execs on same vm id, one destroy", async () => {
    const { fetch, calls } = mockFetch((call) => {
      if (call.url.endsWith("/v1/snapshots/42/fork")) {
        return {
          status: 200,
          body: {
            vm: { id: 101, display: "vm-101", state: "running" },
            fork_ms: 12,
            fork_count: 1,
            fork_total_ms: 12,
          },
        };
      }
      if (call.url.endsWith("/v1/vms/101/exec")) {
        return {
          status: 200,
          body: {
            stdout: "ok\n",
            stderr: "",
            exit_code: 0,
            signal: null,
            duration_ms: 3,
          },
        };
      }
      if (call.url.endsWith("/v1/vms/101") && call.init.method === "DELETE") {
        return { status: 204 };
      }
      throw new Error(`unexpected: ${call.init.method} ${call.url}`);
    });
    const client = new Client("http://stub", { token: "t", fetch });
    const sb = client.sandbox(42);
    await sb.open();
    const r1 = await sb.executePython("print(1)");
    const r2 = await sb.executePython("print(2)");
    await sb.close();
    expect(r1.stdout).toBe("ok\n");
    expect(r2.stdout).toBe("ok\n");
    const methodUrl = calls.map((c) => [c.init.method, c.url]);
    expect(methodUrl).toEqual([
      ["POST", "http://stub/v1/snapshots/42/fork"],
      ["POST", "http://stub/v1/vms/101/exec"],
      ["POST", "http://stub/v1/vms/101/exec"],
      ["DELETE", "http://stub/v1/vms/101"],
    ]);
  });

  it("string snapshot routes to marketplace endpoint (URL-encoded)", async () => {
    let seenForkUrl = "";
    const { fetch } = mockFetch((call) => {
      if (call.url.includes("/marketplace/snapshots/") && call.url.endsWith("/fork")) {
        seenForkUrl = call.url;
        return {
          status: 200,
          body: {
            vm: { id: 55, display: "vm-55", state: "running" },
            fork_ms: 12,
            fork_count: 1,
            fork_total_ms: 12,
          },
        };
      }
      if (call.init.method === "DELETE") return { status: 204 };
      throw new Error(`unexpected: ${call.url}`);
    });
    const client = new Client("http://stub", { token: "t", fetch });
    const sb = client.sandbox("weird/name?with&chars");
    await sb.open();
    await sb.close();
    // encodeURIComponent encodes /?&; the whole name lives in one segment.
    expect(seenForkUrl.endsWith("weird%2Fname%3Fwith%26chars/fork")).toBe(true);
  });

  it("get vm before open throws sandbox_not_open", () => {
    const client = new Client("http://stub", { token: "t", fetch: fetch });
    const sb = client.sandbox(1);
    expect(() => sb.vm).toThrow(NanovmError);
    try {
      const _ = sb.vm;
      // silence unused var
      void _;
    } catch (err) {
      expect((err as NanovmError).code).toBe("sandbox_not_open");
    }
  });

  it("close is idempotent and swallows destroy errors", async () => {
    let destroyCalls = 0;
    const { fetch } = mockFetch((call) => {
      if (call.url.endsWith("/v1/snapshots/1/fork")) {
        return {
          status: 200,
          body: {
            vm: { id: 9, display: "vm-9", state: "running" },
            fork_ms: 12,
            fork_count: 1,
            fork_total_ms: 12,
          },
        };
      }
      if (call.init.method === "DELETE") {
        destroyCalls++;
        return {
          status: 500,
          body: { error: { code: "internal", message: "boom" } },
        };
      }
      throw new Error(`unexpected: ${call.url}`);
    });
    const client = new Client("http://stub", { token: "t", fetch });
    const sb = client.sandbox(1);
    await sb.open();
    await sb.close(); // must NOT throw despite the 500
    await sb.close(); // idempotent — no second DELETE
    expect(destroyCalls).toBe(1);
  });

  it("[Symbol.asyncDispose] fires close on scope exit", async () => {
    // Skip test if the runtime doesn't have Symbol.asyncDispose (Node <20).
    if (typeof Symbol.asyncDispose === "undefined") return;
    const { fetch, calls } = mockFetch((call) => {
      if (call.url.endsWith("/v1/snapshots/1/fork")) {
        return {
          status: 200,
          body: {
            vm: { id: 7, display: "vm-7", state: "running" },
            fork_ms: 12,
            fork_count: 1,
            fork_total_ms: 12,
          },
        };
      }
      if (call.init.method === "DELETE") return { status: 204 };
      throw new Error(`unexpected: ${call.url}`);
    });
    const client = new Client("http://stub", { token: "t", fetch });
    // Manual asyncDispose call (portable across TS build targets that
    // don't yet emit `await using`).
    const sb: Sandbox = client.sandbox(1);
    await sb.open();
    await sb[Symbol.asyncDispose]();
    // fork + destroy = 2 calls.
    expect(calls.length).toBe(2);
    expect(calls[1]!.init.method).toBe("DELETE");
  });
});

// ---------------------------------------------------------------------------
// Marketplace fork one-shot
// ---------------------------------------------------------------------------

describe("forkMarketplace", () => {
  it("URL-encodes reserved chars into one path segment", async () => {
    let seen = "";
    const { fetch } = mockFetch((call) => {
      seen = call.url;
      return {
        status: 200,
        body: {
          vm: { id: 2, display: "vm-2", state: "running" },
          fork_ms: 12,
          fork_count: 1,
          fork_total_ms: 12,
        },
      };
    });
    const client = new Client("http://stub", { token: "t", fetch });
    await client.forkMarketplace("weird/name?with&chars");
    expect(seen).toBe(
      "http://stub/v1/marketplace/snapshots/weird%2Fname%3Fwith%26chars/fork",
    );
  });
});
