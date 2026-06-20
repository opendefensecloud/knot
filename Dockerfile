# syntax=docker/dockerfile:1.7
#
# knot-server multi-arch container.
#   builder #1 (web): node:22 builds the SPA -> /app/web/dist
#   builder #2 (rust): rust:alpine + cargo-zigbuild cross-compiles to
#     ${TARGETARCH}-unknown-linux-musl using the BUILDPLATFORM toolchain
#     (no per-arch QEMU).
#   runtime: scratch + static binary + CA certs + web/dist.

# ----- SPA build -----
FROM --platform=$BUILDPLATFORM node:22-alpine AS web-builder
WORKDIR /app/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ .
RUN pnpm build
# /app/web/dist now contains the built SPA

# ----- Rust build -----
# Base tag MUST stay in sync with rust-toolchain.toml (channel = "1.96.0"). We deliberately
# do NOT copy rust-toolchain.toml into the build (it would pull dev-only components such as
# rust-analyzer/rust-src); instead the base image supplies the pinned compiler so the release
# binary is built with the same toolchain as CI rather than the base image's default.
FROM --platform=$BUILDPLATFORM rust:1.96.0-alpine AS rust-builder
ARG TARGETARCH
RUN apk add --no-cache musl-dev openssl-dev pkgconf clang lld build-base curl xz tar
RUN cargo install cargo-zigbuild --locked
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
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl > /target ;; \
      arm64) echo aarch64-unknown-linux-musl > /target ;; \
      *) echo "unsupported arch: $TARGETARCH" >&2; exit 1 ;; \
    esac
RUN rustup target add "$(cat /target)"

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
COPY tools ./tools

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo zigbuild --release --locked --target "$(cat /target)" --bin knot-server \
 && cp "target/$(cat /target)/release/knot-server" /knot-server

# ----- Runtime: scratch -----
FROM scratch AS runtime
COPY --from=rust-builder /knot-server /knot-server
COPY --from=web-builder /app/web/dist /web/dist
# CA bundle for OIDC discovery, OTLP, etc. (musl scratch has no certs).
COPY --from=rust-builder /etc/ssl/cert.pem /etc/ssl/cert.pem
USER 65534:65534
EXPOSE 3000
ENV KNOT_LOG_FORMAT=json
ENV KNOT_WEB_DIST=/web/dist
ENTRYPOINT ["/knot-server"]
