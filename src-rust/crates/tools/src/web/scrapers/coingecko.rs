// CoinGecko handler: renders a cryptocurrency's market data via the API.

use super::util::{
    build_result, format_iso_date, format_number, load_page, LoadOptions, RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct CoinGeckoHandler;

static COIN_PATH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:/[a-z]{2})?/coins/([^/?#]+)").expect("static coingecko regex"));
static HTML_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").expect("static tag regex"));

fn coin_id(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if !parsed.host_str()?.contains("coingecko.com") {
        return None;
    }
    COIN_PATH
        .captures(parsed.path())
        .map(|m| super::util::percent_decode(&m[1]))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A dotted lookup like `market_data.current_price.usd`.
fn usd(market: &Value, group: &str) -> Option<f64> {
    market.get(group)?.get("usd")?.as_f64()
}

/// Price with decimal places scaled to the magnitude.
fn format_price(price: f64) -> String {
    if price >= 1000.0 {
        format_number(price.round() as u64)
    } else if price >= 1.0 {
        format!("{price:.2}")
    } else if price >= 0.01 {
        format!("{price:.4}")
    } else if price >= 0.0001 {
        format!("{price:.6}")
    } else {
        format!("{price:.8}")
    }
}

fn append_price(md: &mut String, market: &Value) {
    if let Some(price) = usd(market, "current_price") {
        let _ = write!(md, "**Price:** ${}", format_price(price));
        if let Some(change) = market
            .get("price_change_percentage_24h")
            .and_then(Value::as_f64)
        {
            let sign = if change >= 0.0 { "+" } else { "" };
            let _ = write!(md, " ({sign}{change:.2}% 24h)");
        }
        md.push('\n');
    }
    if let Some(cap) = usd(market, "market_cap") {
        let _ = writeln!(md, "**Market Cap:** ${}", format_number(cap.round() as u64));
    }
    if let Some(volume) = usd(market, "total_volume") {
        let _ = writeln!(
            md,
            "**24h Volume:** ${}",
            format_number(volume.round() as u64)
        );
    }
    if let Some(ath) = usd(market, "ath") {
        let _ = write!(md, "**All-Time High:** ${}", format_price(ath));
        if let Some(date) = market.get("ath_date").and_then(|d| str_field(d, "usd")) {
            let formatted = format_iso_date(date);
            if !formatted.is_empty() {
                let _ = write!(md, " ({formatted})");
            }
        }
        md.push('\n');
    }
}

fn append_supply(md: &mut String, market: &Value) {
    let Some(circulating) = market.get("circulating_supply").and_then(Value::as_f64) else {
        return;
    };
    let _ = write!(
        md,
        "**Circulating Supply:** {}",
        format_number(circulating.round() as u64)
    );
    if let Some(max) = market
        .get("max_supply")
        .and_then(Value::as_f64)
        .filter(|m| *m > 0.0)
    {
        let percent = circulating / max * 100.0;
        let _ = write!(
            md,
            " / {} ({percent:.1}%)",
            format_number(max.round() as u64)
        );
    } else if let Some(total) = market.get("total_supply").and_then(Value::as_f64) {
        let _ = write!(md, " / {} total", format_number(total.round() as u64));
    }
    md.push('\n');
}

fn append_links(md: &mut String, coin: &Value) {
    let Some(links) = coin.get("links") else {
        return;
    };
    let first = |group: &str| {
        links
            .get(group)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(home) = first("homepage") {
        parts.push(format!("[Website]({home})"));
    }
    if let Some(explorer) = first("blockchain_site") {
        parts.push(format!("[Explorer]({explorer})"));
    }
    if let Some(github) = links
        .get("repos_url")
        .and_then(|r| r.get("github"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("[GitHub]({github})"));
    }
    if !parts.is_empty() {
        let _ = writeln!(md, "**Links:** {}", parts.join(" · "));
    }
}

fn render(coin: &Value) -> String {
    let name = str_field(coin, "name").unwrap_or("(coin)");
    let symbol = str_field(coin, "symbol").unwrap_or("").to_uppercase();
    let mut md = format!("# {name} ({symbol})\n\n");
    let market = coin.get("market_data").cloned().unwrap_or(Value::Null);
    append_price(&mut md, &market);
    md.push('\n');
    append_supply(&mut md, &market);
    if let Some(date) = str_field(coin, "genesis_date") {
        let _ = writeln!(md, "**Launch Date:** {date}");
    }
    if let Some(categories) = coin.get("categories").and_then(Value::as_array) {
        let list: Vec<&str> = categories.iter().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            let _ = writeln!(md, "**Categories:** {}", list.join(", "));
        }
    }
    append_links(&mut md, coin);
    if let Some(desc) = coin.get("description").and_then(|d| str_field(d, "en")) {
        let cleaned = HTML_TAG.replace_all(desc, "").replace("\r\n", "\n");
        let cleaned = cleaned.trim();
        if !cleaned.is_empty() {
            let _ = write!(md, "\n## About\n\n{cleaned}\n");
        }
    }
    md
}

#[async_trait]
impl SpecialHandler for CoinGeckoHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let id = coin_id(url)?;
        let api_url = format!(
            "https://api.coingecko.com/api/v3/coins/{id}?localization=false&tickers=false&community_data=false&developer_data=false"
        );
        let result = load_page(
            &api_url,
            LoadOptions {
                timeout,
                headers: vec![("Accept".to_string(), "application/json".to_string())],
                ..Default::default()
            },
        )
        .await;
        if !result.ok {
            let fallback = format!(
                "# {id}\n\nCoinGecko market data is currently unavailable for this asset.\n"
            );
            return Some(build_result(
                &fallback,
                url,
                "coingecko",
                vec!["CoinGecko API request failed".to_string()],
            ));
        }
        let coin: Value = serde_json::from_str(&result.content).ok()?;
        let md = render(&coin);
        Some(build_result(
            &md,
            url,
            "coingecko",
            vec!["Fetched via CoinGecko API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coin_id_reads_with_and_without_a_locale_prefix() {
        assert_eq!(
            coin_id("https://www.coingecko.com/en/coins/bitcoin").as_deref(),
            Some("bitcoin")
        );
        assert_eq!(
            coin_id("https://coingecko.com/coins/ethereum").as_deref(),
            Some("ethereum")
        );
        assert!(coin_id("https://example.com/coins/bitcoin").is_none());
    }

    #[test]
    fn price_scales_decimals_by_magnitude() {
        assert_eq!(format_price(45123.4), "45,123");
        assert_eq!(format_price(12.5), "12.50");
        assert_eq!(format_price(0.045), "0.0450");
        assert_eq!(format_price(0.00005), "0.00005000");
    }

    #[test]
    fn render_lays_out_market_data_and_about() {
        let coin = json!({
            "name": "Bitcoin",
            "symbol": "btc",
            "genesis_date": "2009-01-03",
            "categories": ["Cryptocurrency"],
            "description": { "en": "Bitcoin is <b>digital</b> gold." },
            "links": { "homepage": ["https://bitcoin.org"] },
            "market_data": {
                "current_price": { "usd": 45000.0 },
                "price_change_percentage_24h": 2.5,
                "market_cap": { "usd": 880000000000.0 },
                "ath": { "usd": 69000.0 },
                "ath_date": { "usd": "2021-11-10T14:24:00Z" },
                "circulating_supply": 19600000.0,
                "max_supply": 21000000.0
            }
        });
        let md = render(&coin);
        assert!(md.contains("# Bitcoin (BTC)"));
        assert!(md.contains("**Price:** $45,000 (+2.50% 24h)"));
        assert!(md.contains("**Market Cap:** $880,000,000,000"));
        assert!(md.contains("**All-Time High:** $69,000 (2021-11-10)"));
        assert!(md.contains("**Circulating Supply:** 19,600,000 / 21,000,000 (93.3%)"));
        assert!(md.contains("**Links:** [Website](https://bitcoin.org)"));
        assert!(md.contains("## About\n\nBitcoin is digital gold."));
    }
}
