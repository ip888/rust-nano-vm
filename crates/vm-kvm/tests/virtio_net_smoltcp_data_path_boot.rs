//! M1.5 end-to-end TCP data-path test: external HTTP-style client
//! on the host punches through the full chain
//!
//! ```text
//!   host test thread
//!        │  TcpStream::connect(listener.local_addr())
//!        ▼
//!   host OS TcpListener (ephemeral port)
//!        │  accept()
//!        ▼
//!   TcpProxy::serve  (bridged by this test on a spawned thread)
//!        │  bridge_connection → socket_send / socket_recv
//!        ▼
//!   SmoltcpBackend   (userspace TCP/IP stack on the host, obtained
//!                     via KvmHypervisor::net_smoltcp_backend)
//!        │  serialised Ethernet frames
//!        ▼
//!   virtio-net ring  (vm-kvm virtio-mmio device)
//!        │
//!        ▼
//!   guest kernel's virtio_net driver
//!        │
//!        ▼
//!   guest TCP/IP stack (static eth0 = 169.254.0.2/24, brought up
//!                       by the net-echo /init via ioctl)
//!   │  accept + echo
//!        ▼
//!   static C echo server on :8080
//! ```
//!
//! …and verifies the host sees byte-for-byte the same payload the
//! test thread wrote. Every arrow is an integration seam that
//! previous PRs carried on faith; this is the first test that
//! exercises them as a composed chain.
//!
//! Closes Copilot finding #4252252446 on PR #283 (T2 Finding C).
//! The paired `# TODO (end-to-end data-path test — follow-up PR)`
//! stub in `virtio_net_smoltcp_boot.rs` is removed in the same PR.
//!
//! ## Skip conditions
//!
//! Follows the vm-kvm "skip when fixtures missing" pattern:
//!
//!   NANOVM_TEST_KERNEL          (default: tools/kernel/cache/bzImage-jvm)
//!   NANOVM_TEST_INITRAMFS_NET   (default:
//!                                tools/initramfs/cache/initramfs-net-echo.cpio)
//!
//! Build the fixtures with:
//!
//! ```sh
//! # Kernel — same tinyconfig + net fragment the existing smoltcp
//! # bring-up test uses.
//! KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh
//!
//! # Guest initramfs with a static C echo listener as /init.
//! NANOVM_INIT=net-echo tools/initramfs/build-initramfs.sh
//! ```
//!
//! Then run:
//!
//! ```sh
//! cargo test -p vm-kvm --features kvm,virtio-net-smoltcp \
//!     --test virtio_net_smoltcp_data_path_boot -- --nocapture
//! ```

#![cfg(all(feature = "kvm", feature = "virtio-net-smoltcp"))]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use virtio_net::DEFAULT_LISTEN_PORT;
use vm_core::{Hypervisor, NetBackendChoice, PortForward, VmConfig, VmId};
use vm_kvm::KvmHypervisor;

/// Substring the guest `/init` prints when eth0 is configured and
/// the static echo listener has bound its guest TCP socket.
/// Polling for this closes the "guest still booting" race before
/// the host attaches TcpProxy.
const READY_MARKER: &str = "NANOVM_NET_ECHO_READY";

/// Overall test budget. 20 s covers the slowest reference host
/// (1.6 GHz Broadwell) while still failing fast on a regression.
const TEST_DEADLINE_SECS: u64 = 20;

/// Payload sent through the proxied connection. Picked to cross a
/// single-TCP-segment boundary easily (large enough to exercise the
/// scratch buffer in `TcpProxy::pump_bytes`, small enough to arrive
/// in one `read`).
const PAYLOAD: &[u8] = b"hello via virtio-net smoltcp data path\n";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn resolve(env_key: &str, default_rel: &str) -> Option<PathBuf> {
    if let Ok(s) = std::env::var(env_key) {
        let p = PathBuf::from(s);
        return p.exists().then_some(p);
    }
    let p = workspace_root().join(default_rel);
    p.exists().then_some(p)
}

/// Poll the guest's serial output until `needle` appears or the
/// deadline passes. Fails the test with a useful tail when the
/// marker never arrives.
fn wait_for_serial(hv: &KvmHypervisor, vm: VmId, needle: &str, max_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let bytes = hv.serial_output(vm).expect("read serial output");
        let s = String::from_utf8_lossy(&bytes);
        if s.contains(needle) {
            return;
        }
        if Instant::now() >= deadline {
            let tail = {
                let len = s.len();
                let start = len.saturating_sub(4096);
                // Walk forward to a UTF-8 char boundary so logging
                // doesn't panic mid-codepoint on a non-ASCII byte in
                // the kernel log.
                let mut start = start;
                while start < len && !s.is_char_boundary(start) {
                    start += 1;
                }
                s[start..].to_string()
            };
            panic!(
                "guest did not emit marker {needle:?} within {max_secs}s\n\
                 serial tail (last 4 KiB):\n{tail}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn echo_round_trip(address: impl std::net::ToSocketAddrs) -> Vec<u8> {
    let mut client = TcpStream::connect(address).expect("connect to VM port proxy");
    client
        .set_read_timeout(Some(Duration::from_secs(TEST_DEADLINE_SECS)))
        .expect("set read timeout");
    client.write_all(PAYLOAD).expect("client write payload");

    let mut received = Vec::with_capacity(PAYLOAD.len());
    let mut buf = [0u8; 128];
    while received.len() < PAYLOAD.len() {
        let n = client.read(&mut buf).expect("client read echo from proxy");
        if n == 0 {
            break;
        }
        received.extend_from_slice(&buf[..n]);
    }
    let _ = client.shutdown(std::net::Shutdown::Write);
    received
}

#[test]
fn host_to_guest_tcp_round_trip() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage-jvm") else {
        eprintln!(
            "virtio_net_smoltcp_data_path_boot: skipping — \
             run KERNEL_PROFILE=jvm tools/kernel/build-tiny-kernel.sh first."
        );
        return;
    };
    let Some(initrd) = resolve(
        "NANOVM_TEST_INITRAMFS_NET",
        "tools/initramfs/cache/initramfs-net-echo.cpio",
    ) else {
        eprintln!(
            "virtio_net_smoltcp_data_path_boot: skipping — run \
             `NANOVM_INIT=net-echo tools/initramfs/build-initramfs.sh` first."
        );
        return;
    };
    eprintln!(
        "virtio_net_smoltcp_data_path_boot: kernel={} initrd={}",
        kernel.display(),
        initrd.display(),
    );

    let hv = KvmHypervisor::new().expect("open /dev/kvm");
    let cfg = VmConfig {
        vcpus: 1,
        // 128 MiB mirrors the sibling `virtio_net_smoltcp_boot` test
        // + `agent_init_boot`. 64 MiB was marginal for the kernel plus
        // a statically linked init fixture.
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        net_backend: NetBackendChoice::Smoltcp,
        port_forwards: vec![PortForward {
            guest_port: DEFAULT_LISTEN_PORT,
            host_address: "127.0.0.1".into(),
            host_port: 0,
        }],
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // Wait for the guest `/init` to finish bringing eth0 up and
    // print its readiness marker. 15 s is loose enough for a slow
    // laptop kernel boot (~2 s) plus network-interface bring-up.
    wait_for_serial(&hv, handle.id, READY_MARKER, 15);
    // The marker is emitted immediately before exec'ing nc; allow the
    // listener syscall to complete before the first SYN.
    std::thread::sleep(Duration::from_secs(1));

    let endpoint = handle
        .endpoints
        .first()
        .expect("VM must expose the requested endpoint");
    let listen_addr = format!("{}:{}", endpoint.host_address, endpoint.host_port);
    let received = echo_round_trip(listen_addr.as_str());

    let snapshot = hv.snapshot(handle.id).expect("snapshot active network VM");
    let restored = hv.restore(snapshot).expect("restore network VM");
    let restored_endpoint = restored
        .endpoints
        .first()
        .expect("restored VM must expose the requested endpoint");
    let restored_received = echo_round_trip(format!(
        "{}:{}",
        restored_endpoint.host_address, restored_endpoint.host_port
    ));
    hv.stop(restored.id).expect("stop restored VM");
    hv.start(restored.id).expect("restart restored VM");
    wait_for_serial(&hv, restored.id, READY_MARKER, 15);
    std::thread::sleep(Duration::from_secs(1));
    let restarted_received = echo_round_trip(format!(
        "{}:{}",
        restored_endpoint.host_address, restored_endpoint.host_port
    ));

    // Capture serial for the assertion message before teardown, so
    // a failure diagnosis isn't blocked on the VM being alive.
    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();

    let _ = hv.destroy(restored.id);
    let _ = hv.destroy(handle.id);

    assert_eq!(
        &received[..PAYLOAD.len().min(received.len())],
        PAYLOAD,
        "round-trip payload mismatch\n\
         received {} bytes (expected {})\n\
         serial tail (last 2 KiB):\n{}",
        received.len(),
        PAYLOAD.len(),
        {
            let n = serial.len();
            let start = n.saturating_sub(2048);
            let mut start = start;
            while start < n && !serial.is_char_boundary(start) {
                start += 1;
            }
            &serial[start..]
        },
    );
    assert_eq!(
        restored_received, PAYLOAD,
        "restored VM did not serve TCP through the reconstructed virtio-net device"
    );
    assert_eq!(
        restarted_received, PAYLOAD,
        "restarted VM did not release and recreate its TCP listener"
    );
    eprintln!(
        "virtio_net_smoltcp_data_path_boot: round-trip of {} bytes OK (full chain alive)",
        PAYLOAD.len(),
    );
}
