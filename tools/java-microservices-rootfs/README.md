# `tools/java-microservices-rootfs/`

Builds an initramfs containing the full **Spring Petclinic
Microservices** stack — the seven-service variant of Petclinic —
plus Oracle JDK 21 + init/warmup driver, running as a single
nanovm guest.

This is the M2 build target. Same shape and packaging conventions
as the M1 single-jar `tools/java-rootfs/`; every design choice
that carries over unchanged (Debian slim vs Alpine, Oracle JDK 21
NFTC, initramfs over virtio-blk, `linux/amd64` platform pin) is
documented there.

## Services packaged

Built from a pinned commit of `spring-projects/spring-petclinic-microservices`:

| Service | Port | Role |
|---|---|---|
| `config-server` | 8888 | Spring Cloud Config — serves per-service YAML |
| `discovery-server` | 8761 | Netflix Eureka — service registry |
| `admin-server` | 9090 | Spring Boot Admin — ops console |
| `api-gateway` | 8080 | Spring Cloud Gateway — external entrypoint |
| `customers-service` | dynamic | REST for owners/pets |
| `vets-service` | dynamic | REST for veterinarians |
| `visits-service` | dynamic | REST for visits |

Dynamic-port services register with Eureka; api-gateway load-balances
across their Eureka registrations. From the host bench harness's
perspective the only externally-reachable endpoint is
`http://<guest-ip>:8080/api/{customer,vet,visit}/*`.

## Boot ordering

The three "infra" services must reach their steady state before the
four "app" services start, otherwise Spring Cloud clients hard-fail
with `Config server not reachable` or `Registered instances not
found`. `tier-launcher.sh` enforces the sequence:

```
config-server (:8888)                    ← wait for /actuator/health
  ↓
discovery-server (:8761)                 ← wait for /actuator/health
  ↓
admin-server (:9090)                     ← wait for /actuator/health
  ↓
api-gateway + customers + vets + visits  ← launched in parallel
  ↓
NANOVM_MICROSERVICES_READY               ← emitted after all 7 register
                                           with Eureka (/eureka/apps)
```

Total cold-boot time (measured on host tier-1 hardware): ~90–120 s.
That's the number the snapshot-fork demo replaces with ~200 ms.

## Kernel-cmdline dial

The rootfs is single-purpose today — cmdline chooses only "run all 7
services in this guest, orderly". A future PR that adds virtio-net
bridge support to `crates/vm-kvm` will add a `NANOVM_TIER=infra|app`
knob so the same rootfs can run either half of a tier-split
deployment across two guests. Both are baked into the image so the
future split doesn't need a rebuild.

## Sizes

- **`initramfs.cpio.gz`**: expected ~250–300 MiB compressed
  (7 uber-jars ~60 MiB each after Spring Boot's fat-jar packaging;
  gzip -9 gets ~2:1)
- Unpacks to ~800–900 MiB in guest RAM
- Guest memory recommendation: **`--memory-mib 4096`** — 900 MiB
  rootfs + 7 × ~350 MiB JVM heaps + kernel + slack

Yes, the JVM footprint is the main resource driver. `-Xmx256m` per
JVM is set in `tier-launcher.sh` to keep total heap under 2 GiB;
raise on hosts with more RAM to give services headroom for realistic
load.

## Build

Same command shape as M1:

```sh
tools/java-microservices-rootfs/build.sh
# rootless / CI (initramfs only, no ext4):
SKIP_EXT4=1 tools/java-microservices-rootfs/build.sh
# native ARM64 host with an arm64 JDK:
TARGET_PLATFORM=linux/arm64 tools/java-microservices-rootfs/build.sh
```

Host prerequisites: `docker` + `buildx`, `cpio`, `gzip`, `find`,
optionally `sudo` + `mkfs.ext4` for the ext4 pack. `build.sh`
fails fast at startup on any missing tool.

Outputs:
- `tools/java-microservices-rootfs/cache/initramfs.cpio.gz`
- `tools/java-microservices-rootfs/cache/rootfs.ext4` (unless `SKIP_EXT4=1`)

## Pins

Same three pins as M1 (JDK sha256, Debian digest) + one new one:

| Component | Pinned to | Verified with |
|---|---|---|
| Oracle JDK | `21.0.5` | sha256 in `fetch.sh` (currently `SKIP`; strict mode fails) |
| Debian base | `debian:12-slim@sha256:…` | Digest at buildx time |
| **Microservices repo** | commit hash in `Dockerfile` `PETCLINIC_MS_REF` | Git checkout by hash |

Pinning workflow same as M1 — see `docs/prototypes/petclinic.md`
"Pinning workflow" section.
