//! `nanovm` — the rust-nano-vm command-line driver.
//!
//! M0 ships only the subcommand surface. Subcommands that require KVM print
//! `unimplemented: milestone Mx` and exit 2 so downstream tooling can depend
//! on the CLI shape today. Real behaviour lands in M1–M6.
//!
//! ## Control-plane subcommands (available now)
//!
//! The following subcommands talk to a running `nanovm-control-plane` server
//! over HTTP. Set `--server` (or `NANOVM_SERVER`) to the base URL:
//!
//! ```sh
//! nanovm ps                         # list VMs
//! nanovm ps --server http://host:8080
//! NANOVM_SERVER=http://host:8080 nanovm ps
//! ```
//!
//! If the server is not reachable the command prints the error and exits 1.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Ephemeral code-execution sandbox microVM for LLM agents.
#[derive(Debug, Parser)]
#[command(name = "nanovm", version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity. Repeat for more detail.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Base URL of the `nanovm-control-plane` server.
    /// Overridden by the `NANOVM_SERVER` environment variable.
    #[arg(
        long,
        global = true,
        default_value = "http://127.0.0.1:8080",
        env = "NANOVM_SERVER"
    )]
    server: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Boot a new sandbox VM from a rootfs/kernel image. (milestone M1)
    Run {
        /// Path to the rootfs or guest image.
        image: String,
        /// Memory in MiB.
        #[arg(long, default_value_t = 256)]
        memory_mib: u64,
        /// Virtual CPUs.
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
    },
    /// Execute a command inside an already-running sandbox. (milestone M2)
    Exec {
        /// Target sandbox id (as printed by `run`).
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
    /// Take a snapshot of a running sandbox. (milestone M5)
    Snapshot {
        /// Target sandbox id.
        id: String,
    },
    /// Fork a new sandbox from a snapshot. (milestone M5)
    Fork {
        /// Snapshot id to fork from.
        snapshot: String,
        /// Number of children to spawn.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// List running sandboxes (requires a running nanovm-control-plane).
    Ps,
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

    match cli.command {
        Command::Run { .. } => unimplemented_for("run", "M1"),
        Command::Exec { .. } => unimplemented_for("exec", "M2"),
        Command::Cp { .. } => unimplemented_for("cp", "M3"),
        Command::Snapshot { .. } => unimplemented_for("snapshot", "M5"),
        Command::Fork { .. } => unimplemented_for("fork", "M5"),
        Command::Ps => cmd_ps(&cli.server),
    }
}

/// `nanovm ps` — list VMs from the control plane.
fn cmd_ps(server: &str) -> ExitCode {
    let url = format!("{server}/v1/vms");
    let client = reqwest::blocking::Client::new();
    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nanovm ps: could not reach {server}: {e}");
            eprintln!("  Is nanovm-control-plane running? Start it with:");
            eprintln!("    cargo run -p control-plane --bin nanovm-control-plane");
            return ExitCode::from(1);
        }
    };
    if !resp.status().is_success() {
        eprintln!("nanovm ps: server returned HTTP {}", resp.status());
        return ExitCode::from(1);
    }
    let body: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nanovm ps: failed to parse response: {e}");
            return ExitCode::from(1);
        }
    };
    let vms = match body["vms"].as_array() {
        Some(a) => a,
        None => {
            eprintln!("nanovm ps: unexpected response shape");
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
