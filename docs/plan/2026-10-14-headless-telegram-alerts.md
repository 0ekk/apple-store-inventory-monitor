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

2. **Configured in the web UI, with the environment as a fallback.**
   (Revised the same day; the first version read the environment only.)
   `Settings.telegram` (`{botToken, chatId}`) is set from a headless-only
   Telegram panel. While both fields are empty, the monitor uses
   `APW_TELEGRAM_BOT_TOKEN` and `APW_TELEGRAM_CHAT_ID`. Precedence is per
   pair, never per field, so a page token is never paired with an
   environment chat. The settings are read at alert time, so a change needs
   no restart.
   Trade-off: a token set in the page lives in `settings.v2.json` (mode
   0600, like the Bark device key) and goes to every browser that logs in to
   the web UI, which is the same exposure the Bark key already has. Users who
   want the token kept out of the browser leave the page empty and use the
   environment (a Kubernetes Secret).

3. **Both or neither, at every entry point.** Setting only one variable
   exits at startup, like an invalid proxy list. `save_settings` rejects a
   half-filled page setting, and the panel saves only once both fields are
   filled (or both cleared), so moving from the first field to the second
   doesn't trip that check. In the library, both empty means "not
   configured" (`Ok(())`, per the `Notifier` contract) and half-configured is
   a `Config` error.

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

7. **The panel shares the query-route slot.** The desktop `src/App.tsx` has
   no hook point for a new notification panel, and the headless build
   already aliases `@/components/NetworkSettings` to its own file. That file
   now renders `TelegramSettings.tsx` above the proxy-list panel, so it sits
   right below the Bark settings, and `src/` stays unmodified. The panel's
   text is English, like the rest of the headless-only UI; the token field is
   a password input.

8. **A Telegram-only test button in the panel.** The shared test button
   ("测试提醒与跳转", upstream `src/`) sends to Bark and Telegram at once, and
   its log line never mentions Telegram, so users couldn't tell how to check
   a bot. A new `test_telegram` command sends one message with the panel's
   current values, saved or not (a wrong token never has to be saved to be
   tested), using the same page-then-environment resolution as alerts, and
   the panel shows the result inline.

## Impact

- New public type `apw_core::notify::Telegram`. `read_capped` takes the
  channel name, so its errors name the right channel.
- `apw_core::config`: new `TelegramSettings` and `Settings.telegram`
  (serde default, so older settings files load; `normalize` trims both
  values). The desktop app saves and loads the field but never reads it.
- Headless: `AppState` keeps the environment's `Telegram`, and
  `AppState::telegram(&settings)` picks the page pair or that fallback. Stock
  hits and the web UI's test button go through `send_alert` (Bark + Telegram
  via `Multi`). A hit is skipped only when neither channel is configured.
  The test button's error now names both options. Startup logs whether the
  environment bot is set and that the page fields take precedence.
- Web UI: new `crates/apw-headless/web/TelegramSettings.tsx`, rendered by the
  headless `NetworkSettings.tsx`, with a "Send a test message" button.
- New headless-only command `test_telegram` (`{telegram: {botToken,
  chatId}}`), returning a status string. `AppState::telegram` now takes
  `&TelegramSettings` so alerts and this test share the resolution.
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
- Page configuration (second change):
  - `tests/config.rs` covers the `telegram` wire format
    (`{"botToken":..,"chatId":..}`) and trimming in `normalize`;
  - with the page fields empty, the test command used the environment bot
    (the `HTTPS_PROXY` listener saw the `CONNECT`);
  - saving a half-filled pair returned "set both the Telegram bot token and
    chat ID, or neither";
  - a padded pair was saved trimmed to `settings.v2.json`;
  - with no environment bot and the page pair set, the test command reached
    the real Telegram API (`HTTP 401: Unauthorized` for the fake token);
  - `tsc` passes, and a headless Chrome screenshot of the web build shows
    the panel between the Bark settings and the query-route panel.
- Test button (third change), real binary, `test_telegram`:
  - a half-filled pair: "set both ... or neither";
  - nothing in the page or the environment: the "no Telegram bot" error;
  - a fake page token: the real API's `HTTP 401: Unauthorized`, and no
    settings file was written;
  - empty page fields with a fake environment bot: the same 401, so the
    fallback was used;
  - clippy, tests, `tsc` and the vite build pass. The success path is covered
    by the library's unit test; the button's rendering was not checked
    visually.
