//! Web management UI: the desktop app's own React frontend, served over HTTP.
//!
//! The frontend reaches its backend only through Tauri's `invoke` and
//! `listen` (all in `src/lib/store.ts`). The web build aliases those modules
//! to `crates/apw-headless/web/tauri-shim.ts`, which maps
//! `invoke(name, args)` to `POST /api/v1/commands/{name}` and `listen(channel)`
//! to the server-sent event stream at `GET /api/v1/events`. This module is the
//! server half: the same command names and argument shapes as the
//! `#[tauri::command]`s in `src-tauri/src/lib.rs`, calling the same `apw-core`
//! functions. Commands for things a server doesn't have (sound, Clash, the
//! updater) answer with an error the UI shows in its log.
//!
//! Every request, static files included, needs HTTP Basic auth. The listener
//! is loopback-only by default (reached with `kubectl port-forward` or an SSH
//! tunnel); the password still matters there, because any web page the
//! operator visits can send requests to 127.0.0.1.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use apw_core::catalog::CatalogError;
use apw_core::config::{MIN_INTERVAL_SECONDS, Settings};
use apw_core::model::{Category, REGIONS, Target, region_by_locale};
use apw_core::notify::Notification;
use apw_core::watcher::Event;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures_util::Stream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;
use tower_http::services::ServeDir;

use crate::{AppState, apple_lookup, send_alert, target_open_url, target_purchase_url};

pub const USERNAME: &str = "admin";

/// Channel names the frontend subscribes to (`src/lib/store.ts`).
const EVENT_CHANNEL: &str = "watcher://event";
const NOTICE_CHANNEL: &str = "watcher://notice";

/// One message for every open event stream: a Tauri channel name and its
/// JSON payload, serialized once no matter how many tabs are open.
#[derive(Clone)]
pub struct Push {
    channel: &'static str,
    data: String,
}

impl Push {
    pub fn event(event: &Event) -> Self {
        Self {
            channel: EVENT_CHANNEL,
            data: serde_json::to_string(event).unwrap_or_else(|_| "null".into()),
        }
    }

    pub fn notice(text: String) -> Self {
        Self {
            channel: NOTICE_CHANNEL,
            data: Value::String(text).to_string(),
        }
    }
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<AppState>,
    password: String,
    web_dir: PathBuf,
) {
    if let Err(err) = axum::serve(listener, router(app, &password, web_dir)).await {
        eprintln!("web UI server stopped: {err}");
    }
}

fn router(app: Arc<AppState>, password: &str, web_dir: PathBuf) -> Router {
    let expected: Arc<str> = basic_auth_header(USERNAME, password).into();
    Router::new()
        // RPC-style rather than one REST resource per command: the frontend
        // is upstream's, and it speaks Tauri commands.
        .route("/api/v1/commands/{name}", post(command))
        .route("/api/v1/events", get(events))
        .with_state(app)
        .fallback_service(ServeDir::new(web_dir))
        .layer(middleware::from_fn_with_state(expected, require_auth))
}

fn basic_auth_header(user: &str, password: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {token}")
}

/// Constant-time compare of the whole `Authorization` header against the
/// one precomputed value, so no decoding and no early exit on a mismatch.
fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
}

async fn require_auth(State(expected): State<Arc<str>>, request: Request, next: Next) -> Response {
    let mut response = if authorized(request.headers(), &expected) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                r#"Basic realm="apw-monitor", charset="UTF-8""#,
            )],
        )
            .into_response()
    };
    // The browser re-sends cached Basic credentials on every request to this
    // origin, so another site must not be able to frame the UI and steer
    // clicks into it.
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// `Json<Value>` rejects any body that isn't `Content-Type: application/json`
/// (415). That is the CSRF guard: a cross-site form can't send that type, and
/// a cross-site `fetch` with it needs a CORS preflight this server never
/// approves.
async fn command(
    State(app): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(args): Json<Value>,
) -> Response {
    match run_command(&app, &name, &args).await {
        Some(Ok(value)) => Json(value).into_response(),
        // Tauri rejects `invoke` with the command's error string; the shim
        // rethrows this JSON string so `String(err)` in the UI reads the same.
        Some(Err(message)) => (StatusCode::UNPROCESSABLE_ENTITY, Json(message)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(format!("unknown command: {name}")),
        )
            .into_response(),
    }
}

async fn events(
    State(app): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = futures_util::stream::unfold(app.pushes.subscribe(), |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(push) => {
                    let event = SseEvent::default().event(push.channel).data(push.data);
                    return Some((Ok(event), rx));
                }
                // A stalled tab fell behind. Skipping is safe: every
                // cycleComplete carries the full snapshot the table renders.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null))
        .map_err(|err| format!("invalid argument `{key}`: {err}"))
}

fn reply<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| err.to_string())
}

/// `None` means no such command. Names and argument keys (camelCase) match
/// what `src/lib/store.ts` passes to `invoke`.
async fn run_command(app: &AppState, name: &str, args: &Value) -> Option<Result<Value, String>> {
    let result = match name {
        "list_regions" => reply(
            REGIONS
                .iter()
                .map(|r| json!({ "title": r.title, "locale": r.locale }))
                .collect::<Vec<_>>(),
        ),
        "list_categories" => reply(
            Category::ALL
                .iter()
                .map(|c| json!({ "value": c, "title": c.title() }))
                .collect::<Vec<_>>(),
        ),
        "list_stores" => match arg::<String>(args, "locale") {
            Ok(locale) => app
                .catalog
                .stores(&locale)
                .map_err(|e| e.to_string())
                .and_then(reply),
            Err(err) => Err(err),
        },
        "list_products" => match arg::<String>(args, "locale") {
            Ok(locale) => app
                .catalog
                .products(&locale)
                .map_err(|e| e.to_string())
                .and_then(reply),
            Err(err) => Err(err),
        },
        "refresh_products" => refresh_products(app, args).await,
        "get_settings" => reply(app.settings_snapshot()),
        "save_settings" => save_settings(app, args).await,
        "set_targets" => set_targets(app, args).await,
        "set_interval" => set_interval(app, args).await,
        "get_snapshot" => reply(app.watcher.snapshot().await),
        "is_running" => reply(app.watcher.is_running().await),
        "start_watching" => {
            app.watcher.start().await;
            reply(())
        }
        "stop_watching" => {
            app.watcher.stop().await;
            reply(())
        }
        "retry_watching_now" => reply(app.watcher.retry_now().await),
        "test_notify" => test_notify(app).await,
        // The desktop app opens the page itself. Here the shim opens the
        // returned URL in the operator's browser.
        "open_target_product" => arg::<Target>(args, "target").and_then(|target| {
            target_purchase_url(&app.catalog, &target)
                .ok_or_else(|| "cannot build a product URL for this target".to_string())
                .and_then(reply)
        }),
        // `null` is the desktop app's "already up to date" answer, which the
        // UI's silent startup check ignores.
        "check_for_update" => reply(()),
        "install_update" => {
            Err("headless mode has no in-app updater; deploy a newer image instead".into())
        }
        "list_delivery_localities" => list_delivery_localities(app, args).await,
        "list_watch_band_choices" => list_watch_band_choices(app, args).await,
        "list_watch_band_sizes" => list_watch_band_sizes(app, args).await,
        "test_clash_route" | "list_clash_nodes" | "select_clash_node" => Err(
            "not available in headless mode: Clash route rotation is not ported \
             (use the random proxy list instead)"
                .into(),
        ),
        _ => return None,
    };
    Some(result)
}

async fn save_settings(app: &AppState, args: &Value) -> Result<Value, String> {
    let mut next: Settings = arg(args, "settings")?;
    next.normalize();
    crate::camoufox_fetcher::validate_proxies(&next.network.proxies)?;
    if next.telegram.bot_token.is_empty() != next.telegram.chat_id.is_empty() {
        return Err("set both the Telegram bot token and chat ID, or neither".into());
    }
    app.hydrate_targets(&mut next.targets);
    app.put_settings(next.clone())?;
    app.watcher.set_targets(next.targets.clone()).await;
    app.watcher.set_interval(next.interval()).await;
    app.watcher
        .set_delivery_region(next.delivery_region.clone())
        .await;
    reply(next)
}

async fn list_delivery_localities(app: &AppState, args: &Value) -> Result<Value, String> {
    let locale: String = arg(args, "locale")?;
    let state_name: String = arg(args, "stateName")?;
    let city_name: String = arg(args, "cityName")?;
    apple_lookup::delivery_localities(&app.http, &locale, &state_name, &city_name)
        .await
        .and_then(reply)
}

async fn list_watch_band_choices(app: &AppState, args: &Value) -> Result<Value, String> {
    let locale: String = arg(args, "locale")?;
    let case_part: String = arg(args, "casePart")?;
    apple_lookup::watch_band_choices(&app.http, &app.catalog, &locale, &case_part)
        .await
        .and_then(reply)
}

async fn list_watch_band_sizes(app: &AppState, args: &Value) -> Result<Value, String> {
    let locale: String = arg(args, "locale")?;
    let case_part: String = arg(args, "casePart")?;
    let style_key: String = arg(args, "styleKey")?;
    let color_key: String = arg(args, "colorKey")?;
    apple_lookup::watch_band_sizes(
        &app.http,
        &app.catalog,
        &locale,
        &case_part,
        &style_key,
        &color_key,
    )
    .await
    .and_then(reply)
}

async fn set_targets(app: &AppState, args: &Value) -> Result<Value, String> {
    let mut next = app.settings_snapshot();
    next.targets = arg(args, "targets")?;
    next.normalize();
    app.hydrate_targets(&mut next.targets);
    app.put_settings(next.clone())?;
    app.watcher.set_targets(next.targets).await;
    reply(app.watcher.snapshot().await)
}

async fn set_interval(app: &AppState, args: &Value) -> Result<Value, String> {
    let seconds = arg::<u64>(args, "seconds")?.max(MIN_INTERVAL_SECONDS);
    let mut next = app.settings_snapshot();
    next.interval_seconds = seconds;
    app.put_settings(next.clone())?;
    app.watcher.set_interval(next.interval()).await;
    reply(seconds)
}

/// Desktop `refresh_catalog` without the Clash detours: stores, then the
/// selected category's product pages, both over plain HTTPS (the catalog
/// pages aren't behind the bot check the pickup endpoint is).
async fn refresh_products(app: &AppState, args: &Value) -> Result<Value, String> {
    let locale: String = arg(args, "locale")?;
    let category: Option<Category> = arg(args, "category")?;
    let region = region_by_locale(&locale).ok_or_else(|| format!("unknown region {locale}"))?;
    let mut errors = Vec::new();

    let stores = match app.catalog.refresh_stores(region, &app.http).await {
        Ok(count) => Some(count),
        Err(err) => {
            errors.push(format!("store list: {err}"));
            None
        }
    };
    let products = match app
        .catalog
        .refresh_products(region, category, &app.http)
        .await
    {
        Ok(count) => count,
        Err(CatalogError::RefreshFailed { fetched, .. }) if fetched > 0 => {
            errors.push("some product pages failed; the rest were updated".into());
            fetched
        }
        Err(err) => {
            errors.push(format!("product catalog: {err}"));
            0
        }
    };

    // Same follow-up as the desktop app: refreshed titles flow into targets
    // already being watched (their identity, locale|store|part, never changes).
    let mut next = app.settings_snapshot();
    let before = next.targets.clone();
    app.hydrate_targets(&mut next.targets);
    if next.targets != before {
        app.put_settings(next.clone())?;
        app.watcher.set_targets(next.targets).await;
    }

    if stores.is_none() && products == 0 {
        return Err(errors.join("; "));
    }
    reply(json!({ "products": products, "stores": stores, "errors": errors }))
}

/// Bark and Telegram only: there is no speaker, and opening a page happens
/// in the alert's link instead of on this machine.
async fn test_notify(app: &AppState) -> Result<Value, String> {
    let settings = app.settings_snapshot();
    let first = settings.targets.first();
    let bark_url = first.map_or(settings.bark_url.as_str(), |target| {
        settings.bark_url_for(target)
    });
    let telegram = app.telegram(&settings);
    if bark_url.trim().is_empty() && !telegram.is_configured() {
        return Err(
            "set a Bark URL or a Telegram bot first: they are the alert channels in headless mode"
                .into(),
        );
    }
    let mut notification = Notification::new(
        "Notification test (not a stock hit)",
        "If this arrived, in-stock alerts will too",
    );
    if let Some(url) =
        first.and_then(|target| target_open_url(&app.catalog, target, settings.open_on_hit))
    {
        notification = notification.with_url(url);
    }
    send_alert(app, bark_url, telegram, &notification).await?;
    reply(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_accepts_only_the_exact_credentials() {
        let expected = basic_auth_header(USERNAME, "s3cret");
        assert_eq!(expected, "Basic YWRtaW46czNjcmV0");

        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            headers
        };
        assert!(authorized(&with(&expected), &expected));
        assert!(!authorized(&HeaderMap::new(), &expected));
        assert!(!authorized(
            &with(&basic_auth_header(USERNAME, "s3cre")),
            &expected
        ));
        assert!(!authorized(
            &with(&basic_auth_header("root", "s3cret")),
            &expected
        ));
        assert!(!authorized(&with(&format!("{expected}x")), &expected));
    }
}
