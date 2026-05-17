# Self-hosted KVM runner setup

The default CI (`ci.yml`) runs on GitHub-hosted Ubuntu runners,
which **do not** expose `/dev/kvm`. The `kvm-ci.yml` workflow
targets a self-hosted runner labelled `[self-hosted, linux, kvm]`
so the `kvm` feature is exercised against real hardware on every
change that touches `vm-kvm` (or its sibling crates that wire into
the KVM path).

Closes tracked gap **G7** from [`docs/threat-model.md`](threat-model.md)
once a runner is online.

## Host requirements

| Property | Minimum | Notes |
| --- | --- | --- |
| Architecture | `x86_64` (`AArch64` later) | matches `kvm-bindings` |
| Kernel | 5.10+ | KVM API surface vm-kvm uses |
| `/dev/kvm` | present, RW for the runner user | check via `lscpu` for `Virtualization: VT-x` / `AMD-V` |
| CPU | nested virt enabled if the host is itself a VM | GCP `--enable-nested-virtualization`, AWS bare-metal, local hardware all fine |
| RAM | 4 GB free | KVM guests in the test suite are small |
| Disk | 10 GB free | cargo cache + kernel images |
| Network | egress to crates.io | runner pulls deps on first build |

## Step 1 — pick a host

Three vetted options:

1. **GCP nested virtualization** — `gcloud compute instances create
   nanovm-runner --zone=us-central1-a --machine-type=n2-standard-4
   --image-family=ubuntu-2204-lts --image-project=ubuntu-os-cloud
   --enable-nested-virtualization`. Cheap and rebuildable.
2. **AWS bare-metal** — `.metal` instance types (e.g.
   `c5.metal`, `m5.metal`). More expensive, no nested-virt overhead.
3. **Local hardware** — any Linux box with VT-x / AMD-V. Best
   latency, no per-hour bill.

See [`docs/kvm-host.md`](kvm-host.md) for the per-provider commands.

## Step 2 — install the runner

```sh
# Verify KVM is available before going further.
sudo apt-get update && sudo apt-get install -y cpu-checker qemu-utils
kvm-ok                                  # → "KVM acceleration can be used"
ls -l /dev/kvm                          # → crw-rw---- 1 root kvm …

# Create the runner user and give it KVM access.
sudo useradd -m -s /bin/bash gh-runner
sudo usermod -aG kvm gh-runner          # runner needs RW on /dev/kvm

# Install the Rust toolchain for the runner user (matches the
# project's rust-toolchain.toml). Reused across every job — only
# done once.
sudo -iu gh-runner bash <<'EOF'
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
echo 'source "$HOME/.cargo/env"' >> ~/.bashrc
EOF

# Fetch the runner binary. Replace the URL with the latest from
# https://github.com/actions/runner/releases — pin a SHA in prod.
sudo -iu gh-runner bash <<'EOF'
mkdir -p ~/actions-runner && cd ~/actions-runner
RUNNER_VERSION=2.319.1
curl -O -L \
  https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz
tar xzf ./actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz
EOF
```

## Step 3 — register against the repo

In **Settings → Actions → Runners → New self-hosted runner** on
the GitHub repo, grab the one-time registration token, then:

```sh
sudo -iu gh-runner bash <<EOF
cd ~/actions-runner
./config.sh \
  --url https://github.com/ip888/Rust-nano-vm \
  --token <PASTE_TOKEN_HERE> \
  --name kvm-runner-1 \
  --labels kvm \
  --runnergroup default \
  --work _work \
  --unattended
EOF
```

`--labels kvm` is what binds this runner to the `[self-hosted,
linux, kvm]` selector in `.github/workflows/kvm-ci.yml`. The
default `self-hosted` and `linux` labels are added automatically.

## Step 4 — run as a systemd service

```sh
sudo -iu gh-runner bash <<'EOF'
cd ~/actions-runner
sudo ./svc.sh install gh-runner
sudo ./svc.sh start
EOF

systemctl status actions.runner.ip888-Rust-nano-vm.kvm-runner-1
```

## Step 5 — first job

Push a no-op change to `crates/vm-kvm/src/lib.rs` (e.g. fix a typo)
or trigger `kvm-ci` manually via the Actions tab → kvm-ci → Run
workflow. You should see the runner pick up the job within a few
seconds.

## Hardening

The runner user has access to the repo's secrets and can execute
arbitrary code from the PR. Treat the runner host like a build
worker that you would not put on the same network as production
services:

- Run on an isolated VPC / VLAN.
- Disable auto-merge for external contributor PRs that would
  otherwise trigger the runner before review.
- Periodically rotate the runner token (the registration token is
  one-shot but the runner has a long-lived auth token under
  `~/actions-runner/.credentials`).
- Keep `/dev/kvm` mode 660 root:kvm. The KVM ioctls themselves
  cannot escape the guest, but the runner can still escape the
  *runner* — that's a CI compromise, not a KVM compromise.

## Operating

| Symptom | Triage |
| --- | --- |
| `kvm-ci` queued forever | Runner is offline. `systemctl status actions.runner.…` on the host. |
| Job fails on "Verify /dev/kvm" | Runner user lost group membership. `sudo usermod -aG kvm gh-runner && sudo systemctl restart actions.runner.…`. |
| Tests time out at 25 min | Wedged guest; runner needs reboot. Bump `timeout-minutes` only after confirming the test, not the runner, hangs. |
| Disk fills | `gh-runner` clears `_work/` between jobs but the cargo cache grows. `sudo -iu gh-runner cargo cache --autoclean` (after `cargo install cargo-cache`). |

## Decommissioning

```sh
sudo -iu gh-runner bash <<'EOF'
cd ~/actions-runner
sudo ./svc.sh stop
sudo ./svc.sh uninstall
./config.sh remove --token <REMOVAL_TOKEN_FROM_GITHUB_UI>
EOF
sudo userdel -r gh-runner
```
