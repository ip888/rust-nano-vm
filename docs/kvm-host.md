# KVM host setup

The real (non-mock) backend requires a host with `/dev/kvm` accessible.
This document lists the cheapest viable options.

## Quick check

```sh
ls -la /dev/kvm                 # device should exist, mode 0660, owned root:kvm
grep -E 'vmx|svm' /proc/cpuinfo # at least one CPU flag must appear
kvm-ok                          # Ubuntu convenience check; exits 0 if usable
```

If `/dev/kvm` is missing but the CPU flags are present, your BIOS is
disabling virtualisation — enable `Intel VT-x` / `AMD-V` in firmware.

## Continue developing without `/dev/kvm` (safe scope)

You can still make high-value progress before you have a KVM-capable host:

- Advance unit-testable crates and wire formats (`virtio-queue`,
  `virtio-vsock`, `virtio-fs`, `snapshot`, `proto`).
- Improve `vm-mock`-backed control-plane and CLI behavior.
- Keep non-KVM CI quality green:
  - `cargo build --workspace`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
- Keep KVM-specific work behind feature flags/abstractions, validating
  logic with `vm-mock` and unit tests until host bring-up time.

## When `/dev/kvm` becomes mandatory

You must switch to a KVM-capable host once you need to implement or
verify:

- opening `/dev/kvm`
- a real VM/vCPU run loop
- UART serial output path (`ttyS0`, "hello from guest")
- end-to-end `nanovm exec` over real `virtio-vsock` + `/dev/vsock`

### Practical “you now need KVM” signals

- You are blocked on behavior that only appears in real vCPU execution or
  device emulation timing.
- You need `nanovm run ...` to produce actual guest boot serial output.
- You need to validate guest-agent communication over real vsock, not
  stdin/stdout scaffolding.
- You need realistic performance numbers (especially snapshot/fork
  benchmarking).

## Option 1 — Local Linux (free, fastest iteration)

Any modern Intel/AMD laptop or desktop with virtualisation enabled:

```sh
sudo apt install qemu-kvm libvirt-daemon-system
sudo usermod -aG kvm $USER       # log out / back in
ls /dev/kvm                      # should now be accessible without sudo
```

macOS and Windows developers: run Linux in a VM that supports nested virt
(Parallels on Apple Silicon does not expose VT-x; VMware Fusion / UTM on
M-series cannot run KVM). Realistically, use cloud for development if you
aren't on Linux.

## Option 2 — GCP with nested virtualisation (~$0.10/hr)

```sh
gcloud compute instances create rnvmdev \
  --zone=us-central1-a \
  --machine-type=n2-standard-4 \
  --image-family=ubuntu-2204-lts --image-project=ubuntu-os-cloud \
  --enable-nested-virtualization \
  --min-cpu-platform="Intel Cascade Lake"
```

Caveats: nested virt has a non-trivial perf tax (~20–40% vs bare metal).
Fine for development; use Option 3 for benchmarks.

New-account credit ($300) covers many weeks of an n2-standard-4.

## Option 3 — AWS bare metal (~$0.30–0.50/hr)

```sh
aws ec2 run-instances \
  --image-id ami-0c7217cdde317cfec \
  --instance-type c5n.metal \
  --key-name rnvm-bench
```

Real KVM performance, no nested-virt tax. Use for weekly benchmark runs
and reproducing the headline cold-start numbers.

`m5.metal` and `c7i.metal-*` are also good; pick whichever region is
cheapest on-demand the week you need it.

## Option 4 — Hetzner dedicated (~€40/month)

Cheapest continuous bare-metal host. `AX41-NVMe` or similar. Good for a
dedicated CI runner once KVM-backed benchmarks become part of merge gating.

## Option 5 — Free tier / credits stacking

- GCP $300 new-account credit → 2–3 months of an n2-standard-4.
- Oracle Cloud Always Free: A1 (Ampere) instances expose `/dev/kvm`.
- AWS educate (if eligible).

## Recommendation

- **Development**: local Linux if available, else GCP nested virt.
- **Benchmarks**: AWS c5n.metal on demand, run weekly.
- **CI**: Ubuntu runners (no KVM) for trait/mock tests; a self-hosted
  Hetzner or AWS instance for KVM-gated integration tests.

## Once you have a host

```sh
git clone https://github.com/ip888/Rust-nano-vm.git
cd Rust-nano-vm
cargo build -p vm-kvm --features kvm
cargo test -p vm-kvm --features kvm
cargo clippy --workspace --all-targets --features vm-kvm/kvm -- -D warnings
```
