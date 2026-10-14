# Headless random exit proxies (2026-10-14)

Builds on `2026-09-14-camoufox-headless-port.md` and
`2026-10-12-headless-web-ui.md`.

Later change: `2026-10-14-headless-clash-routes.md` brought the desktop
query-route panel back. The proxy list now applies only in the `system`
route mode and sits in its own "Exit proxies" panel.

## Rationale

Apple rate-limits and blocks the pickup endpoint per exit IP (HTTP 541).
Headless had one static exit proxy for the sidecar's whole lifetime
(`SIDECAR_PROXY`), and the web UI's "query route" panel was the desktop
Clash panel, whose commands all fail in headless mode. The user asked for a
socks5/http proxy list in that panel, with a random proxy chosen for every
request.

## Decisions made

1. **Headless only.** The desktop Chromium session takes its proxy from a
   launch flag for the whole process, so per-request proxies there would
   mean moving `chromium_fetcher.rs` to CDP browser contexts. The desktop
   app keeps its Clash rotation. The user chose this scope.

2. **One warmed page per (proxy, region), not a re-warm per request.**
   Apple's bot-check cookies are earned per exit IP by loading a page
   first. The sidecar keys its sessions by `(proxy, locale)` and creates
   each page with `Browser.new_page(proxy=...)`, which gives every page its
   own context: its own cookie jar and its own exit proxy. Only the first
   request on a proxy pays for a page load. The user chose this over
   re-warming on every request (about 5-15 s and double the Apple hits per
   request) and over "random but sticky until blocked".

3. **Pages are LRU-capped (`SIDECAR_MAX_PAGES`, default 4).** N proxies
   times R regions can outgrow the container (the sidecar was OOM-killed at
   1.5 GiB with three regions warming at once). After each warm-up, idle
   pages beyond the cap are closed, least recently used first. A session
   whose lock is held is mid-request and is never closed, so the count can
   briefly exceed the cap.

4. **The monitor picks, the sidecar stays generic.** The list lives in
   `settings.v2.json` as `network.proxies` (in `apw_core::config`, next to
   the Clash settings the desktop app uses and ignores in headless). The
   Rust fetcher picks one uniformly at random for each sidecar call and
   sends it as an optional `proxy` field. The sidecar still knows nothing
   about Apple or settings. With an empty list the field is omitted and the
   sidecar falls back to `SIDECAR_PROXY` or a direct connection, as before.
   Randomness comes from `std::hash::RandomState` (fresh keys per instance)
   instead of adding the `rand` crate.

5. **Validation on save and on startup, matching the sidecar.** Accepted:
   `http`, `https` or `socks5`, a host, and an explicit port. `Url` drops
   default ports, so the explicit-port check reads the authority text.
   Errors name an entry by position only, because entries may carry
   passwords. Logs show a proxy as `scheme://host:port` with credentials
   stripped.

6. **socks5 with a username/password is refused.** Verified live:
   Playwright's Firefox fails page creation with "Browser does not support
   socks5 proxy authentication". Both the monitor and the sidecar refuse
   such entries with a message that suggests the provider's `http://`
   endpoint instead. A local relay that adds SOCKS5 auth would remove the
   limit; it was left out until someone needs it.

7. **No per-proxy health tracking (ponytail).** A dead or blocked proxy
   keeps failing its share of requests until it is removed. The failing
   proxy is named in the log and in sidecar errors. A cooldown like the
   desktop `RoutePool` is the upgrade path if this shows up in practice.

8. **The web UI panel is swapped by a Vite alias, not by editing `src/`.**
   `crates/apw-headless/web/NetworkSettings.tsx` replaces
   `@/components/NetworkSettings` in the web build only. The headless alias
   list became an array because `mergeConfig` puts array entries ahead of
   the desktop config's `@` alias. Otherwise `@` would match first. The
   desktop UI is unchanged, so `src/` still merges cleanly from upstream.

## Impact

- `apw_core::config::NetworkSettings` gains `proxies: Vec<String>` (serde
  default: empty). Old settings files load unchanged. The desktop app keeps
  the field when it saves (its UI spreads `network`) but never uses it.
- Sidecar protocol: `POST /pickup` takes an optional `proxy` string.
  Requests without it behave exactly as before.
- Requests to the same region through different proxies can now run
  concurrently (the per-session lock is per proxy and region). Page loads
  are still serialized by the sidecar's global warm lock.
- Kubernetes users must allow each proxy in `01-networkpolicy.yaml`, as
  they already had to for `SIDECAR_PROXY`.

## Verification

- `cargo test -p apw-core -p apw-headless`: all pass, including new tests
  for validation (accepted and refused forms; no password in errors),
  credential-free labels, random pick coverage, and the settings wire
  format. `cargo clippy --all-targets` on both crates is clean.
  `cargo check -p apple-store-inventory-monitor --tests` (desktop) passes.
- Sidecar `test_server.py` in the sidecar image: proxy parsing (including
  socks5 auth refusal) and LRU eviction that skips a locked session.
- Live sidecar test (Camoufox image, local lab with no Apple traffic): a
  web server plus four proxies. Direct, `http`, `http` with Basic auth and
  `socks5` requests each reached the web server through the expected hop
  (proxy logs and peer IPs). `socks5` with auth was refused. With
  `SIDECAR_MAX_PAGES=2`, a repeated proxy was evicted and re-warmed.
- Headless binary end-to-end (no targets, so no Apple requests):
  `save_settings` with a socks5-auth entry returned 422 with the
  position-only message. A valid list was trimmed, stripped of blank
  lines and persisted. A hand-broken file made startup exit 1.
- `tsc --noEmit` over `src/` plus the new panel, the headless web build
  (bundle contains the proxy panel and not the Clash panel), and the
  desktop web build all pass.
