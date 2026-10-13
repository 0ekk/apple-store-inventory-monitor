# Publish the headless images to GHCR from main

## Rationale

The Kubernetes manifests (`deploy/k8s/10-apw.yaml`) pin both images by an
immutable tag, but nothing produced those images except a manual
`docker build` + `docker push`. A CI build per commit on `main` gives every
commit a pullable, pinnable image without anyone building locally.

## Decisions

- **One workflow, `.github/workflows/images.yml`, triggered by `push` to
  `main` only.** No pull requests (fork PRs get no `packages: write` token,
  and a PR image has nothing to deploy to), no tags (`release.yml` owns tag
  builds of the desktop app), and no path filter: the request is one image per
  commit, so a docs-only commit still gets a `sha-` tag that a manifest can
  name.
- **Tag: `sha-<7-char sha>` only**, via `docker/metadata-action`
  `type=sha`. No `latest` or `main`: the manifests reject moving tags, and a
  moving tag can be added to the `tags:` input later if compose users want
  one. metadata-action also lowercases the image name (GHCR requires it) and
  adds OCI labels; `org.opencontainers.image.source` links the package to the
  repository.
- **Image names `ghcr.io/<repository_owner>/apw-monitor` and
  `.../apw-camoufox-sidecar`**, matching the `REGISTRY/apw-*` placeholders
  already in the manifests, so `REGISTRY` becomes `ghcr.io/<owner>`.
- **Matrix of two jobs with `fail-fast: false`**, so a broken sidecar build
  doesn't cancel the monitor build and vice versa.
- **Auth with the workflow's `GITHUB_TOKEN`** (`packages: write`,
  `contents: read`); no personal access token or extra secret.
- **Buildx with the GitHub Actions cache** (`type=gha`, one scope per image,
  `mode=max`). The monitor's web and Rust stages and the sidecar's
  browser download (`camoufox fetch`) are reused when their inputs haven't
  changed.
- **Actions pinned by major version** (`@v4`, `@v6`, `@v7`), the same style
  as `ci.yml` and `release.yml`. Versions checked against each action's
  latest release on 2026-10-12.
- **`linux/amd64` only** at first. Superseded the same day; see the
  addendum below.

## Impact

- Supply chain: images are now built by GitHub-hosted runners from the
  committed tree, and the workflow's token can write packages under the
  repository owner. The build inputs are unchanged (same Dockerfiles, same
  base images). The sidecar still downloads the Camoufox browser at build
  time. While the cache layer is reused, every build ships the same browser
  build; a cache miss fetches whatever `camoufox fetch` resolves at that
  moment.
- Each push to `main` costs two builds. On a cold cache that is a few
  minutes of runner time for the Rust release build; warm builds are shorter.
- `deploy/k8s/10-apw.yaml` image comments and `docs/headless-deploy.md`
  (Layout, Kubernetes step 1, new "Prebuilt images" section) now point at
  the GHCR images. The placeholders stay, because the owner differs per fork.
- Manual setup outside the repository: enable Actions on a fork, and make
  the packages public or give the cluster a pull secret, because a new GHCR
  package can start out private.

## Verification

- `actionlint` 1.7.7 (`rhysd/actionlint` image) reports no findings for
  `.github/workflows/images.yml`.
- First run on the fork (0ekk/apple-store-inventory-monitor, run
  38223520776, commit 6ab7955): both jobs green on a cold cache,
  apw-camoufox-sidecar in 3m23s and apw-monitor in 3m34s.
- An anonymous registry request (a token from `ghcr.io/token` with no
  credentials, then `GET /v2/0ekk/<image>/manifests/sha-6ab7955`) returned
  200 for both images. On this public fork the packages came out public
  with no manual visibility step; the docs keep the private-package
  instructions for other setups.
- Both Dockerfiles already build locally with the same context and file
  arguments (the kind deployment runs those images: sidecar `dev4`, monitor
  `dev3`).

## Addendum: linux/arm64 (2026-10-12)

Superseded the same day by "one buildx build per image" below; kept
because commit a35a2af shipped it.

### Rationale

The images have to run on arm64 nodes too, not only amd64.

### Decisions

- **Native runners, not QEMU.** Each image is built once per architecture
  on a runner of that architecture: `ubuntu-24.04` for amd64 and
  `ubuntu-24.04-arm` for arm64. GitHub's arm64 runners are free in public
  repositories. Under QEMU emulation the Rust release build of `apw-monitor`
  takes several times longer. The runners are pinned to 24.04 instead of
  `ubuntu-latest`, so both architectures build on the same OS release
  (`ubuntu-latest` moves to Ubuntu 26 from 2026-10-19). The Dockerfiles do
  all the work inside their own base images, so the runner OS only hosts
  Docker.
- **Per-arch tags, then a merge job.** Every build pushes
  `sha-<short sha>-<arch>`. A `merge` job runs `docker buildx imagetools
  create` to publish `sha-<short sha>` as one multi-arch index over the two
  builds. Docker's documented example passes digests between jobs through
  artifacts (`push-by-digest`). Per-arch tags need no artifact
  upload/download steps, at the cost of two extra tags per commit in the
  package's tag list.
- **The merge job needs all four builds** (`needs: build`), so a
  `sha-<short sha>` tag never exists with an architecture missing. When one
  build fails, that commit gets no multi-arch tag, and the per-arch tags
  that did build stay in the registry.
- **One short-SHA length.** `DOCKER_METADATA_SHORT_SHA_LENGTH: 7` is set at
  workflow level. metadata-action reads it for the per-arch tags, and the
  merge step's `${GITHUB_SHA::$DOCKER_METADATA_SHORT_SHA_LENGTH}` reads the
  same variable, so the two names can't drift apart. The merge step
  lowercases the owner (`${OWNER,,}`) because GHCR requires it;
  metadata-action already does that for the build jobs.
- **Cache scope per image and architecture**
  (`scope=<image>-<arch>`), so the two architectures don't overwrite each
  other's layer cache.

### Impact

- Each push to `main` now costs four build jobs plus one short merge job.
- Camoufox publishes Linux arm64 browser builds (`lin.arm64.zip` assets on
  every recent release); `camoufox fetch` is expected to pick it on an
  arm64 build, which the first CI run confirms or refutes.
- The k8s manifests don't change: the same `sha-<short sha>` tag resolves
  to the node's architecture.

### Verification

- `actionlint` 1.7.7 reports no findings for the multi-arch workflow.
- A local emulated arm64 build of the sidecar was attempted and failed
  before running any Dockerfile step (`exec /bin/sh: no such file or
  directory`). The cause is this workstation's QEMU binfmt setup, not the
  Dockerfile, so it says nothing about the image.
- CI run 38234171720 (commit a35a2af): all four builds and the merge job
  green (sidecar about 4m15s per arch, monitor 2m42s arm64 / 2m59s amd64,
  merge 10s). The anonymous manifest for `sha-a35a2af` lists
  `linux/amd64` and `linux/arm64` (plus the two provenance attestation
  entries, `unknown/unknown`) for both images. The arm64 sidecar job's log
  shows `camoufox fetch` downloading the `lin.arm64` browser build.

## Addendum: one buildx build per image (2026-10-12)

### Rationale

The native-runner layout needed five jobs, a merge step, and two extra
single-arch tags per commit, and nobody could reproduce a multi-arch build
locally with one command. Asked to use buildx's own multi-platform build
instead.

### Decisions

- **One job per image, `platforms: linux/amd64,linux/arm64`.** buildx
  builds both architectures and pushes one multi-arch `sha-<short sha>`
  index. There are no per-arch tags, no merge job, and no
  `DOCKER_METADATA_SHORT_SHA_LENGTH` coupling. Runner: `ubuntu-24.04`, plus
  `docker/setup-qemu-action@v4` limited to `arm64`.
- **The monitor cross-compiles instead of emulating rustc.** In
  `crates/apw-headless/Dockerfile` the `web` and `build` stages are
  `FROM --platform=$BUILDPLATFORM`, so they always run on the runner's own
  architecture. The bundle is plain JS. The Rust stage uses
  `tonistiigi/xx:1.9.0`:
  - `xx-cargo` sets the target triple, the clang/lld cross linker, the C
    compiler for `ring`, and the target-aware pkg-config for `alsa-sys`.
  - `xx-apt-get` installs `xx-c-essentials` and `libasound2-dev` for the
    target architecture.
  - `xx-verify` fails the build if the binary is the wrong architecture.

  Only the runtime stage (`useradd`, `COPY`) runs as the target
  architecture.
- **pkg-config is pointed at the target by hand.** `xx-cargo` only sets
  `PKG_CONFIG` when a `<triple>-pkg-config` wrapper exists, and Debian
  trixie ships none (no `pkg-config-aarch64-linux-gnu` package). The first
  local arm64 build failed in `alsa-sys`'s build script ("pkg-config has
  not been configured to support cross-compilation"). The build step now
  exports `PKG_CONFIG_ALLOW_CROSS=1` and
  `PKG_CONFIG_LIBDIR=/usr/lib/$(xx-info)/pkgconfig:/usr/share/pkgconfig`,
  the multiarch directory where `xx-apt-get` installs the target's
  `alsa.pc`. For a native build the same path is the build machine's own.
- **The sidecar Dockerfile is unchanged and its arm64 build runs under
  QEMU.** apt packages, the Playwright wheel and the Camoufox browser all
  have to be the arm64 ones, so there's no build-machine stage to move them
  into. This is the slow part of the arm64 build: emulated `dpkg` plus a
  browser download and unzip in emulated Python.
- **Cache scope back to one per image.** A multi-platform build stores both
  architectures under one scope.
- **`timeout-minutes: 45`** (was 30): headroom for the emulated sidecar
  build on a cold cache.

### Impact

- Each push to `main` costs two jobs again.
- The sidecar's arm64 half is slower than on a native runner (emulation).
  The monitor's isn't, because rustc never runs under QEMU.
- `docker buildx build --platform linux/amd64,linux/arm64` reproduces the
  CI build locally, given QEMU binfmt for arm64.
- New build-time dependency: the `tonistiigi/xx:1.9.0` image (Docker's own
  cross-compilation helpers), pinned by tag like the other base images.
- Tags pushed by a35a2af's run (`sha-a35a2af-amd64` / `-arm64`) stay in
  the registry. Nothing produces new per-arch tags.

### Verification

- `actionlint` 1.7.7 reports no findings for the single-build workflow.
- Local cross-compile on an amd64 workstation, without QEMU:
  `docker buildx build --platform linux/arm64 --target build` produced an
  `ELF 64-bit ... ARM aarch64` binary, and `xx-verify` passed. `readelf -d`
  shows the same NEEDED set as the native build (libgcc_s, libm, libc; no
  libasound).
- Local full amd64 build (`--platform linux/amd64`, the xx path with target
  equal to the build machine): the image starts and logs
  `web UI listening`; `/` returns 401 without credentials and 200 with them.
- Not run locally: the arm64 runtime stage and the arm64 sidecar build. Both
  need QEMU, and this workstation's binfmt setup is broken (see the previous
  addendum). The first CI run on `main` covers them.
- CI run 38235305997 (commit aaf8fc2), cold cache: both jobs green,
  apw-monitor in 4m10s and apw-camoufox-sidecar in 9m26s (the emulated arm64
  half; the native-runner layout took about 4m15s). The anonymous manifest
  for `sha-aaf8fc2` lists `linux/amd64` and `linux/arm64` (plus two
  attestation entries) for both images. The emulated sidecar build's log
  shows `camoufox fetch` downloading the `lin.arm64` browser build. The
  upstream CI workflow on the same commit is green too.
