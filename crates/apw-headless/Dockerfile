# Build context must be the repository root (needs the whole Cargo workspace
# and the desktop UI sources):
#   docker build -f crates/apw-headless/Dockerfile -t apw-headless .
# Multi-platform (what CI publishes):
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     -f crates/apw-headless/Dockerfile .

# Web UI: upstream's desktop frontend, built with the Tauri modules swapped
# for an HTTP shim (crates/apw-headless/web/).
# The bundle is plain JS, so this stage always runs on the build machine,
# never under emulation.
FROM --platform=$BUILDPLATFORM node:22-slim AS web
# The pnpm version the lockfile was installed with; pnpm-workspace.yaml uses
# pnpm 10 settings (allowBuilds).
RUN npm install -g pnpm@10.34.6
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY index.html vite.config.ts tsconfig.json tsconfig.node.json ./
COPY public ./public
COPY src ./src
COPY crates/apw-headless/web ./crates/apw-headless/web
RUN pnpm exec vite build --config crates/apw-headless/web/vite.config.ts

FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.9.0 AS xx

# Runs on the build machine and cross-compiles for TARGETPLATFORM (xx-cargo
# sets the target triple, linker, C compiler and pkg-config), so a
# multi-platform build never runs rustc under QEMU.
FROM --platform=$BUILDPLATFORM rust:1.90-slim AS build
COPY --from=xx / /
# clang/lld: the cross linker and C compiler xx uses. file: for xx-verify.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang lld pkg-config file
ARG TARGETPLATFORM
# Target-architecture libraries. apw-core depends on rodio (the desktop
# app's alert sound), whose alsa-sys build script needs the ALSA headers.
# Build-time only: the headless binary never plays sound, so libasound isn't
# linked at runtime (readelf -d shows only libc/libm/libgcc_s) and the
# runtime stage stays slim.
RUN xx-apt-get update && xx-apt-get install -y --no-install-recommends \
    xx-c-essentials libasound2-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src-tauri ./src-tauri
# xx-cargo only wires up pkg-config when a <triple>-pkg-config wrapper
# exists, and Debian trixie ships none, so point the native pkg-config at
# the target's multiarch .pc files (where xx-apt-get put alsa.pc).
RUN export PKG_CONFIG_ALLOW_CROSS=1 \
        PKG_CONFIG_LIBDIR="/usr/lib/$(xx-info)/pkgconfig:/usr/share/pkgconfig" \
    && xx-cargo build --release -p apw-headless \
    && cp "target/$(xx-cargo --print-target-triple)/release/apw-headless" /apw-headless \
    && xx-verify /apw-headless

# The only stage that runs as the target architecture; under QEMU for a
# foreign platform, which for one useradd takes seconds.
FROM debian:trixie-slim
RUN useradd -m -u 10001 apw
COPY --from=build /apw-headless /usr/local/bin/apw-headless
COPY --from=web /src/crates/apw-headless/web/dist /usr/share/apw-headless/web
USER apw
ENV CAMOUFOX_SIDECAR_URL=http://sidecar:8787
ENTRYPOINT ["/usr/local/bin/apw-headless"]
