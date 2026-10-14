//! Clash route state for the Camoufox fetcher: which node of the dedicated
//! Clash group Apple requests leave through, the per-site cooldowns, and the
//! automatic switch after Apple refuses a node.
//!
//! Port of the route half of the desktop `QueryState`
//! (`src-tauri/src/chromium_fetcher.rs`); keep in sync by hand when upstream
//! changes it. It differs from the desktop in three ways:
//! - the browser is the sidecar's, so instead of reloading its own page after
//!   a switch, the exit proxy names the node (see [`clash_proxy_url`]) and the
//!   sidecar keeps a separately warmed page per node;
//! - the Clash proxy host is the controller's host, not 127.0.0.1, because
//!   Clash may run in another container;
//! - node failures are recognised from Firefox/Playwright errors.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use apw_core::apple::{ApiError, ScheduleHint};
use apw_core::clash::{ClashController, GroupNodes};
use apw_core::config::{ClashSettings, NetworkMode, NetworkSettings, Settings};
use apw_core::model::{Region, region_by_locale};
use apw_core::route_pool::RoutePool;
use serde::Serialize;

/// After this many different nodes in a row were refused, stop switching for
/// the cycle: by then it's probably not one IP's problem, and switching on
/// would only send every node into cooldown.
const MAX_CONSECUTIVE_REJECTIONS: u32 = 10;

/// The sidecar can't reach the Clash port at all: a setup problem, never
/// the node's fault.
const CONFIG_ERRORS: &[&str] = &[
    "NS_ERROR_PROXY_CONNECTION_REFUSED",
    "NS_ERROR_UNKNOWN_PROXY_HOST",
];

const NOT_CLASH: &str = "the query route doesn't use Clash";

/// A node that can't carry traffic. mihomo answers `200 Connection
/// established` before it dials the node, so a dead node shows up as the
/// tunnel closing or stalling: any error while loading the warm-up page
/// (`Page.goto: NS_ERROR_NET_INTERRUPT`, `Page.goto: Timeout 50000ms
/// exceeded`, ...) or a failed in-page `fetch` (status 0). Errors from the
/// sidecar itself (`sidecar unreachable`, `page evaluation failed`) don't
/// count. A false positive only skips the node for a minute.
pub fn is_node_failure(error: &ApiError) -> bool {
    let ApiError::Transport(detail) = error else {
        return false;
    };
    !CONFIG_ERRORS.iter().any(|code| detail.contains(code))
        && (detail.starts_with("in-page fetch failed") || detail.contains("Page.goto:"))
}

/// Cooldowns are kept per Apple host: several regions share `www.apple.com`,
/// and Apple likely counts one exit IP across them.
fn region_site(region: &Region) -> String {
    reqwest::Url::parse(region.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| region.base_url.to_string())
}

/// `host:port` of the Clash proxy port: the controller's host (IPv6 in
/// brackets) with `proxyPort`. Both the sidecar and the monitor must reach it.
pub fn clash_proxy_address(clash: &ClashSettings) -> Result<String, String> {
    let controller = reqwest::Url::parse(&clash.controller)
        .map_err(|e| format!("invalid Clash controller address: {e}"))?;
    let host = controller
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or("the Clash controller address has no host")?;
    if clash.proxy_port == 0 {
        return Err("the Clash proxy port is not set".into());
    }
    Ok(format!("{host}:{}", clash.proxy_port))
}

/// The exit proxy handed to the sidecar: `http://host:port#node`.
// ponytail: the `#node` fragment gives each Clash node its own warmed sidecar
// page only because the sidecar keys pages on the raw proxy string and
// connects to host:port alone. Add an explicit session field to the sidecar
// protocol if it ever normalises proxy strings.
pub fn clash_proxy_url(clash: &ClashSettings, node: Option<&str>) -> Result<String, String> {
    let address = clash_proxy_address(clash)?;
    Ok(match node {
        Some(node) => format!("http://{address}#{node}"),
        None => format!("http://{address}"),
    })
}

/// The Clash group's nodes for the UI's manual node picker.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteList {
    pub now: Option<String>,
    pub nodes: Vec<RouteInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteInfo {
    pub name: String,
    /// Batch delay test in ms; `None` for a timeout or DIRECT.
    pub delay: Option<u32>,
    /// Cooling down after an Apple refusal, or briefly unreachable.
    pub resting: bool,
}

/// Result of the UI's "test the Clash route" button, same shape as the
/// desktop `ClashRouteCheck`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClashRouteCheck {
    version: String,
    #[serde(flatten)]
    group: GroupNodes,
    /// The proxy port accepts connections, checked from the monitor's
    /// network namespace (the sidecar's may differ).
    port_open: bool,
    /// Nodes (DIRECT excluded) that timed out in the batch delay test.
    timeout_count: usize,
}

/// Tests unsaved Clash settings: talks to the controller and the proxy port
/// only, never to Apple.
pub async fn test_route(clash: ClashSettings) -> Result<ClashRouteCheck, String> {
    // `NetworkSettings::normalize` is private; this adds the `http://` the UI's
    // `127.0.0.1:9097` placeholder leaves out, like the desktop command does.
    let mut settings = Settings {
        network: NetworkSettings {
            mode: NetworkMode::Clash,
            clash,
            ..NetworkSettings::default()
        },
        ..Settings::default()
    };
    settings.normalize();
    let clash = settings.network.clash;
    let controller = ClashController::new(&clash)?;
    let version = controller.version().await?;
    let group = controller
        .group_nodes(&clash.group, &clash.node_filter)
        .await?;
    let timeout_count = match controller.group_delays(&clash.group).await {
        Ok(delays) => group
            .nodes
            .iter()
            .filter(|node| node.as_str() != "DIRECT" && !delays.contains_key(*node))
            .count(),
        Err(_) => 0,
    };
    let port_open = match clash_proxy_address(&clash) {
        Ok(address) => tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(address),
        )
        .await
        .is_ok_and(|connected| connected.is_ok()),
        Err(_) => false,
    };
    Ok(ClashRouteCheck {
        version,
        group,
        port_open,
        timeout_count,
    })
}

/// Lists the group's nodes with delays without the route lock: a pickup can
/// hold that lock for minutes, and the UI's node picker asks on every load.
pub async fn list_routes(
    network: &NetworkSettings,
) -> Result<(GroupNodes, HashMap<String, u32>), String> {
    if network.mode == NetworkMode::System {
        return Err(NOT_CLASH.into());
    }
    let clash = ClashController::new(&network.clash)?;
    let nodes = clash
        .group_nodes(&network.clash.group, &network.clash.node_filter)
        .await?;
    let delays = clash
        .group_delays(&network.clash.group)
        .await
        .unwrap_or_default();
    Ok((nodes, delays))
}

#[derive(Debug, Default)]
pub struct ClashRoute {
    network: NetworkSettings,
    /// The controller in pinned/Clash mode; the error for an invalid address.
    clash: Option<Result<ClashController, String>>,
    routes: RoutePool,
    /// Nodes are re-read from Clash once per cycle, to follow edits made there.
    routes_loaded: bool,
    /// This cycle's controller failure; the controller isn't asked again.
    route_failure: Option<String>,
    route_switches: u32,
    /// Nodes refused in a row; reset by a normal answer.
    consecutive_rejections: u32,
    /// Provider of each subscription node, for the pre-switch delay test.
    route_providers: HashMap<String, String>,
    /// Sites already reported as "every node cooling", so one cooldown
    /// isn't announced over and over.
    cooling_announced: HashSet<String>,
}

impl ClashRoute {
    pub fn set_network(&mut self, network: NetworkSettings) {
        let uses_clash = network.mode != NetworkMode::System;
        if self.network == network && self.clash.is_some() == uses_clash {
            return;
        }
        // Changing only the pinned node keeps the rotation's cooldowns.
        let pool_settings = |clash: &ClashSettings| {
            (
                clash.controller.clone(),
                clash.group.clone(),
                clash.node_filter.clone(),
            )
        };
        if pool_settings(&self.network.clash) != pool_settings(&network.clash) {
            self.routes = RoutePool::default();
            self.cooling_announced.clear();
        }
        self.clash = uses_clash.then(|| ClashController::new(&network.clash));
        self.network = network;
        self.routes_loaded = false;
        self.route_failure = None;
    }

    /// Rotating automatically; pinned mode sticks to one node.
    fn rotating(&self) -> bool {
        self.network.mode == NetworkMode::Clash && self.clash.is_some()
    }

    fn controller(&self) -> Result<ClashController, String> {
        match &self.clash {
            None => Err(NOT_CLASH.into()),
            Some(Err(error)) => Err(error.clone()),
            Some(Ok(clash)) => Ok(clash.clone()),
        }
    }

    /// Re-reads the group from Clash and takes Clash's selection as current.
    async fn reload_routes(&mut self, clash: &ClashController) -> Result<(), String> {
        let group = self.network.clash.group.clone();
        let nodes = clash
            .group_nodes(&group, &self.network.clash.node_filter)
            .await?;
        if nodes.nodes.is_empty() {
            return Err(format!(
                "the Clash group \"{group}\" has no node usable for queries"
            ));
        }
        self.route_providers = nodes.providers;
        self.routes.set_routes(nodes.nodes);
        self.routes.set_active(nodes.now);
        self.routes_loaded = true;
        Ok(())
    }

    /// Picks the exit before a real request: switches the node when needed
    /// and closes the group's old connections, then returns the exit proxy.
    pub async fn prepare(&mut self, region: &'static Region) -> Result<String, ApiError> {
        let clash = self.controller().map_err(ApiError::Transport)?;
        if let Some(failure) = &self.route_failure {
            return Err(ApiError::Transport(failure.clone()));
        }
        if !self.routes_loaded
            && let Err(error) = self.reload_routes(&clash).await
        {
            let failure = format!("Clash route unavailable: {error}");
            self.route_failure = Some(failure.clone());
            return Err(ApiError::Transport(failure));
        }
        if self.network.mode == NetworkMode::Pinned {
            self.apply_pinned_route(&clash).await?;
        } else {
            self.rotate(&clash, region).await?;
        }
        clash_proxy_url(&self.network.clash, self.routes.active()).map_err(ApiError::Transport)
    }

    async fn rotate(
        &mut self,
        clash: &ClashController,
        region: &'static Region,
    ) -> Result<(), ApiError> {
        let group = self.network.clash.group.clone();
        let site = region_site(region);
        // Delay-test before switching: a node that doesn't answer is skipped
        // (and rested briefly) instead of wasting an Apple request on it.
        let chosen = loop {
            let now = Instant::now();
            let Some(candidate) = self.routes.choose(&site, now).map(str::to_string) else {
                let remaining = self.routes.wait_for(&site, now).unwrap_or_default();
                return Err(ApiError::CoolingDown {
                    remaining_seconds: remaining.as_secs()
                        + u64::from(remaining.subsec_nanos() > 0),
                    detail: format!(
                        "all {} nodes of the Clash group \"{group}\" were refused by Apple or are unreachable; waiting",
                        self.routes.routes().len()
                    ),
                    newly_started: self.cooling_announced.insert(site),
                });
            };
            if self.routes.active() == Some(candidate.as_str()) {
                return Ok(());
            }
            if self.consecutive_rejections >= MAX_CONSECUTIVE_REJECTIONS {
                return Err(ApiError::Blocked(format!(
                    "Apple refused {MAX_CONSECUTIVE_REJECTIONS} nodes in a row, probably not an IP \
                     problem; no more switching this cycle"
                )));
            }
            // DIRECT can't reach the delay-test URL from mainland China anyway.
            if candidate == "DIRECT" {
                break candidate;
            }
            let provider = self.route_providers.get(&candidate).map(String::as_str);
            match clash.node_delay(&candidate, provider).await {
                Ok(delay) => {
                    eprintln!("[clash] node {candidate} delay {delay} ms");
                    break candidate;
                }
                Err(error) => {
                    eprintln!("[clash] node {candidate} skipped: {error}");
                    self.routes.mark_unreachable(&candidate, Instant::now());
                }
            }
        };
        clash.select(&group, &chosen).await.map_err(|error| {
            ApiError::Transport(format!("switching the Clash node failed: {error}"))
        })?;
        match clash.close_group_connections(&group).await {
            Ok(closed) => {
                eprintln!("[clash] switched to node {chosen}, closed {closed} old connection(s)")
            }
            Err(error) => {
                eprintln!(
                    "[clash] switched to node {chosen}, but closing old connections failed: {error}"
                )
            }
        }
        self.routes.set_active(Some(chosen));
        self.route_switches = self.route_switches.saturating_add(1);
        Ok(())
    }

    /// Pinned mode: make the group use the chosen node; no delay test, no
    /// automatic switching.
    async fn apply_pinned_route(&mut self, clash: &ClashController) -> Result<(), ApiError> {
        let node = self.network.clash.pinned_node.clone();
        if self.routes.active() == Some(node.as_str()) {
            return Ok(());
        }
        if !self.routes.routes().contains(&node) {
            let failure = format!(
                "the pinned node \"{node}\" isn't in the Clash group, or the node filter excludes it"
            );
            self.route_failure = Some(failure.clone());
            return Err(ApiError::Transport(failure));
        }
        let group = self.network.clash.group.clone();
        clash.select(&group, &node).await.map_err(|error| {
            ApiError::Transport(format!("switching the Clash node failed: {error}"))
        })?;
        if let Err(error) = clash.close_group_connections(&group).await {
            eprintln!("[clash] closing old connections after pinning failed: {error}");
        }
        eprintln!("[clash] Apple queries use the pinned node {node}");
        self.routes.set_active(Some(node));
        Ok(())
    }

    /// The UI picked a node: switch now and lift its cooldown; automatic
    /// switching carries on. Returns its delay test (`None` for DIRECT or a
    /// failed test, which still switches).
    // ponytail: runs under the route lock like the desktop, so it waits for an
    // in-flight pickup's switch-and-retry loop to finish first.
    pub async fn select_manually(&mut self, node: &str) -> Result<Option<u32>, String> {
        let clash = self.controller()?;
        if !self.routes.routes().iter().any(|route| route == node) {
            self.reload_routes(&clash).await?;
        }
        if !self.routes.routes().iter().any(|route| route == node) {
            return Err(format!(
                "the node \"{node}\" isn't in the Clash group, or the node filter excludes it"
            ));
        }
        let delay = if node == "DIRECT" {
            None
        } else {
            let provider = self.route_providers.get(node).map(String::as_str);
            clash.node_delay(node, provider).await.ok()
        };
        let group = self.network.clash.group.clone();
        clash
            .select(&group, node)
            .await
            .map_err(|error| format!("switching the Clash node failed: {error}"))?;
        if let Err(error) = clash.close_group_connections(&group).await {
            eprintln!("[clash] closing old connections after a manual switch failed: {error}");
        }
        self.routes.clear_route(node);
        self.routes.set_active(Some(node.to_string()));
        self.consecutive_rejections = 0;
        self.cooling_announced.clear();
        self.route_failure = None;
        eprintln!("[clash] manually switched to node {node}");
        Ok(delay)
    }

    /// Apple refused the current node. Returns whether another node can be
    /// tried right away.
    pub fn rejected(&mut self, region: &'static Region) -> bool {
        if !self.rotating() {
            return false;
        }
        let Some(active) = self.routes.active().map(str::to_string) else {
            return false;
        };
        let site = region_site(region);
        let cooldown = self.routes.block(&active, &site, Instant::now());
        self.consecutive_rejections = self.consecutive_rejections.saturating_add(1);
        eprintln!(
            "[clash] Apple refused node {active} on {site}; cooling it for {} min",
            cooldown.as_secs().div_ceil(60)
        );
        self.can_switch(region)
    }

    /// The current node can't carry traffic. Returns whether another node can
    /// be tried right away. Doesn't count as a refusal.
    pub fn unreachable(&mut self, region: &'static Region) -> bool {
        if !self.rotating() {
            return false;
        }
        let Some(active) = self.routes.active().map(str::to_string) else {
            return false;
        };
        self.routes.mark_unreachable(&active, Instant::now());
        eprintln!("[clash] node {active} unreachable; skipping it for now");
        self.can_switch(region)
    }

    fn can_switch(&self, region: &'static Region) -> bool {
        self.rotating()
            && self.consecutive_rejections < MAX_CONSECUTIVE_REJECTIONS
            && self
                .routes
                .choose(&region_site(region), Instant::now())
                .is_some()
    }

    /// Apple answered through the current node.
    pub fn succeeded(&mut self, region: &'static Region) {
        if !self.rotating() {
            return;
        }
        let site = region_site(region);
        if let Some(active) = self.routes.active().map(str::to_string) {
            self.routes.succeed(&active, &site);
        }
        self.consecutive_rejections = 0;
        self.cooling_announced.remove(&site);
    }

    /// Each retry rests a node, so the node count bounds a request's retries;
    /// doubled in case short rests expire within one request.
    pub fn max_attempts(&self) -> usize {
        self.routes.routes().len().max(1) * 2
    }

    /// Replaces "the sidecar can't reach the Clash port" with what to check.
    pub fn explain(&self, error: ApiError) -> ApiError {
        match &error {
            ApiError::Transport(detail)
                if self.clash.is_some()
                    && CONFIG_ERRORS.iter().any(|code| detail.contains(code)) =>
            {
                let address = clash_proxy_address(&self.network.clash).unwrap_or_default();
                ApiError::Transport(format!(
                    "the browser can't reach the Clash proxy port {address}: check that Clash is \
                     running, listens on 0.0.0.0 (allow-lan) and that the controller host resolves \
                     from the sidecar ({detail})"
                ))
            }
            _ => error,
        }
    }

    pub fn is_resting(&self, node: &str) -> bool {
        self.routes.is_resting(node, Instant::now())
    }

    /// Clash mode only: when every site has no usable node, wait for the
    /// earliest one to come back before the next cycle.
    pub fn schedule_hint(&self, locales: &[String]) -> ScheduleHint {
        if self.network.mode != NetworkMode::Clash {
            return ScheduleHint::default();
        }
        let now = Instant::now();
        let sites: BTreeSet<String> = locales
            .iter()
            .filter_map(|locale| region_by_locale(locale))
            .map(region_site)
            .collect();
        let waits: Vec<Option<Duration>> = sites
            .iter()
            .map(|site| self.routes.wait_for(site, now))
            .collect();
        let cooling = waits.iter().any(Option::is_some);
        let delay = if !waits.is_empty() && waits.iter().all(Option::is_some) {
            waits.into_iter().flatten().min().unwrap_or_default()
        } else {
            Duration::ZERO
        };
        ScheduleHint { delay, cooling }
    }

    /// The cycle's route line. Same Chinese text as the desktop: upstream's
    /// node picker parses `Clash 节点 {node}` out of it to follow switches.
    pub fn summary(&self) -> Option<String> {
        match self.network.mode {
            NetworkMode::System => None,
            NetworkMode::Pinned => Some(format!("指定节点 {}", self.network.clash.pinned_node)),
            NetworkMode::Clash => Some(if let Some(failure) = &self.route_failure {
                failure.clone()
            } else {
                let node = self.routes.active().unwrap_or("尚未选定");
                if self.route_switches > 0 {
                    format!("Clash 节点 {node}（本轮切换 {} 次）", self.route_switches)
                } else {
                    format!("Clash 节点 {node}")
                }
            }),
        }
    }

    pub fn begin_cycle(&mut self) {
        self.routes_loaded = false;
        self.route_failure = None;
        self.route_switches = 0;
        self.consecutive_rejections = 0;
    }

    pub fn retry_now(&mut self) {
        self.routes.release_waits(Instant::now());
        self.route_failure = None;
        self.cooling_announced.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use apw_core::apple::{Fetcher, StoreAvailability};
    use apw_core::model::{DeliveryRegion, Target};
    use axum::extract::{Path, State};
    use axum::routing::{delete, get, post, put};
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::*;
    use crate::camoufox_fetcher::CamoufoxFetcher;

    const PICKUP: &str = r#"{"head":{"status":"200"},"body":{"content":{"pickupMessage":{"stores":[
        {"storeNumber":"R390","storeName":"x","partsAvailability":{
            "A/A":{"partNumber":"A/A","pickupDisplay":"available"}}}]}}}}"#;

    /// One fake for both the Clash controller and the sidecar, so the
    /// sidecar can tell which node Clash had selected when a request left.
    #[derive(Default)]
    struct Fake {
        now: String,
        log: Vec<String>,
        refuse: Vec<&'static str>,
        dead: Vec<&'static str>,
    }

    type Shared = Arc<Mutex<Fake>>;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn clash(fake: Shared) -> Router {
        Router::new()
            .route(
                "/version",
                get(|| async { Json(json!({"version": "fake"})) }),
            )
            .route(
                "/proxies",
                get(|State(fake): State<Shared>| async move {
                    let now = fake.lock().unwrap().now.clone();
                    Json(json!({"proxies": {
                        "G": {"type": "Selector", "all": ["A", "B"], "now": now},
                        "A": {"type": "Shadowsocks"},
                        "B": {"type": "Shadowsocks"},
                    }}))
                }),
            )
            .route(
                "/proxies/{group}",
                put(
                    |State(fake): State<Shared>,
                     Path(group): Path<String>,
                     Json(body): Json<Value>| async move {
                        let node = body["name"].as_str().unwrap().to_string();
                        let mut fake = fake.lock().unwrap();
                        fake.log.push(format!("select {group} {node}"));
                        fake.now = node;
                    },
                ),
            )
            .route(
                "/proxies/{node}/delay",
                get(|| async { Json(json!({"delay": 42})) }),
            )
            .route(
                "/providers/proxies",
                get(|| async { Json(json!({"providers": {}})) }),
            )
            .route(
                "/group/{group}/delay",
                get(|| async { Json(json!({"A": 40})) }),
            )
            .route(
                "/connections",
                get(|| async { Json(json!({"connections": [{"id": "c1", "chains": ["G"]}]})) }),
            )
            .route(
                "/connections/{id}",
                delete(
                    |State(fake): State<Shared>, Path(id): Path<String>| async move {
                        fake.lock().unwrap().log.push(format!("close {id}"));
                    },
                ),
            )
            .with_state(fake)
    }

    fn sidecar(fake: Shared) -> Router {
        Router::new()
            .route(
                "/pickup",
                post(
                    |State(fake): State<Shared>, Json(request): Json<Value>| async move {
                        let proxy = request["proxy"].as_str().unwrap_or_default().to_string();
                        let mut fake = fake.lock().unwrap();
                        let entry = format!("fetch {proxy} (clash on {})", fake.now);
                        fake.log.push(entry);
                        let node = fake.now.clone();
                        Json(if fake.dead.contains(&node.as_str()) {
                            json!({"error": "Page.goto: NS_ERROR_NET_INTERRUPT\nCall log: ..."})
                        } else if fake.refuse.contains(&node.as_str()) {
                            json!({"status": 541, "body": ""})
                        } else {
                            json!({"status": 200, "body": PICKUP})
                        })
                    },
                ),
            )
            .with_state(fake)
    }

    async fn setup(mode: NetworkMode, fake: &Shared) -> CamoufoxFetcher {
        let controller = serve(clash(fake.clone())).await;
        let fetcher = CamoufoxFetcher::new(serve(sidecar(fake.clone())).await).unwrap();
        fetcher.set_network(NetworkSettings {
            mode,
            clash: ClashSettings {
                controller,
                group: "G".into(),
                pinned_node: "B".into(),
                ..ClashSettings::default()
            },
            proxies: Vec::new(),
        });
        fetcher
    }

    fn fake(now: &str, refuse: Vec<&'static str>, dead: Vec<&'static str>) -> Shared {
        Arc::new(Mutex::new(Fake {
            now: now.into(),
            refuse,
            dead,
            ..Fake::default()
        }))
    }

    fn take_log(fake: &Shared) -> Vec<String> {
        std::mem::take(&mut fake.lock().unwrap().log)
    }

    fn target() -> Target {
        serde_json::from_value(json!({
            "locale": "zh_CN", "storeNumber": "R390", "storeTitle": "",
            "partNumber": "A/A", "productName": ""
        }))
        .unwrap()
    }

    async fn pickup(
        fetcher: &CamoufoxFetcher,
        delivery: Option<&DeliveryRegion>,
    ) -> Result<StoreAvailability, ApiError> {
        let region = region_by_locale("zh_CN").unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            fetcher.pickup_message(region, "R390", &[target()], delivery),
        )
        .await
        .expect("pickup hung (route lock taken twice?)")
    }

    const EXIT: &str = "http://127.0.0.1:7899";

    #[tokio::test]
    async fn a_refused_node_is_swapped_and_delivery_follows_it() {
        let fake = fake("A", vec!["A"], vec![]);
        let fetcher = setup(NetworkMode::Clash, &fake).await;
        let shanghai = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        pickup(&fetcher, Some(&shanghai)).await.unwrap();
        assert_eq!(
            take_log(&fake),
            [
                format!("fetch {EXIT}#A (clash on A)"),
                "select G B".into(),
                "close c1".into(),
                format!("fetch {EXIT}#B (clash on B)"),
                // The delivery quote, through the node that just answered.
                format!("fetch {EXIT}#B (clash on B)"),
            ]
        );
        assert_eq!(
            fetcher.cycle_stats().await.route.as_deref(),
            Some("Clash 节点 B（本轮切换 1 次）")
        );

        // The next query sticks to B.
        pickup(&fetcher, None).await.unwrap();
        assert_eq!(take_log(&fake), [format!("fetch {EXIT}#B (clash on B)")]);

        let list = fetcher.list_routes().await.unwrap();
        let resting: Vec<(&str, bool, Option<u32>)> = list
            .nodes
            .iter()
            .map(|node| (node.name.as_str(), node.resting, node.delay))
            .collect();
        assert_eq!(resting, [("A", true, Some(40)), ("B", false, None)]);
    }

    #[tokio::test]
    async fn every_node_refused_means_cooling_down_until_retry_or_manual_pick() {
        let fake = fake("A", vec!["A", "B"], vec![]);
        let fetcher = setup(NetworkMode::Clash, &fake).await;
        let error = pickup(&fetcher, None).await.unwrap_err();
        assert!(matches!(error, ApiError::Blocked(_)), "{error:?}");

        let error = pickup(&fetcher, None).await.unwrap_err();
        assert!(
            matches!(
                error,
                ApiError::CoolingDown {
                    newly_started: true,
                    ..
                }
            ),
            "{error:?}"
        );
        let error = pickup(&fetcher, None).await.unwrap_err();
        assert!(
            matches!(
                error,
                ApiError::CoolingDown {
                    newly_started: false,
                    ..
                }
            ),
            "{error:?}"
        );
        let hint = fetcher.schedule_hint(&["zh_CN".into()]).await;
        assert!(
            hint.cooling && hint.delay > Duration::from_secs(60),
            "{hint:?}"
        );

        // "Retry now" ends the wait (Apple decides again on the next query).
        fetcher.retry_now().await;
        let hint = fetcher.schedule_hint(&["zh_CN".into()]).await;
        assert!(!hint.cooling && hint.delay.is_zero(), "{hint:?}");

        // Picking a node by hand switches to it right away.
        take_log(&fake);
        assert_eq!(fetcher.select_route("A").await, Ok(Some(42)));
        assert_eq!(take_log(&fake), ["select G A", "close c1"]);
        fake.lock().unwrap().refuse.clear();
        pickup(&fetcher, None).await.unwrap();
        assert_eq!(take_log(&fake), [format!("fetch {EXIT}#A (clash on A)")]);
    }

    #[tokio::test]
    async fn a_dead_node_is_skipped_without_counting_as_a_refusal() {
        let fake = fake("A", vec![], vec!["A"]);
        let fetcher = setup(NetworkMode::Clash, &fake).await;
        pickup(&fetcher, None).await.unwrap();
        assert_eq!(
            take_log(&fake),
            [
                format!("fetch {EXIT}#A (clash on A)"),
                "select G B".into(),
                "close c1".into(),
                format!("fetch {EXIT}#B (clash on B)"),
            ]
        );
    }

    #[tokio::test]
    async fn pinned_mode_selects_the_pinned_node_and_never_rotates() {
        let fake = fake("A", vec!["B"], vec![]);
        let fetcher = setup(NetworkMode::Pinned, &fake).await;
        let error = pickup(&fetcher, None).await.unwrap_err();
        assert!(matches!(error, ApiError::Blocked(_)), "{error:?}");
        assert_eq!(
            take_log(&fake),
            [
                "select G B".to_string(),
                "close c1".into(),
                format!("fetch {EXIT}#B (clash on B)"),
            ]
        );
        assert_eq!(
            fetcher.cycle_stats().await.route.as_deref(),
            Some("指定节点 B")
        );
    }

    #[tokio::test]
    async fn system_mode_never_touches_clash() {
        let fake = fake("A", vec![], vec![]);
        let fetcher = setup(NetworkMode::System, &fake).await;
        pickup(&fetcher, None).await.unwrap();
        assert_eq!(take_log(&fake), ["fetch  (clash on A)"]);
        assert_eq!(fetcher.cycle_stats().await.route, None);
        assert!(fetcher.list_routes().await.is_err());
    }

    #[tokio::test]
    async fn route_test_normalises_the_draft_and_checks_the_port() {
        let fake = fake("A", vec![], vec![]);
        let controller = serve(clash(fake)).await;
        let port = controller.rsplit_once(':').unwrap().1.parse().unwrap();
        let check = test_route(ClashSettings {
            // As the UI's placeholder suggests: no scheme.
            controller: controller.trim_start_matches("http://").into(),
            group: "G".into(),
            // Any open port will do for the reachability check.
            proxy_port: port,
            ..ClashSettings::default()
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(check).unwrap(),
            json!({"version": "fake", "now": "A", "nodes": ["A", "B"], "skipped": 0,
                   "portOpen": true, "timeoutCount": 1})
        );
    }

    #[test]
    fn exit_proxy_comes_from_the_controller_host() {
        let url = |controller: &str, port: u16, node: Option<&str>| {
            clash_proxy_url(
                &ClashSettings {
                    controller: controller.into(),
                    proxy_port: port,
                    ..ClashSettings::default()
                },
                node,
            )
        };
        assert_eq!(
            url("http://mihomo:9090", 7899, Some("香港 01")),
            Ok("http://mihomo:7899#香港 01".into())
        );
        assert_eq!(
            url("http://10.0.0.2:9097/", 7899, None),
            Ok("http://10.0.0.2:7899".into())
        );
        assert_eq!(
            url("http://[fd00::1]:9097", 80, None),
            Ok("http://[fd00::1]:80".into())
        );
        assert!(url("http://mihomo:9090", 0, None).is_err());
        assert!(url("not a url", 7899, None).is_err());
    }

    #[test]
    fn node_failures_are_told_apart_from_setup_errors() {
        let transport = |detail: &str| ApiError::Transport(detail.into());
        for failure in [
            "camoufox sidecar: Page.goto: NS_ERROR_NET_INTERRUPT\nCall log: ...",
            "camoufox sidecar: Page.goto: Timeout 50000ms exceeded.",
            "in-page fetch failed: TypeError: NetworkError when attempting to fetch resource.",
        ] {
            assert!(is_node_failure(&transport(failure)), "{failure}");
        }
        for other in [
            "camoufox sidecar: Page.goto: NS_ERROR_PROXY_CONNECTION_REFUSED",
            "camoufox sidecar: Page.goto: NS_ERROR_UNKNOWN_PROXY_HOST",
            "camoufox sidecar unreachable: connection refused",
            "camoufox sidecar: page evaluation failed: Target closed",
            "Apple service temporarily unavailable: HTTP 503",
        ] {
            assert!(!is_node_failure(&transport(other)), "{other}");
        }
        assert!(!is_node_failure(&ApiError::Blocked("HTTP 541".into())));

        let mut route = ClashRoute::default();
        route.set_network(NetworkSettings {
            mode: NetworkMode::Clash,
            clash: ClashSettings {
                controller: "http://mihomo:9090".into(),
                ..ClashSettings::default()
            },
            ..NetworkSettings::default()
        });
        let explained = route.explain(transport("Page.goto: NS_ERROR_PROXY_CONNECTION_REFUSED"));
        assert!(
            matches!(&explained, ApiError::Transport(d) if d.contains("mihomo:7899") && d.contains("0.0.0.0")),
            "{explained:?}"
        );
    }
}
