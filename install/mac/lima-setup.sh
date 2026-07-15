#!/usr/bin/env bash
# Set up a Lima VM on macOS that runs a local nanovm control plane
# against real KVM. This is the only way to get "real microVMs on a
# Mac" without paying for a hosted control plane — Lima brings up a
# Linux VM (via Apple's Hypervisor.framework), nested KVM works on
# that Linux, and the nanovm-control-plane inside can spawn real
# microVMs.
#
# Prereqs (on the Mac side):
#   brew install lima
#   brew install --formula ./install/brew/nanovm.rb   # optional
#
# What this script does:
#   1. Creates a Lima config that requests nested virt.
#   2. `limactl start` the VM (first run downloads an Ubuntu image).
#   3. Installs Docker inside the VM.
#   4. Pulls the nanovm-control-plane container.
#   5. Prints the docker-run command with /dev/kvm mapped in.
#
# The Lima VM lives at ~/.lima/nanovm. `limactl stop nanovm` puts
# it on ice; `limactl delete nanovm` throws it away.

set -euo pipefail

VM_NAME=${VM_NAME:-nanovm}
LIMA_CONFIG_DIR="$HOME/.lima/_config"
mkdir -p "$LIMA_CONFIG_DIR"

if ! command -v limactl >/dev/null 2>&1; then
    echo "limactl not found. Install with: brew install lima" >&2
    exit 1
fi

# Write a Lima config with nested virt on. Apple Silicon exposes
# `vz` type + `vmType: "vz"` for the Apple Virtualization.framework;
# `x86_64` Macs fall back to qemu but Rosetta will translate.
CONFIG_FILE="$LIMA_CONFIG_DIR/${VM_NAME}.yaml"
cat > "$CONFIG_FILE" <<'YAML'
# Lima VM tuned for running nanovm-control-plane against real KVM.
# On Apple Silicon this uses the vz driver + nested virtualization.
vmType: "vz"

images:
  # Ubuntu 24.04 minimal — small download, kernel new enough (6.8) for
  # nested KVM under Apple's vz driver. Lima picks the arch that
  # matches the host.
  - location: "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-arm64.img"
    arch: "aarch64"
  - location: "https://cloud-images.ubuntu.com/minimal/releases/noble/release/ubuntu-24.04-minimal-cloudimg-amd64.img"
    arch: "x86_64"

cpus: 4
memory: "8GiB"
disk:   "40GiB"

# The important knob — surfaces /dev/kvm inside the guest.
rosetta:
  enabled: false
  binfmt: false
mountType: "virtiofs"

# Install docker on first boot so the second `limactl shell` call
# below just works. Cloud-init reads this.
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update
      apt-get install -y ca-certificates curl gnupg
      install -m 0755 -d /etc/apt/keyrings
      curl -fsSL https://download.docker.com/linux/ubuntu/gpg | \
        gpg --dearmor -o /etc/apt/keyrings/docker.gpg
      chmod a+r /etc/apt/keyrings/docker.gpg
      echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
            https://download.docker.com/linux/ubuntu $(. /etc/os-release; echo $VERSION_CODENAME) stable" \
            > /etc/apt/sources.list.d/docker.list
      apt-get update
      apt-get install -y docker-ce docker-ce-cli containerd.io
      usermod -aG docker $(id -un 1000)
YAML

echo "➜ Lima config written to $CONFIG_FILE"
echo "➜ Starting VM (first run downloads ~600MB and can take ~5 min)..."
limactl start --name="$VM_NAME" "$CONFIG_FILE"

echo "➜ Verifying /dev/kvm inside the VM..."
if limactl shell "$VM_NAME" -- test -c /dev/kvm; then
    echo "✓ /dev/kvm present in the Lima VM — real microVMs will work."
else
    echo "! /dev/kvm NOT present. Apple's vz driver may not have exposed nested KVM on your Mac model." >&2
    echo "! You can still run the mock backend inside the VM, but not real KVM." >&2
fi

cat <<'NEXT'

Next steps (run these inside the Lima VM):

  limactl shell nanovm
  docker run --rm --device=/dev/kvm -p 8080:8080 \
    ghcr.io/ip888/nanovm-control-plane-kvm:main

Then, back on your Mac (in another terminal):
  export NANOVM_API_URL=http://localhost:8080
  nanovm status

Lima forwards ports from the guest to the Mac by default, so
`localhost:8080` on your Mac hits the control-plane inside the VM.
NEXT
