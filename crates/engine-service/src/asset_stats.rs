use super::*;
use axum::extract::Query;
use serde_json::{json, Value};
use std::{
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
pub(super) struct StatsQuery {
    currency: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct Metrics {
    cap: Option<f64>,
    volume: Option<f64>,
    liquidity: Option<f64>,
}

impl Metrics {
    fn any(self) -> bool {
        self.cap.is_some() || self.volume.is_some() || self.liquidity.is_some()
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    usd: Metrics,
    read_at: u64,
}

type StatsCache = Mutex<HashMap<String, (Instant, Snapshot)>>;
static STATS: LazyLock<StatsCache> = LazyLock::new(Default::default);
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .user_agent("Atlas-market-data")
        .build()
        .expect("market data client")
});

pub(super) async fn stats(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    Query(q): Query<StatsQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let currency = q.currency.unwrap_or_else(|| "NGN".into());
    markets::checked_currency(&currency)?;
    let ((), rate) = tokio::try_join!(
        app_balance::signed_in(&state, &headers),
        app_balance::fx_rate(&currency),
    )?;
    let (network, token) = if let Some(found) = markets::stats_token(&asset_id) {
        found
    } else if let Some(token) = asset_id.strip_prefix("near:ref:").filter(|t| {
        (2..=64).contains(&t.len())
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    }) {
        ("near", token.into())
    } else {
        near_intents::chart_token(&state, &asset_id)
            .await?
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    "This asset has no market data yet.".into(),
                )
            })?
    };
    let data = market_stats(network, &token).await;
    Ok(Json(public(
        &asset_id, network, &token, data, &currency, rate,
    )?))
}

fn public(
    asset_id: &str,
    network: &str,
    token: &str,
    data: Snapshot,
    currency: &str,
    rate: u128,
) -> Result<Value, ApiError> {
    let money = |usd: Option<f64>| {
        usd.map(|usd| markets::money_from_usd(usd, currency, rate))
            .transpose()
    };
    let address = (token != markets::NATIVE_COIN).then(|| {
        json!({
            "address":token,
            "chain":if network == "sui-network" { "sui" } else { network },
            "kind":match network {
                "solana" => "mint",
                "sui" | "sui-network" => "coinType",
                _ => "contract",
            },
        })
    });
    Ok(json!({
        "assetId":asset_id,
        "marketCap":money(data.usd.cap)?,
        "volume24h":money(data.usd.volume)?,
        "liquidity":money(data.usd.liquidity)?,
        "tokenAddress":address,
        "asOfUnixMs":data.read_at,
    }))
}

async fn market_stats(network: &str, token: &str) -> Snapshot {
    let key = format!("{network}:{token}");
    let last = STATS.lock().ok().and_then(|held| held.get(&key).copied());
    if let Some((at, data)) = last {
        let ttl = if data.usd.any() { 60 } else { 15 };
        if at.elapsed() < Duration::from_secs(ttl) {
            return data;
        }
    }
    let data = Snapshot {
        usd: fetch_stats(network, token).await,
        read_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    if let Ok(mut held) = STATS.lock() {
        held.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(120));
        if held.len() < 2_000 || held.contains_key(&key) {
            held.insert(key, (Instant::now(), data));
        }
    }
    data
}

async fn get(url: reqwest::Url) -> Option<Value> {
    HTTP.get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

async fn fetch_stats(network: &str, token: &str) -> Metrics {
    let jupiter = async {
        if network != "solana" {
            return None;
        }
        let mut url = reqwest::Url::parse("https://lite-api.jup.ag/tokens/v2/search").ok()?;
        url.query_pairs_mut().append_pair("query", token);
        Some(jupiter_stats(&get(url).await?, token))
    };
    let dex = async {
        let chain = if network == "sui-network" {
            "sui"
        } else {
            network
        };
        let mut url = reqwest::Url::parse("https://api.dexscreener.com/tokens/v1/").ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(chain)
            .push(token);
        Some(dex_stats(&get(url).await?, chain, token))
    };
    let gecko = async {
        if network == "solana" {
            return None;
        }
        let mut url = reqwest::Url::parse("https://api.geckoterminal.com/api/v2/networks/").ok()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .push(network)
            .push("tokens")
            .push(token);
        Some(gecko_stats(&get(url).await?, network, token))
    };
    // Optional statistics never hold up a quote or make search wait for another source.
    let (jupiter, dex, gecko) = tokio::join!(jupiter, dex, gecko);
    let jupiter = jupiter.unwrap_or_default();
    let dex = dex.unwrap_or_default();
    let gecko = gecko.unwrap_or_default();
    Metrics {
        cap: jupiter.cap.or(dex.cap).or(gecko.cap),
        volume: jupiter.volume.or(gecko.volume).or(dex.volume),
        liquidity: jupiter.liquidity.or(gecko.liquidity).or(dex.liquidity),
    }
}

fn positive(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v > 0.0)
}

fn matches_token(chain: &str, actual: &str, token: &str) -> bool {
    if matches!(chain, "base" | "eth" | "arbitrum" | "bsc" | "monad") {
        actual.eq_ignore_ascii_case(token)
    } else if matches!(chain, "sui" | "sui-network") {
        let normalize = |s: &str| -> Option<String> {
            let (address, rest) = s.split_once("::")?;
            let address = address.strip_prefix("0x")?;
            Some(format!(
                "{}::{rest}",
                address.trim_start_matches('0').to_ascii_lowercase()
            ))
        };
        normalize(actual)
            .zip(normalize(token))
            .is_some_and(|(a, b)| a == b)
    } else {
        actual == token
    }
}

fn jupiter_cap(body: &Value, token: &str) -> Option<f64> {
    let row = body
        .as_array()?
        .iter()
        .find(|row| row["id"].as_str() == Some(token))?;
    positive(&row["mcap"])
}

fn dex_cap(body: &Value, chain: &str, token: &str) -> Option<f64> {
    body.as_array()?
        .iter()
        .filter_map(|pair| {
            if pair["chainId"].as_str() != Some(chain)
                || !matches_token(chain, pair["baseToken"]["address"].as_str()?, token)
            {
                return None;
            }
            Some((
                positive(&pair["liquidity"]["usd"])?,
                positive(&pair["marketCap"])?,
            ))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, cap)| cap)
}

fn gecko_cap(body: &Value, chain: &str, token: &str) -> Option<f64> {
    let at = &body["data"]["attributes"];
    if !matches_token(chain, at["address"].as_str()?, token) {
        return None;
    }
    // FDV assumes every token is circulating and must not masquerade as market cap.
    positive(&at["market_cap_usd"])
}

// A quiet market can have zero volume or liquidity. Missing fields are not zero.
fn nonnegative(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
}

fn jupiter_stats(body: &Value, token: &str) -> Metrics {
    let Some(row) = body
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(token)))
    else {
        return Metrics::default();
    };
    Metrics {
        cap: jupiter_cap(body, token),
        volume: nonnegative(&row["stats24h"]["buyVolume"])
            .zip(nonnegative(&row["stats24h"]["sellVolume"]))
            .map(|(buy, sell)| buy + sell)
            .filter(|v| v.is_finite()),
        liquidity: nonnegative(&row["liquidity"]),
    }
}

fn dex_stats(body: &Value, chain: &str, token: &str) -> Metrics {
    let mut data = Metrics {
        cap: dex_cap(body, chain, token),
        ..Metrics::default()
    };
    let Some(pairs) = body.as_array() else {
        return data;
    };
    let mut seen = std::collections::HashSet::new();
    for pair in pairs {
        if pair["chainId"].as_str() != Some(chain)
            || !["baseToken", "quoteToken"].iter().any(|side| {
                pair[side]["address"]
                    .as_str()
                    .is_some_and(|address| matches_token(chain, address, token))
            })
        {
            continue;
        }
        let Some(pool) = pair["pairAddress"].as_str().filter(|a| !a.is_empty()) else {
            continue;
        };
        let pool = if matches!(chain, "solana" | "near") {
            pool.into()
        } else {
            pool.to_ascii_lowercase()
        };
        if !seen.insert(pool) {
            continue;
        }
        for (sum, field) in [
            (&mut data.volume, &pair["volume"]["h24"]),
            (&mut data.liquidity, &pair["liquidity"]["usd"]),
        ] {
            if let Some(value) = nonnegative(field) {
                let total = sum.unwrap_or(0.0) + value;
                if total.is_finite() {
                    *sum = Some(total);
                }
            }
        }
    }
    data
}

fn gecko_stats(body: &Value, chain: &str, token: &str) -> Metrics {
    let at = &body["data"]["attributes"];
    if !at["address"]
        .as_str()
        .is_some_and(|a| matches_token(chain, a, token))
    {
        return Metrics::default();
    }
    Metrics {
        cap: gecko_cap(body, chain, token),
        volume: nonnegative(&at["volume_usd"]["h24"]),
        liquidity: nonnegative(&at["total_reserve_in_usd"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BONK: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
    const BRETT: &str = "0x532f27101965dd16442E59d40670FaF5eBB142E4";
    const DEEP: &str =
        "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP";

    #[test]
    fn captured_market_caps_match_the_exact_coin() {
        let jupiter =
            serde_json::from_str(include_str!("../fixtures/market-cap/bonk-jupiter.json")).unwrap();
        let bonk =
            serde_json::from_str(include_str!("../fixtures/market-cap/bonk-dex.json")).unwrap();
        let brett =
            serde_json::from_str(include_str!("../fixtures/market-cap/brett-dex.json")).unwrap();
        let deep =
            serde_json::from_str(include_str!("../fixtures/market-cap/deep-dex.json")).unwrap();
        assert_eq!(jupiter_cap(&jupiter, BONK), Some(338970411.4694302));
        assert!(dex_cap(&bonk, "solana", BONK).unwrap() > 0.0);
        assert_eq!(dex_cap(&brett, "base", BRETT), Some(57196507.0));
        assert_eq!(dex_cap(&deep, "sui", DEEP), Some(61076319.0));
        assert_eq!(jupiter_cap(&jupiter, "another-coin"), None);
        assert_eq!(dex_cap(&bonk, "base", BONK), None);
        assert_eq!(
            dex_cap(&deep, "sui", &DEEP.replace("::deep::", "::DEEP::")),
            None
        );
    }

    #[test]
    fn the_deepest_matching_pool_wins_and_fdv_is_not_market_cap() {
        let pairs = json!([
            {"chainId":"base","baseToken":{"address":BRETT},"liquidity":{"usd":100},"marketCap":200},
            {"chainId":"base","baseToken":{"address":BRETT.to_ascii_lowercase()},"liquidity":{"usd":200},"marketCap":300},
            {"chainId":"solana","baseToken":{"address":BRETT},"liquidity":{"usd":900},"marketCap":999},
            {"chainId":"base","baseToken":{"address":"another"},"quoteToken":{"address":BRETT},"liquidity":{"usd":1000},"marketCap":1000}
        ]);
        assert_eq!(dex_cap(&pairs, "base", BRETT), Some(300.0));
        assert_eq!(
            dex_cap(
                &json!([{"chainId":"base","baseToken":{"address":BRETT},
            "liquidity":{"usd":100},"fdv":900}]),
                "base",
                BRETT
            ),
            None
        );
        assert_eq!(jupiter_cap(&json!([{"id":BONK,"fdv":900}]), BONK), None);
        let token =
            json!({"data":{"attributes":{"address":BRETT,"market_cap_usd":"300","fdv_usd":"900"}}});
        assert_eq!(gecko_cap(&token, "base", BRETT), Some(300.0));
        assert_eq!(gecko_cap(&token, "base", "another"), None);
        for value in [
            json!(0),
            json!(-10),
            json!("NaN"),
            json!("inf"),
            Value::Null,
        ] {
            assert_eq!(positive(&value), None);
        }
    }

    #[test]
    fn caps_use_the_selected_currency_and_missing_data_stays_missing() {
        let cap = Snapshot {
            usd: Metrics {
                cap: Some(100.0),
                volume: Some(20.0),
                liquidity: Some(30.0),
            },
            read_at: 123,
        };
        let row = public(BONK, "solana", BONK, cap, "NGN", 1_500_000_000).unwrap();
        assert_eq!(
            row["marketCap"],
            json!({"amount":"150000","currency":"NGN"})
        );
        assert_eq!(row["asOfUnixMs"], 123);
        let missing = public(
            BONK,
            "solana",
            BONK,
            Snapshot {
                usd: Metrics::default(),
                read_at: 123,
            },
            "NGN",
            1_500_000_000,
        )
        .unwrap();
        assert!(missing["marketCap"].is_null());
        assert!(matches_token("sui", "0x0002::sui::SUI", "0x2::sui::SUI"));
    }

    #[test]
    fn captured_volume_liquidity_and_copy_address_match_the_coin() {
        let jupiter: Value =
            serde_json::from_str(include_str!("../fixtures/asset-details/bonk-jupiter.json"))
                .unwrap();
        let bonk = jupiter_stats(&jupiter, BONK);
        assert_eq!(bonk.volume, Some(662839.5803848626 + 684334.1063868757));
        assert_eq!(bonk.liquidity, Some(6223767.590438207));
        assert!(!jupiter_stats(&jupiter, "another").any());
        let brett: Value =
            serde_json::from_str(include_str!("../fixtures/asset-details/brett-dex.json")).unwrap();
        let data = dex_stats(&brett, "base", BRETT);
        assert_eq!(data.volume, Some(128930.41));
        assert_eq!(data.liquidity, Some(891843.63));
        let deep: Value =
            serde_json::from_str(include_str!("../fixtures/asset-details/deep-dex.json")).unwrap();
        assert_eq!(dex_stats(&deep, "sui", DEEP).volume, Some(256992.91));
        let gecko: Value =
            serde_json::from_str(include_str!("../fixtures/asset-details/brett-gecko.json"))
                .unwrap();
        assert_eq!(
            gecko_stats(&gecko, "base", BRETT).volume,
            Some(200423.258129092)
        );
        assert!(!gecko_stats(&gecko, "base", "another").any());
        let row = public(
            "brett-base",
            "base",
            BRETT,
            Snapshot {
                usd: data,
                read_at: 123,
            },
            "NGN",
            1_500_000_000,
        )
        .unwrap();
        assert_eq!(
            row["volume24h"],
            json!({"amount":"193395615","currency":"NGN"})
        );
        assert_eq!(
            row["liquidity"],
            json!({"amount":"1337765445","currency":"NGN"})
        );
        assert_eq!(
            row["tokenAddress"],
            json!({"address":BRETT,"chain":"base","kind":"contract"})
        );
        let row = public(
            "deep",
            "sui-network",
            DEEP,
            Snapshot {
                usd: Metrics::default(),
                read_at: 123,
            },
            "USD",
            1_000_000,
        )
        .unwrap();
        assert_eq!(row["tokenAddress"]["address"], DEEP);
        assert_eq!(row["tokenAddress"]["kind"], "coinType");
        assert!(row["volume24h"].is_null());
        assert!(row["liquidity"].is_null());
        let native = public(
            "mon",
            "monad",
            markets::NATIVE_COIN,
            Snapshot {
                usd: Metrics::default(),
                read_at: 123,
            },
            "USD",
            1_000_000,
        )
        .unwrap();
        assert!(native["tokenAddress"].is_null());
    }

    #[test]
    fn pool_totals_count_each_matching_pool_once_and_preserve_zero() {
        let pool = json!({"chainId":"base","pairAddress":"0xabc","baseToken":{"address":BRETT},"liquidity":{"usd":100},"volume":{"h24":20},"marketCap":300});
        let pairs = json!([
            pool.clone(), pool,
            {"chainId":"base","pairAddress":"0xdef","quoteToken":{"address":BRETT.to_ascii_lowercase()},"baseToken":{"address":"other"},"liquidity":{"usd":200},"volume":{"h24":30},"marketCap":999},
            {"chainId":"solana","pairAddress":"wrong-chain","baseToken":{"address":BRETT},"liquidity":{"usd":1000},"volume":{"h24":1000}},
            {"chainId":"base","pairAddress":"wrong-token","baseToken":{"address":"other"},"liquidity":{"usd":1000},"volume":{"h24":1000}}
        ]);
        let data = dex_stats(&pairs, "base", BRETT);
        assert_eq!(data.volume, Some(50.0));
        assert_eq!(data.liquidity, Some(300.0));
        assert_eq!(data.cap, Some(300.0));
        let quiet = jupiter_stats(
            &json!([{"id":BONK,"liquidity":0,"stats24h":{"buyVolume":0,"sellVolume":0}}]),
            BONK,
        );
        assert_eq!(quiet.volume, Some(0.0));
        assert_eq!(quiet.liquidity, Some(0.0));
        assert!(quiet.cap.is_none());
        for field in [json!(-1), json!("NaN"), json!("inf"), Value::Null] {
            assert!(nonnegative(&field).is_none());
        }
        assert!(
            jupiter_stats(&json!([{"id":BONK,"stats24h":{"buyVolume":10}}]), BONK)
                .volume
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "live read-only market data"]
    async fn live_market_caps() {
        for (name, network, token) in [
            ("BONK", "solana", BONK),
            ("BRETT", "base", BRETT),
            ("DEEP", "sui-network", DEEP),
        ] {
            let data = fetch_stats(network, token).await;
            println!(
                "{name}: market cap = {:?}, 24h volume = {:?}, liquidity = {:?} (USD)",
                data.cap, data.volume, data.liquidity
            );
            assert!(data.cap.is_some(), "{name} market cap could not load");
            assert!(data.volume.is_some(), "{name} volume could not load");
            assert!(data.liquidity.is_some(), "{name} liquidity could not load");
            let row = public(
                name,
                network,
                token,
                Snapshot {
                    usd: data,
                    read_at: 123,
                },
                "NGN",
                1_500_000_000,
            )
            .unwrap();
            assert_eq!(row["tokenAddress"]["address"], token);
        }
    }
}
