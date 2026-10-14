# Headless web management UI (2026-10-12)

Builds on `2026-09-14-camoufox-headless-port.md`.

## Rationale

The headless deployment could only be configured by editing
`settings.v2.json` and restarting, and only observed through `kubectl logs`.
The user asked for a web management frontend.

The desktop app already has a complete management UI (`src/`, React), and
every backend call in it goes through Tauri's `invoke`/`listen` in one file,
`src/lib/store.ts`. The Tauri command layer (`src-tauri/src/lib.rs`) is thin:
each command is a few lines calling `apw-core`. Serving that same UI and
reimplementing the thin command layer over HTTP gives the full desktop
feature set (region/store/model pickers, live poll log, per-model Bark) with
no edits to upstream files. Future upstream UI changes then arrive through
an ordinary merge.

## Decisions made

- **Reuse the upstream UI unmodified (user's choice).** A separate Vite
  config, `crates/apw-headless/web/vite.config.ts`, imports the root
  `vite.config.ts` and adds aliases. `@tauri-apps/api/core`,
  `@tauri-apps/api/event` and `@tauri-apps/plugin-opener` all resolve to
  `crates/apw-headless/web/tauri-shim.ts`:
  - `invoke(name, args)` becomes `POST /api/v1/commands/{name}` with the
    args as the JSON body.
  - `listen(channel)` subscribes to one shared `EventSource` on
    `GET /api/v1/events`; each SSE event name is the Tauri channel name.
  - `openUrl` opens a new tab.
  - `open_target_product` returns the URL and the shim opens it, because the
    desktop version opens it on the app's own machine.
- **API shape.** It is RPC-style, not REST, because the client speaks Tauri
  commands. It is versioned under `/api/v1/`. Command names and camelCase
  argument keys match `store.ts`. Errors come back as a JSON string with
  HTTP 422, which the shim rethrows, so the UI's `String(err)` logging reads
  the same as with Tauri. Unknown commands return 404.
- **Commands with no server counterpart answer with an explicit error.** This
  covers Clash route test/list/select and `install_update`.
  `check_for_update` returns `null` ("up to date") so the UI's silent
  startup check stays quiet. `test_notify` sends Bark only. Delivery
  localities and Watch band lookup were in this list at first; see the
  delivery addendum below.
- **Server:** `axum` 0.8, with only the `http1`, `json` and `tokio` features,
  and `tower-http` `ServeDir` for the static build. axum is the only crate
  new to the lockfile; hyper, tower, tower-http, base64, subtle and
  futures-util were already in it. Engine events fan out through a
  `tokio::sync::broadcast` channel (capacity 256). A lagging tab skips
  events, which is safe because every `cycleComplete` carries the full
  snapshot.
- **Access: loopback + port-forward + password (user's choice).**
  `APW_WEB_ADDR` defaults to `127.0.0.1:8080`. The k8s manifest keeps it on
  loopback with no Service and no NetworkPolicy change, and access is
  through `kubectl port-forward` (verified to reach loopback-bound
  listeners). Compose publishes `127.0.0.1:8080` on the host, for use over
  an SSH tunnel.
- **Auth: HTTP Basic, user `admin`, password from `APW_WEB_PASSWORD`.**
  It applies to every path, static files included. The expected
  `Authorization` header is precomputed and compared in constant time
  (`subtle`). The web UI is off when the variable is unset or empty.
  - Basic auth needs no login page or session code, and the browser re-sends
    it on `fetch` and `EventSource` by itself.
  - The password matters even on loopback, because any page the operator
    visits can send requests to 127.0.0.1.
- **CSRF and clickjacking.** The command handler uses axum's `Json`
  extractor, which rejects anything but `Content-Type: application/json`
  with 415. A cross-site form can't send that type, and a cross-site `fetch`
  with it needs a CORS preflight the server never answers. All responses
  carry `X-Frame-Options: DENY`.
- **Settings are owned by headless again, and live on a PVC.** This
  supersedes the read-only `read_settings()` / Secret-mount decision in the
  kind-test addendum of the previous plan, because a UI that can't save is
  pointless.
  - `SettingsStore::at(path)` with upstream's `load()`/`save()`: atomic
    write, 0700 directory, 0600 file.
  - k8s uses a `ReadWriteOnce` PVC `apw-data` (single writer: `replicas: 1`
    plus `Recreate`), with settings at `/data/config/settings.v2.json`. The
    subdirectory is needed because `load()`/`save()` chmod the parent
    directory, and the volume root belongs to kubelet/root.
  - The `apw-settings` Secret is gone. A new `apw-web` Secret holds the
    password.
  - Catalog refreshes are cached in `catalog-cache/` next to the settings
    file (`Catalog::with_cache_dir`), like the desktop app.
- **Startup rules.**
  - A missing file is a normal first start with the UI on. With the UI off
    it is still fatal, since nothing could add targets.
  - A file that exists but can't be read is always fatal (exit 1). The
    desktop app falls back to defaults and disables saving instead; headless
    exits, so the problem shows up as a crash.
- **Startup now hydrates targets** with the same three catalog passes the
  desktop app runs: Watch metadata, store titles and pickup locations.
- **Pump shutdown.** The event pump now stops on an explicit oneshot signal
  and drains queued events. The web server and the pump both hold the
  `AppState` that owns the watcher, so the old "wait for the channel to
  close" would never return.
- **In-stock alerts read settings at the moment of the hit**, not once at
  startup, because the UI can change the Bark URL and the hit destination
  while the watcher runs.
- **The watcher still auto-starts on boot** (the desktop app waits for a
  click), so a pod restart resumes monitoring.
- **Monitor memory limit 64Mi → 128Mi** (request 32Mi); measured peak 19Mi,
  details under Verification.
- **Docker.** A new `node:22-slim` stage builds the web UI with pnpm
  10.34.6, the version the lockfile installs cleanly with
  (`pnpm-workspace.yaml` uses `allowBuilds`). The output goes to
  `/usr/share/apw-headless/web` (`APW_WEB_DIR`). A root `.dockerignore`
  keeps `target/`, `node_modules/`, `.git` and local build output out of the
  build context.

## Impact

- No changes to `src/`, `src-tauri/`, `crates/apw-core/` or the root
  `vite.config.ts`. An upstream command added to `store.ts` shows up in the
  web UI as a logged "unknown command" until `web.rs` gets a matching arm.
- **Breaking for existing headless deployments.**
  - The settings directory must be writable and owned by uid 10001. A
    read-only Secret or `:ro` mount now fails at startup.
  - k8s users move their settings onto the PVC (documented `kubectl cp`
    path) and create the `apw-web` Secret.
- Some UI controls (alert sound, Clash panel, updater) render but do
  nothing, or log a "not available in headless mode" error. (The Clash
  panel works since `2026-10-14-headless-clash-routes.md`.)
- No rate limiting on the password; loopback-only exposure is the
  mitigation.

## Verification

- `cargo clippy -p apw-headless --all-targets -- -D warnings`: clean.
  `cargo test -p apw-headless`: 2 passed, including
  `basic_auth_accepts_only_the_exact_credentials` (exact match, missing
  header, wrong password, wrong user, extra trailing byte).
- `vite build --config crates/apw-headless/web/vite.config.ts` succeeds, and
  the bundle contains no `@tauri-apps`/`__TAURI` references.
- Upstream `src/` type-checks against the shim under the project's strict
  `tsconfig.json`, using a temporary tsconfig that maps the three Tauri
  module names to the shim (`tsc` exit 0).
- Local binary + curl:
  - 401 without or with wrong credentials.
  - `index.html` and `icon.png` return 200.
  - A form-encoded POST returns 415.
  - An unknown command returns 404, and Clash/updater/notify-without-Bark
    return 422 with their messages.
  - `list_stores` works, and `set_targets` hydrates the store title from
    the catalog.
  - `set_interval 3` is clamped to 5.
  - `/api/v1/events` streams `cycleStarted`/`stateChanged`/`cycleComplete`.
  - The settings file is written 0600 in a 0700 directory.
- kind (`apw-test`, k8s 1.37, local-path StorageClass):
  - The PVC bound and the pod reached 2/2.
  - A first start without a settings file comes up with 0 targets and the
    UI on.
  - `kubectl cp` of an existing settings file into `/data/config/`, followed
    by a restart, loads 2 targets.
  - In Chrome through `kubectl port-forward`, the UI renders and loads
    regions, 50 stores and 73 models.
  - Adding a target from the pickers, deleting it, and changing the interval
    to 45 all persisted to the PVC.
  - The catalog refresh button fetched 49 stores and 73 models and wrote
    `catalog-cache/zh_CN.json`.
  - Test notify logs the Bark-required error, and pause works through
    `stop_watching`.
  - No console errors.
  - Monitor container `memory.peak` was 19Mi including the refresh.
- During this run Apple answered every pickup query with HTTP 541. It did so
  across two fresh Camoufox fingerprints, and also for a request sent to the
  sidecar directly, bypassing the monitor. The same images had returned real
  results 25 minutes earlier, and the Apple homepage still returned 200.
  This points to the exit IP being temporarily flagged after about 10
  browser sessions in 30 minutes of pod restarts, not to this change: the
  only new startup step, `attach_pickup_locations`, sets a field the Camoufox
  fetcher never reads.
- Not verified:
  - Bark delivery.
  - The docker compose path.
- Live SSE updates in the browser were verified later, during the delivery
  port test: the activity log scrolled each cycle's events.

## Addendum: delivery quotes ported (2026-10-12)

**Rationale.** With the UI live, the delivery region panel showed "not
available in headless mode" inline on every load. The user chose a full
port over hiding the error.

**Decisions.**
- **Option lookups:** `crates/apw-headless/src/apple_lookup.rs`.
  - `list_delivery_localities`, `list_watch_band_choices` and
    `list_watch_band_sizes` are ported from the Tauri commands in
    `src-tauri/src/lib.rs` (upstream v1.0.19), behavior unchanged.
  - They use plain HTTPS from the monitor container like the desktop app:
    20s timeout, size caps, `fae=true`, the same query order, and mainland
    China only.
  - The parsers are copied, and so are the desktop tests for them.
- **Delivery quotes in `CamoufoxFetcher`:** a port of the delivery half of
  the desktop `QueryState` in `src-tauri/src/chromium_fetcher.rs`, with the
  same rules:
  - It runs only after a successful pickup query, only for mainland China
    (the watcher only passes a delivery region for `zh_CN` groups).
  - One batched `/shop/fulfillment-messages` request covers regular
    products; Watch kit + band pairs get one request each.
  - A real quote is cached for 1h, an empty answer for 5min, and an empty
    answer never overwrites a real quote.
  - Each target+address is tried at most once per cycle. A 403/541/429
    pauses that region's delivery lookups until the next cycle.
  - It never changes a pickup result.
  - The fetcher only fills the cache. The watcher already merges
    `cached_delivery()` into every row, so the desktop's extra in-fetcher
    merge was redundant and wasn't copied.
- **Locking:** a `std::sync::Mutex<DeliveryState>` held only for cache
  bookkeeping, never across a sidecar call. Attempts are marked before
  sending, so concurrent store queries don't duplicate a request. The
  desktop holds one async lock over the whole browser round trip; here the
  sidecar already serializes and throttles per region.
- **The sidecar keeps the session on a delivery refusal.** Requests carry
  `keep_session_on_block: true`. Upstream notes that the fulfillment
  endpoint can 541 on its own while pickup works, and dropping the page
  would cost the pickup session its warm-up. Pickup requests still drop the
  page on 403/541 as before.
- `save_settings` now forwards `deliveryRegion` to the watcher, and startup
  passes it in `WatcherConfig`.
- **Sync cost:** about 400 lines (lookups, delivery cache, parsers) now
  mirror desktop code. Upstream changes to delivery or band lookup need a
  manual port; both module headers say so.

**Verification.**
- `cargo clippy -p apw-headless --all-targets -- -D warnings` is clean, and
  `cargo test -p apw-headless` passes 8 tests. The new ones cover:
  - delivery query key order and the fulfillment URL
  - product delivery parsing (reason/message kept, empty entries dropped,
    `errorMessage` → `ApiError::Apple`, non-JSON → error)
  - the nested Watch display-name lookup
  - the cache rules (a real quote survives a later empty answer, one
    attempt per cycle, only Blocked/RateLimited mark a region,
    `begin_cycle` resets both)
  - the two desktop parser fixtures for localities and band choices/sizes
- Live on kind through the web API, against Apple:
  - Province list, then 上海 → 上海, then its districts (黄浦区, 徐汇区, …).
  - `en_SG` is refused with 422.
  - Band choices for Watch case `MJKA4CH/B` (sololoop / burgundy, …), and
    band sizes for that style/color (MKJH4FE/A "0" … ).
  - In Chrome the panel no longer shows the error, and the province dropdown
    lists every province.
- Not verified: actual delivery quotes. They only run after a successful
  pickup query, and pickup queries from this IP were still getting HTTP 541
  at the time.
