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
//!                       by the net-echo /init via busybox `ip`)
//!        │  accept
//!        ▼
//!   `busybox nc -lk -p 8080 -e cat`  (echoes every byte back)
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
//!   NANOVM_TEST_KERNEL          (default: tools/kernel/cache/bzImage)
//!   NANOVM_TEST_INITRAMFS_NET   (default:
//!                                tools/initramfs/cache/initramfs-net-echo.cpio)
//!
//! Build the fixtures with:
//!
//! ```sh
//! # Kernel — same tinyconfig + net fragment the existing smoltcp
//! # bring-up test uses.
//! tools/kernel/build-tiny-kernel.sh
//!
//! # Guest initramfs with `busybox nc` echo listener as /init.
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
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use virtio_net::{IpAddress, IpEndpoint, TcpProxy, DEFAULT_GUEST_IPV4, DEFAULT_LISTEN_PORT};
use vm_core::{Hypervisor, NetBackendChoice, VmConfig, VmId};
use vm_kvm::KvmHypervisor;

/// Substring the guest `/init` prints when eth0 is configured and
/// the `busybox nc` echo listener is about to take over PID 1.
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

#[test]
fn host_to_guest_tcp_round_trip() {
    let Some(kernel) = resolve("NANOVM_TEST_KERNEL", "tools/kernel/cache/bzImage") else {
        eprintln!(
            "virtio_net_smoltcp_data_path_boot: skipping — \
             run tools/kernel/build-tiny-kernel.sh first."
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
        // + `agent_init_boot`. 64 MiB was marginal — busybox + nc +
        // transient `ip` subprocesses don't need much, but a tight
        // memory envelope can OOM the kernel silently at execve
        // time with no userspace output, which was indistinguishable
        // from the `init died exit 1` symptom.
        memory_mib: 128,
        kernel: Some(kernel),
        initrd: Some(initrd),
        net_backend: NetBackendChoice::Smoltcp,
        cmdline: "console=ttyS0,115200 panic=-1 rdinit=/init".into(),
        ..VmConfig::default()
    };

    let handle = hv.create_vm(&cfg).expect("create_vm");
    hv.start(handle.id).expect("start");

    // Grab the backend handle up front — if this returns None the
    // test is pointless, and failing here beats a confusing TCP
    // timeout further down.
    let smoltcp = hv
        .net_smoltcp_backend(handle.id)
        .expect("net_smoltcp_backend lookup")
        .expect("smoltcp backend must be attached when NetBackendChoice::Smoltcp was selected");

    // Wait for the guest `/init` to finish bringing eth0 up and
    // print its readiness marker. 15 s is loose enough for a slow
    // laptop kernel boot (~2 s) plus busybox `ip` latency.
    wait_for_serial(&hv, handle.id, READY_MARKER, 15);

    // Bind the external listener. Ephemeral port keeps the test
    // parallel-safe (CI matrix can run several of these at once
    // without the OS stepping on itself).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind external ephemeral port");
    let listen_addr = listener.local_addr().expect("local_addr");

    // The guest echo listener is on 169.254.0.2:8080 — same subnet
    // the smoltcp backend considers its gateway, same port its
    // `nc -l` is bound to.
    let guest_endpoint = IpEndpoint::new(
        IpAddress::v4(
            DEFAULT_GUEST_IPV4[0],
            DEFAULT_GUEST_IPV4[1],
            DEFAULT_GUEST_IPV4[2],
            DEFAULT_GUEST_IPV4[3],
        ),
        DEFAULT_LISTEN_PORT,
    );
    let proxy = Arc::new(TcpProxy::new(Arc::clone(&smoltcp), guest_endpoint));
    let serve_stop = Arc::new(AtomicBool::new(false));
    let serve_proxy = Arc::clone(&proxy);
    let serve_stop_handle = Arc::clone(&serve_stop);
    let serve_thread = thread::spawn(move || {
        serve_proxy
            .serve(listener, serve_stop_handle)
            .expect("TcpProxy::serve returned an error")
    });

    // Connect from the test thread → external listener → proxy →
    // guest echo. Short read timeout fails fast when any seam in
    // the chain silently wedges.
    let mut client =
        TcpStream::connect(listen_addr).expect("external client connect to proxy listener");
    client
        .set_read_timeout(Some(Duration::from_secs(TEST_DEADLINE_SECS)))
        .expect("set read timeout");

    client.write_all(PAYLOAD).expect("client write payload");

    // `busybox nc -e cat` streams every input byte straight back,
    // so we expect the full payload mirrored. Loop to tolerate
    // short reads across TCP segment boundaries.
    let mut received = Vec::with_capacity(PAYLOAD.len());
    let mut buf = [0u8; 128];
    while received.len() < PAYLOAD.len() {
        let n = client.read(&mut buf).expect("client read echo from proxy");
        if n == 0 {
            break;
        }
        received.extend_from_slice(&buf[..n]);
    }

    // Half-close our write side so the proxy knows we're done; its
    // smoltcp socket then FINs the guest side, `nc` exits, the
    // bridge thread returns Ok.
    let _ = client.shutdown(std::net::Shutdown::Write);
    drop(client);

    // Capture serial for the assertion message before teardown, so
    // a failure diagnosis isn't blocked on the VM being alive.
    let serial = hv
        .serial_output(handle.id)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();

    serve_stop.store(true, Ordering::Relaxed);
    serve_thread.join().expect("serve thread must not panic");

    let _ = hv.stop(handle.id);
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
    eprintln!(
        "virtio_net_smoltcp_data_path_boot: round-trip of {} bytes OK (full chain alive)",
        PAYLOAD.len(),
    );
}
