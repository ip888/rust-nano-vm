# syntax=docker/dockerfile:1.7

# nanovm-control-plane container image.
#
# Two-stage build: a `cargo-chef`-style dependency cache layer followed by
# a workspace build, then a minimal `gcr.io/distroless/cc-debian12` runtime
# that ships only the compiled binary, a non-root user, and the CA bundle.
#
# Build:
#   docker build -t nanovm-control-plane:dev .
#
# Run (auth disabled — DEV ONLY; see README for production env vars):
#   docker run --rm -p 8080:8080 \
#     -e NANOVM_CONTROL_PLANE_ADDR=0.0.0.0:8080 \
#     nanovm-control-plane:dev
#
# Run (production posture):
#   docker run --rm -p 8080:8080 \
#     -e NANOVM_CONTROL_PLANE_ADDR=0.0.0.0:8080 \
#     -e NANOVM_API_TOKENS="$(cat /path/to/tokens.txt)" \
#     -e NANOVM_RATE_LIMIT_RPS=100 \
#     nanovm-control-plane:dev

# ---------- builder ----------
# Pin to the same toolchain as rust-toolchain.toml. Bump in lockstep.
FROM rust:1.94.1-bookworm AS builder

WORKDIR /src

# Copy the manifest set first so the dependency layer caches cleanly: a
# source-only edit doesn't re-pull crates.io.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Workspace build. We only need the control-plane binary in the runtime
# image — building just `-p control-plane` skips the cli/guest-agent
# compile units. `--locked` mirrors what CI does (no surprise lockfile
# drift in the published image).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p control-plane --bin nanovm-control-plane && \
    cp /src/target/release/nanovm-control-plane /usr/local/bin/nanovm-control-plane

# ---------- runtime ----------
# `distroless/cc` ships glibc + ca-certificates and nothing else (no
# shell, no package manager). Drops attack surface and image size to
# roughly 25 MB + the binary. Non-root by default via the `:nonroot`
# tag (uid/gid 65532).
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /usr/local/bin/nanovm-control-plane /usr/local/bin/nanovm-control-plane

# `0.0.0.0:8080` so the binary listens on the container's external
# interface — the runtime default of `127.0.0.1:8080` is host-only and
# unreachable from `-p 8080:8080`. Operators override per-deploy.
ENV NANOVM_CONTROL_PLANE_ADDR=0.0.0.0:8080

EXPOSE 8080

# Runs as uid 65532 (nonroot) by virtue of the base-image tag.
ENTRYPOINT ["/usr/local/bin/nanovm-control-plane"]
