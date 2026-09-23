# syntax=docker/dockerfile:1.7
# Multi-stage build for the Petclinic-Microservices-in-a-microVM rootfs.
#
# Stage 1 (`petclinic-ms-builder`): Maven-builds ALL seven services
# from a pinned commit of spring-projects/spring-petclinic-microservices.
# Yields seven uber-jars under target/, copied to /jars/ under known
# stable names.
#
# Stage 2 (`jdk-extract`): identical to the single-jar rootfs — unpacks
# Oracle JDK 21, strips docs/man/jmods, dumps a Class Data Sharing
# archive. Kept as its own stage so the JDK layer is cache-hit
# across both rootfs builds when contributors iterate.
#
# Stage 3 (final): Debian slim + JDK + seven jars + init + warmup +
# tier-launcher scripts. Base image pinned by digest via DEBIAN_DIGEST
# in both FROM references so `docker buildx` fails closed on a bad
# override.

ARG DEBIAN_DIGEST=sha256:PLACEHOLDER_PIN_BEFORE_MERGE
ARG PETCLINIC_MS_REPO=https://github.com/spring-petclinic/spring-petclinic-microservices.git
ARG PETCLINIC_MS_CONFIG_REPO=https://github.com/spring-petclinic/spring-petclinic-microservices-config.git
# Pin to specific commits on `main` before merging any release track.
# Sentinels make the build fail loudly if left unpinned in a shipping
# build.
ARG PETCLINIC_MS_REF=PLACEHOLDER_PIN_BEFORE_MERGE
ARG PETCLINIC_MS_CONFIG_REF=PLACEHOLDER_PIN_BEFORE_MERGE
ARG JDK_TARBALL=jdk-21.0.5_linux-x64_bin.tar.gz

# ---- stage 1: build the seven microservice jars ----------------------
FROM eclipse-temurin:21-jdk AS petclinic-ms-builder
ARG PETCLINIC_MS_REPO
ARG PETCLINIC_MS_REF
ARG PETCLINIC_MS_CONFIG_REPO
ARG PETCLINIC_MS_CONFIG_REF
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git clone "$PETCLINIC_MS_REPO" spring-petclinic-microservices
WORKDIR /src/spring-petclinic-microservices
RUN git checkout "$PETCLINIC_MS_REF"
# `-DskipTests -Dmaven.javadoc.skip=true`: same rationale as M1 — we're
# packaging Petclinic, not validating it.
RUN ./mvnw -B package -DskipTests -Dmaven.javadoc.skip=true
# Copy every service jar under a stable name. Version suffix in the
# upstream artifacts varies, so glob the target/ directories.
RUN mkdir -p /jars \
 && cp spring-petclinic-config-server/target/spring-petclinic-config-server-*.jar     /jars/config-server.jar \
 && cp spring-petclinic-discovery-server/target/spring-petclinic-discovery-server-*.jar /jars/discovery-server.jar \
 && cp spring-petclinic-admin-server/target/spring-petclinic-admin-server-*.jar       /jars/admin-server.jar \
 && cp spring-petclinic-api-gateway/target/spring-petclinic-api-gateway-*.jar         /jars/api-gateway.jar \
 && cp spring-petclinic-customers-service/target/spring-petclinic-customers-service-*.jar /jars/customers-service.jar \
 && cp spring-petclinic-vets-service/target/spring-petclinic-vets-service-*.jar       /jars/vets-service.jar \
 && cp spring-petclinic-visits-service/target/spring-petclinic-visits-service-*.jar   /jars/visits-service.jar \
 && ls -lh /jars
# Clone the config repository too. Upstream's config-server defaults to
# reading YAML from a Git backend pointed at
# `spring-petclinic-microservices-config` — the KVM guests have no
# network by default, so we bundle the repo contents inside the rootfs
# and switch config-server to Spring Cloud Config's `native` file
# backend (see tier-launcher.sh).
WORKDIR /src
RUN git clone "$PETCLINIC_MS_CONFIG_REPO" spring-petclinic-microservices-config
WORKDIR /src/spring-petclinic-microservices-config
RUN git checkout "$PETCLINIC_MS_CONFIG_REF" \
 && mkdir -p /config \
 && cp -a . /config/ \
 && rm -rf /config/.git \
 && ls -la /config

# ---- stage 2: extract Oracle JDK 21 ----------------------------------
FROM debian:12-slim@${DEBIAN_DIGEST} AS jdk-extract
ARG JDK_TARBALL
COPY cache/${JDK_TARBALL} /tmp/jdk.tar.gz
RUN mkdir -p /opt/jdk-21 \
 && tar -xzf /tmp/jdk.tar.gz -C /opt/jdk-21 --strip-components=1 \
 && rm /tmp/jdk.tar.gz
RUN rm -rf /opt/jdk-21/jmods /opt/jdk-21/legal /opt/jdk-21/man \
           /opt/jdk-21/lib/src.zip
RUN /opt/jdk-21/bin/java -Xshare:dump

# ---- stage 3: assemble the rootfs ------------------------------------
FROM debian:12-slim@${DEBIAN_DIGEST}
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl iproute2 procps \
 && rm -rf /var/lib/apt/lists/* \
 && rm -rf /usr/share/doc /usr/share/man /usr/share/locale
COPY --from=jdk-extract           /opt/jdk-21  /opt/jdk-21
COPY --from=petclinic-ms-builder  /jars        /opt/petclinic-ms
# Config repo bundled into the rootfs alongside the jars. config-server
# reads from `/opt/petclinic-ms/config/` via Spring Cloud Config's
# native (file) backend — no outbound Git needed at guest boot.
COPY --from=petclinic-ms-builder  /config      /opt/petclinic-ms/config
COPY init.sh            /sbin/init
COPY tier-launcher.sh   /tier-launcher.sh
COPY warmup.sh          /warmup.sh
RUN chmod +x /sbin/init /tier-launcher.sh /warmup.sh \
 && ln -s /sbin/init /init
# Same device-node fix as the M1 rootfs (docker export drops the
# runtime /dev mount; PID 1's stdio needs /dev/console at boot).
RUN mkdir -p /dev \
 && mknod -m 600 /dev/console c 5 1 \
 && mknod -m 666 /dev/null    c 1 3 \
 && mknod -m 666 /dev/zero    c 1 5 \
 && mknod -m 666 /dev/tty     c 5 0 \
 && mknod -m 666 /dev/random  c 1 8 \
 && mknod -m 666 /dev/urandom c 1 9
CMD ["/sbin/init"]
