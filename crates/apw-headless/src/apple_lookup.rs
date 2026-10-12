//! Option lookups behind the web UI's delivery-region and Watch band pickers.
//!
//! Ported from the `list_delivery_localities` / `list_watch_band_*` Tauri
//! commands in `src-tauri/src/lib.rs` (upstream v1.0.19), behavior unchanged:
//! plain HTTPS requests with no browser session, exactly like the desktop
//! app. These are user-triggered option lists, not stock queries. Keep in
//! sync by hand when upstream changes those commands.

use std::time::Duration;

use apw_core::catalog::Catalog;
use apw_core::model::{Category, Product, region_by_locale};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryLocalityOption {
    text: String,
    value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryLocalities {
    states: Vec<DeliveryLocalityOption>,
    cities: Vec<DeliveryLocalityOption>,
    districts: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WatchBandChoice {
    style_key: String,
    style_name: String,
    color_key: String,
    color_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WatchBandSize {
    part_number: String,
    text: String,
}

#[derive(Debug, Deserialize, Default)]
struct LocalityField {
    #[serde(default)]
    data: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Deserialize, Default)]
struct LocalityBody {
    #[serde(default)]
    state: LocalityField,
    #[serde(default)]
    city: LocalityField,
    #[serde(default)]
    district: LocalityField,
}

#[derive(Debug, Deserialize)]
struct LocalityResponse {
    body: LocalityBody,
}

/// Apple puts a placeholder ("省份", value "") first in every list.
fn usable(field: LocalityField) -> Vec<DeliveryLocalityOption> {
    field
        .data
        .into_iter()
        .filter(|option| !option.value.trim().is_empty())
        .collect()
}

fn parse_delivery_localities(body: &[u8]) -> Result<DeliveryLocalities, serde_json::Error> {
    let parsed: LocalityResponse = serde_json::from_slice(body)?;
    Ok(DeliveryLocalities {
        states: usable(parsed.body.state),
        cities: usable(parsed.body.city),
        districts: usable(parsed.body.district),
    })
}

fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_watch_band_choices(body: &[u8]) -> Result<Vec<WatchBandChoice>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple band response is not JSON: {error}"))?;
    let items = value
        .pointer("/body/items")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Apple band response has no body.items".to_string())?;
    let mut choices = Vec::new();
    for (style_key, style) in items {
        let style_name = plain_text(
            style
                .get("sectionHeader")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(style_key),
        );
        let style_order = style
            .get("sortOrder")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(i64::MAX);
        let Some(colors) = style
            .get("subDimensionValue")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for color in colors {
            let Some(color_key) = color
                .get("dimensionValue")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let color_name = color
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(plain_text)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| color_key.to_string());
            let color_order = color
                .get("sortOrder")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            choices.push((
                style_order,
                color_order,
                WatchBandChoice {
                    style_key: style_key.clone(),
                    style_name: style_name.clone(),
                    color_key: color_key.to_string(),
                    color_name,
                },
            ));
        }
    }
    choices.sort_by_key(|(style_order, color_order, choice)| {
        (
            *style_order,
            *color_order,
            choice.style_key.clone(),
            choice.color_key.clone(),
        )
    });
    Ok(choices.into_iter().map(|(_, _, choice)| choice).collect())
}

fn parse_watch_band_sizes(body: &[u8]) -> Result<Vec<WatchBandSize>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple band size response is not JSON: {error}"))?;
    let options = value
        .pointer("/body/options")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Apple band size response has no body.options".to_string())?;
    let mut sizes = Vec::new();
    for option in options {
        let Some(part_number) = option
            .pointer("/product/options/watch_bands")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| value.contains('/'))
        else {
            continue;
        };
        let text = option
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(plain_text)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                option
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| part_number.to_string());
        sizes.push(WatchBandSize {
            part_number: part_number.to_string(),
            text,
        });
    }
    Ok(sizes)
}

/// GET an Apple JSON endpoint the way the desktop app does: XHR headers,
/// 20s timeout, size cap, non-2xx is an error.
async fn get_json(
    http: &reqwest::Client,
    url: reqwest::Url,
    label: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let response = http
        .get(url)
        .timeout(Duration::from_secs(20))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/javascript, */*; q=0.01",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .map_err(|error| format!("failed to load Apple {label}: {error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read Apple {label}: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "Apple {label} endpoint returned HTTP {}",
            status.as_u16()
        ));
    }
    if body.len() > max_bytes {
        return Err(format!(
            "Apple {label} response exceeded the {max_bytes}-byte cap"
        ));
    }
    Ok(body.to_vec())
}

/// Province / city / district options from the mainland China store's
/// locality lookup. Other regions use postcodes, so they're refused rather
/// than given a picker whose values would query nothing.
pub async fn delivery_localities(
    http: &reqwest::Client,
    locale: &str,
    state_name: &str,
    city_name: &str,
) -> Result<DeliveryLocalities, String> {
    if locale != "zh_CN" {
        return Err(
            "Apple only offers province/city/district selection on the mainland China store".into(),
        );
    }
    let region = region_by_locale(locale).ok_or_else(|| format!("unknown region {locale}"))?;
    let mut url = reqwest::Url::parse(&format!("{}/shop/address/locality-lookup", region.base_url))
        .map_err(|error| format!("cannot build the Apple locality URL: {error}"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("fae", "true");
        if !state_name.trim().is_empty() {
            query.append_pair("state", state_name.trim());
        }
        if !city_name.trim().is_empty() {
            query.append_pair("city", city_name.trim());
        }
    }
    let body = get_json(http, url, "delivery localities", 512 * 1024).await?;
    parse_delivery_localities(&body)
        .map_err(|error| format!("cannot parse Apple delivery localities: {error}"))
}

fn watch_band_product(catalog: &Catalog, locale: &str, case_part: &str) -> Result<Product, String> {
    let mut product = catalog
        .product_by_part(locale, case_part)
        .ok_or_else(|| format!("Apple Watch case {case_part} is not in the catalog"))?;
    if product.category != Category::Watch {
        return Err(format!("{case_part} is not an Apple Watch case"));
    }
    if product.kit_part.as_deref().is_none_or(str::is_empty) {
        return Err(
            "this Apple Watch model has no kit part number; refresh the catalog first".into(),
        );
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        // Same fallback as the desktop app: "… 46 毫米 …" in the zh_CN title.
        let words: Vec<_> = product.title.split_whitespace().collect();
        product.watch_case_size = words.windows(2).find_map(|pair| {
            (pair[1] == "毫米" && pair[0].chars().all(|ch| ch.is_ascii_digit()))
                .then(|| format!("{}mm", pair[0]))
        });
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        return Err("this Apple Watch model has no case size; refresh the catalog first".into());
    }
    Ok(product)
}

/// Band API URL for a Watch case: `fae`, the lowercase kit part and the case
/// part, followed by `extra`, then the case size last (desktop query order).
fn band_api_url(
    catalog: &Catalog,
    locale: &str,
    case_part: &str,
    endpoint: &str,
    extra: &[(&str, &str)],
) -> Result<reqwest::Url, String> {
    if locale != "zh_CN" {
        return Err(
            "exact Apple Watch band selection is only supported on the mainland China store".into(),
        );
    }
    let region = region_by_locale(locale).ok_or_else(|| format!("unknown region {locale}"))?;
    let product = watch_band_product(catalog, locale, case_part)?;
    let mut url = reqwest::Url::parse(&format!("{}/shop/api/{endpoint}", region.base_url))
        .map_err(|error| format!("cannot build the Apple band URL: {error}"))?;
    let kit = product
        .kit_part
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    url.query_pairs_mut()
        .append_pair("fae", "true")
        .append_pair("product", &kit)
        .append_pair("option.watch_cases", &product.part_number)
        .extend_pairs(extra)
        .append_pair(
            "dm.watch_cases-dimensionCaseSize",
            product.watch_case_size.as_deref().unwrap_or_default(),
        );
    Ok(url)
}

pub async fn watch_band_choices(
    http: &reqwest::Client,
    catalog: &Catalog,
    locale: &str,
    case_part: &str,
) -> Result<Vec<WatchBandChoice>, String> {
    let url = band_api_url(catalog, locale, case_part, "band-selection", &[])?;
    let body = get_json(http, url, "band options", 2 * 1024 * 1024).await?;
    parse_watch_band_choices(&body)
}

pub async fn watch_band_sizes(
    http: &reqwest::Client,
    catalog: &Catalog,
    locale: &str,
    case_part: &str,
    style_key: &str,
    color_key: &str,
) -> Result<Vec<WatchBandSize>, String> {
    if style_key.len() > 80 || color_key.len() > 80 {
        return Err("Apple band option value is too long".into());
    }
    let url = band_api_url(
        catalog,
        locale,
        case_part,
        "band_sizes",
        &[
            ("dm.watch_bands-dimensionBandStyle", style_key.trim()),
            ("dm.watch_bands-dimensionColor", color_key.trim()),
        ],
    )?;
    let body = get_json(http, url, "band sizes", 2 * 1024 * 1024).await?;
    parse_watch_band_sizes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same fixtures as the desktop tests in src-tauri/src/lib.rs.
    #[test]
    fn localities_drop_placeholders_and_keep_three_levels() {
        let response = r#"{
          "body": {
            "state": {"data":[{"text":"省份","value":""},{"text":"江苏","value":"江苏"}]},
            "city": {"data":[{"text":"城镇/城市","value":""},{"text":"苏州","value":"苏州"}]},
            "district": {"data":[{"text":"区","value":""},{"text":"吴江区","value":"吴江区"}]}
          }
        }"#;
        let parsed = parse_delivery_localities(response.as_bytes()).unwrap();
        assert_eq!(parsed.states[0].value, "江苏");
        assert_eq!(parsed.cities[0].value, "苏州");
        assert_eq!(parsed.districts[0].value, "吴江区");
        assert_eq!(parsed.states.len(), 1);
    }

    #[test]
    fn band_api_keeps_style_color_and_exact_size_part() {
        let choices = r#"{"body":{"items":{"sololoop":{"sortOrder":20,"sectionHeader":"<span>新配色</span> 单圈表带","subDimensionValue":[{"dimensionValue":"burgundy","text":"勃艮第酒红色","sortOrder":10}]}}}}"#;
        let parsed = parse_watch_band_choices(choices.as_bytes()).unwrap();
        assert_eq!(parsed[0].style_key, "sololoop");
        assert_eq!(parsed[0].style_name, "新配色 单圈表带");
        assert_eq!(parsed[0].color_name, "勃艮第酒红色");

        let sizes = r#"{"body":{"options":[{"key":"6","text":"6","product":{"options":{"watch_bands":"MKJP4FE/A"}}}]}}"#;
        let parsed = parse_watch_band_sizes(sizes.as_bytes()).unwrap();
        assert_eq!(parsed[0].text, "6");
        assert_eq!(parsed[0].part_number, "MKJP4FE/A");
    }
}
