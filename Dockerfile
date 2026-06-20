# syntax=docker/dockerfile:1.7
#
# knot-server multi-arch container.
#   builder #1 (web): node:22 builds the SPA -> /app/web/dist
#   builder #2 (rust): chef base installs the toolchain -> planner derives recipe.json ->
#     builder cooks deps (gha-cacheable layer) then cargo-zigbuild cross-compiles to
#     ${TARGETARCH}-unknown-linux-musl on the BUILDPLATFORM toolchain (no per-arch QEMU).
#   runtime: scratch + static binary + web/dist (TLS roots are compiled into the binary).

# ----- SPA build -----
FROM --platform=$BUILDPLATFORM node:22-alpine AS web-builder
WORKDIR /app/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ .
RUN pnpm build
# /app/web/dist now contains the built SPA

# ----- Rust build (cargo-chef: dependency compile lands in a cache-exportable layer) -----
# Base tag MUST stay in sync with rust-toolchain.toml (channel = "1.96.0"). We deliberately do
# NOT copy rust-toolchain.toml into the build (it would pull dev-only components such as
# rust-analyzer/rust-src); instead the base image supplies the pinned compiler so the release
# binary is built with the same toolchain as CI rather than the base image's default.
#
# Why cargo-chef: the workspace's compiled artifacts (target/) used to live only in a BuildKit
# --mount=type=cache, which the gha/registry cache exporters do NOT persist — so every tagged
# release recompiled all dependencies from zero on a fresh runner. `cargo chef cook` compiles
# only third-party deps into a normal filesystem layer keyed on recipe.json, and type=gha
# (mode=max) DOES persist that layer, so unchanged deps are a cache hit on later releases.
FROM --platform=$BUILDPLATFORM rust:1.96.0-alpine AS chef
RUN apk add --no-cache musl-dev openssl-dev pkgconf clang lld build-base curl xz tar
# Pinned exactly (=) for reproducible release builds; bump deliberately.
RUN cargo install cargo-chef --locked --version =0.1.77 \
 && cargo install cargo-zigbuild --locked --version =0.23.0
# Fetch zig (the cross-linker cargo-zigbuild drives) and verify its SHA256 before extracting —
# the tarball ends up in every released binary, so an unverified download is a build-time
# supply-chain hole. Digests are for zig 0.13.0, keyed by BUILDPLATFORM arch (uname -m).
RUN ARCH=$(uname -m) \
 && case "$ARCH" in \
      x86_64)  ZIG_SHA=d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea ;; \
      aarch64) ZIG_SHA=041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556 ;; \
      *) echo "unsupported build arch: $ARCH" >&2; exit 1 ;; \
    esac \
 && curl -sSLo /tmp/zig.tar.xz "https://ziglang.org/download/0.13.0/zig-linux-${ARCH}-0.13.0.tar.xz" \
 && echo "${ZIG_SHA}  /tmp/zig.tar.xz" | sha256sum -c - \
 && tar -xJf /tmp/zig.tar.xz -C /usr/local \
 && ln -s /usr/local/zig-linux-${ARCH}-0.13.0/zig /usr/local/bin/zig \
 && rm /tmp/zig.tar.xz
WORKDIR /src

# Planner: derive the dependency recipe from the manifests. Source-only edits that don't change
# the dependency graph leave recipe.json byte-identical, keeping the cook layer below a cache hit.
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tools ./tools
RUN cargo chef prepare --recipe-path recipe.json

# Builder: cook deps into a cacheable layer, then compile the workspace.
FROM chef AS builder
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl > /target ;; \
      arm64) echo aarch64-unknown-linux-musl > /target ;; \
      *) echo "unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
    esac
RUN rustup target add "$(cat /target)"
# Compile ONLY dependencies. No --mount=type=cache here on purpose: the compiled artifacts must
# land in this image layer so the gha cache can export and restore them across CI runs.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --zigbuild --target "$(cat /target)" --recipe-path recipe.json
# Real sources; from here only the workspace crates recompile (deps are already cooked above).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
COPY tools ./tools
# Stamp version/commit into the binary (knot-server build.rs reads these). Defaults match
# build.rs's own fallbacks so local builds still report dev/unknown; release.yaml passes the
# real tag + sha. Placed after the dep cook so a new commit recompiles only the workspace
# crates, leaving the cooked-deps layer a cache hit.
ARG KNOT_VERSION=dev
ARG KNOT_COMMIT=unknown
ENV KNOT_VERSION=${KNOT_VERSION} KNOT_COMMIT=${KNOT_COMMIT}
RUN cargo zigbuild --release --locked --target "$(cat /target)" --bin knot-server \
 && cp "target/$(cat /target)/release/knot-server" /knot-server

# ----- Runtime: scratch -----
FROM scratch AS runtime
COPY --from=builder /knot-server /knot-server
COPY --from=web-builder /app/web/dist /web/dist
# No CA bundle is shipped: every outbound TLS client links compiled-in webpki roots — reqwest
# (OIDC) and rust-s3/attohttpc (blob backend) both use rustls + webpki-roots, while OTLP and
# metrics are plaintext (tonic h2c / Prometheus scrape). Nothing reads the OS trust store. If a
# client is ever switched to native roots, or the Prometheus push-gateway / OTLP-over-TLS is
# enabled, add `apk add ca-certificates` in the builder, COPY /etc/ssl/certs/ca-certificates.crt
# here, and set ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt.
USER 65534:65534
EXPOSE 3000
ENV KNOT_LOG_FORMAT=json
ENV KNOT_WEB_DIST=/web/dist
ENTRYPOINT ["/knot-server"]
