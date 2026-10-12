# Build context must be the repository root (needs the whole Cargo workspace
# and the desktop UI sources):
#   docker build -f crates/apw-headless/Dockerfile -t apw-headless .

# Web UI: upstream's desktop frontend, built with the Tauri modules swapped
# for an HTTP shim (crates/apw-headless/web/).
FROM node:22-slim AS web
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

FROM rust:1.90-slim AS build
# apw-core depends on rodio (the desktop app's alert sound), whose alsa-sys
# build script needs pkg-config and the ALSA headers. Build-time only: the
# headless binary never plays sound, so libasound isn't linked at runtime
# (readelf -d shows only libc/libm/libgcc_s) and the runtime stage stays slim.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libasound2-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src-tauri ./src-tauri
RUN cargo build --release -p apw-headless

FROM debian:trixie-slim
RUN useradd -m -u 10001 apw
COPY --from=build /src/target/release/apw-headless /usr/local/bin/apw-headless
COPY --from=web /src/crates/apw-headless/web/dist /usr/share/apw-headless/web
USER apw
ENV CAMOUFOX_SIDECAR_URL=http://sidecar:8787
ENTRYPOINT ["/usr/local/bin/apw-headless"]
