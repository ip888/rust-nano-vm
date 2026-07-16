#!/usr/bin/env bash
# Install the `nanovm` CLI + Python SDK inside a Windows WSL2 shell.
#
# Run this INSIDE your WSL2 distribution (Ubuntu / Debian / Fedora
# etc.), NOT from a Windows PowerShell or cmd.exe. Everything happens
# on the Linux side.
#
# What it does:
#   1. Verifies /dev/kvm is present (WSL2 kernel 5.10+ exposes it —
#      warns if missing so you can enable nested virt in the .wslconfig).
#   2. Installs python3 + pip if either is missing.
#   3. `pip install --user nanovm`.
#   4. Prints the next-step commands and where the binary landed.
#
# No sudo assumed — if your WSL2 distro doesn't have Python installed
# and you don't have sudo, the script prints the exact command you
# need and exits.

set -euo pipefail

info()  { printf '\033[36m➜\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m!\033[0m %s\n' "$*" >&2; }
error() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }

# ---- 1. Preconditions ----------------------------------------------------

if [ ! -f /proc/version ] || ! grep -qi 'microsoft' /proc/version; then
    warn "This doesn't look like a WSL2 shell (/proc/version has no 'microsoft'). Continuing anyway."
fi

if [ -c /dev/kvm ]; then
    ok "/dev/kvm present — nested KVM is available, real microVMs will work."
else
    warn "/dev/kvm not found. You'll only get the mock backend."
    warn "To enable nested KVM in WSL2, add to %USERPROFILE%\\.wslconfig on Windows:"
    warn "  [wsl2]"
    warn "  nestedVirtualization=true"
    warn "  kernelCommandLine=intel_iommu=on"
    warn "Then in PowerShell as admin: wsl --shutdown"
fi

# ---- 2. Python + pip -----------------------------------------------------

# Two-step check: only probe `python3 -m pip` if python3 itself exists.
# Otherwise `set -euo pipefail` on some shells (and `command not found`
# handlers on others) would blow up before we can print the friendly
# install hint. `pip` is useless without python3 anyway — asking for both
# at once when python3 is missing is the correct message.
need_install=()
if ! command -v python3 >/dev/null 2>&1; then
    need_install+=("python3" "python3-pip")
elif ! python3 -m pip --version >/dev/null 2>&1; then
    need_install+=("python3-pip")
fi

if [ "${#need_install[@]}" -gt 0 ]; then
    warn "Missing: ${need_install[*]}"
    if command -v apt-get >/dev/null 2>&1; then
        info "Run:  sudo apt-get update && sudo apt-get install -y ${need_install[*]}"
    elif command -v dnf >/dev/null 2>&1; then
        info "Run:  sudo dnf install -y ${need_install[*]}"
    elif command -v pacman >/dev/null 2>&1; then
        # Arch: package names differ.
        info "Run:  sudo pacman -Syu python python-pip"
    else
        info "Install python3 + pip via your distro's package manager, then re-run this script."
    fi
    exit 1
fi
ok "python3 + pip present."

# ---- 3. Install nanovm ---------------------------------------------------

# `--user` puts the entry point at ~/.local/bin/nanovm without needing
# root. `--upgrade` is a no-op on first install but keeps re-runs idempotent
# (bumps to latest PyPI).
info "Installing nanovm SDK + CLI from PyPI..."
python3 -m pip install --user --upgrade nanovm

# On most WSL2 distros ~/.local/bin isn't on PATH by default. Print a
# helpful next-step but don't silently mutate the user's shell rc.
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$HOME/.local/bin"; then
    warn "~/.local/bin isn't on your PATH. Add it with:"
    warn "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc"
    warn "  source ~/.bashrc"
fi

# ---- 4. Verify + next steps ---------------------------------------------

if command -v nanovm >/dev/null 2>&1; then
    ok "nanovm installed at $(command -v nanovm)"
    nanovm --version || true
else
    ok "nanovm installed at $HOME/.local/bin/nanovm (not on PATH yet — see warning above)"
fi

cat <<'NEXT'

Next steps:
  # Point the CLI at your control plane (SaaS or local):
  nanovm login --api-url https://api.your-saas.com

  # Verify the login worked:
  nanovm status

  # Run something in a sandbox:
  nanovm python 'print(1+1)'
NEXT
