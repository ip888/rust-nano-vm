# systemd packaging

This directory ships a hardened systemd unit and matching
`EnvironmentFile` example for running `nanovm-control-plane` on
Debian/Ubuntu/RHEL hosts where Docker is overkill or undesired.

## Files

| File | Purpose |
| --- | --- |
| `nanovm-control-plane.service` | systemd unit. Conservative sandbox: no privileges, no network egress (override per deployment), seccomp filter, read-only FS. |
| `nanovm-control-plane.env.example` | Operator-facing knobs. Mirrors every `NANOVM_*` env var the binary reads, with the production defaults pre-filled. |

## Install

```sh
# 1. Build the binary on the same kernel/glibc combination as the
#    target host, or use the distroless image instead (see Dockerfile).
cargo build --release -p control-plane

# 2. Stage the binary.
sudo install -m 0755 target/release/nanovm-control-plane /usr/local/bin/

# 3. Stage the env file with restrictive perms (NANOVM_API_TOKENS
#    is a secret).
sudo install -d -o root -g nanovm -m 0750 /etc/nanovm
sudo install -o root -g nanovm -m 0640 \
    packaging/systemd/nanovm-control-plane.env.example \
    /etc/nanovm/control-plane.env
sudo vi /etc/nanovm/control-plane.env   # set NANOVM_API_TOKENS

# 4. Create the unprivileged service user.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin nanovm

# 5. Pre-create the writable directory the unit allows
#    (ReadWritePaths=/var/lib/nanovm). Used for snapshot manifests.
sudo install -d -o nanovm -g nanovm -m 0750 /var/lib/nanovm

# 6. Install and start.
sudo install -m 0644 packaging/systemd/nanovm-control-plane.service \
    /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nanovm-control-plane.service

# 7. Verify.
systemctl status nanovm-control-plane
curl -fsS http://127.0.0.1:8080/healthz
journalctl -u nanovm-control-plane -e
```

## Drop-in customisation

Don't edit the unit. Override per-deployment via a drop-in file:

```sh
sudo systemctl edit nanovm-control-plane
# ...drops you into an editor for
# /etc/systemd/system/nanovm-control-plane.service.d/override.conf
```

Common drop-ins:

```ini
# Allow egress to a snapshot upload bucket.
[Service]
IPAddressAllow=10.0.0.0/8
IPAddressDeny=any

# Raise file-descriptor limit for many concurrent connections.
[Service]
LimitNOFILE=131072
```

## Hardening notes

The default unit applies most of the
[systemd hardening "stack"](https://www.freedesktop.org/software/systemd/man/systemd.exec.html)
relevant to a Rust HTTP server:

- `User=nanovm`, `Group=nanovm` — unprivileged identity.
- `CapabilityBoundingSet=`, `AmbientCapabilities=` — no capabilities.
- `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true` —
  filesystem isolation.
- `IPAddressDeny=any` — no outbound by default; operators that need
  egress override via drop-in (see above).
- `SystemCallFilter=@system-service ~@privileged …` — seccomp
  filter, `EPERM` on disallowed syscalls. If `nanovm-control-plane`
  fails with `SIGSYS`, add the missing class via drop-in rather than
  removing the filter.
- `NoNewPrivileges=true`, `MemoryDenyWriteExecute=true` — exploit
  mitigation classics.

Closes part of tracked gap **G6** from `docs/threat-model.md` (the
systemd-confined deployment path; a dedicated seccomp-BPF filter
inside the binary itself remains future work for the container/raw
deployment paths).

## When NOT to use this

- You're already running the distroless Docker image — use that; the
  container runtime applies its own isolation.
- You need multi-host orchestration — graduate to Kubernetes /
  Nomad. The env vars in the example map 1:1 to Helm
  `extraEnvVars`.
