# RBAC — role-based access control

The control plane classifies every authenticated caller into one of
three roles and gates the destructive/admin surface accordingly. The
role is attached at token-issue time (env var or, in a follow-up, IdP
group mapping) and carried in `Extension<Role>` alongside the caller's
`Extension<OrgId>` on every request.

## Roles

Ordered least → most privileged:

| Role         | Intended for                                                       | Wire name           |
|--------------|--------------------------------------------------------------------|---------------------|
| `Viewer`     | Auditors, dashboards that only need to read.                       | `viewer` / `readonly` / `read-only` |
| `Developer`  | Everyday users: create/destroy VMs, fork snapshots, run exec, sandbox actions. | `developer` / `dev` |
| `Admin`      | Org owners: mint/revoke API keys, manage billing subscription.     | `admin`             |

`Role::Admin >= Role::Developer >= Role::Viewer`. A stricter role always
satisfies a looser requirement — `require_role(caller, min)` succeeds
when `caller >= min`.

## Enforcement matrix

| Endpoint                          | Minimum role  | Denies with                          |
|-----------------------------------|---------------|--------------------------------------|
| `POST /v1/keys`                   | `Admin`       | `403 { error.code = "role_required" }` |
| `DELETE /v1/keys/:id`             | `Admin`       | `403 { error.code = "role_required" }` |
| `GET /v1/billing/portal`          | `Admin`       | `403 { error.code = "role_required" }` |
| `DELETE /v1/vms/:id`              | `Developer`   | `403 { error.code = "role_required" }` |
| `DELETE /v1/snapshots/:id`        | `Developer`   | `403 { error.code = "role_required" }` |
| every other authenticated route   | `Viewer`      | (any authenticated caller passes)    |

The `role_required` error code is machine-readable — SDKs and dashboards
key off it to differentiate role denials from ownership-mismatch 403s
(which use `forbidden_cross_org`).

## Env-var format

`NANOVM_API_TOKENS` is a comma-separated list. Each entry:

```
[org:]token[@role]
```

Examples:

```bash
# Single-tenant deployment — every token lands in the `default` org as Admin.
# This is the legacy shape; existing deployments keep working byte-for-byte.
NANOVM_API_TOKENS=t-legacy-1,t-legacy-2

# Multi-tenant, no explicit roles — everyone is Admin (safe migration path).
NANOVM_API_TOKENS=acme:t-acme,globex:t-globex

# Multi-tenant with role suffixes — the real production shape.
NANOVM_API_TOKENS=acme:t-viewer@viewer,acme:t-dev@developer,acme:t-owner@admin
```

The `@role` separator is deliberately `@` and not `:` — a token secret
containing `:` (e.g. `sk-live:abc123`) used to silently truncate when
the tail happened to match a role name. `@` is not a legal role
character AND is highly unusual inside opaque bearer tokens, so
`sk-live:abc123` parses unambiguously as the full token, and
`sk-live:abc123@admin` unambiguously means "with admin role."

Role names are case-insensitive (`Admin` == `admin` == `ADMIN`).
`viewer` also accepts `readonly` / `read-only`; `developer` also
accepts `dev`. Unknown role tails fall back to `Admin` and are treated
as part of the token, not as a role marker — a typo can't silently
privilege-escalate.

## Backward compatibility

- Tokens without an `@role` suffix default to `Admin`. Existing
  single-tenant deployments migrate zero-config.
- Runtime-issued keys (via `POST /v1/keys`) currently default to
  `Admin` too. A follow-up will accept an optional `role` body field so
  a dashboard-minted "read-only CI token" can be scoped down.
- Auth-disabled dev mode (empty `NANOVM_API_TOKENS`) injects `Admin`
  so every handler stays reachable for local `cargo run`.

## What role enforcement does NOT do

- **Resource ownership** (which VMs/snapshots a token may touch) lives
  in `OwnershipMap` at the handler layer. A `Developer` in org A still
  can't touch a VM in org B — that's a `forbidden_cross_org` 403, not a
  role denial.
- **Rate limiting** — that's `ForkQuota`, per-token token-bucket.
- **SSO group mapping** — the role field is populated from
  `NANOVM_API_TOKENS` today. SSO integration (Clerk / WorkOS) will
  map IdP group membership to `Role` in a follow-up PR without
  touching this file.
