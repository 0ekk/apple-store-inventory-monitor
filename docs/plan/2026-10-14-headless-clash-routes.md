# Headless Clash query routes (2026-10-14)

Builds on `2026-10-12-headless-web-ui.md` and
`2026-10-14-headless-random-proxies.md`.

## Rationale

The headless web UI had replaced the desktop 查询线路 (query route) panel
(跟随系统代理 / 指定节点 / Clash 节点轮换) with the random proxy list. The
Clash modes never worked headless: `network.mode` was ignored and the
`test_clash_route` / `list_clash_nodes` / `select_clash_node` commands
returned "not available". The user asked to keep the original route
settings, working, with the proxy list as an addition.

## Decisions made

1. **The desktop Clash client and route pool moved into `apw-core`.**
   `clash.rs` and `route_pool.rs` moved by `git mv` from `src-tauri/src/` to
   `crates/apw-core/src/`, unchanged apart from one import, so both apps share
   them and their unit tests run under `cargo test -p apw-core`. The user
   chose this over copying them into the headless crate. Their Chinese error
   texts stay as they were (moved code).

2. **The route state is a hand-kept port.** `crates/apw-headless/src/clash_route.rs`
   ports the route half of the desktop `QueryState`
   (`src-tauri/src/chromium_fetcher.rs`): node choice, delay test, select,
   group connection reset, cooldown, `CoolingDown` when every node rests,
   `Blocked` after 10 consecutive rejections, schedule hint, summary, retry.
   Its header says to keep it in sync by hand, like `DeliveryState`. The
   desktop session is a single Chromium process with a launch-flag proxy, so
   sharing the state machine itself would have meant reshaping the desktop
   fetcher.

3. **Mode semantics.** `system` keeps the random proxy list (else
   `SIDECAR_PROXY`, else direct). `pinned` and `clash` send every Apple
   pickup and delivery query to `http://{controller host}:{proxyPort}` and
   ignore the list. The desktop hardcodes `127.0.0.1` because Clash runs on
   the same machine; headless uses the controller's host, the one name both
   containers can reach (`127.0.0.1` inside a k8s pod, a service name in
   compose). IPv6 hosts stay bracketed.

4. **One warmed sidecar page per node, with no sidecar change.** The proxy
   string carries the node as a URL fragment (`http://host:7899#node`, raw,
   not percent-encoded). The sidecar keys pages on the raw proxy string and
   builds the Playwright proxy from host and port only, so each node gets its
   own page and cookies (Apple's bot check is passed per exit IP), and
   switching back reuses a page that is still open. A `ponytail:` comment
   marks the implicit coupling; an explicit session field in the sidecar
   protocol is the upgrade path if the sidecar ever normalises proxy
   strings.

5. **Node failures are classified by where they happened.** mihomo answers
   `200 Connection established` before dialling the node, so a dead node
   never shows up as a proxy status. Any `Page.goto:` error (including the
   Playwright timeout) or a failed in-page fetch counts as the node failing
   and skips it for a minute (`UNREACHABLE_COOLDOWN`).
   `NS_ERROR_PROXY_CONNECTION_REFUSED` and `NS_ERROR_UNKNOWN_PROXY_HOST` mean
   the browser can't reach the Clash port at all; they are reported as a
   setup error naming the address and the usual causes (Clash not running,
   not listening on `0.0.0.0`, controller host not resolvable from the
   sidecar) instead of rotating through every node. Sidecar-side errors
   (`sidecar unreachable`, `page evaluation failed`) never count. A false
   positive costs a 60 s skip.

6. **The route lock is held through delivery.** In the Clash modes a pickup
   locks the route, runs the desktop switch-and-retry loop
   (Blocked / RateLimited → reject and cool the node; node failure → skip;
   up to twice the node count), then fetches delivery quotes under the same
   lock with the proxy that just worked, so no other query switches the node
   in between. `sidecar_fetch` takes the proxy as a parameter and never
   locks, which rules out the deadlock the plan originally avoided by
   dropping the lock. In `system` mode delivery now reuses the pickup's
   randomly picked proxy instead of drawing a new one per request, matching
   the desktop ("delivery through the exit that just proved to work").

7. **The UI reads route state without waiting on a pickup.** `list_routes`
   asks the controller for nodes and delays outside the lock and reads
   cooldowns with `try_lock`; while a pickup holds the lock, every node shows
   as not resting (`ponytail:` comment). `select_clash_node` runs under the
   lock like the desktop, so it waits for an in-flight switch loop.

8. **Summary strings stay in the desktop's Chinese.** By the user's choice,
   `Clash 节点 {node}（本轮切换 N 次）`, `指定节点 {node}` and the
   `尚未选定` placeholder are kept exactly, because the upstream node picker
   parses them (`nodeFromRoute`) to follow automatic switches. All other new
   text (logs, errors, the headless panel) is English.

9. **The web UI shows the original panel plus the list.** The headless
   `NetworkSettings.tsx` renders the Telegram panel, the upstream panel
   (imported by relative path, which escapes the Vite alias that points
   `@/components/NetworkSettings` at the headless file), then the proxy list,
   now titled "Exit proxies" and summarised "Off (Clash route)" outside
   `system` mode. Its panel id changed from `network` to `proxies`; the id is
   only the localStorage open/closed key. The upstream panel spreads
   `...network` on save, so `proxies` survives route edits. `src/` stays
   unmodified.

10. **Deliberate limits.** The catalog refresh stays on the monitor's own
    network (the desktop routes it via Clash); a `ponytail:` comment in
    `web.rs` names the upgrade path. The route test's port check runs from
    the monitor's network namespace, which matches the sidecar's only when
    they share one (same pod) or the same compose network.

## Impact

- `apw_core` gains `pub mod clash` and `pub mod route_pool`. The desktop
  imports them from there; its behaviour is unchanged.
- Headless:
  - `CamoufoxFetcher` holds `NetworkSettings` (`set_network`) instead of the
    bare proxy list, plus the route state. `cycle_stats`, `schedule_hint`,
    `retry_now` and `begin_cycle` now carry the route, so the watcher's
    cooling-down handling and the per-cycle route log work as on the
    desktop.
  - `test_clash_route`, `list_clash_nodes` and `select_clash_node` are
    implemented; the "not available" arm is gone.
  - Startup logs the route: the proxy-list size in `system` mode, or the
    Clash port with "pinned node" / "node rotation".
- Deploy: Clash modes need a Clash/mihomo the sidecar can reach.
  `docs/headless-deploy.md` gains a "Clash node rotation" section (mihomo
  config, compose service with `0.0.0.0` listeners, a third container in the
  k8s pod, NetworkPolicy rules for the node ports). No manifest changes: the
  mihomo container is opt-in and depends on the user's subscription.
- `camoufox-sidecar/` is untouched.

## Verification

- `cargo fmt --all --check`; `cargo clippy -p apw-core -p apw-headless
  --all-targets --all-features -- -D warnings` is clean;
  `cargo test -p apw-core -p apw-headless` passes (86 apw-core unit tests,
  including the moved Clash and route-pool tests; 18 apw-headless tests);
  `cargo check -p apple-store-inventory-monitor --tests` (desktop) passes.
  Workspace-wide clippy with the local Rust 1.96 reports three
  `nonminimal_bool` errors in desktop `chromium_fetcher.rs` code this change
  doesn't touch; CI passed on the parent commit.
- New `clash_route.rs` tests against an axum fake controller and fake
  sidecar:
  - a refused node is cooled, the next is selected and delivery follows it;
  - every node refused gives cooling down until retry or a manual pick;
  - a dead node is skipped without counting as a refusal;
  - pinned mode selects the pinned node and never rotates;
  - system mode never touches Clash;
  - the route test normalises the draft and checks the port;
  - the exit proxy comes from the controller host (name, IPv4, IPv6, port 0);
  - node failures are told apart from setup errors.
- Sidecar lab (`apw-sidecar-dev` image with the current `server.py`, a fake
  proxy, no Apple traffic) confirmed the Firefox error strings the
  classifier relies on:
  - a refused port gives `Page.goto: NS_ERROR_PROXY_CONNECTION_REFUSED`;
  - an unknown host gives `Page.goto: NS_ERROR_UNKNOWN_PROXY_HOST`;
  - a CONNECT answered with 200 and then closed gives
    `Page.goto: NS_ERROR_NET_RESET`;
  - an in-page fetch through a dead tunnel gives status 0 with
    "NetworkError when attempting to fetch resource";
  - `#A` twice warmed one page, and `#香港 01` warmed a separate one.
- Real headless binary with a fake controller, via curl:
  - `test_clash_route` with the draft controller `127.0.0.1:19097`:
    `portOpen` true and false as expected, one info node skipped,
    `timeoutCount` 1;
  - `list_clash_nodes` returned the nodes;
  - `select_clash_node B` returned its delay (77), and the controller saw
    the select, the connection listing and the delete;
  - an unknown node returned 422 with the English error.
- `tsc` over `src/` plus the headless panels and the headless vite build
  pass. A headless Chrome screenshot of the web UI showed the Telegram
  panel, the 查询线路 panel in Clash 节点轮换 mode with the node picker
  (1/2 available), and the Exit proxies panel.
- Not tested: a real mihomo with real nodes against Apple, and the
  compose and k8s mihomo recipes in the deploy guide.
