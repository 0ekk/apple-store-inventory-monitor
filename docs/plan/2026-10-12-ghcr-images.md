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
- **`linux/amd64` only.** Marked with a `ponytail:` comment in the workflow.
  arm64 would need QEMU emulation, which makes the Rust build several times
  slower, and no arm64 node exists yet.

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
- The workflow has not run yet, because nothing is pushed. First real check
  is the first push to `main`: both jobs green, and
  `docker pull ghcr.io/<owner>/apw-monitor:sha-<short sha>` succeeds (after
  the visibility step above).
- Both Dockerfiles already build locally with the same context and file
  arguments (the kind deployment runs those images: sidecar `dev4`, monitor
  `dev3`).
