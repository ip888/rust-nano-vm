// Session storage — the API key is the whole identity.
//
// Stored in `localStorage` on the user's browser. This is a
// deliberate simplification: the dashboard is fully client-side, so
// there's no server-side session to invalidate. Rotating the key
// (via `POST /v1/keys`) invalidates any lingering copies.
//
// Storing high-value bearer tokens in `localStorage` is a well-known
// XSS risk. Two mitigations for a real prod deploy:
//   1. Serve the dashboard over strict CSP (no inline scripts,
//      no eval, no third-party origins).
//   2. Consider moving to httpOnly cookies + a same-origin API in a
//      future PR (requires cookie handling in the control plane).
//
// For the MVP, localStorage is honest to the deploy shape: a single
// dashboard binary, an API on a subdomain, and a customer who can
// paste their key and revoke it later.

const STORAGE_KEY = "nanovm.session";

export interface Session {
  apiKey: string;
  org: string;
}

/** SSR-safe: returns null in a non-browser environment. */
export function getSession(): Session | null {
  if (typeof window === "undefined") return null;
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as Session;
    if (typeof parsed.apiKey === "string" && typeof parsed.org === "string") {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

export function setSession(session: Session): void {
  if (typeof window === "undefined") return;
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(session));
}

export function clearSession(): void {
  if (typeof window === "undefined") return;
  window.localStorage.removeItem(STORAGE_KEY);
}
