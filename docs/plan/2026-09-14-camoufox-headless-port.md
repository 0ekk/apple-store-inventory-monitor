# Headless / Camoufox port

## Rationale

The user asked whether this project could be rewritten to run as a
Cloudflare Worker instead of a Tauri desktop app.

Investigation of the existing fetch path
(`src-tauri/src/chromium_fetcher.rs`) showed the actual blocker: Apple's
product pages run a bot-check script (`shop/shld/v2_1/verify.js`) before the
`/shop/fulfillment-messages` endpoint answers. A request without a session
that cleared that check gets HTTP 541. Clearing it requires executing
Apple's JS inside a real browser with a DOM — which is why the desktop app
already launches a real headless Chrome/Edge over CDP for every query,
instead of using a plain HTTP client (the plain-`reqwest` `AppleClient` in
`crates/apw-core/src/apple.rs` exists but is not what production code uses).

A Cloudflare Worker is a bare V8 isolate with no DOM, so it cannot run that
check under any rewrite — this is a hard architectural blocker, not a
configuration issue. The only Cloudflare product that could clear the check
is Browser Rendering (a metered, managed-Chrome product with per-session
cost and ≥1-minute cron granularity), which is not a "Worker rewrite" in any
meaningful sense.

The user then asked about swapping the browser layer for Camoufox (a
patched-Firefox, anti-fingerprinting browser) for extra stealth. Camoufox is
a real browser binary driven via Playwright — it needs a real OS process and
cannot run inside a Cloudflare Worker isolate either. Given that, the user
chose: keep a real VPS/container as the deployment target, and use Camoufox
(instead of plain Chrome/Edge) as the browser that clears Apple's check.

## Decisions made

1. **New crate, not a rewrite of `apw-core`.** `crates/apw-core` (watcher
   engine, config storage, catalog, Bark notifier, availability model) is
   already decoupled from any UI framework and is reused completely
   unmodified. Only a new `Fetcher` implementation and a new binary shell
   were added (`crates/apw-headless`), mirroring how `src-tauri` is a thin
   shell over the same core. The existing Tauri desktop app is untouched.

2. **Camoufox runs out-of-process, as a Python sidecar
   (`camoufox-sidecar/`), not embedded in the Rust binary.** Camoufox is a
   Python/Playwright package with no Rust binding. Rather than reimplement
   its anti-fingerprinting patches or add a Rust-Python FFI boundary, the
   sidecar is a small always-on process the Rust binary talks to over a
   local HTTP call, mirroring the boundary `chromium_fetcher.rs` already
   draws between "drive a browser" and "everything else."

3. **The sidecar is generic; all Apple-specific knowledge stays in Rust.**
   The sidecar's `/pickup` request carries a fully-formed warm-up URL and
   the fully-formed fetch URL (originally also a list of required cookie
   names — removed, see the 2026-10-12 addendum below) — it has no
   built-in knowledge of Apple's URL shapes, regions, or query parameters.
   This keeps a single source of truth (`apw-core::model::Region`) for
   anything that changes if Apple's API changes, instead of duplicating
   that knowledge in Python.

4. **Hand-rolled minimal HTTP in the sidecar, not aiohttp/FastAPI.** There is
   exactly one real endpoint (`POST /pickup`) plus a health check. A ~30-line
   `asyncio.start_server`-based parser covers both; pulling in a full async
   web framework for two routes would be a dependency with nothing to show
   for it.

5. **Desktop-only alert channels are dropped, not stubbed.** `soundEnabled`
   is read (for config-file compatibility with the desktop app) but has no
   effect — there is no local speaker on a server. `openOnHit`'s "auto-open
   a browser tab" behavior has no server equivalent; instead the chosen
   destination URL is embedded as the link in the Bark push, which already
   supports a tappable URL.

6. **Same `settings.v2.json` config format as the desktop app**, read from
   the OS-conventional config directory by default (or a path from
   `APW_SETTINGS_PATH`). This means a config built via the desktop app's UI
   can be copied to the server as-is; no separate config format was
   invented.

## Addendum: Kubernetes manifests (same day)

Added `deploy/k8s/` as an alternative to `deploy/docker-compose.yml` for
users deploying to an existing cluster rather than a standalone VPS.

Same architecture, three k8s-specific decisions:

1. **`replicas: 1` with `strategy: Recreate` on both Deployments, not just
   left at the default `RollingUpdate`.** A rolling update briefly runs the
   old and new pod together; for the sidecar that means two concurrent
   Camoufox browser sessions hitting Apple at once (the exact burst pattern
   its bot-check exists to catch), and for the monitor it means the same
   targets polled twice as often. Neither workload has a way to coordinate
   across replicas, so `Recreate` (old pod fully gone before the new one
   starts) is required, not just preferred.

2. **Default-deny `NetworkPolicy` with explicit allows** (DNS, sidecar
   reachable only from the monitor pod, HTTPS egress for both). Applied via
   the `kubernetes-skill` reference workflow's network-exposure guidance.
   Caveat carried into the docs: this only takes effect if the cluster's CNI
   enforces `NetworkPolicy` at all.

3. **`readOnlyRootFilesystem: false` on the sidecar container only**, as an
   explicit, commented deviation from the Pod Security Standards "restricted"
   baseline the namespace otherwise enforces (PSS "restricted" itself does
   not check this field, so this is a defense-in-depth choice, not an
   admission requirement). Firefox/Camoufox writes cache and per-launch
   profile data to multiple paths under `$HOME`; enumerating and mounting
   exactly those paths as `emptyDir`s wasn't something that could be verified
   without running the real browser, which this environment cannot do. The
   monitor container keeps `readOnlyRootFilesystem: true` — it has no runtime
   writes to make.

Both Dockerfiles were also updated to run as a fixed non-root UID (10001),
matching the manifests' `runAsUser`/`runAsNonRoot`; the sidecar's browser
binary is now fetched during the image build as that same user, not root,
so its cache directory ends up somewhere the runtime user can actually use.

Separately, fixed a real correctness gap the Kubernetes manifests exposed:
`crates/apw-headless/src/main.rs` only listened for SIGINT
(`tokio::signal::ctrl_c()`), but Kubernetes (and `docker stop`) send SIGTERM
to PID 1. Without a SIGTERM handler, the process would never call
`watcher.stop()` on termination and would simply be SIGKILLed once
`terminationGracePeriodSeconds` expired. Added a `wait_for_shutdown_signal()`
that selects over both signals on Unix.

## Addendum: readiness check corrected after upstream sync check (2026-10-12)

**Rationale.** While adding an `upstream` remote
(`suversal/apple-store-inventory-monitor`) to assess sync difficulty,
upstream was found 42 commits ahead (v1.0.5 → v1.0.19), with
`src-tauri/src/chromium_fetcher.rs` rewritten (806 → 4865 lines). Upstream
removed its cookie-based session-readiness check entirely; its test
`海外会话就绪不依赖中国区cookie名称` ("overseas session readiness does not
depend on China-region cookie names") documents why: `shld_bt_ck` and
`as_atb` only reliably appear on the mainland China site. This port had
copied that exact check from the v1.0.5 desktop app and applied it to all
seven regions, so non-China targets could stall in warm-up until the 50s
timeout.

**Decisions.**
- Dropped `REQUIRED_COOKIES` and the `required_cookies` field from the
  sidecar request (`WarmSpec` now carries only `url`).
- Sidecar readiness now matches upstream's signal: `document.readyState ==
  "complete"`. Implemented via Playwright's native
  `page.goto(..., wait_until="load")` (the `load` event fires exactly when
  `readyState` becomes `"complete"`) rather than a hand-rolled polling
  loop. Note this is *stricter* than the old `readyState != "loading"`
  condition; dropping the cookie gate while keeping the looser condition
  would have made warm-up less strict than both the old code and upstream.

**Impact.** Sidecar wire protocol changed (`warm.required_cookies` removed).
Both sides ship together from this repo, so no compatibility shim. Git-level
sync with upstream remains clean: upstream's 42 commits touch only the
`version` line of `Cargo.toml` and unrelated `Cargo.lock` package blocks,
and do not touch `NOTICE` or `.gitignore`.

**Verification.** `cargo clippy -p apw-headless --all-targets` clean;
`python3 -m py_compile camoufox-sidecar/server.py` passes; no remaining
references to the old cookie names outside the explanatory comment. Still
not verified against Apple's live site (same limitation as above).

**Not yet adopted from upstream** (deliberately out of scope for this fix):
upstream's v1.0.6–v1.0.19 also added proxy route rotation
(`src-tauri/src/clash.rs`, `route_pool.rs`), protection cooldowns
(`ApiError::CoolingDown`), per-cycle response caching, and new default
methods on `Fetcher` (`begin_cycle`, `plan_cycle`, `cached_delivery`).

**Merging upstream's `apw-core` will break `apw-headless`'s build** — checked
by exporting `upstream/main` (`git archive`) into a scratch directory, adding
`crates/apw-headless` to its workspace, and running `cargo check -p
apw-headless`. Two errors:

1. `Fetcher::pickup_message` changed signature: the parts argument is now
   `&[Target]` instead of `&[String]`, and there is a new fifth parameter
   `Option<&DeliveryRegion>`. `CamoufoxFetcher`'s impl must be updated.
2. `OpenOnHit` gained an `EducationBag` variant, so `target_open_url`'s
   exhaustive `match` in `main.rs` must handle it.

These cannot be fixed ahead of the merge: this fork still builds against
v1.0.5's `apw-core`, where the old signatures are the correct ones. Fix them
in the same change that merges upstream. (Done — see the next addendum.)

## Addendum: merged upstream v1.0.19 and ported the fetcher (2026-10-12)

**Rationale.** Keep the headless port on upstream's current `apw-core`
instead of a month-old fork point, and pick up upstream's fixes to how the
pickup query itself is made.

**How the merge was done.** `main` (`42baf89`, v1.0.5) is an ancestor of
`upstream/main` (`fe708dd`, v1.0.19), so this was a fast-forward: the
uncommitted headless work was stashed (`git stash push -u`, plus a tar backup
in the session scratchpad), `main` fast-forwarded, and the stash re-applied
with no conflicts. No merge commit was needed.

**Decisions** (each mirrors upstream's desktop `chromium_fetcher.rs`):
- `CamoufoxFetcher::pickup_message` takes `&[Target]` plus
  `Option<&DeliveryRegion>`. Queried parts are each target's `part_number`
  plus its `companion_part` (Watch band), sorted and deduplicated.
- Query endpoint and shape follow upstream's single-store scope: the URL
  comes from `Region::pickup_message_url()` (now
  `/shop/retail/pickup-message`, previously `/shop/fulfillment-messages`),
  with params `pl`, `mts.0=regular`, `store`, `parts.N` in that order and no
  `fae`. Because the endpoint was always taken from `apw-core`, the endpoint
  move needed no code change here; only the parameter list did.
- Warm-up page is `Region::session_page_url()` (region home page, except a
  buy page for `zh_CN`) instead of the first buy page. Upstream's reason:
  Australia closes `/shop/buy-*` connections for automated browsers.
- HTTP 5xx maps to `ApiError::Transport`, not `RateLimited`, matching
  upstream. Status 0 (in-page `fetch()` exception reported by the sidecar)
  maps to `Transport` carrying the exception text.
- `OpenOnHit::EducationBag` links to `Region::education_bag_url()`.
- Delivery quotes (`delivery_region`) are ignored, with a `ponytail:`
  comment: they need a separate endpoint and extra requests per cycle, and
  stock detection does not depend on them.
- Removed the unused `thiserror` dependency from `apw-headless`.

**Impact.** Fork now sits on upstream v1.0.19 plus the uncommitted headless
work. Not ported (documented in `docs/headless-deploy.md`): Clash proxy route
rotation, delivery quotes, nearby-store query batching, and the
`begin_cycle`/`plan_cycle`/`cycle_stats`/`schedule_hint`/`retry_now` hooks
(all have default implementations, so they are simply inactive).

**Verification.** `cargo clippy -p apw-headless --all-targets` clean;
`cargo test -p apw-headless` passes a new test
(`single_store_query_matches_upstream_shape`: companion parts included and
deduplicated; URL equals `pickup_message_url()` with params exactly `pl,
mts.0, store, parts.0..`); `cargo build --release -p apw-headless` succeeds;
upstream's own suite `cargo test -p apw-core` passes (215 passed,
2 ignored); the `Cargo.lock` diff against upstream is purely additive (the
`apw-headless` package block plus `signal-hook-registry` for tokio's
signal feature). Still not run against Apple's live site.

## Addendum: single pod with a native sidecar (2026-10-12)

Supersedes the two-Deployment layout in the Kubernetes addendum above.

**Rationale.** The sidecar and the monitor are a 1:1 pair of singletons that
can never scale independently, so separate Deployments bought nothing and
cost three guarantees: start order (the monitor's first poll could hit a
sidecar that isn't up), stop order (on termination both got SIGTERM at once,
so the monitor's final in-flight cycle could lose its browser), and
isolation that doesn't depend on the CNI (the sidecar was reachable over the
pod network, guarded only by a NetworkPolicy that some CNIs ignore).

**Decisions.**
- One Deployment, `deploy/k8s/10-apw.yaml`, replacing `10-sidecar.yaml`
  and `20-monitor.yaml`. The sidecar is an init container with
  `restartPolicy: Always` (Kubernetes native sidecar), so the monitor starts
  only after the sidecar's `startupProbe` passes and the sidecar is stopped
  only after the monitor exits. Requires Kubernetes 1.29+.
- The sidecar binds `127.0.0.1` (`SIDECAR_HOST` env, overriding the image's
  `0.0.0.0`, which the compose setup still needs). The monitor calls
  `http://127.0.0.1:8787`. The `Service` is removed.
- Sidecar probes switched from `httpGet` to `exec` + `curl` against
  loopback: kubelet `httpGet` probes connect to the pod IP and cannot reach a
  loopback-only server. `curl` was already in the image. The readiness probe
  and the `preStop` sleep were dropped, since with no Service there are no
  endpoints to gate or de-register.
- `terminationGracePeriodSeconds` raised from 75 to 90: the budget now
  covers the monitor's up-to-65s final cycle *plus* the sidecar closing the
  browser afterwards, since both share the pod's grace period.
- NetworkPolicy reduced to default-deny plus one egress policy (DNS, HTTPS
  to public IPs) on the single pod; monitor → sidecar traffic is loopback
  and never subject to NetworkPolicy.
- docker compose keeps two services: there the containers are separate
  network namespaces and the sidecar port is already unpublished.

**Impact.** Fewer resources (1 Deployment, 2 NetworkPolicies, no Service).
Updating either image restarts the whole pod and re-warms the browser
session. Clusters below 1.29 are unsupported; on 1.28 the
`restartPolicy` field is silently dropped and the pod hangs in `Init`.

**Verification.** No live cluster here, so a scripted offline cross-check
of the manifests: YAML parses; exactly one Deployment and no Service;
selector matches pod labels; `replicas: 1` + `Recreate`; the sidecar has
`restartPolicy: Always` and a `startupProbe`; the monitor's sidecar URL
host/port equals the sidecar's bind host/port (127.0.0.1:8787); no sidecar
probe uses `httpGet`, and each targets 127.0.0.1:8787; every volume mount is
declared; both containers have `allowPrivilegeEscalation: false` and drop
ALL capabilities; the pod has `runAsNonRoot` and RuntimeDefault seccomp;
`APW_SETTINGS_PATH` sits under the Secret mount with the Secret's key as
the file name; both NetworkPolicies select the pod. Also confirmed that
`server.py` reads `SIDECAR_HOST` and that the sidecar image installs `curl`.
Run `kubectl apply --dry-run=server -f deploy/k8s/` against a real cluster
before applying.

## Addendum: first live run on a local kind cluster (2026-10-12)

**Rationale.** Everything above was verified offline only. A kind cluster
(kind v0.33.0, Kubernetes v1.37.0, kindnet CNI) running both real images
against Apple's real endpoint surfaced three bugs that no offline check
could.

**Decisions (fixes found by the run).**
- `crates/apw-headless/Dockerfile`: the build stage installs `pkg-config`
  and `libasound2-dev`. `apw-core` pulls in `alsa-sys` (the desktop sound
  alert), whose build script needs them. Build stage only; `ldd` on the
  final binary shows only libc/libm/libgcc, so the runtime image is unchanged.
- (Superseded the same day by `2026-10-12-headless-web-ui.md`: with a web
  UI, headless writes its settings again, on a PVC.)
  Settings are read-only input. Upstream's `SettingsStore::load()` chmods the
  config dir/file to 0700/0600 before reading; on the read-only Secret mount
  that fails with EROFS and the load is treated as failed, so the monitor
  started with 0 targets and looked healthy. Headless now uses its own
  `read_settings()` (open, 1 MiB cap, parse, `normalize()`), never writes
  the file, and exits 1 on any read/parse error instead of running empty.
  Permissions are the provider's job: the Secret volume now has
  `defaultMode: 0400`.
- `camoufox-sidecar/server.py` launches Camoufox with
  `exclude_addons=[DefaultAddons.UBO]`. By default Camoufox downloads uBlock
  Origin from addons.mozilla.org at every launch; that request returned
  HTTP 451 from the test network, and an ad blocker on Apple's own pages is
  a risk anyway. Upstream's desktop Chromium runs with
  `--disable-extensions`.

**Impact.** A missing or broken settings file is now a crash
(CrashLoopBackOff), not a silently idle pod. Bark pushes repeat every
cycle while a target stays in stock: that is upstream's deliberate watcher
behavior (`watcher.rs`, emits `InStock` on every in-stock cycle), not a
headless change.

**Verification (kind cluster `apw-test`, images loaded with
`kind load docker-image`).**
- `kubectl apply --dry-run=server -f` passes with no Pod Security warnings
  under the namespace's `restricted` enforce label.
- Native sidecar ordering: the sidecar started at 05:49:48, the monitor at
  05:49:54, after the `startupProbe` passed. The pod reached 2/2 Running
  with 0 restarts. Firefox ran under the restricted profile without
  `MOZ_DISABLE_CONTENT_SANDBOX`.
- Live polling, 2 targets (zh_CN R390 MG6W4CH/A, en_SG R669 MG6J4X/A):
  cycle 1 took 5661 ms (browser warm-up), cycles 2–4 took 352–605 ms, all
  `healthy=true`, and every target got a definite result (`InStock`) from
  `parse_pickup_message`, not `Unknown`.
- Secret mount: `/data/settings.v2.json` is `-r--r----- root:apw` (kubelet
  adds group read for `fsGroup`), readable by uid 10001; writes to `/data`
  and `/etc` fail with EROFS.
- Loopback bind: from inside the pod, `127.0.0.1:8787/healthz` answers and
  `<podIP>:8787` is refused.
- NetworkPolicy is enforced by kindnet: HTTPS to www.apple.com returns 200;
  plain HTTP (port 80) and the in-cluster API server `10.96.0.1:443`
  (RFC1918, excluded from the egress rule) both time out.
- Not verified: Bark delivery (the test settings had no Bark URL) and
  `readOnlyRootFilesystem: true` on the sidecar.

## Addendum: optional exit proxy for the sidecar (2026-10-12)

**Rationale.** During web UI testing, every pickup query returned HTTP 541.
A diagnosis ruled out the code and the browser:
- The block survived two fresh Camoufox fingerprints and a request sent to
  the sidecar directly.
- The user's own desktop Chrome on the same exit IP (47.178.15.247, no
  proxy) also got 541. That held even after loading `/sg/shop/buy-iphone`,
  which runs Apple's verify script, and then fetching the endpoint
  in-page.

Apple had flagged the exit IP. The desktop app answers this with Clash
node rotation; headless had no way to change its exit at all.

**Decisions.**
- `SIDECAR_PROXY=scheme://[user:pass@]host:port` (`http`, `https` or
  `socks5`) is parsed by `proxy_from_env()` in `server.py` and passed to
  `AsyncCamoufox(proxy=...)` as Playwright proxy settings. Credentials are
  split out and URL-decoded.
- An invalid value fails startup with a message that never echoes the
  value, because it may contain the password.
- It is static for the process lifetime. Automatic rotation on 541 would need
  a browser relaunch per switch plus a node list, so it was left out and
  marked with `ponytail:`.
- Only the browser uses it. Bark and catalog refresh stay direct.
- k8s: the sidecar reads `SIDECAR_PROXY` from Secret `apw-proxy`, key `url`,
  with `optional: true`. No Secret means a direct connection, so the default
  manifest needs no edit. Credentials stay out of the Deployment spec.
- The egress NetworkPolicy gets a commented example rule. Proxies are
  typically on a private IP or a port other than 443, both denied by the
  existing rule, and the address is deployment-specific.
- compose: `SIDECAR_PROXY: ${SIDECAR_PROXY:-}`.
- `geoip=True` (matching timezone/locale to the proxy) is not enabled. It
  needs the `camoufox[geoip]` extra, skipped earlier for image size.
  Camoufox logs a `LeakWarning` about it; this is documented.

**Impact.** No behavior change without the Secret or env var.

**Verification.**
- `camoufox-sidecar/test_server.py` passes inside the sidecar image. It
  covers: empty → `None`; plain, IPv6 and credentialed URLs; rejection of a
  missing scheme, an unsupported scheme and a missing port; and that the
  error message never contains the password.
- kind: ran a throwaway CONNECT proxy on the kind gateway (172.18.0.1:3128),
  created `apw-proxy`, and filled in the example NetworkPolicy rule.
  - The sidecar logged `apple traffic via http://172.18.0.1:3128`.
  - The proxy logged tunnels to `www.apple.com`, `www.apple.com.cn`, the
    Apple image CDN and Apple metrics.
  - Apple still answered 541, as expected: that proxy exited through the
    same flagged IP.
  - One zh_CN warm-up timed out at 50s because the single-threaded test
    proxy relayed 36 CDN image tunnels slowly.
  - Removing the Secret and the rule restored `via direct connection`.
- Not verified: a proxy with a different, unflagged exit IP actually
  clearing the 541.

## Addendum: sidecar memory with several regions (2026-10-12)

**Rationale.** With 13 targets across three regions (zh_CN, zh_HK, en_SG),
the sidecar was OOM-killed at its 1536Mi limit (exit 137). The sidecar keeps
one Apple page per region. While Apple answers 541, every cycle closes each
region's page and the next cycle loads a fresh one, so three heavy page
loads (a buy page pulls dozens of images) could run at once. The desktop
app never does this: it keeps a single page and navigates it between
regions.

**Decisions.**
- A sidecar-wide `asyncio.Lock` (`_warm_lock`) serializes page creation
  and loading across regions. Fetches on already-warm pages still run per
  region in parallel. The cost: when several regions need warming, the
  later ones wait their turn (one cycle took about 34s with all three
  re-warming after 541s).
- The sidecar memory limit goes from 1536Mi to 2560Mi; the request stays at
  512Mi.
- Not done: a single shared page like the desktop app (a bigger rework of
  the session model), and image blocking (`block_images`). Image blocking
  would cut memory further, but whether Apple's bot check notices is
  untested.

**Verification (kind).**
- Before: OOMKilled, exit 137, `memory.peak` 1536Mi.
- After: four minutes, three cycles, all three regions re-warming every
  cycle, 0 restarts.
  - cgroup anon memory peaked at 755Mi.
  - `memory.peak` reached 2361Mi, but that is almost all `inactive_file`
    (about 1.25Gi of clean page cache from reading the 2.4GB Camoufox
    install), which the kernel reclaims before OOM-killing. A high peak is
    therefore expected and not a warning sign.

## Impact

- New workspace member `crates/apw-headless` (binary `apw-headless`),
  depending only on `apw-core`. Verified it does not pull in the `tauri`
  dependency tree (`cargo tree -p apw-headless` shows no `tauri*` crates).
- New directory `camoufox-sidecar/` (Python, not part of the Cargo
  workspace).
- New `deploy/docker-compose.yml` wiring the two together, with the sidecar
  port internal-only (not published to the host).
- New `deploy/k8s/` (Namespace, NetworkPolicies, one Deployment whose pod
  runs the sidecar as a native sidecar container) as an alternative to the
  compose file.
- `camoufox-sidecar/Dockerfile` and `crates/apw-headless/Dockerfile` both
  updated to run as a fixed non-root UID (10001) instead of root.
- `crates/apw-headless/src/main.rs` now handles SIGTERM, not just SIGINT.
- No changes to `crates/apw-core`, `src-tauri`, or the existing frontend —
  the desktop app builds and behaves exactly as before.

## Verification

- `cargo check -p apw-headless` and `cargo build --release -p apw-headless`
  both succeed from the full workspace; `cargo clippy -p apw-headless
  --all-targets` reports no issues.
- `cargo tree -p apw-headless | grep -i tauri` returns nothing, confirming
  the headless binary's dependency graph is independent of the Tauri shell.
- `python3 -m py_compile camoufox-sidecar/server.py` passes.
- All `deploy/k8s/*.yaml` parse as valid YAML (`yaml.safe_load_all`), and a
  scripted cross-check confirms: the Service selector matches the sidecar
  Deployment's pod labels, the Service's `targetPort` name matches a named
  container port, every `volumeMounts` entry references a declared `volumes`
  entry, every probe's named port exists on its container, and both
  NetworkPolicy `podSelector`s match an actual Deployment's pod labels. This
  environment has no live cluster (`kubectl config current-context` is
  unset) and no `kubeconform`, so `kubectl apply --dry-run=server` /
  schema validation against a real API server was not possible — run one of
  those before applying to a real cluster.
- End-to-end against Apple's real endpoint with a real Camoufox session:
  see the kind-cluster addendum above. The docker compose path has not been
  run live.
