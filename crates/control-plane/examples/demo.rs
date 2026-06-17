//! End-to-end demo of the rust-nano-vm control plane.
//!
//! Boots an in-process control plane backed by the `MockHypervisor`,
//! then drives the lifecycle (create VM → start → snapshot → fork ×5 →
//! `/v1/usage` → `/metrics`) and prints a human-readable report.
//!
//! Identical behaviour on Linux, macOS, and Windows — no `/dev/kvm`,
//! no shell-specific quoting, no `curl`/`jq` required.
//!
//! ```sh
//! cargo run -p control-plane --example demo --release
//! ```

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use control_plane::{router, ApiTokens, AppState};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use vm_mock::MockHypervisor;

const TOKEN: &str = "demo-token";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bind to a free port so concurrent demos don't collide.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    println!("✔ control-plane up on http://{addr}");

    let tokens = Arc::new(ApiTokens::new([TOKEN.to_string()]));
    let hypervisor: Arc<dyn vm_core::Hypervisor> = Arc::new(MockHypervisor::new());
    let app = router()
        .layer(Extension(tokens))
        .with_state(AppState::new(hypervisor));

    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = format!("http://{addr}");

    let vm: Value = client
        .post(format!("{base}/v1/vms"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .json(&json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let vm_id = vm["id"].as_u64().ok_or("create_vm: missing id")?;
    println!("✔ created   vm-{vm_id:016}");

    client
        .post(format!("{base}/v1/vms/{vm_id}/start"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?;
    println!("✔ started   vm-{vm_id:016}");

    let snap: Value = client
        .post(format!("{base}/v1/vms/{vm_id}/snapshot"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let snap_id = snap["id"].as_u64().ok_or("snapshot: missing id")?;
    println!("✔ snapshot  snap-{snap_id:016}");

    for _ in 0..5 {
        let fork: Value = client
            .post(format!("{base}/v1/snapshots/{snap_id}/fork"))
            .bearer_auth(TOKEN)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let child = fork["vm"]["id"].as_u64().unwrap_or(0);
        let fork_ms = fork["fork_ms"].as_u64().unwrap_or(0);
        println!("✔ forked    vm-{child:016} in {fork_ms} ms");
    }

    let usage: Value = client
        .get(format!("{base}/v1/usage"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    println!(
        "\nusage     : fork_count={} fork_total_ms={}",
        usage["fork_count"], usage["fork_total_ms"]
    );

    let metrics = client
        .get(format!("{base}/metrics"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    for line in metrics.lines() {
        if line.starts_with("nanovm_forks_total{")
            || line.starts_with("nanovm_fork_quota_throttled_total{")
        {
            println!("metrics   : {line}");
        }
    }

    server.abort();
    Ok(())
}
