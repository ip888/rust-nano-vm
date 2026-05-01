//! `nanovm` — the rust-nano-vm command-line driver.
//!
//! Talks to a running `nanovm-control-plane` server over HTTP. Set
//! `--server` (or `NANOVM_SERVER`) to the base URL, and `--token` (or
//! `NANOVM_TOKEN`) when the server has bearer-token auth enabled.
//!
//! ```sh
//! nanovm run /path/to/kernel.bin
//! nanovm run --snapshot-dir /var/lib/nanovm/snap-001
//! nanovm ps
//! nanovm snapshot vm-0000000000000001
//! nanovm fork snap-0000000000000001 --count 4
//! ```
//!
//! Subcommands that require a real KVM guest (`exec`, `cp`) are still
//! stubs and exit 2. They land in M2 / M3 alongside the guest agent.
//!
//! Failures from the server are surfaced via the structured error
//! envelope produced by the control plane.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use reqwest::blocking::{Client as Http, RequestBuilder};
use reqwest::Method;
use serde_json::{json, Value};

/// Ephemeral code-execution sandbox microVM for LLM agents.
#[derive(Debug, Parser)]
#[command(name = "nanovm", version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity. Repeat for more detail.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Base URL of the `nanovm-control-plane` server.
    #[arg(
        long,
        global = true,
        default_value = "http://127.0.0.1:8080",
        env = "NANOVM_SERVER"
    )]
    server: String,

    /// Bearer token presented to the server's `/v1/*` routes. Required
    /// when the server has `NANOVM_API_TOKENS` set; harmless otherwise.
    #[arg(long, global = true, env = "NANOVM_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Boot a new sandbox VM. Cold-boots from `image` if given;
    /// restores from `--snapshot-dir` if that's given (the snapshot's
    /// recorded geometry overrides `--memory-mib` / `--vcpus`).
    Run {
        /// Path to the kernel/rootfs image. Optional when
        /// `--snapshot-dir` is given.
        image: Option<String>,
        /// Restore from a saved snapshot directory instead of cold-booting.
        #[arg(long)]
        snapshot_dir: Option<String>,
        /// Memory in MiB. Ignored when `--snapshot-dir` is set.
        #[arg(long, default_value_t = 256)]
        memory_mib: u64,
        /// Virtual CPUs. Ignored when `--snapshot-dir` is set.
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
        /// Skip the implicit `start` after creating the VM.
        #[arg(long)]
        no_start: bool,
    },
    /// Execute a command inside an already-running sandbox. (milestone M2)
    Exec {
        /// Target sandbox id (raw u64 or `vm-...` display form).
        id: String,
        /// Command + args to run inside the guest.
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Copy a file into or out of a sandbox. (milestone M3)
    Cp {
        /// Source (either local path or `<id>:/path`).
        src: String,
        /// Destination (either local path or `<id>:/path`).
        dst: String,
    },
    /// Take a snapshot of a running sandbox. Prints the new snapshot id.
    Snapshot {
        /// Target sandbox id (raw u64 or `vm-...` display form).
        id: String,
    },
    /// Fork one or more new sandboxes from a snapshot. Prints each
    /// resulting VM id on its own line.
    Fork {
        /// Snapshot id (raw u64 or `snap-...` display form).
        snapshot: String,
        /// Number of children to spawn.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// List sandboxes (requires a running nanovm-control-plane).
    Ps,
    /// List captured snapshots.
    Snapshots,
    /// Delete a snapshot. After this returns, `nanovm fork <id>` on it
    /// fails with `unknown_snapshot`.
    RmSnap {
        /// Snapshot id (raw u64 or `snap-...` display form).
        snapshot: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| log_level.into()),
        )
        .try_init();

    let client = Client::new(cli.server.clone(), cli.token.clone());
    match cli.command {
        Command::Run {
            image,
            snapshot_dir,
            memory_mib,
            vcpus,
            no_start,
        } => cmd_run(&client, image, snapshot_dir, memory_mib, vcpus, no_start),
        Command::Exec { .. } => unimplemented_for("exec", "M2"),
        Command::Cp { .. } => unimplemented_for("cp", "M3"),
        Command::Snapshot { id } => cmd_snapshot(&client, &id),
        Command::Fork { snapshot, count } => cmd_fork(&client, &snapshot, count),
        Command::Ps => cmd_ps(&client),
        Command::Snapshots => cmd_snapshots(&client),
        Command::RmSnap { snapshot } => cmd_rm_snap(&client, &snapshot),
    }
}

// ---------------------------------------------------------------------------
// HTTP client wrapper
// ---------------------------------------------------------------------------

struct Client {
    base: String,
    token: Option<String>,
    http: Http,
}

impl Client {
    fn new(base: String, token: Option<String>) -> Self {
        Self {
            base,
            token,
            http: Http::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self, mut req: RequestBuilder) -> RequestBuilder {
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req
    }

    fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, CliError> {
        let url = self.url(path);
        let mut req = self.http.request(method, &url);
        req = self.auth(req);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .map_err(|e| CliError::Network(format!("could not reach {}: {e}", self.base)))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        // The control-plane returns JSON for both success and error
        // envelopes; capture the body bytes and try to parse so we
        // surface the structured `code` / `message` even on 4xx/5xx.
        let bytes = resp
            .bytes()
            .map_err(|e| CliError::Network(format!("read body: {e}")))?;
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        if status.is_success() {
            Ok(value)
        } else {
            let code = value["error"]["code"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            let message = value["error"]["message"]
                .as_str()
                .unwrap_or(&value.to_string())
                .to_owned();
            Err(CliError::Http {
                status: status.as_u16(),
                code,
                message,
            })
        }
    }

    fn get(&self, path: &str) -> Result<Value, CliError> {
        self.send(Method::GET, path, None)
    }
    fn post(&self, path: &str, body: Option<Value>) -> Result<Value, CliError> {
        self.send(Method::POST, path, body)
    }
    fn delete(&self, path: &str) -> Result<Value, CliError> {
        self.send(Method::DELETE, path, None)
    }
}

#[derive(Debug)]
enum CliError {
    Network(String),
    Http {
        status: u16,
        code: String,
        message: String,
    },
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Network(s) => write!(f, "{s}"),
            CliError::Http {
                status,
                code,
                message,
            } => write!(f, "server returned HTTP {status} ({code}): {message}"),
        }
    }
}

fn fail(cmd: &str, err: &CliError) -> ExitCode {
    eprintln!("nanovm {cmd}: {err}");
    if matches!(err, CliError::Network(_)) {
        eprintln!("  Is nanovm-control-plane running? Start it with:");
        eprintln!("    cargo run -p control-plane --bin nanovm-control-plane");
    }
    ExitCode::from(1)
}

// ---------------------------------------------------------------------------
// ID parsing
// ---------------------------------------------------------------------------

/// Accept either a raw u64 (`42`, `0x42`) or the `vm-XXXX` / `snap-XXXX`
/// display form produced by the control plane and return the raw u64
/// the URL path expects.
fn parse_id(input: &str, expected_prefix: &str) -> Result<u64, String> {
    if let Some(hex) = input.strip_prefix(&format!("{expected_prefix}-")) {
        return u64::from_str_radix(hex, 16)
            .map_err(|e| format!("bad {expected_prefix}-id hex `{input}`: {e}"));
    }
    if let Some(hex) = input.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).map_err(|e| format!("bad hex id `{input}`: {e}"));
    }
    input
        .parse::<u64>()
        .map_err(|e| format!("bad id `{input}`: {e}"))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_run(
    client: &Client,
    image: Option<String>,
    snapshot_dir: Option<String>,
    memory_mib: u64,
    vcpus: u32,
    no_start: bool,
) -> ExitCode {
    if image.is_none() && snapshot_dir.is_none() {
        eprintln!(
            "nanovm run: provide either an image path or --snapshot-dir.\n  \
             For mock-backed dev: `nanovm run /tmp/dummy.bin`"
        );
        return ExitCode::from(2);
    }
    let mut body = json!({
        "vcpus": vcpus,
        "memory_mib": memory_mib,
    });
    if let Some(img) = image {
        body["kernel"] = Value::String(img);
    }
    if let Some(snap) = snapshot_dir {
        body["snapshot_dir"] = Value::String(snap);
    }
    let created = match client.post("/v1/vms", Some(body)) {
        Ok(v) => v,
        Err(e) => return fail("run", &e),
    };
    let id = match created["id"].as_u64() {
        Some(n) => n,
        None => {
            eprintln!("nanovm run: server returned unexpected response shape: {created}");
            return ExitCode::from(1);
        }
    };
    let display = created["display"].as_str().unwrap_or("?");

    if no_start {
        println!("{display} created");
        return ExitCode::SUCCESS;
    }

    if let Err(e) = client.post(&format!("/v1/vms/{id}/start"), None) {
        eprintln!("nanovm run: created {display} but start failed: {e}");
        return ExitCode::from(1);
    }
    println!("{display} running");
    ExitCode::SUCCESS
}

fn cmd_snapshot(client: &Client, id: &str) -> ExitCode {
    let vm_id = match parse_id(id, "vm") {
        Ok(n) => n,
        Err(e) => {
            eprintln!("nanovm snapshot: {e}");
            return ExitCode::from(2);
        }
    };
    let snap = match client.post(&format!("/v1/vms/{vm_id}/snapshot"), None) {
        Ok(v) => v,
        Err(e) => return fail("snapshot", &e),
    };
    let display = snap["display"].as_str().unwrap_or("?");
    println!("{display}");
    ExitCode::SUCCESS
}

fn cmd_fork(client: &Client, snapshot: &str, count: u32) -> ExitCode {
    if count == 0 {
        eprintln!("nanovm fork: --count must be at least 1");
        return ExitCode::from(2);
    }
    let snap_id = match parse_id(snapshot, "snap") {
        Ok(n) => n,
        Err(e) => {
            eprintln!("nanovm fork: {e}");
            return ExitCode::from(2);
        }
    };
    for i in 0..count {
        let restored = match client.post(&format!("/v1/snapshots/{snap_id}/restore"), None) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("nanovm fork: child {} of {count} failed: {e}", i + 1);
                return ExitCode::from(1);
            }
        };
        let display = restored["display"].as_str().unwrap_or("?");
        println!("{display}");
    }
    ExitCode::SUCCESS
}

fn cmd_snapshots(client: &Client) -> ExitCode {
    let body = match client.get("/v1/snapshots") {
        Ok(v) => v,
        Err(e) => return fail("snapshots", &e),
    };
    let snaps = match body["snapshots"].as_array() {
        Some(a) => a,
        None => {
            eprintln!("nanovm snapshots: unexpected response shape: {body}");
            return ExitCode::from(1);
        }
    };
    if snaps.is_empty() {
        println!("no snapshots");
        return ExitCode::SUCCESS;
    }
    for s in snaps {
        let display = s["display"].as_str().unwrap_or("?");
        println!("{display}");
    }
    ExitCode::SUCCESS
}

fn cmd_rm_snap(client: &Client, snapshot: &str) -> ExitCode {
    let snap_id = match parse_id(snapshot, "snap") {
        Ok(n) => n,
        Err(e) => {
            eprintln!("nanovm rm-snap: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = client.delete(&format!("/v1/snapshots/{snap_id}")) {
        return fail("rm-snap", &e);
    }
    println!("snap-{snap_id:016x} deleted");
    ExitCode::SUCCESS
}

fn cmd_ps(client: &Client) -> ExitCode {
    let body = match client.get("/v1/vms") {
        Ok(v) => v,
        Err(e) => return fail("ps", &e),
    };
    let vms = match body["vms"].as_array() {
        Some(a) => a,
        None => {
            eprintln!("nanovm ps: unexpected response shape: {body}");
            return ExitCode::from(1);
        }
    };
    if vms.is_empty() {
        println!("no VMs");
        return ExitCode::SUCCESS;
    }
    println!("{:<24} STATE", "ID");
    for vm in vms {
        let display = vm["display"].as_str().unwrap_or("?");
        let state = vm["state"].as_str().unwrap_or("?");
        println!("{display:<24} {state}");
    }
    ExitCode::SUCCESS
}

fn unimplemented_for(cmd: &str, milestone: &str) -> ExitCode {
    eprintln!("nanovm {cmd}: unimplemented — arrives in milestone {milestone}");
    eprintln!("see docs/PLAN.md for the roadmap.");
    ExitCode::from(2)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_accepts_raw_u64() {
        assert_eq!(parse_id("42", "vm").unwrap(), 42);
    }

    #[test]
    fn parse_id_accepts_display_form_for_vm() {
        assert_eq!(parse_id("vm-0000000000000042", "vm").unwrap(), 0x42);
        assert_eq!(parse_id("vm-00000000deadbeef", "vm").unwrap(), 0xdead_beef);
    }

    #[test]
    fn parse_id_accepts_display_form_for_snap() {
        assert_eq!(parse_id("snap-0000000000000007", "snap").unwrap(), 7);
    }

    #[test]
    fn parse_id_accepts_0x_hex() {
        assert_eq!(parse_id("0xff", "vm").unwrap(), 255);
    }

    #[test]
    fn parse_id_rejects_garbage() {
        assert!(parse_id("not-a-number", "vm").is_err());
        assert!(parse_id("vm-zzzz", "vm").is_err());
    }

    #[test]
    fn parse_id_with_wrong_prefix_falls_through_to_raw() {
        // Expecting "vm-..." but got "snap-..." → tries to parse the whole
        // string as u64, which fails. Good — caller sees an error.
        assert!(parse_id("snap-0000000000000001", "vm").is_err());
    }
}
