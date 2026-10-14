# Headless Telegram alerts (2026-10-14)

Builds on `2026-09-14-camoufox-headless-port.md` and
`2026-10-12-headless-web-ui.md`.

## Rationale

Bark was the only alert channel in headless mode, and Bark only reaches iOS
devices. The user asked for a Telegram bot as an additional alert channel.

## Decisions made

1. **The channel lives in `apw-core`, in its own file.**
   `apw_core::notify::Telegram` implements the existing `Notifier` trait, so
   it composes with Bark through `Multi` (concurrent sends, one failure never
   blocks the other). It sits in `crates/apw-core/src/notify/telegram.rs`
   rather than in `notify.rs` so upstream changes to `notify.rs` keep merging
   cleanly. The desktop app could use it later; it doesn't today.

2. **Configured by environment, not by settings.** `APW_TELEGRAM_BOT_TOKEN`
   and `APW_TELEGRAM_CHAT_ID`. The bot token is a full credential for the
   bot. In the environment it can come from a Kubernetes Secret, and it never
   reaches `settings.v2.json`, which the web UI sends to every browser that
   logs in. This also needs no UI change: the web UI reuses the desktop
   `src/App.tsx`, which has no hook point for another notification field.
   Trade-off: changing the chat needs a restart.

3. **Both or neither.** Setting only one variable exits at startup, like an
   invalid proxy list. In the library, both empty means "not configured"
   (`Ok(())`, per the `Notifier` contract) and half-configured is a `Config`
   error.

4. **Plain-text `sendMessage`, one chat for all targets.** Title, body and
   the `openOnHit` link, one per line, with no `parse_mode`, so store and
   product names never need escaping and Telegram links the URL itself.
   There is no per-model routing like `productBarkUrls`; a group chat covers
   several people.

5. **The token never appears in errors.** It is in the request path, so
   transport errors use the existing `safe_transport_detail` (no URL), the
   shared `read_capped` now strips the URL from body-read errors (this also
   protects Bark's device key), and a rejection's `description` has the
   token replaced with `***` in case the server echoes it. `Telegram` does
   not implement `Debug`.

6. **No dedicated proxy setting.** reqwest honours `HTTPS_PROXY`, which
   covers deployments that can't reach `api.telegram.org` directly.

## Impact

- New public type `apw_core::notify::Telegram`. `read_capped` takes the
  channel name, so its errors name the right channel.
- Headless: `AppState` holds a `Telegram`; stock hits and the web UI's test
  button go through `send_alert` (Bark + Telegram via `Multi`). A hit is
  skipped only when neither channel is configured. The test button's error
  now names both options.
- Deploy: `docker-compose.yml` passes both variables through;
  `deploy/k8s/10-apw.yaml` reads them from an optional `apw-telegram`
  Secret. The NetworkPolicy already allows public HTTPS, so no rule changes.
- The desktop app is unchanged.

## Verification

- `cargo test -p apw-core -p apw-headless` passes, including four new unit
  tests against a local HTTP server: request path and JSON body (chat ID,
  three-line text with `&` and `/` intact), 401 rejection reporting the
  description with the token masked, a refused connection with no token in
  the error, and the unconfigured / half-configured cases. Clippy
  (`--all-targets`) is clean on both crates;
  `cargo check -p apple-store-inventory-monitor --tests` (desktop) passes.
- Headless binary, no targets and no sidecar:
  - only `APW_TELEGRAM_BOT_TOKEN` set: exit 1 with the "both or neither"
    message;
  - nothing configured: the test command returns the new error;
  - a fake token against the real Telegram API: the test command returns
    `HTTP 401: Unauthorized` with no token in the text;
  - with `HTTPS_PROXY` pointing at a local listener, the monitor sent
    `CONNECT api.telegram.org:443` to it.
- `docker compose config` accepts the compose file, and the k8s manifest
  parses as YAML. Not tested: delivery to a real chat (needs the user's
  bot token).
