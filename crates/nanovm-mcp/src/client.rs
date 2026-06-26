//! HTTP client for the rust-nano-vm control plane's
//! `POST /v1/sandbox/invoke` endpoint.
//!
//! Single async helper — every MCP tool call funnels through it.
//! The bridge process holds one `reqwest::Client` for connection
//! reuse so back-to-back tool calls don't re-handshake TLS.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Server response envelope. Mirrors the
/// `crates/control-plane/src/sandbox.rs::SandboxResult` shape
/// one-for-one so the MCP layer can render it without translation.
#[derive(Debug, Deserialize)]
pub struct SandboxResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub cold_start: bool,
}

/// Bridge-side configuration. Loaded once at startup from the
/// environment so the stdio loop doesn't repeatedly hit `std::env`.
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub token: Option<String>,
    /// Optional default snapshot id forwarded on every invoke when
    /// the AI agent doesn't pass `snapshot` itself. `None` lets the
    /// server fall back to its own `NANOVM_SANDBOX_SNAPSHOT_ID`.
    pub snapshot: Option<u64>,
}

impl Config {
    /// Build from env. `NANOVM_BASE_URL` defaults to
    /// `http://localhost:8080`; `NANOVM_API_TOKEN` is omitted when
    /// the var isn't set (control plane in auth-disabled mode);
    /// `NANOVM_SANDBOX_SNAPSHOT_ID` is parsed if present.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_reader(|k| std::env::var(k).ok())
    }

    /// Pure parser used by both [`from_env`] and the tests. Takes a
    /// reader closure so test cases can inject a fixed map without
    /// touching the process-global env (which collides with
    /// `#![forbid(unsafe_code)]` post Rust 2024).
    pub fn from_reader<R>(read: R) -> anyhow::Result<Self>
    where
        R: Fn(&str) -> Option<String>,
    {
        let base_url =
            read("NANOVM_BASE_URL").unwrap_or_else(|| "http://localhost:8080".to_owned());
        let token = read("NANOVM_API_TOKEN").filter(|t| !t.is_empty());
        let snapshot = match read("NANOVM_SANDBOX_SNAPSHOT_ID") {
            Some(s) => Some(s.parse::<u64>().map_err(|e| {
                anyhow::anyhow!(
                    "NANOVM_SANDBOX_SNAPSHOT_ID={s:?} is not a non-negative integer ({e})"
                )
            })?),
            None => None,
        };
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            token,
            snapshot,
        })
    }
}

/// HTTP client + per-run config. Cheap to clone (Arc-backed).
#[derive(Debug, Clone)]
pub struct SandboxClient {
    http: reqwest::Client,
    cfg: Config,
}

impl SandboxClient {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("nanovm-mcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, cfg })
    }

    /// POST to `/v1/sandbox/invoke` with the action-discriminated
    /// body and parse the `SandboxResult` envelope. Errors fall into
    /// three buckets:
    ///
    /// - transport (DNS, connect, TLS) → `Err(InvokeError::Transport)`
    /// - HTTP non-2xx → `Err(InvokeError::Http)` carrying status +
    ///   body so the MCP layer can pass an actionable diagnostic
    ///   back to the LLM
    /// - JSON parse on success body → `Err(InvokeError::BadResponse)`
    pub async fn invoke(&self, body: Value) -> Result<SandboxResult, InvokeError> {
        // Merge in the bridge-configured snapshot fallback when the
        // caller didn't pass one. Mirrors the server's same fallback
        // chain — agent-supplied wins, else bridge env, else server
        // env, else 400.
        let mut body = body;
        if let Some(snap) = self.cfg.snapshot {
            if let Some(obj) = body.as_object_mut() {
                obj.entry("snapshot").or_insert_with(|| Value::from(snap));
            }
        }

        let url = format!("{}/v1/sandbox/invoke", self.cfg.base_url);
        let mut req = self.http.post(&url).json(&body);
        if let Some(ref token) = self.cfg.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| InvokeError::Transport(format!("POST {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(InvokeError::Http {
                status: status.as_u16(),
                body: body_text,
            });
        }
        resp.json::<SandboxResult>()
            .await
            .map_err(|e| InvokeError::BadResponse(format!("parse SandboxResult: {e}")))
    }
}

#[derive(Debug, Serialize, thiserror::Error)]
pub enum InvokeError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("bad response: {0}")]
    BadResponse(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a reader closure from a static map. Lets each test
    /// declare exactly the env it wants without touching
    /// process-global state. The closure owns its map so the
    /// resulting reader has no borrow lifetime.
    fn reader(entries: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<&'static str, &'static str> = entries.iter().copied().collect();
        move |k| map.get(k).map(|s| (*s).to_owned())
    }

    #[test]
    fn config_defaults_when_env_is_empty() {
        let cfg = Config::from_reader(reader(&[])).unwrap();
        assert_eq!(cfg.base_url, "http://localhost:8080");
        assert!(cfg.token.is_none());
        assert!(cfg.snapshot.is_none());
    }

    #[test]
    fn config_strips_trailing_slash_from_base_url() {
        let cfg = Config::from_reader(reader(&[("NANOVM_BASE_URL", "http://example.com:9090/")]))
            .unwrap();
        assert_eq!(cfg.base_url, "http://example.com:9090");
    }

    #[test]
    fn config_reads_token_and_snapshot() {
        let cfg = Config::from_reader(reader(&[
            ("NANOVM_API_TOKEN", "secret"),
            ("NANOVM_SANDBOX_SNAPSHOT_ID", "42"),
        ]))
        .unwrap();
        assert_eq!(cfg.token.as_deref(), Some("secret"));
        assert_eq!(cfg.snapshot, Some(42));
    }

    #[test]
    fn config_rejects_garbage_snapshot_id() {
        let err = Config::from_reader(reader(&[("NANOVM_SANDBOX_SNAPSHOT_ID", "not-a-number")]))
            .unwrap_err();
        assert!(format!("{err}").contains("NANOVM_SANDBOX_SNAPSHOT_ID"));
    }

    #[test]
    fn config_empty_token_is_treated_as_unset() {
        let cfg = Config::from_reader(reader(&[("NANOVM_API_TOKEN", "")])).unwrap();
        assert!(cfg.token.is_none());
    }
}
