// Thin fetch wrapper around the nanovm control-plane REST API.
//
// Every helper takes an optional apiKey argument; the standard use is
// to call `getSession()` first and pass session.apiKey through. That
// way handlers stay pure functions of (endpoint, body, key) — easy to
// test, and no hidden global state.
//
// All exports are typed to match the Rust DTOs the control plane
// serialises (see `crates/control-plane/src/{billing,api}.rs`).

// -------- Server base URL ----------------------------------------

/**
 * Base URL of the control-plane API. Falls back to localhost so a
 * fresh clone can `npm run dev` without an env file.
 *
 * We accept the env var only when it's non-empty after trimming.
 * `?? "..."` alone would treat an explicit empty string as valid
 * — every fetch would then go to the dashboard's own origin and
 * silently 404.
 */
export const API_BASE: string = pickApiBase(
  process.env.NEXT_PUBLIC_NANOVM_API_URL,
);

function pickApiBase(raw: string | undefined): string {
  const trimmed = raw?.trim();
  return trimmed && trimmed.length > 0 ? trimmed : "http://localhost:8080";
}

// -------- Types (mirror the Rust DTOs) ---------------------------

export interface SignupRequestBody {
  email: string;
  org: string;
}

export interface SignupRequestResponse {
  message: string;
}

export interface SignupVerifyResponse {
  org: string;
  api_key: string;
  stripe_customer_id: string;
}

export interface PlanTier {
  name: string;
  rps: number;
}

export interface PlanResponse {
  plan: PlanTier | null;
  subscription_status: string | null;
  price_id: string | null;
}

export interface UsageResponseDto {
  token: string;
  fork_count: number;
  fork_total_ms: number;
}

/** One row in `GET /v1/usage/by-org`. Shape: `org_id` + cumulative
 *  counters (no per-token field — unlike `UsageResponseDto`). The
 *  caller sees only their own row unless they hold an operator-scoped
 *  token and pass `?all=true` (server accepts `all=1` too). */
export interface UsageByOrgEntry {
  org_id: string;
  fork_count: number;
  fork_total_ms: number;
}

export interface UsageByOrgResponse {
  orgs: UsageByOrgEntry[];
}

/** Wire-format VM state — mirrors `VmStateDto` in `crates/control-plane/src/api.rs`. */
export type VmStateDto = "created" | "running" | "stopped";

/** Row in `GET /v1/vms`. Geometry fields are optional — the mock
 *  backend surfaces them; a real backend that can't may omit. */
export interface VmListEntry {
  id: number;
  display: string;
  state: VmStateDto;
  vcpus?: number;
  memory_mib?: number;
  kernel_cmdline?: string;
  /** Absolute snapshot directory the VM was restored from. Serialised
   *  from a `PathBuf` — always the server-side path, not the caller's.
   *  Field name matches the server's `VmListEntry.snapshot_dir`. */
  snapshot_dir?: string;
}

/** Cursor-paginated envelope for `GET /v1/vms`. `next` is the last
 *  returned id — pass as `after=<next>` for the next page. Absent
 *  when the page fits in one response. */
export interface VmListResponse {
  vms: VmListEntry[];
  next?: number;
}

/** Row in `GET /v1/snapshots`. Same optional-geometry pattern. */
export interface SnapshotListEntry {
  id: number;
  display: string;
  vcpu_count?: number;
  memory_bytes?: number;
  page_size?: number;
  kernel_cmdline?: string;
}

export interface SnapshotListResponse {
  snapshots: SnapshotListEntry[];
  next?: number;
}

/** A row in `GET /v1/keys`. `token` is NEVER returned by list — only mint. */
export interface KeyEntry {
  id: string;
  org: string;
  created_at: string;
}

export interface ListKeysResponse {
  keys: KeyEntry[];
}

/** `POST /v1/keys` returns the new bearer once. Never fetched again. */
export interface IssueKeyResponse {
  /** Opaque bearer token. Shown once; the server never returns it again. */
  token: string;
  id: string;
  org: string;
  created_at: string;
}

/** One entry in `GET /v1/marketplace/snapshots`. Mirrors the Rust
 *  `MarketplaceSnapshot` DTO. */
export interface MarketplaceSnapshot {
  name: string;
  description: string;
  size_bytes: number;
  kernel_url: string;
  rootfs_url: string;
  /** Optional `.tar.gz` URL of the pre-captured snapshot. When absent,
   *  the entry is browse-only — `POST .../fork` returns 501. */
  snapshot_url?: string | null;
  cmdline: string;
  labels: string[];
  maintainer: string;
}

export interface MarketplaceListResponse {
  snapshots: MarketplaceSnapshot[];
}

/** Response body of `POST /v1/marketplace/snapshots/:name/fork` and
 *  `POST /v1/snapshots/:id/fork` — the same DTO. */
export interface ForkResponse {
  vm: { id: number; display: string; state: "created" | "running" | "stopped" };
  fork_ms: number;
  fork_count: number;
  fork_total_ms: number;
}

// -------- Fetch wrappers -----------------------------------------

/**
 * Structured error surfaced to the UI. `.status` mirrors the HTTP
 * status; `.code` and `.message` come from the control-plane's
 * `{"error": {"code": "...", "message": "..."}}` envelope when
 * present, otherwise a synthetic value.
 *
 * `.upgradeEndpoint` is set on 402 Payment Required responses (dunning
 * blocks): the server points the client at the API path that returns
 * a live Stripe billing-portal URL, so the UI can render an actionable
 * "Open billing" link rather than a plain error.
 */
export class ApiError extends Error {
  status: number;
  code: string;
  upgradeEndpoint?: string;

  constructor(
    status: number,
    code: string,
    message: string,
    upgradeEndpoint?: string,
  ) {
    super(message);
    this.status = status;
    this.code = code;
    this.upgradeEndpoint = upgradeEndpoint;
    this.name = "ApiError";
  }
}

async function request<T>(
  path: string,
  init: RequestInit & { apiKey?: string } = {},
): Promise<T> {
  const { apiKey, headers, ...rest } = init;
  const h: Record<string, string> = {
    Accept: "application/json",
    ...(headers as Record<string, string> | undefined),
  };
  if (rest.body && !h["Content-Type"]) {
    h["Content-Type"] = "application/json";
  }
  if (apiKey) {
    h["Authorization"] = `Bearer ${apiKey}`;
  }
  const resp = await fetch(`${API_BASE}${path}`, {
    ...rest,
    headers: h,
  });
  const text = await resp.text();
  const parsed = text ? safeJson(text) : null;
  if (!resp.ok) {
    const errObj = parsed as {
      error?: { code?: string; message?: string; upgrade_endpoint?: string };
    };
    const code = errObj?.error?.code ?? String(resp.status);
    const message =
      errObj?.error?.message ??
      (typeof parsed === "string" ? parsed : text || resp.statusText);
    // 402 dunning responses extend the envelope with `upgrade_endpoint`
    // pointing at `/v1/billing/portal`. Preserve it on the ApiError so
    // UIs can render a "Manage billing" link instead of a dead error.
    throw new ApiError(resp.status, code, message, errObj?.error?.upgrade_endpoint);
  }
  return parsed as T;
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

// -------- Public API helpers -------------------------------------

export function requestSignup(body: SignupRequestBody): Promise<SignupRequestResponse> {
  return request<SignupRequestResponse>("/v1/signup/request", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export function verifySignup(token: string): Promise<SignupVerifyResponse> {
  return request<SignupVerifyResponse>("/v1/signup/verify", {
    method: "POST",
    body: JSON.stringify({ token }),
  });
}

export function getPlan(apiKey: string): Promise<PlanResponse> {
  return request<PlanResponse>("/v1/billing/plan", { apiKey });
}

export function getUsage(apiKey: string): Promise<UsageResponseDto> {
  return request<UsageResponseDto>("/v1/usage", { apiKey });
}

/** Per-org fork counters. Returns only the caller's own org row unless
 *  the caller has operator scope AND passes `all: true` (which forwards
 *  as `?all=true`; the server also accepts `all=1`). Cheap: reads from
 *  Prometheus counters in-process. */
export function getUsageByOrg(
  apiKey: string,
  opts: { all?: boolean } = {},
): Promise<UsageByOrgResponse> {
  const suffix = opts.all ? "?all=true" : "";
  return request<UsageByOrgResponse>(`/v1/usage/by-org${suffix}`, { apiKey });
}

/** Cursor-paginated `GET /v1/vms`. `after` is the last-seen id; empty
 *  → first page. `limit` defaults server-side. */
export function listVms(
  apiKey: string,
  opts: { limit?: number; after?: number } = {},
): Promise<VmListResponse> {
  const params = new URLSearchParams();
  if (opts.limit !== undefined) params.set("limit", String(opts.limit));
  if (opts.after !== undefined) params.set("after", String(opts.after));
  const suffix = params.toString() ? `?${params.toString()}` : "";
  return request<VmListResponse>(`/v1/vms${suffix}`, { apiKey });
}

export function destroyVm(apiKey: string, id: number): Promise<void> {
  return request<void>(`/v1/vms/${id}`, { apiKey, method: "DELETE" });
}

/** Response of `POST /v1/vms/:id/exec`. `exit_code` is null when the
 *  guest process was killed by a signal (mirror the Rust `Option<i32>`). */
export interface ExecResponse {
  exit_code: number | null;
  signal: number | null;
  stdout: string;
  stderr: string;
  duration_ms: number;
}

/** Run a program in an existing VM. Blocks until the process exits or
 *  hits `timeout_ms`. Not streaming — for streaming output use
 *  `/v1/vms/:id/exec/stream`. */
export function execVm(
  apiKey: string,
  id: number,
  req: { program: string; args?: string[]; timeout_ms?: number },
): Promise<ExecResponse> {
  return request<ExecResponse>(`/v1/vms/${id}/exec`, {
    apiKey,
    method: "POST",
    body: JSON.stringify(req),
  });
}

export function listSnapshots(
  apiKey: string,
  opts: { limit?: number; after?: number } = {},
): Promise<SnapshotListResponse> {
  const params = new URLSearchParams();
  if (opts.limit !== undefined) params.set("limit", String(opts.limit));
  if (opts.after !== undefined) params.set("after", String(opts.after));
  const suffix = params.toString() ? `?${params.toString()}` : "";
  return request<SnapshotListResponse>(`/v1/snapshots${suffix}`, { apiKey });
}

export function destroySnapshot(apiKey: string, id: number): Promise<void> {
  return request<void>(`/v1/snapshots/${id}`, { apiKey, method: "DELETE" });
}

export function listKeys(apiKey: string): Promise<ListKeysResponse> {
  return request<ListKeysResponse>("/v1/keys", { apiKey });
}

export function issueKey(apiKey: string): Promise<IssueKeyResponse> {
  return request<IssueKeyResponse>("/v1/keys", {
    apiKey,
    method: "POST",
    // Server-side generates the secret; body is empty JSON so
    // Content-Type gets set correctly.
    body: JSON.stringify({}),
  });
}

export function revokeKey(apiKey: string, id: string): Promise<void> {
  return request<void>(`/v1/keys/${encodeURIComponent(id)}`, {
    apiKey,
    method: "DELETE",
  });
}

export function getBillingPortalUrl(
  apiKey: string,
): Promise<{ url: string }> {
  return request<{ url: string }>("/v1/billing/portal", { apiKey });
}

/** Public — no bearer required. The marketplace catalogue is meant
 *  for browse-before-signup. */
export function listMarketplaceSnapshots(): Promise<MarketplaceListResponse> {
  return request<MarketplaceListResponse>("/v1/marketplace/snapshots");
}

/** Fork a marketplace entry into the caller's tenant. First call per
 *  `(tenant, name, snapshot_url)` downloads the tarball (seconds);
 *  subsequent calls are ~12 ms warm-pool pops. Requires bearer auth. */
export function forkMarketplaceSnapshot(
  apiKey: string,
  name: string,
): Promise<ForkResponse> {
  return request<ForkResponse>(
    `/v1/marketplace/snapshots/${encodeURIComponent(name)}/fork`,
    {
      apiKey,
      method: "POST",
      body: JSON.stringify({}),
    },
  );
}
