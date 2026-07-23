/**
 * @nanovm/sdk — TypeScript client for rust-nano-vm.
 *
 * Mirrors the Python SDK's public surface (`clients/python`) with
 * TS-native async semantics. Zero runtime deps: uses global `fetch`
 * (Node 18+ / all evergreen browsers).
 *
 * @example
 * ```ts
 * import { Client } from "@nanovm/sdk";
 *
 * const client = new Client("https://api.nanovm.example.com", {
 *   token: "nv_...",
 * });
 *
 * // Fork-once-reuse-N — ~12 ms fork amortised over N execs.
 * await using sb = client.sandbox("python-3.12-ds");
 * const r1 = await sb.executePython("import pandas as pd");
 * const r2 = await sb.executePython("df = pd.DataFrame({'x':[1,2,3]})");
 * console.log(r2.stdout);
 * ```
 */

// ---------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------

/**
 * Base error for every failure raised by the SDK. Carries the stable
 * server-side `code` string (e.g. `"unknown_vm"`) and HTTP `status`.
 * Callers should match on `code`; the message is free to change
 * between server releases.
 */
export class NanovmError extends Error {
  code?: string;
  status?: number;

  constructor(message: string, opts: { code?: string; status?: number } = {}) {
    super(message);
    this.name = "NanovmError";
    this.code = opts.code;
    this.status = opts.status;
  }
}

/** 401 — missing or invalid bearer token. */
export class AuthError extends NanovmError {
  constructor(message: string, opts: { code?: string } = {}) {
    super(message, { code: opts.code, status: 401 });
    this.name = "AuthError";
  }
}

/** 404 — unknown VM or snapshot id. */
export class NotFoundError extends NanovmError {
  constructor(message: string, opts: { code?: string } = {}) {
    super(message, { code: opts.code, status: 404 });
    this.name = "NotFoundError";
  }
}

/** 409 — invalid state transition (e.g. start an already-running VM). */
export class ConflictError extends NanovmError {
  constructor(message: string, opts: { code?: string } = {}) {
    super(message, { code: opts.code, status: 409 });
    this.name = "ConflictError";
  }
}

/**
 * 429 — per-token fork quota exhausted. `retryAfter` is the seconds
 * the server's `Retry-After` header asks the caller to wait before
 * retrying. Absent → default to 0.
 */
export class RateLimitedError extends NanovmError {
  retryAfter: number;
  constructor(message: string, retryAfter: number) {
    super(message, { code: "too_many_requests", status: 429 });
    this.name = "RateLimitedError";
    this.retryAfter = retryAfter;
  }
}

/**
 * 402 — subscription dunning-blocked (past_due / unpaid / canceled
 * past the grace window). `upgradeEndpoint` is the relative API path
 * (typically `/v1/billing/portal`) the client can hit to obtain a
 * live Stripe billing-portal URL — dashboards render a "Manage
 * billing" CTA off this.
 */
export class PaymentRequiredError extends NanovmError {
  upgradeEndpoint?: string;
  constructor(
    message: string,
    opts: { code?: string; upgradeEndpoint?: string } = {},
  ) {
    super(message, { code: opts.code, status: 402 });
    this.name = "PaymentRequiredError";
    this.upgradeEndpoint = opts.upgradeEndpoint;
  }
}

// ---------------------------------------------------------------------------
// DTOs — mirror the OpenAPI schemas in docs/openapi.json.
// ---------------------------------------------------------------------------

export type VmStateDto = "created" | "running" | "stopped";

export interface VmHandleDto {
  id: number;
  display: string;
  state: VmStateDto;
}

export interface VmSummary {
  id: number;
  display: string;
  state: VmStateDto;
  vcpus?: number;
  memory_mib?: number;
  kernel_cmdline?: string;
  snapshot_source?: string;
}

export interface VmListResponse {
  vms: VmSummary[];
  next?: number;
}

export interface SnapshotDto {
  id: number;
  display: string;
}

export interface ExecResult {
  stdout: string;
  stderr: string;
  exit_code: number | null;
  signal: number | null;
  duration_ms: number;
}

/** Chunk of streaming exec output. `data` is base64-decoded to a `Uint8Array`. */
export interface ExecChunk {
  kind: "stdout" | "stderr";
  data: Uint8Array;
}

/** Terminal event yielded exactly once from `execStream`. */
export interface ExecExit {
  kind: "exit";
  exit_code: number | null;
  signal: number | null;
  duration_ms: number;
}

export type ExecStreamEvent = ExecChunk | ExecExit;

export interface SandboxResult {
  stdout: string;
  stderr: string;
  exit_code: number;
  duration_ms: number;
  cold_start: boolean;
}

export interface Health {
  ok: boolean;
  backend: string;
  version: string;
  uptime_secs: number;
  started_at: string;
}

export interface Usage {
  token: string;
  fork_count: number;
  fork_total_ms: number;
}

/** Response body of `POST /v1/snapshots/:id/fork` and marketplace-fork. */
export interface ForkResponse {
  vm: VmHandleDto;
  fork_ms: number;
  fork_count: number;
  fork_total_ms: number;
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

export interface ClientOptions {
  token: string;
  /** Per-request timeout in ms. Default 30000. */
  timeoutMs?: number;
  /** Custom fetch. Default: global `fetch`. Useful for tests. */
  fetch?: typeof fetch;
}

/** Thin async wrapper over the control-plane REST surface. */
export class Client {
  readonly baseUrl: string;
  private readonly token: string;
  private readonly timeoutMs: number;
  private readonly fetchImpl: typeof fetch;

  constructor(baseUrl: string, opts: ClientOptions) {
    // Strip trailing slash so `${baseUrl}${path}` never doubles up.
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.token = opts.token;
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    this.fetchImpl = opts.fetch ?? fetch;
  }

  /** Public — do not call unless you're writing a subclass. */
  async _request<T>(
    method: string,
    path: string,
    body?: unknown,
    extra: { headers?: Record<string, string>; timeoutMs?: number } = {},
  ): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    const headers: Record<string, string> = {
      Accept: "application/json",
      Authorization: `Bearer ${this.token}`,
      ...(extra.headers ?? {}),
    };
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const controller = new AbortController();
    const timeoutMs = extra.timeoutMs ?? this.timeoutMs;
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    init.signal = controller.signal;
    let resp: Response;
    try {
      resp = await this.fetchImpl(url, init);
    } finally {
      clearTimeout(timer);
    }
    return handleResponse<T>(resp);
  }

  // ---- VM lifecycle --------------------------------------------------

  async createVm(config: Record<string, unknown> = {}): Promise<Vm> {
    const dto = await this._request<VmHandleDto>("POST", "/v1/vms", config);
    return new Vm(this, dto);
  }

  async getVm(id: number): Promise<Vm> {
    const dto = await this._request<VmHandleDto>("GET", `/v1/vms/${id}`);
    return new Vm(this, dto);
  }

  async listVms(
    opts: { cursor?: number; limit?: number } = {},
  ): Promise<{ items: VmSummary[]; nextCursor: number | null }> {
    const q = queryString({ after: opts.cursor, limit: opts.limit });
    const resp = await this._request<VmListResponse>(
      "GET",
      `/v1/vms${q}`,
    );
    return { items: resp.vms, nextCursor: resp.next ?? null };
  }

  async destroyVm(id: number): Promise<void> {
    await this._request<null>("DELETE", `/v1/vms/${id}`);
  }

  // ---- Snapshot / fork ----------------------------------------------

  async snapshotVm(id: number): Promise<Snapshot> {
    const dto = await this._request<SnapshotDto>(
      "POST",
      `/v1/vms/${id}/snapshot`,
      {},
    );
    return new Snapshot(this, dto);
  }

  async forkSnapshot(id: number): Promise<Vm> {
    const resp = await this._request<ForkResponse>(
      "POST",
      `/v1/snapshots/${id}/fork`,
      {},
    );
    return new Vm(this, resp.vm);
  }

  async forkMarketplace(name: string): Promise<Vm> {
    const encoded = encodeURIComponent(name);
    const resp = await this._request<ForkResponse>(
      "POST",
      `/v1/marketplace/snapshots/${encoded}/fork`,
      {},
    );
    return new Vm(this, resp.vm);
  }

  // ---- Sandbox (fork-once-reuse-N) ----------------------------------

  /** Build a `Sandbox` handle. Open it with `await using` or `.open()`. */
  sandbox(snapshot: number | string): Sandbox {
    return new Sandbox(this, snapshot);
  }

  // ---- One-shot sandbox actions -------------------------------------

  async executePython(
    code: string,
    opts: { snapshot: number | string; timeoutMs?: number },
  ): Promise<SandboxResult> {
    return this._sandboxInvoke("execute_python", { code }, opts);
  }

  async executeShell(
    cmd: string,
    opts: { snapshot: number | string; timeoutMs?: number },
  ): Promise<SandboxResult> {
    return this._sandboxInvoke("execute_shell", { cmd }, opts);
  }

  async readFile(
    path: string,
    opts: { snapshot: number | string; timeoutMs?: number },
  ): Promise<SandboxResult> {
    return this._sandboxInvoke("read_file", { path }, opts);
  }

  async writeFile(
    path: string,
    data: string,
    opts: {
      snapshot: number | string;
      mode?: number;
      timeoutMs?: number;
    },
  ): Promise<SandboxResult> {
    return this._sandboxInvoke(
      "write_file",
      { path, data, mode: opts.mode },
      opts,
    );
  }

  async listFiles(
    path: string,
    opts: { snapshot: number | string; timeoutMs?: number },
  ): Promise<SandboxResult> {
    return this._sandboxInvoke("list_files", { path }, opts);
  }

  private async _sandboxInvoke(
    action: string,
    args: Record<string, unknown>,
    opts: { snapshot: number | string; timeoutMs?: number },
  ): Promise<SandboxResult> {
    const body: Record<string, unknown> = { action, ...args };
    if (typeof opts.snapshot === "number") {
      body.snapshot_id = opts.snapshot;
    } else {
      body.marketplace_name = opts.snapshot;
    }
    return this._request<SandboxResult>(
      "POST",
      "/v1/sandbox/invoke",
      body,
      opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {},
    );
  }

  // ---- Meta ---------------------------------------------------------

  health(): Promise<Health> {
    return this._request<Health>("GET", "/v1/health");
  }

  usage(): Promise<Usage> {
    return this._request<Usage>("GET", "/v1/usage");
  }
}

// ---------------------------------------------------------------------------
// Vm / Snapshot / Sandbox
// ---------------------------------------------------------------------------

/** Handle for one VM on the control plane. */
export class Vm {
  readonly id: number;
  readonly display: string;
  state: VmStateDto;

  constructor(
    private readonly client: Client,
    dto: VmHandleDto,
  ) {
    this.id = dto.id;
    this.display = dto.display;
    this.state = dto.state;
  }

  async start(): Promise<void> {
    await this.client._request<null>("POST", `/v1/vms/${this.id}/start`, {});
    this.state = "running";
  }

  async stop(): Promise<void> {
    await this.client._request<null>("POST", `/v1/vms/${this.id}/stop`, {});
    this.state = "stopped";
  }

  async destroy(): Promise<void> {
    await this.client._request<null>("DELETE", `/v1/vms/${this.id}`);
  }

  async snapshot(): Promise<Snapshot> {
    return this.client.snapshotVm(this.id);
  }

  async exec(
    program: string,
    opts: { args?: string[]; timeoutMs?: number } = {},
  ): Promise<ExecResult> {
    return this.client._request<ExecResult>(
      "POST",
      `/v1/vms/${this.id}/exec`,
      { program, args: opts.args ?? [] },
      opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {},
    );
  }

  /**
   * Stream exec output as it arrives — for long-running programs
   * where you want tail behaviour instead of full-buffer.
   *
   * Yields `ExecChunk` events (stdout/stderr slices) followed by
   * exactly one terminal `ExecExit`. After `ExecExit` the iterator
   * is exhausted. Errors raised BEFORE the stream opens surface
   * synchronously via `NanovmError` subclasses; errors mid-stream
   * throw from the iterator.
   */
  execStream(
    program: string,
    opts: { args?: string[] } = {},
  ): AsyncGenerator<ExecStreamEvent, void, void> {
    return execStreamImpl(this.client, this.id, program, opts.args ?? []);
  }
}

export class Snapshot {
  readonly id: number;
  readonly display: string;

  constructor(
    private readonly client: Client,
    dto: SnapshotDto,
  ) {
    this.id = dto.id;
    this.display = dto.display;
  }

  fork(): Promise<Vm> {
    return this.client.forkSnapshot(this.id);
  }
}

/**
 * Fork-once-reuse-N handle. Opens one VM on `open()` / `__enter__`,
 * uses it for every convenience call in between, destroys it on
 * `close()` / `__exit__`.
 *
 * Prefer `await using` where available (Node ≥ 20, TS ≥ 5.2):
 *
 * ```ts
 * await using sb = client.sandbox(42);
 * console.log((await sb.executePython("print(1+1)")).stdout);
 * // sb.close() fires automatically when the scope exits.
 * ```
 *
 * Fall back to explicit `open()` / `close()` on older runtimes.
 */
export class Sandbox {
  private vm_: Vm | null = null;

  constructor(
    private readonly client: Client,
    private readonly snapshot: number | string,
  ) {}

  /** Fork the snapshot and hold the returned VM. Idempotent. */
  async open(): Promise<Vm> {
    if (this.vm_ !== null) return this.vm_;
    if (typeof this.snapshot === "number") {
      this.vm_ = await this.client.forkSnapshot(this.snapshot);
    } else {
      this.vm_ = await this.client.forkMarketplace(this.snapshot);
    }
    return this.vm_;
  }

  /** Destroy the held VM. Errors are silently swallowed so the
   *  caller's cleanup path isn't blocked. Idempotent. */
  async close(): Promise<void> {
    if (this.vm_ === null) return;
    const held = this.vm_;
    this.vm_ = null;
    try {
      await held.destroy();
    } catch {
      // Best-effort — destroy on an already-gone VM shouldn't
      // propagate.
    }
  }

  /** `await using` support (TC39 explicit-resource-management). */
  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  /**
   * The held VM. Throws `NanovmError("sandbox_not_open")` when
   * called before `open()` (or after `close()`).
   */
  get vm(): Vm {
    if (this.vm_ === null) {
      throw new NanovmError(
        "Sandbox not opened — call `await sb.open()` or use `await using sb = client.sandbox(...)`",
        { code: "sandbox_not_open" },
      );
    }
    return this.vm_;
  }

  async executePython(
    code: string,
    opts: { timeoutMs?: number } = {},
  ): Promise<ExecResult> {
    await this.open();
    return this.vm.exec("python3", {
      args: ["-c", code],
      ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
    });
  }

  async executeShell(
    cmd: string,
    opts: { timeoutMs?: number } = {},
  ): Promise<ExecResult> {
    await this.open();
    return this.vm.exec("sh", {
      args: ["-c", cmd],
      ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
    });
  }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async function handleResponse<T>(resp: Response): Promise<T> {
  const text = await resp.text();
  const parsed: unknown = text ? safeJson(text) : null;
  if (resp.ok) {
    // 204 No Content responses have empty bodies — cast to `null`
    // and let the caller's generic parameter accept it.
    return parsed as T;
  }
  const requestId = resp.headers.get("x-request-id");
  const err = parsed as {
    error?: { code?: string; message?: string; upgrade_endpoint?: string };
  } | null;
  const code = err?.error?.code;
  const rawMessage = err?.error?.message ?? text ?? resp.statusText;
  const message =
    requestId && resp.status >= 500
      ? `HTTP ${resp.status}: ${rawMessage} [request_id=${requestId}]`
      : `HTTP ${resp.status}: ${rawMessage}`;
  switch (resp.status) {
    case 401:
      return Promise.reject(
        new AuthError(message, code ? { code } : {}),
      );
    case 402: {
      const opts: { code?: string; upgradeEndpoint?: string } = {};
      if (code !== undefined) opts.code = code;
      if (err?.error?.upgrade_endpoint !== undefined)
        opts.upgradeEndpoint = err.error.upgrade_endpoint;
      return Promise.reject(new PaymentRequiredError(message, opts));
    }
    case 404:
      return Promise.reject(
        new NotFoundError(message, code ? { code } : {}),
      );
    case 409:
      return Promise.reject(
        new ConflictError(message, code ? { code } : {}),
      );
    case 429: {
      const raw = resp.headers.get("retry-after");
      const retryAfter = raw ? parseInt(raw, 10) || 0 : 0;
      return Promise.reject(new RateLimitedError(message, retryAfter));
    }
    default:
      return Promise.reject(
        new NanovmError(message, {
          status: resp.status,
          ...(code !== undefined ? { code } : {}),
        }),
      );
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function queryString(params: Record<string, unknown>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null) continue;
    parts.push(`${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`);
  }
  return parts.length > 0 ? `?${parts.join("&")}` : "";
}

/**
 * Base64-decode into a `Uint8Array` — no assumption of UTF-8 boundary
 * alignment (server sends raw bytes; exec streams can carry binary
 * data mid-line). Uses `atob` (universally available in Node 18+ and
 * every browser).
 */
function base64ToBytes(b64: string): Uint8Array {
  const binaryString = atob(b64);
  const out = new Uint8Array(binaryString.length);
  for (let i = 0; i < binaryString.length; i++) {
    out[i] = binaryString.charCodeAt(i);
  }
  return out;
}

/**
 * Line-oriented SSE parser. Yields ExecChunk / ExecExit events from
 * the streaming exec endpoint. Handles keepalive comments (`:` prefix)
 * and multi-line `data:` blocks per the SSE spec.
 */
async function* execStreamImpl(
  client: Client,
  vmId: number,
  program: string,
  args: string[],
): AsyncGenerator<ExecStreamEvent, void, void> {
  const url = `${client.baseUrl}/v1/vms/${vmId}/exec/stream`;
  const resp = await fetch(url, {
    method: "POST",
    headers: {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      Authorization: (client as unknown as { token: string }).token
        ? `Bearer ${(client as unknown as { token: string }).token}`
        : "",
      "Content-Type": "application/json",
      Accept: "text/event-stream",
    },
    body: JSON.stringify({ program, args }),
  });
  if (!resp.ok) {
    await handleResponse<void>(resp); // throws
    return;
  }
  if (!resp.body) {
    throw new NanovmError("streaming exec response had no body", {
      status: resp.status,
    });
  }
  const decoder = new TextDecoder("utf-8");
  const reader = resp.body.getReader();
  let buf = "";
  let eventName: string | null = null;
  const dataLines: string[] = [];

  const emit = (): ExecStreamEvent | null => {
    if (eventName === null) return null;
    const dataStr = dataLines.join("\n");
    if (eventName === "stdout" || eventName === "stderr") {
      return { kind: eventName, data: base64ToBytes(dataStr) };
    }
    if (eventName === "exit") {
      const parsed = safeJson(dataStr) as {
        exit_code?: number | null;
        signal?: number | null;
        duration_ms?: number;
      } | null;
      return {
        kind: "exit",
        exit_code: parsed?.exit_code ?? null,
        signal: parsed?.signal ?? null,
        duration_ms: parsed?.duration_ms ?? 0,
      };
    }
    return null;
  };

  const flush = function* (): Generator<ExecStreamEvent> {
    const ev = emit();
    eventName = null;
    dataLines.length = 0;
    if (ev) yield ev;
  };

  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let lineEnd: number;
    while ((lineEnd = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, lineEnd).replace(/\r$/, "");
      buf = buf.slice(lineEnd + 1);
      if (line === "") {
        yield* flush();
        continue;
      }
      if (line.startsWith(":")) continue; // keepalive
      if (line.startsWith("event:")) {
        eventName = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        dataLines.push(line.slice(5).trimStart());
      }
    }
  }
  // Trailing partial line (missing final newline) — flush what we have.
  if (buf.length > 0) {
    yield* flush();
  }
}
