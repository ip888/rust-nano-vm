# nanovm install recipes

Cross-platform ways to get the `nanovm` CLI + Python SDK working on a
developer's laptop. Pick the row that matches your setup.

| Platform | Recipe | Real KVM? | SDK/CLI? |
|---|---|---|---|
| **Linux with `/dev/kvm`** | `pip install nanovm` | ✅ | ✅ |
| **Linux without KVM** (CI, generic VPS) | `pip install nanovm` | ❌ (mock backend only) | ✅ |
| **macOS (Intel or Apple Silicon)** | Fill in the sha256s in `install/brew/nanovm.rb.template`, then `brew install --formula ./install/brew/nanovm.rb` (or wait for the published tap) | ❌ natively — use SaaS OR `bash install/mac/lima-setup.sh` for local Linux VM w/ nested KVM | ✅ |
| **Windows** | `bash install/wsl2/install.sh` (run inside WSL2 shell) | ✅ (WSL2 kernel ≥ 5.10 exposes nested KVM) | ✅ |

## The honest matrix

- **KVM is a Linux-kernel feature.** Mac and Windows can only get "real" microVM isolation by running Linux themselves (WSL2 on Windows; Lima/UTM/Docker Desktop on Mac).
- **The SDK + CLI ship everywhere Python does.** If you're targeting our SaaS (`nanovm.io`), or someone else's hosted control-plane, the CLI works on any Python-supported OS with no VM stack needed locally.
- **Local mock backend works everywhere.** Great for SDK development / CI / testing agent integrations without a real hypervisor. Not usable for real workloads.

## Common flow (any platform)

Once the CLI is on your PATH:

```bash
# Sign up (via the dashboard) or paste an existing API key.
nanovm login --api-url https://api.your-saas.com

# Verify it worked.
nanovm status

# Run something.
nanovm python 'print(sum(range(100)))'
nanovm shell  'uname -a'

nanovm logout
```

See [`clients/python/README.md`](../clients/python/README.md) for the Python SDK reference and [`docs/saas-billing.md`](../docs/saas-billing.md) for operator-side SaaS setup.
