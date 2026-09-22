# syntax=docker/dockerfile:1.7
# Multi-stage build for the Petclinic-in-a-microVM rootfs.
#
# Stage 1 (`petclinic-builder`): Maven-builds spring-petclinic from a
# pinned commit, producing a single uber-jar.
#
# Stage 2 (`jdk-extract`): unpacks the Oracle JDK 21 tarball fetched
# by `fetch.sh` into `/opt/jdk-21`, strips docs / man pages to trim
# ~40 MiB off the final rootfs.
#
# Stage 3 (final): Debian 12 slim + JDK + jar + init/warmup scripts.
# The output is intended to be flattened into an ext4 image by
# `build.sh`, not run as a container.
#
# The Debian slim base is pinned by digest (DEBIAN_DIGEST) rather than
# the mutable `debian:12-slim` tag. Both `FROM` references below
# interpolate `${DEBIAN_DIGEST}` so an override via
# `--build-arg DEBIAN_DIGEST=sha256:...` takes effect everywhere.

ARG DEBIAN_DIGEST=sha256:PLACEHOLDER_PIN_BEFORE_MERGE
ARG PETCLINIC_REPO=https://github.com/spring-projects/spring-petclinic.git
# Commit on `main` from spring-projects/spring-petclinic. Pin to a real
# hash before merging so a rebuild is reproducible; the sentinel below
# fails the build loudly if left unpinned.
ARG PETCLINIC_REF=PLACEHOLDER_PIN_BEFORE_MERGE
ARG JDK_TARBALL=jdk-21.0.5_linux-x64_bin.tar.gz

# ---- stage 1: build the Petclinic jar --------------------------------
FROM eclipse-temurin:21-jdk AS petclinic-builder
ARG PETCLINIC_REPO
ARG PETCLINIC_REF
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# `git clone` + explicit `checkout` (rather than `--branch <ref>`)
# because `--branch` doesn't accept a commit hash.
RUN git clone "$PETCLINIC_REPO" spring-petclinic
WORKDIR /src/spring-petclinic
RUN git checkout "$PETCLINIC_REF"
# `-DskipTests`: we're not validating Petclinic here, just packaging
# it. Its own CI covers correctness.
# `-Dmaven.javadoc.skip=true`: no javadoc in the final image.
RUN ./mvnw -B package -DskipTests -Dmaven.javadoc.skip=true
# The output filename varies across Petclinic versions (e.g.
# spring-petclinic-3.5.0-SNAPSHOT.jar). Copy under a stable name.
RUN cp target/spring-petclinic-*.jar /petclinic.jar \
 && ls -lh /petclinic.jar

# ---- stage 2: extract Oracle JDK 21 ----------------------------------
FROM debian:12-slim@${DEBIAN_DIGEST} AS jdk-extract
ARG JDK_TARBALL
COPY cache/${JDK_TARBALL} /tmp/jdk.tar.gz
RUN mkdir -p /opt/jdk-21 \
 && tar -xzf /tmp/jdk.tar.gz -C /opt/jdk-21 --strip-components=1 \
 && rm /tmp/jdk.tar.gz
# Strip cruft that doesn't affect JVM behaviour but bloats the rootfs.
# `jmods/` is only needed at jlink time; `legal/` and `man/` are docs.
RUN rm -rf /opt/jdk-21/jmods /opt/jdk-21/legal /opt/jdk-21/man \
           /opt/jdk-21/lib/src.zip
# Precompute a Class Data Sharing archive (`-Xshare:dump`) so the JVM
# skips class-loading work at boot. Init.sh uses `-Xshare:auto` to pick
# it up.
RUN /opt/jdk-21/bin/java -Xshare:dump

# ---- stage 3: assemble the rootfs ------------------------------------
FROM debian:12-slim@${DEBIAN_DIGEST}
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl iproute2 procps \
 && rm -rf /var/lib/apt/lists/* \
 && rm -rf /usr/share/doc /usr/share/man /usr/share/locale
COPY --from=jdk-extract         /opt/jdk-21     /opt/jdk-21
COPY --from=petclinic-builder   /petclinic.jar  /opt/petclinic.jar
COPY init.sh    /sbin/init
COPY warmup.sh  /warmup.sh
# `/sbin/init` is what Linux expects to exec when the kernel command
# line either omits `init=` or an initramfs hands off to the rootfs
# with no override. A `/init` symlink covers the alternative style
# (some KVM boot recipes pass `init=/init`) so the same rootfs works
# either way.
RUN chmod +x /sbin/init /warmup.sh \
 && ln -s /sbin/init /init
# Docker export drops the runtime /dev bind-mount, so the layer would
# otherwise ship with no device nodes at all. Linux wires PID 1's
# stdin/stdout/stderr from /dev/console BEFORE running the binary, so
# a rootfs missing /dev/console loses every log line the JVM and
# warmup driver print — the host-side ready-marker tail sees nothing.
# Create the essentials here so they land in the docker-export tar
# and, via that, in the initramfs cpio. `init.sh` still mounts
# devtmpfs to pick up dynamic nodes, but the baseline entries have to
# exist before PID 1 runs.
RUN mkdir -p /dev \
 && mknod -m 600 /dev/console c 5 1 \
 && mknod -m 666 /dev/null    c 1 3 \
 && mknod -m 666 /dev/zero    c 1 5 \
 && mknod -m 666 /dev/tty     c 5 0 \
 && mknod -m 666 /dev/random  c 1 8 \
 && mknod -m 666 /dev/urandom c 1 9
# `docker run` isn't the intended launch path — `build.sh` extracts
# the layer contents and packs them into an initramfs cpio the KVM
# backend loads via `VmConfig.initrd`. The CMD below is set only so
# `docker run` works for smoke-checking the layer contents (see the
# reproduce doc's step 3), and needs `--privileged` in Docker because
# `init.sh`'s pseudo-fs mounts require CAP_SYS_ADMIN outside the
# nanovm guest.
CMD ["/sbin/init"]
