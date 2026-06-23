# syntax=docker/dockerfile:1.6
#
# Build a production-shaped container image for `nanovm-control-plane`.
# Two stages:
#
#   1. `builder` — full Rust toolchain (pinned to rust-toolchain.toml),
#      compiles the control-plane binary with `--locked` so the
#      published image is reproducibly tied to the workspace's
#      Cargo.lock.
#
#   2. runtime — distroless `cc-debian12:nonroot` (~20 MiB, glibc-only,
#      no shell, no package manager). Just the binary + its dynamic
#      linker dependencies.
#
# This image bundles the control plane built against the `vm-mock`
# backend — the default the binary uses today. It does NOT yet ship a
# `vm-kvm`-backed build; real `/dev/kvm` integration is gated by a
# follow-up that turns the `kvm` feature on in this build and the
# release workflow.
#
# Build with:
#   docker build -t nanovm-control-plane:dev .
#
# Run with:
#   docker run -d --rm -p 8080:8080 \
#     -e NANOVM_API_TOKENS=dev-token \
#     nanovm-control-plane:dev
#
# Published image (set on tag push by .github/workflows/docker.yml):
#   ghcr.io/ip888/nanovm-control-plane:<version>

# ---- Stage 1: build the binary ---------------------------------------
FROM rust:1.94.1-bookworm AS builder

WORKDIR /src

# Copy the whole workspace. We rely on Docker's layer cache (and the
# release workflow's GHA cache) for fast incremental rebuilds rather
# than the more elaborate "copy Cargo.toml first" trick — which doesn't
# play well with this workspace's many path-dep crates.
COPY . .

# `--locked` mirrors what the release workflow's tarball builds. Build
# only what we need for the runtime image: the control plane binary
# (the CLI ships in a separate tarball asset, not the container).
RUN cargo build --release --locked -p control-plane --bin nanovm-control-plane

# ---- Stage 2: runtime ------------------------------------------------
# `cc-debian12:nonroot` is the right Distroless variant: includes
# glibc + libgcc for our dynamically-linked binary, no shell, no
# package manager, runs as uid 65532 by default. Image size ~22 MiB.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder \
     /src/target/release/nanovm-control-plane \
     /usr/local/bin/nanovm-control-plane

# Bind to all interfaces by default so `-p 8080:8080` works out of
# the box. The binary's own default is 127.0.0.1:8080 (sane for
# host-level dev, but unreachable from a container's port mapping).
# Operators that want loopback-only inside the container can override:
#   -e NANOVM_CONTROL_PLANE_ADDR=127.0.0.1:8080
ENV NANOVM_CONTROL_PLANE_ADDR=0.0.0.0:8080

EXPOSE 8080

# Distroless ships a `nonroot` user (uid 65532). The control plane
# only needs to bind a high port and read env vars; no privileged
# capabilities required for the mock backend.
USER nonroot

# ENTRYPOINT not CMD so `docker run ... extra-args` is rejected — the
# binary is env-var-driven, there are no CLI flags worth surfacing.
ENTRYPOINT ["/usr/local/bin/nanovm-control-plane"]
