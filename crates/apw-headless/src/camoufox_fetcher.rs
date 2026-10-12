//! Fetches Apple pickup availability through a local Camoufox sidecar.
//!
//! Apple's product pages run a bot-check script (`shop/shld/v2_1/verify.js`)
//! before its pickup endpoint accepts requests. A bare HTTP
//! client cannot pass that check, and a Cloudflare Worker cannot run it
//! either (no DOM). This fetcher keeps the real browser session out of the
//! Rust process entirely: it hands the sidecar a plain "warm this page, then
//! fetch this URL" instruction over a local HTTP call, and the sidecar (a
//! long-lived Camoufox/Playwright process) is the only thing that ever
//! touches a browser.
//!
//! Every piece of Apple-specific knowledge (URLs, query parameters, which
//! page to warm up on) stays in `apw-core` and is filled in here, in Rust.
//! The sidecar is intentionally dumb: it does not know what Apple, a store,
//! or a part number is.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apw_core::apple::{ApiError, DeliveryInfo, Fetcher, StoreAvailability, parse_pickup_message};
use apw_core::model::{DeliveryRegion, Region, Target};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The sidecar may need to load a fresh page and wait out Apple's challenge,
/// which the desktop app allows up to 50s for. Give the round trip room for
/// that plus the sidecar's own throttling.
const SIDECAR_TIMEOUT_SECS: u64 = 65;
/// Same lifetimes as the desktop fetcher: a real delivery quote is reused
/// for an hour, an empty answer is retried after five minutes.
const DELIVERY_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const EMPTY_DELIVERY_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The page the sidecar loads (and waits on until `document.readyState` is
/// `"complete"`) before running the real fetch from inside it.
///
/// No cookie-name check: an earlier version waited for `shld_bt_ck` and
/// `as_atb`, but those only reliably appear on the mainland China site.
/// Upstream's desktop app dropped the same check for the same reason
/// (`海外会话就绪不依赖中国区cookie名称` in its `chromium_fetcher.rs` tests).
#[derive(Serialize)]
struct WarmSpec {
    url: String,
}

#[derive(Serialize)]
struct FetchSpec {
    url: String,
    headers: std::collections::BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
struct PickupRequest<'a> {
    locale: &'a str,
    warm: WarmSpec,
    fetch: FetchSpec,
    /// The sidecar normally drops the page after a 403/541 so the next
    /// pickup query starts a fresh session. Delivery requests set this: the
    /// fulfillment endpoint can refuse on its own while pickup still works,
    /// and that must not cost the pickup session.
    keep_session_on_block: bool,
}

#[derive(Deserialize)]
struct PickupResponse {
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Talks to the Camoufox sidecar instead of driving a browser in-process.
#[derive(Debug, Clone)]
pub struct CamoufoxFetcher {
    http: reqwest::Client,
    sidecar_url: String,
    delivery: Arc<Mutex<DeliveryState>>,
}

/// Delivery quotes are cached per target and address, like the desktop
/// fetcher's `DeliveryCacheKey`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeliveryKey {
    locale: String,
    part: String,
    kit: Option<String>,
    companion: Option<String>,
    location: DeliveryRegion,
}

impl DeliveryKey {
    fn new(target: &Target, location: &DeliveryRegion) -> Self {
        Self {
            locale: target.locale.clone(),
            part: target.part_number.clone(),
            kit: target.kit_part.clone(),
            companion: target.companion_part.clone(),
            location: location.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedDelivery {
    fetched_at: Instant,
    info: DeliveryInfo,
}

/// Port of the delivery half of the desktop `QueryState`
/// (`src-tauri/src/chromium_fetcher.rs`, upstream v1.0.19); keep in sync by
/// hand when upstream changes it. Same rules:
/// - delivery is extra information; no outcome here changes a pickup result
/// - each target+address is tried at most once per cycle, failures included
/// - after the fulfillment endpoint refuses (403/541/429) a region, it isn't
///   asked again until the next cycle, and pickup queries carry on
/// - an empty answer never overwrites an earlier real quote
#[derive(Debug, Default)]
struct DeliveryState {
    cache: HashMap<DeliveryKey, CachedDelivery>,
    empty_cache: HashMap<DeliveryKey, CachedDelivery>,
    attempts: HashSet<DeliveryKey>,
    rejected_regions: HashSet<&'static str>,
}

impl DeliveryState {
    fn begin_cycle(&mut self) {
        self.attempts.clear();
        self.rejected_regions.clear();
        self.empty_cache
            .retain(|_, entry| entry.fetched_at.elapsed() < EMPTY_DELIVERY_CACHE_TTL);
    }

    /// An expired real quote is still shown until a refresh replaces it.
    fn cached(&self, target: &Target, location: &DeliveryRegion) -> Option<DeliveryInfo> {
        let key = DeliveryKey::new(target, location);
        self.cache
            .get(&key)
            .or_else(|| {
                self.empty_cache
                    .get(&key)
                    .filter(|entry| entry.fetched_at.elapsed() < EMPTY_DELIVERY_CACHE_TTL)
            })
            .map(|entry| entry.info.clone())
    }

    /// Not tried yet this cycle, and no cached answer still within its TTL.
    fn needs(&self, target: &Target, location: &DeliveryRegion) -> bool {
        let key = DeliveryKey::new(target, location);
        let expired = |cache: &HashMap<DeliveryKey, CachedDelivery>, ttl: Duration| {
            cache
                .get(&key)
                .is_none_or(|entry| entry.fetched_at.elapsed() >= ttl)
        };
        !self.attempts.contains(&key)
            && expired(&self.cache, DELIVERY_CACHE_TTL)
            && expired(&self.empty_cache, EMPTY_DELIVERY_CACHE_TTL)
    }

    fn store(&mut self, target: &Target, location: &DeliveryRegion, info: DeliveryInfo) {
        let key = DeliveryKey::new(target, location);
        let entry = CachedDelivery {
            fetched_at: Instant::now(),
            info,
        };
        if entry
            .info
            .sale_message
            .as_deref()
            .is_some_and(|message| !message.trim().is_empty())
        {
            self.empty_cache.remove(&key);
            self.cache.insert(key, entry);
        } else {
            self.empty_cache.insert(key, entry);
        }
    }

    fn reject_on(&mut self, region: &'static Region, error: &ApiError) {
        if matches!(error, ApiError::Blocked(_) | ApiError::RateLimited(_)) {
            self.rejected_regions.insert(region.locale);
        }
    }
}

impl CamoufoxFetcher {
    /// `sidecar_url` is the sidecar's base address, e.g. `http://127.0.0.1:8787`.
    pub fn new(sidecar_url: String) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(SIDECAR_TIMEOUT_SECS))
            .build()
            .map_err(|e| ApiError::Transport(format!("failed to build sidecar client: {e}")))?;
        Ok(Self {
            http,
            sidecar_url,
            delivery: Arc::default(),
        })
    }

    fn delivery_state(&self) -> std::sync::MutexGuard<'_, DeliveryState> {
        self.delivery.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One round trip: the sidecar makes sure the region's session page is
    /// loaded, then runs `fetch(url)` inside it. Returns Apple's status and
    /// body; sidecar-side failures become `Transport`.
    async fn sidecar_fetch(
        &self,
        region: &'static Region,
        url: String,
        keep_session_on_block: bool,
    ) -> Result<(u16, String), ApiError> {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Accept",
            "application/json, text/javascript, */*; q=0.01".to_string(),
        );
        headers.insert("X-Requested-With", "XMLHttpRequest".to_string());
        let request = PickupRequest {
            locale: region.locale,
            // Upstream switched the warm-up page from a buy page to the region
            // home page (mainland China keeps a buy page): Australia closes
            // `/shop/buy-*` connections for automated browsers, while the
            // home page establishes the same same-origin session.
            warm: WarmSpec {
                url: region.session_page_url(),
            },
            fetch: FetchSpec { url, headers },
            keep_session_on_block,
        };

        let response = self
            .http
            .post(format!("{}/pickup", self.sidecar_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| ApiError::Transport(format!("camoufox sidecar unreachable: {e}")))?;
        let payload: PickupResponse = response.json().await.map_err(|e| {
            ApiError::Transport(format!("camoufox sidecar returned invalid JSON: {e}"))
        })?;
        if let Some(error) = payload.error {
            return Err(ApiError::Transport(format!("camoufox sidecar: {error}")));
        }
        let status = payload
            .status
            .ok_or_else(|| ApiError::Transport("camoufox sidecar omitted status".into()))?;
        Ok((status, payload.body.unwrap_or_default()))
    }

    /// Fetches the delivery quotes this store's targets still need for
    /// `location`, into the cache the watcher reads through
    /// `cached_delivery`. Never fails: errors are logged and, if Apple
    /// refused, the region's delivery lookups pause for the rest of the cycle.
    async fn refresh_delivery(
        &self,
        region: &'static Region,
        targets: &[Target],
        location: &DeliveryRegion,
    ) {
        // Regular products: one request for every part still due. Mark them
        // attempted before sending, so concurrent store queries in the same
        // cycle don't ask again for the same part.
        let due: Vec<&Target> = {
            let mut state = self.delivery_state();
            if state.rejected_regions.contains(region.locale) {
                return;
            }
            let due: Vec<&Target> = targets
                .iter()
                .filter(|t| t.kit_part.is_none() && state.needs(t, location))
                .collect();
            for target in &due {
                state.attempts.insert(DeliveryKey::new(target, location));
            }
            due
        };
        let mut parts: Vec<String> = due.iter().map(|t| t.part_number.clone()).collect();
        parts.sort_unstable();
        parts.dedup();
        if !parts.is_empty() {
            let result = self.fetch_product_delivery(region, &parts, location).await;
            let mut state = self.delivery_state();
            match result {
                Ok(quotes) => {
                    for target in &due {
                        let info = quotes.get(&target.part_number).cloned().unwrap_or_default();
                        state.store(target, location, info);
                    }
                }
                Err(error) => {
                    eprintln!("[delivery] {} product quote failed: {error}", region.locale);
                    state.reject_on(region, &error);
                }
            }
        }

        // Watch case + band combinations: one request each.
        for target in targets.iter().filter(|t| t.kit_part.is_some()) {
            {
                let mut state = self.delivery_state();
                if state.rejected_regions.contains(region.locale) {
                    return;
                }
                if !state.needs(target, location) {
                    continue;
                }
                state.attempts.insert(DeliveryKey::new(target, location));
            }
            let result = self.fetch_watch_delivery(region, target, location).await;
            let mut state = self.delivery_state();
            match result {
                Ok(message) => state.store(
                    target,
                    location,
                    DeliveryInfo {
                        sale_reason: None,
                        sale_message: message,
                    },
                ),
                Err(error) => {
                    eprintln!(
                        "[delivery] {} Watch quote for {} failed: {error}",
                        region.locale, target.part_number
                    );
                    state.reject_on(region, &error);
                }
            }
        }
    }

    async fn fetch_product_delivery(
        &self,
        region: &'static Region,
        parts: &[String],
        location: &DeliveryRegion,
    ) -> Result<BTreeMap<String, DeliveryInfo>, ApiError> {
        let url = url_with_pairs(
            &region.delivery_message_url(),
            &delivery_query_pairs(parts, location),
        );
        let (status, body) = self.sidecar_fetch(region, url, true).await?;
        match status {
            200 => parse_product_delivery(body.as_bytes()),
            403 | 541 => Err(ApiError::Blocked(format!("HTTP {status}"))),
            429 => Err(ApiError::RateLimited("HTTP 429".into())),
            0 => Err(ApiError::Transport(format!("in-page fetch failed: {body}"))),
            s => Err(ApiError::Transport(format!("HTTP {s}"))),
        }
    }

    /// `Ok(None)` without a request when the target lacks its kit or band
    /// part (the desktop app caches that as an empty answer too).
    async fn fetch_watch_delivery(
        &self,
        region: &'static Region,
        target: &Target,
        location: &DeliveryRegion,
    ) -> Result<Option<String>, ApiError> {
        let non_empty = |part: &Option<String>| {
            part.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let (Some(kit), Some(companion)) = (
            non_empty(&target.kit_part),
            non_empty(&target.companion_part),
        ) else {
            return Ok(None);
        };
        let pairs = [
            ("fae", "true".to_string()),
            ("pl", "true".to_string()),
            ("fts", "true".to_string()),
            ("mts.0", "expanded".to_string()),
            ("parts.0", kit),
            ("option.0", format!("{},{companion}", target.part_number)),
            ("state", location.state.clone()),
            ("city", location.city.clone()),
            ("district", location.district.clone()),
        ];
        let url = url_with_pairs(&region.delivery_message_url(), &pairs);
        let (status, body) = self.sidecar_fetch(region, url, true).await?;
        match status {
            200 => parse_delivery_display_name(body.as_bytes()),
            403 | 541 => Err(ApiError::Blocked(format!("HTTP {status}"))),
            429 => Err(ApiError::RateLimited("HTTP 429".into())),
            // The desktop app treats a 5xx here as rate limiting (unlike the
            // product request); kept as-is.
            s if s >= 500 => Err(ApiError::RateLimited(format!("HTTP {s}"))),
            0 => Err(ApiError::Transport(format!("in-page fetch failed: {body}"))),
            s => Err(ApiError::Transport(format!("HTTP {s}"))),
        }
    }

    /// Part numbers to ask Apple about: each target's own part plus its Watch
    /// band "companion" part, sorted and deduplicated — same as upstream's
    /// desktop `pickup()`.
    fn query_parts(targets: &[Target]) -> Vec<String> {
        let mut parts: Vec<String> = targets
            .iter()
            .flat_map(|t| std::iter::once(t.part_number.clone()).chain(t.companion_part.clone()))
            .collect();
        parts.sort_unstable();
        parts.dedup();
        parts
    }

    /// Single-store query: same parameter set and order as upstream's
    /// `pickup_query_pairs` for `PickupScope::Store` (no `fae`; `store` before
    /// the parts). The endpoint itself comes from `Region::pickup_message_url`.
    fn build_pickup_url(region: &Region, store_number: &str, parts: &[String]) -> String {
        let mut url = reqwest::Url::parse(&region.pickup_message_url())
            .expect("region pickup_message_url is always a valid URL");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pl", "true");
            query.append_pair("mts.0", "regular");
            query.append_pair("store", store_number);
            for (i, part) in parts.iter().enumerate() {
                query.append_pair(&format!("parts.{i}"), part);
            }
        }
        url.into()
    }
}

impl Fetcher for CamoufoxFetcher {
    async fn begin_cycle(&self) {
        self.delivery_state().begin_cycle();
    }

    async fn cached_delivery(
        &self,
        target: &Target,
        location: Option<&DeliveryRegion>,
    ) -> Option<DeliveryInfo> {
        self.delivery_state().cached(target, location?)
    }

    async fn pickup_message(
        &self,
        region: &'static Region,
        store_number: &str,
        targets: &[Target],
        // The watcher passes this only for mainland China targets with a
        // delivery address set in the UI.
        delivery_region: Option<&DeliveryRegion>,
    ) -> Result<StoreAvailability, ApiError> {
        if store_number.is_empty() {
            return Err(ApiError::Transport("store number is empty".into()));
        }
        if targets.is_empty() {
            return Err(ApiError::Transport("part number list is empty".into()));
        }
        let parts = Self::query_parts(targets);
        let url = Self::build_pickup_url(region, store_number, &parts);
        let (status, body) = self.sidecar_fetch(region, url, false).await?;

        match status {
            200 => {
                let bytes = body.as_bytes();
                if bytes
                    .iter()
                    .find(|b| !b.is_ascii_whitespace())
                    .is_some_and(|b| *b != b'{' && *b != b'[')
                {
                    return Err(ApiError::Blocked("HTTP 200 but body is not JSON".into()));
                }
                let availability = parse_pickup_message(bytes, store_number)?;
                // Same place as the desktop app: only after pickup succeeded,
                // reusing the session that just proved to work. The watcher
                // merges the cached quotes into each row itself.
                if let Some(location) = delivery_region {
                    self.refresh_delivery(region, targets, location).await;
                }
                Ok(availability)
            }
            403 | 541 => Err(ApiError::Blocked(format!("HTTP {status}"))),
            429 => Err(ApiError::RateLimited("HTTP 429".into())),
            // The sidecar reports an in-page fetch() exception as status 0
            // with the exception text as the body.
            0 => Err(ApiError::Transport(format!("in-page fetch failed: {body}"))),
            // Upstream treats 5xx as a transient Apple-side fault, not rate
            // limiting, so it doesn't trigger the rate-limit protection path.
            s if s >= 500 => Err(ApiError::Transport(format!(
                "Apple service temporarily unavailable: HTTP {s}"
            ))),
            s => Err(ApiError::Transport(format!("HTTP {s}"))),
        }
    }
}

fn url_with_pairs<K: AsRef<str>, V: AsRef<str>>(base: &str, pairs: &[(K, V)]) -> String {
    let mut url = reqwest::Url::parse(base).expect("region endpoint URLs are always valid");
    url.query_pairs_mut().extend_pairs(pairs);
    url.into()
}

/// Desktop `delivery_query_pairs`: mainland delivery takes the separate
/// province/city/district. (`location` is the pickup search parameter; the
/// delivery endpoint may ignore it and answer with a generic ship time.)
fn delivery_query_pairs(parts: &[String], location: &DeliveryRegion) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("fae".to_string(), "true".to_string()),
        ("pl".to_string(), "true".to_string()),
        ("mts.0".to_string(), "regular".to_string()),
    ];
    pairs.extend(
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| (format!("parts.{index}"), part.clone())),
    );
    pairs.extend([
        ("state".to_string(), location.state.clone()),
        ("city".to_string(), location.city.clone()),
        ("district".to_string(), location.district.clone()),
    ]);
    pairs
}

fn parse_product_delivery(raw: &[u8]) -> Result<BTreeMap<String, DeliveryInfo>, ApiError> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| ApiError::SchemaDrift {
        field: "body.content.deliveryMessage".into(),
        raw: format!("delivery response is not JSON: {e}"),
    })?;
    if let Some(message) = value
        .pointer("/body/errorMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Err(ApiError::Apple(message.to_string()));
    }
    let Some(messages) = value
        .pointer("/body/content/deliveryMessage")
        .and_then(Value::as_object)
    else {
        return Ok(BTreeMap::new());
    };
    Ok(messages
        .iter()
        .filter_map(|(part, message)| {
            let regular = message.get("regular")?;
            let sale_reason = regular
                .pointer("/buyability/reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let sale_message = regular
                .pointer("/deliveryOptionMessages/0/displayName")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (sale_reason.is_some() || sale_message.is_some()).then(|| {
                (
                    part.clone(),
                    DeliveryInfo {
                        sale_reason,
                        sale_message,
                    },
                )
            })
        })
        .collect())
}

/// Watch responses nest the message at varying depths; take the first
/// `deliveryOptionMessages[0].displayName` anywhere under deliveryMessage.
fn parse_delivery_display_name(raw: &[u8]) -> Result<Option<String>, ApiError> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| ApiError::SchemaDrift {
        field: "body.content.deliveryMessage".into(),
        raw: format!("Watch delivery response is not JSON: {e}"),
    })?;
    fn find(value: &Value) -> Option<String> {
        if let Some(message) = value
            .get("deliveryOptionMessages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("displayName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            return Some(message.to_string());
        }
        match value {
            Value::Array(items) => items.iter().find_map(find),
            Value::Object(fields) => fields.values().find_map(find),
            _ => None,
        }
    }
    Ok(value
        .pointer("/body/content/deliveryMessage")
        .and_then(find))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apw_core::model::region_by_locale;

    #[test]
    fn single_store_query_matches_upstream_shape() {
        let watch: Target = serde_json::from_value(serde_json::json!({
            "locale": "zh_CN", "storeNumber": "R390", "storeTitle": "",
            "partNumber": "MEP24CH/B", "productName": "", "companionPart": "MXLD3FE/A"
        }))
        .unwrap();
        let phone: Target = serde_json::from_value(serde_json::json!({
            "locale": "zh_CN", "storeNumber": "R390", "storeTitle": "",
            "partNumber": "MG6X4CH/A", "productName": ""
        }))
        .unwrap();

        // Companion parts are included, and duplicates collapse.
        let parts = CamoufoxFetcher::query_parts(&[watch, phone.clone(), phone]);
        assert_eq!(parts, ["MEP24CH/B", "MG6X4CH/A", "MXLD3FE/A"]);

        let region = region_by_locale("zh_CN").unwrap();
        let url = reqwest::Url::parse(&CamoufoxFetcher::build_pickup_url(region, "R390", &parts))
            .unwrap();
        assert_eq!(
            url.as_str().split('?').next(),
            Some(region.pickup_message_url().as_str())
        );
        let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        assert_eq!(
            keys,
            ["pl", "mts.0", "store", "parts.0", "parts.1", "parts.2"]
        );
    }

    fn shanghai() -> DeliveryRegion {
        DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        }
    }

    #[test]
    fn delivery_query_matches_desktop_shape() {
        let pairs = delivery_query_pairs(&["A/A".into(), "B/A".into()], &shanghai());
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "fae", "pl", "mts.0", "parts.0", "parts.1", "state", "city", "district"
            ]
        );
        let url = url_with_pairs(
            &region_by_locale("zh_CN").unwrap().delivery_message_url(),
            &pairs,
        );
        assert!(url.contains("/shop/fulfillment-messages?fae=true&pl=true"));
        assert!(url.contains("district=%E9%BB%84%E6%B5%A6%E5%8C%BA"));
    }

    #[test]
    fn product_delivery_keeps_parts_with_a_reason_or_message() {
        let body = r#"{"body":{"content":{"deliveryMessage":{
            "A/A":{"regular":{"buyability":{"reason":"OK"},"deliveryOptionMessages":[{"displayName":"10月20日送达"}]}},
            "B/A":{"regular":{}},
            "geoLocated":true}}}}"#;
        let quotes = parse_product_delivery(body.as_bytes()).unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes["A/A"].sale_message.as_deref(), Some("10月20日送达"));
        assert_eq!(quotes["A/A"].sale_reason.as_deref(), Some("OK"));

        let error = parse_product_delivery(br#"{"body":{"errorMessage":"busy"}}"#).unwrap_err();
        assert!(matches!(error, ApiError::Apple(m) if m == "busy"));
        assert!(parse_product_delivery(b"<html>").is_err());
    }

    #[test]
    fn watch_delivery_finds_the_nested_display_name() {
        let body = r#"{"body":{"content":{"deliveryMessage":{"Z0YQ":{"expanded":[{"deliveryOptionMessages":[{"displayName":" 2-3 周 "}]}]}}}}}"#;
        assert_eq!(
            parse_delivery_display_name(body.as_bytes())
                .unwrap()
                .as_deref(),
            Some("2-3 周")
        );
        assert_eq!(
            parse_delivery_display_name(br#"{"body":{"content":{}}}"#).unwrap(),
            None
        );
    }

    #[test]
    fn delivery_cache_follows_desktop_rules() {
        let target: Target = serde_json::from_value(serde_json::json!({
            "locale": "zh_CN", "storeNumber": "R390", "storeTitle": "",
            "partNumber": "A/A", "productName": ""
        }))
        .unwrap();
        let location = shanghai();
        let region = region_by_locale("zh_CN").unwrap();
        let quote = |message: Option<&str>| DeliveryInfo {
            sale_reason: None,
            sale_message: message.map(str::to_owned),
        };
        let mut state = DeliveryState::default();
        assert!(state.needs(&target, &location));

        // A real quote is served and not re-fetched within the hour.
        state.store(&target, &location, quote(Some("明天送达")));
        assert!(!state.needs(&target, &location));
        // A later empty answer never hides it.
        state.store(&target, &location, quote(None));
        assert_eq!(
            state
                .cached(&target, &location)
                .unwrap()
                .sale_message
                .as_deref(),
            Some("明天送达")
        );

        // One attempt per cycle; a new cycle forgets attempts and refusals.
        let other = DeliveryRegion {
            district: "徐汇区".into(),
            ..shanghai()
        };
        state.attempts.insert(DeliveryKey::new(&target, &other));
        state.reject_on(region, &ApiError::Transport("timeout".into()));
        assert!(!state.rejected_regions.contains("zh_CN"));
        state.reject_on(region, &ApiError::Blocked("HTTP 541".into()));
        assert!(state.rejected_regions.contains("zh_CN"));
        assert!(!state.needs(&target, &other));
        state.begin_cycle();
        assert!(state.needs(&target, &other));
        assert!(state.rejected_regions.is_empty());
    }
}
