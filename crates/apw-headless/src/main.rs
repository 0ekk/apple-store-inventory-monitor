//! Headless shell for apw-core.
//!
//! Same role as `src-tauri`'s `run()`: wire up the watcher, forward its
//! events, and send alerts. The differences are deliberate, not missing
//! pieces: there is no tray, no updater, and no local speaker, because none
//! of those exist on a server. Alerts go out as Bark pushes and, with
//! `APW_TELEGRAM_BOT_TOKEN`/`APW_TELEGRAM_CHAT_ID` set, Telegram messages —
//! both just HTTP requests in `apw-core`. "Open the bag page automatically"
//! has no meaning without a local browser, so that destination is instead
//! embedded as the link in the alert.
//!
//! Reads and writes the exact same `settings.v2.json` shape the desktop app
//! does (see `apw_core::config::Settings`), so an existing config can be
//! copied over as-is. With `APW_WEB_PASSWORD` set it also serves the desktop
//! app's own UI over HTTP (see `web.rs`). See `docs/headless-deploy.md`.

mod apple_lookup;
mod camoufox_fetcher;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use apw_core::catalog::Catalog;
use apw_core::config::{OpenOnHit, Settings, SettingsStore};
use apw_core::model::{Target, region_by_locale};
use apw_core::notify::{Bark, Multi, Notification, Notifier, Telegram};
use apw_core::watcher::{Event, Watcher, WatcherConfig};
use camoufox_fetcher::CamoufoxFetcher;
use tokio::sync::broadcast;

const DEFAULT_SIDECAR_URL: &str = "http://127.0.0.1:8787";
const DEFAULT_WEB_ADDR: &str = "127.0.0.1:8080";
const DEFAULT_WEB_DIR: &str = "/usr/share/apw-headless/web";

/// Everything the event pump and the web commands share. Mirrors the Tauri
/// shell's `AppState`, minus the Chromium fetcher and the updater.
pub(crate) struct AppState {
    watcher: Watcher,
    /// A clone of the watcher's fetcher, to hand it new proxy lists.
    fetcher: CamoufoxFetcher,
    catalog: Catalog,
    http: reqwest::Client,
    /// From the environment at startup; a no-op when not configured.
    telegram: Telegram,
    store: SettingsStore,
    /// In-memory copy, replaced only after the new settings are on disk.
    settings: RwLock<Settings>,
    /// Fan-out to the web UI's event streams. Sending with no subscriber is
    /// a no-op, so this works the same with the web UI off.
    pushes: broadcast::Sender<web::Push>,
}

impl AppState {
    fn settings_snapshot(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Disk first, then memory: if the write fails, the UI, the watcher and
    /// the next start all keep seeing the same old settings.
    fn put_settings(&self, next: Settings) -> Result<(), String> {
        self.store.save(&next).map_err(|e| e.to_string())?;
        self.fetcher.set_proxies(next.network.proxies.clone());
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(())
    }

    /// Same three catalog passes the desktop app runs on every target list
    /// it accepts: fill in Watch case/band metadata, store titles, and the
    /// pickup locations used for querying.
    fn hydrate_targets(&self, targets: &mut [Target]) {
        self.catalog.hydrate_watch_targets(targets);
        self.catalog.hydrate_store_targets(targets);
        self.catalog.attach_pickup_locations(targets);
    }

    fn notice(&self, text: String) {
        eprintln!("[notice] {text}");
        self.pushes.send(web::Push::notice(text)).ok();
    }
}

fn target_purchase_url(catalog: &Catalog, target: &Target) -> Option<String> {
    let product = catalog.product_by_part(&target.locale, &target.part_number);
    target.purchase_url(product.as_ref())
}

/// Same routing `target_open_url` in the Tauri shell uses, minus the part
/// where it's actually opened in a browser — there is none here.
fn target_open_url(catalog: &Catalog, target: &Target, destination: OpenOnHit) -> Option<String> {
    match destination {
        OpenOnHit::None => None,
        OpenOnHit::Bag => region_by_locale(&target.locale).map(|region| region.bag_url()),
        OpenOnHit::EducationBag => {
            region_by_locale(&target.locale).map(|region| region.education_bag_url())
        }
        OpenOnHit::Product => target_purchase_url(catalog, target),
    }
}

/// Bark and Telegram at once; an unconfigured channel sends nothing.
async fn send_alert(
    app: &AppState,
    bark_url: &str,
    notification: &Notification,
) -> Result<(), String> {
    Multi::new()
        .with(Bark::new(bark_url.to_string(), app.http.clone()))
        .with(app.telegram.clone())
        .notify(notification)
        .await
        .map_err(|err| err.to_string())
}

/// Forwards engine events until `shutdown` fires, then drains what's queued.
///
/// Can't just wait for the channel to close: the web server and this task
/// both hold the `AppState` that owns the watcher, so it never would.
async fn pump_events(
    app: Arc<AppState>,
    mut events: tokio::sync::mpsc::Receiver<Event>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => handle_event(&app, event).await,
                None => return,
            },
            _ = &mut shutdown => {
                // Fired after watcher.stop() returned, so the final cycle's
                // InStock events are already queued; push them before exiting.
                while let Ok(event) = events.try_recv() {
                    handle_event(&app, event).await;
                }
                return;
            }
        }
    }
}

async fn handle_event(app: &AppState, event: Event) {
    app.pushes.send(web::Push::event(&event)).ok();
    match event {
        Event::InStock { state } => {
            let target = &state.target;
            // Read per hit, not once at startup: the web UI can change
            // the Bark address and destination while the watcher runs.
            let settings = app.settings_snapshot();
            let mut notification = Notification::new(
                "In stock",
                format!("{} {}", target.store_title, target.product_name),
            );
            if let Some(url) = target_open_url(&app.catalog, target, settings.open_on_hit) {
                notification = notification.with_url(url);
            }
            println!("[in-stock] {} {}", target.store_title, target.product_name);
            let bark_url = settings.bark_url_for(target);
            if bark_url.trim().is_empty() && !app.telegram.is_configured() {
                println!("[notify] no Bark URL or Telegram chat for this target, alert skipped");
            } else if let Err(err) = send_alert(app, bark_url, &notification).await {
                app.notice(format!("alert failed: {err}"));
            }
        }
        Event::CycleComplete {
            cycle,
            elapsed_ms,
            healthy,
            ..
        } => {
            println!("[cycle {cycle}] finished in {elapsed_ms}ms, healthy={healthy}");
        }
        Event::Trouble { reason, .. } => {
            eprintln!("[trouble] {reason}");
        }
        Event::StateChanged { .. } | Event::CycleStarted { .. } | Event::RunStateChanged { .. } => {
        }
    }
}

/// Waits for whichever shutdown signal the environment actually sends.
///
/// `tokio::signal::ctrl_c()` alone only catches SIGINT — fine at a terminal,
/// but Kubernetes and `docker stop` send SIGTERM to PID 1, which `ctrl_c()`
/// never sees. Without also handling it here, the process would just hang
/// until `terminationGracePeriodSeconds` expires and gets SIGKILLed, skipping
/// `watcher.stop()` entirely.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn exit_with(message: String) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let sidecar_url =
        std::env::var("CAMOUFOX_SIDECAR_URL").unwrap_or_else(|_| DEFAULT_SIDECAR_URL.to_string());
    let web_password = std::env::var("APW_WEB_PASSWORD")
        .ok()
        .filter(|password| !password.is_empty());

    let store = match std::env::var_os("APW_SETTINGS_PATH") {
        Some(path) => SettingsStore::at(PathBuf::from(path)),
        None => SettingsStore::new().unwrap_or_else(|err| {
            exit_with(format!("failed to locate the user config directory: {err}"))
        }),
    };
    let settings_path = store.path().to_path_buf();
    let settings_exists = settings_path.symlink_metadata().is_ok();

    // A file that exists but can't be read is fatal, never "fall back to
    // defaults": the first save from the web UI would overwrite the user's
    // real target list with an empty one. Like the desktop app, `load()`
    // also tightens the directory and file to 0700/0600, so the directory
    // must be writable and owned by this user (see docs/headless-deploy.md).
    let mut settings = store.load().unwrap_or_else(|err| {
        exit_with(format!(
            "failed to read settings from {}: {err} (see docs/headless-deploy.md for the JSON shape)",
            settings_path.display()
        ))
    });
    // Without the web UI a missing file means nothing can ever add targets:
    // exit so the problem shows up as a crash, not a pod that looks healthy
    // and monitors nothing. With the web UI, it's a normal first start.
    if !settings_exists {
        if web_password.is_none() {
            exit_with(format!(
                "no settings file at {} and the web UI is off (APW_WEB_PASSWORD unset), \
                 so there is no way to add targets",
                settings_path.display()
            ));
        }
        println!(
            "no settings file at {} yet; add targets in the web UI",
            settings_path.display()
        );
    }

    // Refreshed store/product lists are cached next to the settings file,
    // same layout as the desktop app's config directory.
    let catalog = Catalog::with_cache_dir(
        settings_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("catalog-cache"),
    );

    // Same rule as an unreadable file: fail now, not on every Nth query.
    camoufox_fetcher::validate_proxies(&settings.network.proxies).unwrap_or_else(|err| {
        exit_with(format!(
            "invalid network.proxies in {}: {err}",
            settings_path.display()
        ))
    });
    let fetcher = CamoufoxFetcher::new(sidecar_url.clone())
        .unwrap_or_else(|err| exit_with(format!("failed to build the sidecar client: {err}")));
    fetcher.set_proxies(settings.network.proxies.clone());
    println!(
        "using camoufox sidecar at {sidecar_url}, {} exit proxy(ies) in the random pool",
        settings.network.proxies.len()
    );

    // Env, not settings: the token is a bot credential, so it belongs in a
    // Secret, not in a JSON file the web UI sends to every browser.
    let token = std::env::var("APW_TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("APW_TELEGRAM_CHAT_ID").unwrap_or_default();
    if token.trim().is_empty() != chat_id.trim().is_empty() {
        exit_with("set both APW_TELEGRAM_BOT_TOKEN and APW_TELEGRAM_CHAT_ID, or neither".into());
    }
    let http = reqwest::Client::new();
    let telegram = Telegram::new(token, chat_id, http.clone());
    println!(
        "Telegram alerts {}",
        if telegram.is_configured() {
            "on"
        } else {
            "off (set APW_TELEGRAM_BOT_TOKEN and APW_TELEGRAM_CHAT_ID to enable)"
        }
    );

    let (watcher, events) = Watcher::spawn(
        fetcher.clone(),
        WatcherConfig {
            interval: settings.interval(),
            delivery_region: settings.delivery_region.clone(),
            ..WatcherConfig::default()
        },
    );
    let (pushes, _) = broadcast::channel(256);
    let app = Arc::new(AppState {
        watcher,
        fetcher,
        catalog,
        http,
        telegram,
        store,
        settings: RwLock::new(Settings::default()),
        pushes,
    });
    app.hydrate_targets(&mut settings.targets);
    app.watcher.set_targets(settings.targets.clone()).await;
    *app.settings.write().unwrap_or_else(|e| e.into_inner()) = settings.clone();

    let (stop_pump, pump_shutdown) = tokio::sync::oneshot::channel();
    let pump_handle = tokio::spawn(pump_events(app.clone(), events, pump_shutdown));

    if let Some(password) = web_password {
        let addr: SocketAddr = std::env::var("APW_WEB_ADDR")
            .unwrap_or_else(|_| DEFAULT_WEB_ADDR.to_string())
            .parse()
            .unwrap_or_else(|err| exit_with(format!("invalid APW_WEB_ADDR: {err}")));
        let web_dir = PathBuf::from(
            std::env::var("APW_WEB_DIR").unwrap_or_else(|_| DEFAULT_WEB_DIR.to_string()),
        );
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|err| exit_with(format!("cannot listen on {addr}: {err}")));
        println!(
            "web UI listening on http://{addr} (user \"{}\")",
            web::USERNAME
        );
        tokio::spawn(web::serve(listener, app.clone(), password, web_dir));
    } else {
        println!("web UI off (set APW_WEB_PASSWORD to enable it)");
    }

    // Unlike the desktop app, start right away: a server restart (deploy,
    // node drain) should resume monitoring without someone clicking Start.
    app.watcher.start().await;
    println!(
        "watcher started: {} target(s), polling every {:?}",
        settings.targets.len(),
        settings.interval()
    );

    wait_for_shutdown_signal().await;
    println!("shutting down...");
    app.watcher.stop().await;
    let _ = stop_pump.send(());
    let _ = pump_handle.await;
}
