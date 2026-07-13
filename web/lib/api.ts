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
  /** Bearer token in the standard `org:secret` form. Shown once. */
  token: string;
  id: string;
  org: string;
  created_at: string;
}

// -------- Fetch wrappers -----------------------------------------

/**
 * Structured error surfaced to the UI. `.status` mirrors the HTTP
 * status; `.code` and `.message` come from the control-plane's
 * `{"error": {"code": "...", "message": "..."}}` envelope when
 * present, otherwise a synthetic value.
 */
export class ApiError extends Error {
  status: number;
  code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
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
    const errObj = parsed as { error?: { code?: string; message?: string } };
    const code = errObj?.error?.code ?? String(resp.status);
    const message =
      errObj?.error?.message ??
      (typeof parsed === "string" ? parsed : text || resp.statusText);
    throw new ApiError(resp.status, code, message);
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
