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

#[derive(Clone, Copy)]
struct Cap {
    usd: Option<f64>,
    read_at: u64,
}

type CapCache = Mutex<HashMap<String, (Instant, Cap)>>;
static CAPS: LazyLock<CapCache> = LazyLock::new(Default::default);
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
    let cap = market_cap(network, &token).await;
    Ok(Json(public(&asset_id, cap, &currency, rate)?))
}

fn public(asset_id: &str, cap: Cap, currency: &str, rate: u128) -> Result<Value, ApiError> {
    let amount = cap
        .usd
        .map(|usd| markets::money_from_usd(usd, currency, rate))
        .transpose()?;
    Ok(json!({"assetId":asset_id,"marketCap":amount,"asOfUnixMs":cap.read_at}))
}

async fn market_cap(network: &str, token: &str) -> Cap {
    let key = format!("{network}:{token}");
    let last = CAPS.lock().ok().and_then(|held| held.get(&key).copied());
    if let Some((at, cap)) = last {
        let ttl = if cap.usd.is_some() { 60 } else { 15 };
        if at.elapsed() < Duration::from_secs(ttl) {
            return cap;
        }
    }
    let cap = Cap {
        usd: fetch_cap(network, token).await,
        read_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    if let Ok(mut held) = CAPS.lock() {
        held.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(120));
        if held.len() < 2_000 || held.contains_key(&key) {
            held.insert(key, (Instant::now(), cap));
        }
    }
    cap
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

async fn fetch_cap(network: &str, token: &str) -> Option<f64> {
    let jupiter = async {
        if network != "solana" {
            return None;
        }
        let mut url = reqwest::Url::parse("https://lite-api.jup.ag/tokens/v2/search").ok()?;
        url.query_pairs_mut().append_pair("query", token);
        jupiter_cap(&get(url).await?, token)
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
        dex_cap(&get(url).await?, chain, token)
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
        gecko_cap(&get(url).await?, network, token)
    };
    // Optional statistics never hold up a quote or make search wait for another source.
    let (jupiter, dex, gecko) = tokio::join!(jupiter, dex, gecko);
    jupiter.or(dex).or(gecko)
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
        let cap = Cap {
            usd: Some(100.0),
            read_at: 123,
        };
        let row = public(BONK, cap, "NGN", 1_500_000_000).unwrap();
        assert_eq!(
            row["marketCap"],
            json!({"amount":"150000","currency":"NGN"})
        );
        assert_eq!(row["asOfUnixMs"], 123);
        let missing = public(
            BONK,
            Cap {
                usd: None,
                read_at: 123,
            },
            "NGN",
            1_500_000_000,
        )
        .unwrap();
        assert!(missing["marketCap"].is_null());
        assert!(matches_token("sui", "0x0002::sui::SUI", "0x2::sui::SUI"));
    }

    #[tokio::test]
    #[ignore = "live read-only market data"]
    async fn live_market_caps() {
        for (name, network, token) in [
            ("BONK", "solana", BONK),
            ("BRETT", "base", BRETT),
            ("DEEP", "sui-network", DEEP),
        ] {
            let cap = fetch_cap(network, token).await;
            println!("{name}: market cap in USD = {cap:?}");
            assert!(cap.is_some(), "{name} market cap could not load");
        }
    }
}
