use super::*;
use axum::extract::Query;
use engine_execution::swaps::{
    jupiter::{JupiterClient, JupiterOrderRequest, UserPaidSwap},
    kyberswap::{KyberClient, KyberRoute, KYBER_ROUTER},
    oneinch::BaseSwapRequest,
    uniswap::{BaseV3Quote, UniswapV3Client, BASE_USDC, BASE_WETH, SWAP_ROUTER_02},
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SOL_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub(super) const SOL_USDC_MINT: &str = SOL_USDC;
pub(super) const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const BRETT: &str = "0x532f27101965dd16442E59d40670FaF5eBB142E4";
const AAPLC: &str = "0xb200000000000000000000C2e324d24d7eEcd1fb";
#[cfg(test)]
const JUPITER_VERIFIED: &str = "https://lite-api.jup.ag/tokens/v2/tag?query=verified";

// Jupiter's keyless API (lite-api) is limited per address, and shared hosting shares addresses.
// With JUPITER_API_KEY (the key swaps already use) every data call goes to api.jup.ag under the
// key's own limit; the paths are the same.
static JUPITER_KEY: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| {
    env::var("JUPITER_API_KEY")
        .ok()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
});
pub(super) fn jupiter_keyed() -> bool {
    JUPITER_KEY.is_some()
}
pub(super) fn jupiter_get(http: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
    match JUPITER_KEY.as_deref() {
        Some(key) => http
            .get(format!("https://api.jup.ag{path}"))
            .header("x-api-key", key),
        None => http.get(format!("https://lite-api.jup.ag{path}")),
    }
}
// Prices this old still stand in when Jupiter refuses a refresh.
const PRICE_STALE_OK: Duration = Duration::from_secs(10 * 60);
// Liquid enough that an order of up to $10,000 (the quote cap) routes without wrecking the price.
const MIN_LIQUIDITY_USD: f64 = 100_000.0;
const CATALOG_TTL: Duration = Duration::from_secs(30 * 60);
const PRICE_TTL: Duration = Duration::from_secs(15);
const MULTIPLIER_TTL: Duration = Duration::from_secs(10 * 60);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(super) struct Asset {
    pub(super) id: String,
    pub(super) symbol: String,
    pub(super) name: String,
    // crypto | meme | stock
    pub(super) kind: String,
    // base | solana
    pub(super) chain: String,
    pub(super) token: String,
    pub(super) decimals: u32,
    pub(super) icon_url: Option<String>,
    // 24h traded volume in USD, for ranking. xStocks carry a share multiplier that must stay 1.
    pub(super) volume_24h: f64,
    pub(super) xstock: bool,
    // False only for tokens found by pasting their address (Jupiter hasn't verified them).
    pub(super) verified: bool,
    // Jupiter's USD price when the catalog was read: orders the Crypto list, never shown or traded on.
    pub(super) ref_price: f64,
    // 24h price change (%) when the catalog was read: orders Trending.
    pub(super) change_24h: f64,
    // False keeps an asset out of Trade (another row already sells the same coin) while the balance
    // still values it.
    pub(super) listed: bool,
}

// Coins Jupiter only carries as wrapped copies (cbBTC, WBTC, xBTC, zBTC… are all Bitcoin), or under
// just a ticker. Each shows once under its real name, backed by one deep copy.
// (backing mint, symbol, name)
type Major = (&'static str, &'static str, &'static str);
const MAJORS: &[Major] = &[
    (
        "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij",
        "BTC",
        "Bitcoin",
    ),
    (
        "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
        "ETH",
        "Ethereum",
    ),
    (
        "cbLTC4T5NpzSUtQ7ekgEMZGaUPVJY1ko6BUikqa4gGf",
        "LTC",
        "Litecoin",
    ),
    ("9gP2kCy3wA1ctvYWQk75guqXuHfrEomqydHLtcTCqiLa", "BNB", "BNB"),
    (
        "avaxGHCq3T7hoxd73oY2KY9hJSTaeMibXvHy5KNzh5D",
        "AVAX",
        "Avalanche",
    ),
    (
        "98sMhvDwXj1RQi5c5Mndm3vPe9cBqPrbLaufMXFNMh5g",
        "HYPE",
        "Hyperliquid",
    ),
];
// Another copy of a major carries its symbol and trades within this much of its price.
const COPY_PRICE_BAND: f64 = 0.1;

// Base assets route through fixed Uniswap V3 pools, so they stay a short fixed list.
fn base_assets() -> Vec<Asset> {
    let base = |id: &str, symbol: &str, name: &str, kind: &str, token: &str, decimals: u32| Asset {
        id: id.into(),
        symbol: symbol.into(),
        name: name.into(),
        kind: kind.into(),
        chain: "base".into(),
        token: token.into(),
        decimals,
        icon_url: None,
        volume_24h: 0.0,
        xstock: false,
        verified: true,
        ref_price: 0.0,
        change_24h: 0.0,
        listed: true,
    };
    vec![
        // Ethereum is listed once, as the Solana copy; WETH held on Base still counts in the balance.
        Asset {
            listed: false,
            ..base(
                "weth-base",
                "WETH",
                "Wrapped Ether",
                "crypto",
                BASE_WETH,
                18,
            )
        },
        base("brett-base", "BRETT", "Brett", "meme", BRETT, 18),
        base(
            "aaplc-base",
            "AAPLc",
            "Coinbase Wrapped Apple",
            "stock",
            AAPLC,
            8,
        ),
    ]
}

// Classic memes Jupiter doesn't tag as memes.
const CLASSIC_MEMES: &[&str] = &[
    "DOGE", "PEPE", "SHIB", "FLOKI", "WIF", "POPCAT", "BONK", "MEW", "FARTCOIN",
];
// A token pasted by address is tradable if at least this much liquidity backs it.
const MIN_PASTED_LIQUIDITY_USD: f64 = 5_000.0;

// One Jupiter token as a tradable Solana asset, or None if Atlas shouldn't list it: only verified,
// liquid tokens; stablecoins, liquid-staking and yield tokens belong to cash and Earn, not Trade.
fn solana_asset(token: &Value) -> Option<Asset> {
    if token["isVerified"].as_bool() != Some(true) {
        return None;
    }
    if token["liquidity"].as_f64().unwrap_or(0.0) < MIN_LIQUIDITY_USD {
        return None;
    }
    token_asset(token, true)
}

// Any Jupiter token someone pasted the address of: unverified allowed (flagged in the app), but it
// still needs real liquidity to trade.
fn pasted_asset(token: &Value) -> Option<Asset> {
    if token["liquidity"].as_f64().unwrap_or(0.0) < MIN_PASTED_LIQUIDITY_USD {
        return None;
    }
    token_asset(token, token["isVerified"].as_bool() == Some(true))
}

fn token_asset(token: &Value, verified: bool) -> Option<Asset> {
    let mint = token["id"].as_str()?;
    let symbol = token["symbol"].as_str().filter(|s| !s.is_empty())?;
    let name = token["name"].as_str().unwrap_or(symbol);
    let decimals = u32::try_from(token["decimals"].as_u64()?).ok()?;
    let tags: Vec<&str> = token["tags"]
        .as_array()
        .map(|tags| tags.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let has = |tag: &str| tags.contains(&tag);
    let upper = symbol.to_ascii_uppercase();
    // Pool shares and leveraged wrappers (JLP "Jupiter Perps", Hylo's xSOL) aren't assets to trade here.
    let derivative = name.contains("Perps")
        || name.contains("Leveraged")
        || name.ends_with(" LP")
        || upper == "JLP";
    if mint == SOL_USDC
        || ["stable", "lst", "yield", "yb", "jup-lend-earn"]
            .iter()
            .any(|t| has(t))
        || upper.contains("USD")
        || upper.contains("EUR")
        || derivative
    {
        return None;
    }
    let xstock = has("xstocks") || name.ends_with("xStock");
    // Launchpad coins (pump.fun, bonk.fun, stonkfun, LaunchLab, Meteora DBC…) are memes even when
    // Jupiter doesn't tag them; their mints often end in the launchpad's suffix.
    let launched = [&token["launchpad"], &token["firstPool"]["launchpad"]]
        .iter()
        .any(|l| l.as_str().is_some_and(|l| !l.is_empty()))
        || mint.ends_with("pump")
        || mint.ends_with("bonk");
    // Tokenized equities come from several issuers (xStocks, Backpack Securities, Tessera), not all
    // tagged "stocks" on Jupiter.
    let kind = if xstock
        || [
            "stocks",
            "equities",
            "prestocks",
            "rwa",
            "backpack",
            "tessera",
        ]
        .iter()
        .any(|t| has(t))
        || name.contains(" Securities")
    {
        "stock"
    } else if has("meme") || launched || CLASSIC_MEMES.contains(&upper.as_str()) {
        "meme"
    } else {
        "crypto"
    };
    let volume = ["buyVolume", "sellVolume"]
        .iter()
        .map(|k| token["stats24h"][*k].as_f64().unwrap_or(0.0))
        .sum();
    // "Trump Media & Technology Group Corp. Common Stock - Backpack Securities" → the company name.
    let name = name
        .trim_end_matches(" (Portal)")
        .trim_end_matches(" - Backpack Securities")
        .trim_end_matches(" Common Stock")
        .trim();
    let (symbol, name) = if mint == SOL_MINT {
        ("SOL", "Solana")
    } else if let Some((_, symbol, name)) = MAJORS.iter().find(|(m, _, _)| *m == mint) {
        (*symbol, *name)
    } else {
        (symbol, name)
    };
    Some(Asset {
        id: mint.into(),
        symbol: symbol.into(),
        name: name.into(),
        kind: kind.into(),
        chain: "solana".into(),
        token: mint.into(),
        decimals,
        icon_url: token["icon"].as_str().map(str::to_owned),
        volume_24h: volume,
        xstock,
        verified,
        ref_price: token["usdPrice"].as_f64().unwrap_or(0.0),
        change_24h: token["stats24h"]["priceChange"].as_f64().unwrap_or(0.0),
        listed: true,
    })
}

// A Solana mint address as typed into search: 32–44 base58 characters.
pub(super) fn looks_like_mint(text: &str) -> bool {
    (32..=44).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l'))
}

// A token by its address, for anything the catalog doesn't list: Jupiter's token search, cached.
pub(super) async fn pasted_token(
    state: &MarketState,
    mint: &str,
) -> Result<Option<Asset>, ApiError> {
    let held = state.pasted.lock().map_err(internal)?.get(mint).cloned();
    if let Some((at, asset)) = &held {
        if at.elapsed() < CATALOG_TTL {
            return Ok(asset.clone());
        }
    }
    let fetched: Result<Vec<Value>, ApiError> = async {
        jupiter_get(&state.http, "/tokens/v2/search")
            .query(&[("query", mint)])
            .send()
            .await
            .map_err(unavailable)?
            .error_for_status()
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)
    }
    .await;
    // Jupiter busy: the token as last seen, so a held coin doesn't drop out of the balance.
    let tokens = match fetched {
        Ok(tokens) => tokens,
        Err(error) => return held.map(|(_, asset)| asset).ok_or(error),
    };
    let asset = tokens
        .iter()
        .find(|t| t["id"].as_str() == Some(mint))
        .and_then(pasted_asset);
    state
        .pasted
        .lock()
        .map_err(internal)?
        .insert(mint.to_owned(), (Instant::now(), asset.clone()));
    Ok(asset)
}

// A Base token address as typed into search: 0x and 40 hex digits.
pub(super) fn looks_like_evm_address(text: &str) -> bool {
    text.len() == 42 && text.starts_with("0x") && text[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

// Base tokens outside the fixed list go by their address, so they never clash with catalog ids.
fn base_token_id(address: &str) -> String {
    format!("base:{}", address.to_ascii_lowercase())
}

const GECKO_BASE: &str = "https://api.geckoterminal.com/api/v2/networks/base/";
const BASE_TRENDING_TTL: Duration = Duration::from_secs(10 * 60);
const BASE_RATE_TTL: Duration = Duration::from_secs(15);

fn gecko_number(value: &Value) -> f64 {
    value
        .as_str()
        .and_then(|v| v.parse().ok())
        .or_else(|| value.as_f64())
        .filter(|v: &f64| v.is_finite())
        .unwrap_or(0.0)
}
fn gecko_icon(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|url| url.starts_with("https://") && !url.contains("missing"))
        .map(str::to_owned)
}

// What GeckoTerminal knows about a Base token: liquidity across its pools, 24h volume, its logo, and
// whether it flags the token as a honeypot (buyable, never sellable). Its free limit is per IP and
// often spent on Render, so callers cope without it.
struct GeckoToken {
    reserve_usd: f64,
    volume_usd: f64,
    icon: Option<String>,
    honeypot: bool,
}
async fn gecko_base_token(state: &MarketState, address: &str) -> Option<GeckoToken> {
    let get = |path: String| async move {
        state
            .http
            .get(format!("{GECKO_BASE}tokens/{path}"))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Value>()
            .await
            .ok()
    };
    let (token, info) = tokio::join!(get(address.to_owned()), get(format!("{address}/info")));
    let at = &token?["data"]["attributes"];
    Some(GeckoToken {
        reserve_usd: gecko_number(&at["total_reserve_in_usd"]),
        volume_usd: gecko_number(&at["volume_usd"]["h24"]),
        icon: gecko_icon(&at["image_url"]),
        honeypot: info
            .is_some_and(|i| i["data"]["attributes"]["is_honeypot"].as_bool() == Some(true)),
    })
}

// Real liquidity without GeckoTerminal: $100 through Kyber fills within 3% of the market price
// (about $5k of depth or more), when Kyber prices both sides.
async fn kyber_depth_ok(state: &MarketState, address: &str) -> bool {
    match state.kyber.route(BASE_USDC, address, 100_000_000).await {
        Ok(route) => route.price_impact().is_some_and(|impact| impact < 0.03),
        Err(_) => false,
    }
}

// A Base token by its address: name, symbol and decimals from the contract, tradable only with real
// liquidity behind it and no honeypot flag. Unverified until CoinGecko is checked (the caller does,
// for what it shows). Kept for the catalog's lifetime once found; not-found is asked again next time.
pub(super) async fn pasted_base_token(
    state: &MarketState,
    address: &str,
) -> Result<Option<Asset>, ApiError> {
    let address = address.to_ascii_lowercase();
    if let Some((at, asset)) = state.pasted.lock().map_err(internal)?.get(&address) {
        if at.elapsed() < CATALOG_TTL {
            return Ok(asset.clone());
        }
    }
    // USDC is cash and WETH is the gas tank: neither is traded here.
    if address == BASE_USDC.to_ascii_lowercase() || address == BASE_WETH.to_ascii_lowercase() {
        return Ok(None);
    }
    let Ok((name, symbol, decimals)) = state.base.token_info(&address).await else {
        return Ok(None);
    };
    let gecko = gecko_base_token(state, &address).await;
    if gecko.as_ref().is_some_and(|g| g.honeypot) {
        return Ok(None);
    }
    let liquid = match &gecko {
        Some(g) if g.reserve_usd >= MIN_PASTED_LIQUIDITY_USD => true,
        _ => kyber_depth_ok(state, &address).await,
    };
    if !liquid {
        return Ok(None);
    }
    let asset = Asset {
        id: base_token_id(&address),
        symbol,
        name,
        kind: "meme".into(),
        chain: "base".into(),
        token: address.clone(),
        decimals,
        icon_url: gecko.as_ref().and_then(|g| g.icon.clone()),
        volume_24h: gecko.as_ref().map_or(0.0, |g| g.volume_usd),
        xstock: false,
        verified: false,
        ref_price: 0.0,
        change_24h: 0.0,
        listed: true,
    };
    state
        .pasted
        .lock()
        .map_err(internal)?
        .insert(address, (Instant::now(), Some(asset.clone())));
    Ok(Some(asset))
}

// GeckoTerminal's trending Base pools → coins for the Trade list: CoinGecko-listed (`listed`, the
// exact contract), liquid like the catalog, not cash, not a bridged copy, and not a coin Atlas
// already lists (`taken`, uppercase symbols). Kinds come later (`meme_or_crypto`).
fn trending_base_assets(
    body: &Value,
    listed: impl Fn(&str) -> bool,
    taken: &std::collections::HashSet<String>,
) -> Vec<(Asset, Option<String>)> {
    let tokens: HashMap<&str, &Value> = body["included"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|t| Some((t["id"].as_str()?, &t["attributes"])))
                .collect()
        })
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for pool in body["data"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let at = &pool["attributes"];
        let Some(token) = pool["relationships"]["base_token"]["data"]["id"]
            .as_str()
            .and_then(|id| tokens.get(id))
        else {
            continue;
        };
        let (Some(address), Some(symbol)) = (token["address"].as_str(), token["symbol"].as_str())
        else {
            continue;
        };
        let address = address.to_ascii_lowercase();
        let upper = symbol.to_ascii_uppercase();
        let coingecko = token["coingecko_coin_id"].as_str().map(str::to_owned);
        if !looks_like_evm_address(&address)
            || gecko_number(&at["reserve_in_usd"]) < MIN_LIQUIDITY_USD
            || !listed(&address)
            || upper.contains("USD")
            || upper.contains("EUR")
            || upper.contains("ETH")
            || upper.contains("BTC")
            || coingecko
                .as_deref()
                .is_some_and(|id| id.contains("bridged"))
            || taken.contains(&upper)
            || !seen.insert(address.clone())
        {
            continue;
        }
        let Some(decimals) = token["decimals"]
            .as_u64()
            .and_then(|d| u32::try_from(d).ok())
        else {
            continue;
        };
        found.push((
            Asset {
                id: base_token_id(&address),
                symbol: symbol.into(),
                name: token["name"].as_str().unwrap_or(symbol).into(),
                kind: "crypto".into(),
                chain: "base".into(),
                token: address,
                decimals,
                icon_url: gecko_icon(&token["image_url"]),
                volume_24h: gecko_number(&at["volume_usd"]["h24"]),
                xstock: false,
                verified: true,
                ref_price: gecko_number(&at["base_token_price_usd"]),
                change_24h: gecko_number(&at["price_change_percentage"]["h24"]),
                listed: true,
            },
            coingecko,
        ));
    }
    found
}

// Whether CoinGecko files a coin under memes; asked once per coin, remembered.
async fn meme_or_crypto(state: &MarketState, coingecko_id: &str) -> Option<&'static str> {
    if let Some(kind) = state.coin_kinds.lock().ok()?.get(coingecko_id) {
        return Some(kind);
    }
    let body: Value = state
        .http
        .get(format!(
            "https://api.coingecko.com/api/v3/coins/{coingecko_id}"
        ))
        .query(&[
            ("localization", "false"),
            ("tickers", "false"),
            ("market_data", "false"),
            ("community_data", "false"),
            ("developer_data", "false"),
        ])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let meme = body["categories"].as_array().is_some_and(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .any(|c| c.to_ascii_lowercase().contains("meme"))
    });
    let kind = if meme { "meme" } else { "crypto" };
    state
        .coin_kinds
        .lock()
        .ok()?
        .insert(coingecko_id.to_owned(), kind);
    Some(kind)
}

// The Base coins trending right now (see trending_base_assets), read every 10 minutes. When
// GeckoTerminal is out of reach the last good list stays, however old.
pub(super) async fn base_trending(state: &AppState) -> Arc<Vec<Asset>> {
    let last = state
        .markets
        .base_trending
        .lock()
        .ok()
        .and_then(|held| held.clone());
    if let Some((at, list)) = &last {
        if at.elapsed() < BASE_TRENDING_TTL {
            return list.clone();
        }
    }
    let previous = last.map(|(_, list)| list).unwrap_or_default();
    let fetched: Option<Value> = async {
        state
            .markets
            .http
            .get(format!("{GECKO_BASE}trending_pools"))
            .query(&[("include", "base_token"), ("page", "1")])
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()
    }
    .await;
    let (Some(body), Ok(catalog)) = (fetched, catalog(&state.markets).await) else {
        // Try again in a minute rather than waiting out the full ten.
        if let Ok(mut held) = state.markets.base_trending.lock() {
            *held = Some((
                Instant::now() - BASE_TRENDING_TTL + Duration::from_secs(60),
                previous.clone(),
            ));
        }
        return previous;
    };
    let listed = state.near.listed().await;
    let taken = catalog
        .iter()
        .filter(|a| a.listed)
        .map(|a| a.symbol.to_ascii_uppercase())
        .collect();
    let mut assets = Vec::new();
    for (mut asset, coingecko) in trending_base_assets(&body, |a| listed.has("base", a), &taken) {
        if let Some(kind) = match coingecko {
            Some(id) => meme_or_crypto(&state.markets, &id).await,
            None => None,
        } {
            asset.kind = kind.into();
        }
        assets.push(asset);
    }
    let assets = Arc::new(assets);
    if let Ok(mut held) = state.markets.base_trending.lock() {
        *held = Some((Instant::now(), assets.clone()));
    }
    assets
}

// The trending Base list stays warm, so a Trade list never waits on GeckoTerminal and CoinGecko.
pub(super) fn keep_base_trending_warm(state: AppState) {
    tokio::spawn(async move {
        loop {
            base_trending(&state).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

// How many base units of a Base token $1 buys right now (Kyber, then Uniswap), shared for 15 seconds.
pub(super) async fn base_rate(state: &MarketState, a: &Asset) -> Option<u128> {
    if let Some((at, rate)) = state.base_rates.lock().ok()?.get(&a.token) {
        if at.elapsed() < BASE_RATE_TTL {
            return Some(*rate);
        }
    }
    let rate = base_quote(state, a, "buy", 1_000_000)
        .await
        .ok()?
        .amount_out;
    state
        .base_rates
        .lock()
        .ok()?
        .insert(a.token.clone(), (Instant::now(), rate));
    (rate > 0).then_some(rate)
}

// Every asset Atlas trades: the Base list plus Jupiter's verified Solana catalog, refreshed every
// 30 minutes. If Jupiter is down, the last good catalog keeps serving.
pub(super) async fn catalog(state: &MarketState) -> Result<Arc<Vec<Asset>>, ApiError> {
    let cached = state.catalog.lock().map_err(internal)?.clone();
    if let Some((at, assets)) = &cached {
        if at.elapsed() < CATALOG_TTL {
            return Ok(assets.clone());
        }
    }
    let fetched: Result<Vec<Value>, reqwest::Error> = async {
        jupiter_get(&state.http, "/tokens/v2/tag?query=verified")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
    .await;
    let tokens = match fetched {
        Ok(tokens) => tokens,
        Err(error) => {
            return cached
                .map(|(_, assets)| assets)
                .ok_or_else(|| unavailable(format!("Jupiter token list unavailable: {error}")));
        }
    };
    let assets = Arc::new(curate(&tokens));
    *state.catalog.lock().map_err(internal)? = Some((Instant::now(), assets.clone()));
    Ok(assets)
}

// Trending: what's rising most, with each kind judged against its own kind. A meme up 300% and a
// stock up 6% can both be the top mover of their kind, so the top meme, stock and coin all lead.
// Assets that aren't rising follow, most traded first.
fn trending_order(assets: Vec<&Asset>) -> Vec<&Asset> {
    let mut scored: Vec<(f64, &Asset)> = Vec::with_capacity(assets.len());
    let mut kinds: Vec<&str> = assets.iter().map(|a| a.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    for kind in kinds {
        let mut rising: Vec<&Asset> = assets
            .iter()
            .copied()
            .filter(|a| a.kind == kind && a.change_24h > 0.0)
            .collect();
        rising.sort_by(|a, b| b.change_24h.total_cmp(&a.change_24h));
        let n = rising.len() as f64;
        for (i, a) in rising.into_iter().enumerate() {
            // 1.0 for the top mover of its kind, down towards 0 for the last one rising.
            scored.push((1.0 - i as f64 / n, a));
        }
    }
    scored.sort_by(|(sa, a), (sb, b)| sb.total_cmp(sa).then(b.change_24h.total_cmp(&a.change_24h)));
    let mut rest: Vec<&Asset> = assets
        .into_iter()
        .filter(|a| !(a.change_24h > 0.0))
        .collect();
    rest.sort_by(|a, b| b.volume_24h.total_cmp(&a.volume_24h));
    scored.into_iter().map(|(_, a)| a).chain(rest).collect()
}

// Jupiter's list → Atlas's catalog: one row per coin. Wrapped copies of a major give way to its
// backing copy, and within a kind one row per symbol (the most traded wins).
fn curate(tokens: &[Value]) -> Vec<Asset> {
    let mut solana: Vec<Asset> = tokens.iter().filter_map(solana_asset).collect();
    let majors: Vec<(&str, f64)> = MAJORS
        .iter()
        .filter_map(|(mint, symbol, _)| {
            let price = solana.iter().find(|a| a.id == *mint)?.ref_price;
            Some((*symbol, price))
        })
        .collect();
    let copy_of_major = |a: &Asset| {
        a.kind == "crypto"
            && !MAJORS.iter().any(|(mint, _, _)| *mint == a.id)
            && majors.iter().any(|(symbol, price)| {
                a.symbol.to_ascii_uppercase().contains(symbol)
                    && *price > 0.0
                    && (a.ref_price - price).abs() <= price * COPY_PRICE_BAND
            })
    };
    solana.retain(|a| !copy_of_major(a));
    solana.sort_by(|a, b| b.volume_24h.total_cmp(&a.volume_24h));
    let mut symbols = std::collections::HashSet::new();
    let mut mints = std::collections::HashSet::new();
    let mut assets = base_assets();
    for asset in solana {
        if mints.insert(asset.id.clone())
            && symbols.insert((asset.symbol.to_ascii_uppercase(), asset.kind.clone()))
        {
            assets.push(asset);
        }
    }
    assets
}

pub(super) async fn find_asset(state: &MarketState, id: &str) -> Result<Asset, ApiError> {
    let catalog = catalog(state).await?;
    if let Some(asset) = catalog.iter().find(|a| a.id == id) {
        return Ok(asset.clone());
    }
    if let Some(address) = id
        .strip_prefix("base:")
        .filter(|a| looks_like_evm_address(a))
    {
        // A coin on the fixed Base list keeps its own row.
        if let Some(asset) = catalog
            .iter()
            .find(|a| a.chain == "base" && a.token.eq_ignore_ascii_case(address))
        {
            return Ok(asset.clone());
        }
        if let Some(asset) = pasted_base_token(state, address).await? {
            return Ok(asset);
        }
    }
    if looks_like_mint(id) {
        if let Some(asset) = pasted_token(state, id).await? {
            return Ok(asset);
        }
    }
    Err((StatusCode::NOT_FOUND, "unsupported asset".into()))
}

// Display statistics resolve a coin without warming the trading catalog first.
pub(super) fn stats_token(id: &str) -> Option<(&'static str, String)> {
    if looks_like_mint(id) {
        return Some(("solana", id.into()));
    }
    if let Some(address) = id
        .strip_prefix("base:")
        .filter(|a| looks_like_evm_address(a))
    {
        return Some(("base", address.into()));
    }
    base_assets()
        .into_iter()
        .find(|a| a.id == id)
        .map(|a| ("base", a.token))
}

// Live USD price and 24h change for Solana mints, from Jupiter, 50 at a time, shared for 15 seconds.
pub(super) async fn usd_prices(
    state: &MarketState,
    mints: &[String],
) -> Result<HashMap<String, (f64, Option<f64>)>, ApiError> {
    let mut result = HashMap::new();
    let mut missing = Vec::new();
    {
        let cache = state.prices.lock().map_err(internal)?;
        for mint in mints {
            match cache.get(mint) {
                Some((at, price, change)) if at.elapsed() < PRICE_TTL => {
                    result.insert(mint.clone(), (*price, *change));
                }
                _ => missing.push(mint.clone()),
            }
        }
    }
    let mut refused = None;
    for chunk in missing.chunks(50) {
        let fetched: Result<Value, ApiError> = async {
            jupiter_get(&state.http, "/price/v3")
                .query(&[("ids", chunk.join(","))])
                .send()
                .await
                .map_err(unavailable)?
                .error_for_status()
                .map_err(unavailable)?
                .json()
                .await
                .map_err(unavailable)
        }
        .await;
        let body = match fetched {
            Ok(body) => body,
            // Refused or down: the last prices from the past few minutes stand in.
            Err(error) => {
                let cache = state.prices.lock().map_err(internal)?;
                for mint in chunk {
                    if let Some((at, price, change)) = cache.get(mint) {
                        if at.elapsed() < PRICE_STALE_OK {
                            result.insert(mint.clone(), (*price, *change));
                        }
                    }
                }
                refused = Some(error);
                continue;
            }
        };
        let mut cache = state.prices.lock().map_err(internal)?;
        for mint in chunk {
            let Some(price) = body[mint]["usdPrice"].as_f64().filter(|p| *p > 0.0) else {
                continue;
            };
            let change = body[mint]["priceChange24h"].as_f64();
            cache.insert(mint.clone(), (Instant::now(), price, change));
            result.insert(mint.clone(), (price, change));
        }
    }
    match refused {
        Some(error) if result.is_empty() && !mints.is_empty() => Err(error),
        _ => Ok(result),
    }
}

#[derive(Clone)]
pub(super) struct MarketState {
    pub(super) base: UniswapV3Client,
    kyber: KyberClient,
    jupiter: JupiterClient,
    pub(super) kora: engine_execution::kora::Client,
    pub(super) gas_near: engine_execution::near_intents::Client,
    rpc: reqwest::Url,
    http: reqwest::Client,
    quotes: Arc<Mutex<HashMap<String, StoredQuote>>>,
    intents: Arc<Mutex<HashMap<String, StoredIntent>>>,
    catalog: Arc<Mutex<Option<(Instant, Arc<Vec<Asset>>)>>>,
    pasted: Arc<Mutex<HashMap<String, (Instant, Option<Asset>)>>>,
    prices: Arc<Mutex<HashMap<String, (Instant, f64, Option<f64>)>>>,
    // Base tokens: how many base units $1 buys (Kyber), shared for a few seconds.
    base_rates: Arc<Mutex<HashMap<String, (Instant, u128)>>>,
    // Base coins trending on GeckoTerminal, and when they were read (the last good list stays).
    base_trending: Arc<Mutex<Option<(Instant, Arc<Vec<Asset>>)>>>,
    // CoinGecko's word on whether a coin (by CoinGecko id) is a meme; it doesn't change.
    coin_kinds: Arc<Mutex<HashMap<String, &'static str>>>,
    multipliers: Arc<Mutex<HashMap<String, (Instant, bool)>>>,
    chart_pools: Arc<Mutex<HashMap<String, (Instant, String)>>>,
    charts: ChartCache,
    // Complete list and search answers, by currency, chip and words, for a few seconds.
    searches: Arc<Mutex<HashMap<String, (Instant, Value)>>>,
    // Spot, send and Earn intents outlive a restart here; without DATABASE_URL they stay in `intents`.
    postgres: Option<Arc<tokio_postgres::Client>>,
}
type ChartCache = Arc<Mutex<HashMap<String, (Instant, Arc<Vec<(u64, f64)>>)>>>;
#[derive(Clone)]
struct StoredQuote {
    owner: String,
    asset: Asset,
    side: String,
    currency: String,
    input_units: u128,
    output_units: u128,
    // USDC (6 decimals) to move over before the buy: from Base for a Solana buy, from Solana for a
    // Base buy; 0 when cash on the asset's chain covers it.
    funding_units: u128,
    // The move's fee inside funding_units, shown as the network fee.
    funding_fee: u128,
    fee_preview: Option<solana_fees::Preview>,
    expires: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct StoredIntent {
    owner: String,
    wallet: String,
    chain: String,
    expected: Vec<(String, String)>,
    request_id: Option<String>,
    #[serde(default)]
    own_swap: Option<UserPaidSwap>,
    #[serde(default)]
    fee_preview: Option<solana_fees::Preview>,
    #[serde(default)]
    fee_reserve: Option<solana_fees::Reserve>,
    status: IntentStatus,
    // Set for spot buys and sells, so the fill can be kept as a trade.
    trade: Option<PlannedTrade>,
    // A Solana buy paid with Base cash: Layerswap moves it first (the Base transfer is `expected`).
    #[serde(default)]
    funding: Option<CashMove>,
    // A funded Solana step that isn't a trade (Earn into Jupiter Lend): what /next swaps USDC into.
    #[serde(default)]
    buy_mint: Option<String>,
    // A gas top-up (Jupiter order) that runs just before the main Solana transaction.
    #[serde(default)]
    gas_request_id: Option<String>,
    // A plain Solana USDC transfer (a friend send), landed by the engine rather than Jupiter.
    #[serde(default)]
    solana_transfer: bool,
    // Base transactions from a wallet with no ETH: a gasless CoW top-up runs first, then /next hands
    // out `expected`.
    #[serde(default)]
    base_topup: Option<BaseTopup>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct BaseTopup {
    pub(super) uid: Option<String>,
    #[serde(default)]
    pub(super) permit: Option<Value>,
    #[serde(default)]
    pub(super) order: Option<engine_execution::cow::GasOrder>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CashMove {
    swap_id: String,
    amount_units: u128,
    tx_hash: Option<String>,
    // Moved by Relay without Base gas (`swap_id` is Relay's request): what the user's wallet signs
    // once they confirm. None for a Layerswap move started by a transaction.
    #[serde(default)]
    authorization: Option<Authorization>,
    // Moved by Relay from Solana with a transaction the engine landed (`swap_id` is Relay's request).
    #[serde(default)]
    relay: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Authorization {
    typed_data: Value,
    // Relay's name for the flow, handed back with the signature.
    api: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct PlannedTrade {
    asset_id: String,
    side: String,
    pay_units: u128,
    // What the quote expects back; the fill's real amount replaces it when the venue reports one.
    get_units: u128,
    // Base: the token the wallet receives (lowercase), read from the swap receipt.
    receive_token: String,
    // A Base buy whose swap /next handed out: the wallet's next nonce then. Once the wallet has moved
    // past it, those transactions went out, and no fresh swap is handed out again.
    #[serde(default)]
    handed_nonce: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IntentStatus {
    pub(super) intent_id: String,
    pub(super) stage: String,
    pub(super) state: String,
    pub(super) tx_ids: Vec<String>,
    pub(super) error: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct AssetsQuery {
    currency: Option<String>,
    category: Option<String>,
    q: Option<String>,
    // A search in two answers: "listed" (Atlas's own coins, fast) or "other" (other chains and Base
    // coins by name, slower). Both when absent.
    part: Option<String>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuoteRequest {
    pub(super) asset_id: String,
    pub(super) side: String,
    pub(super) amount: Money,
    // A sell of everything held (Max): the whole holding, whatever `amount` says it's worth.
    #[serde(default)]
    pub(super) all: bool,
}
#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Money {
    pub(super) amount: String,
    pub(super) currency: String,
}
#[derive(Deserialize)]
pub(super) struct Submission {
    pub(super) sent: Vec<Sent>,
    pub(super) signed: Vec<Signed>,
}
#[derive(Deserialize)]
pub(super) struct Sent {
    pub(super) chain: String,
    pub(super) id: String,
}
#[derive(Deserialize)]
pub(super) struct Signed {
    pub(super) index: usize,
    pub(super) transaction: String,
}

impl MarketState {
    pub(super) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let rpc: reqwest::Url = env_url(
            "ATLAS_BASE_MAINNET_RPC_URL",
            "https://base-rpc.publicnode.com",
        )
        .parse()?;
        Ok(Self {
            base: UniswapV3Client::new(rpc.clone())?,
            kyber: KyberClient::new()?,
            jupiter: JupiterClient::new(env::var("JUPITER_API_KEY").ok()),
            kora: engine_execution::kora::Client::new(
                &env_url("ATLAS_KORA_RPC_URL", engine_execution::kora::PUBLIC_MAINNET),
                env::var("KORA_API_KEY").ok(),
            )?,
            gas_near: engine_execution::near_intents::Client::new(
                env::var("NEAR_INTENTS_API_KEY").ok(),
            )?,
            rpc,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("atlas-engine")
                .build()?,
            quotes: Arc::new(Mutex::new(HashMap::new())),
            intents: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(Mutex::new(None)),
            pasted: Arc::new(Mutex::new(HashMap::new())),
            prices: Arc::new(Mutex::new(HashMap::new())),
            base_rates: Arc::new(Mutex::new(HashMap::new())),
            base_trending: Arc::new(Mutex::new(None)),
            coin_kinds: Arc::new(Mutex::new(HashMap::new())),
            multipliers: Arc::new(Mutex::new(HashMap::new())),
            chart_pools: Arc::new(Mutex::new(HashMap::new())),
            charts: Arc::new(Mutex::new(HashMap::new())),
            searches: Arc::new(Mutex::new(HashMap::new())),
            postgres: None,
        })
    }
    // /next may race a signature report. Never overwrite a sent step with a fresh unsigned plan.
    async fn replace_plan(
        &self,
        id: &str,
        before: &StoredIntent,
        after: &StoredIntent,
    ) -> Result<bool, ApiError> {
        let previous = serde_json::to_string(before).map_err(internal)?;
        let payload = serde_json::to_string(after).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            return Ok(pg.execute("UPDATE atlas_intents SET payload=$2,stage=$3,updated_at_ms=$4 WHERE intent_id=$1 AND payload=$5",
                &[&id,&payload,&after.status.stage,&now_i64(),&previous]).await.map_err(internal)? == 1);
        }
        let mut intents = self.intents.lock().map_err(internal)?;
        let Some(current) = intents.get(id) else {
            return Ok(false);
        };
        if serde_json::to_string(current).map_err(internal)? != previous {
            return Ok(false);
        }
        intents.insert(id.into(), after.clone());
        Ok(true)
    }
    // Typing a word again, or going back to the list, answers at once; prices are already shared
    // for longer than this.
    fn recent_search(&self, key: &str) -> Option<Value> {
        let held = self.searches.lock().ok()?;
        let (at, answer) = held.get(key)?;
        (at.elapsed() < SEARCH_TTL).then(|| answer.clone())
    }
    fn keep_search(&self, key: String, answer: &Value) {
        if let Ok(mut held) = self.searches.lock() {
            held.retain(|_, (at, _)| at.elapsed() < SEARCH_TTL);
            held.insert(key, (Instant::now(), answer.clone()));
        }
    }
    pub(super) async fn with_database(mut self) -> Result<Self, Box<dyn std::error::Error>> {
        if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("intents database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_intents (
                    intent_id TEXT PRIMARY KEY,
                    owner TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    updated_at_ms BIGINT NOT NULL
                )",
                )
                .await?;
            self.postgres = Some(Arc::new(client));
        }
        Ok(self)
    }
    async fn insert_intent(&self, id: &str, intent: &StoredIntent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(intent).map_err(internal)?;
            pg.execute(
                "INSERT INTO atlas_intents (intent_id,owner,payload,stage,updated_at_ms) VALUES ($1,$2,$3,$4,$5)",
                &[&id, &intent.owner, &payload, &intent.status.stage, &now_i64()],
            )
            .await
            .map_err(internal)?;
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .insert(id.into(), intent.clone());
        }
        Ok(())
    }
    async fn get_intent(&self, id: &str) -> Result<Option<StoredIntent>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT payload FROM atlas_intents WHERE intent_id=$1",
                    &[&id],
                )
                .await
                .map_err(internal)?;
            return row
                .map(|r| serde_json::from_str(r.get::<_, &str>(0)).map_err(internal))
                .transpose();
        }
        Ok(self.intents.lock().map_err(internal)?.get(id).cloned())
    }
    async fn save_intent(&self, id: &str, intent: &StoredIntent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(intent).map_err(internal)?;
            pg.execute(
                "UPDATE atlas_intents SET payload=$2,stage=$3,updated_at_ms=$4 WHERE intent_id=$1",
                &[&id, &payload, &intent.status.stage, &now_i64()],
            )
            .await
            .map_err(internal)?;
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .insert(id.into(), intent.clone());
        }
        Ok(())
    }
    // Moves a Solana intent from validate to execute exactly once, so a repeated /signed can't hand
    // Jupiter the same order twice. False if another request already did.
    async fn claim_execution(
        &self,
        id: &str,
        intent: &StoredIntent,
        from: &str,
    ) -> Result<bool, ApiError> {
        self.claim_execution_to(id, intent, from, "execute").await
    }

    async fn claim_execution_to(
        &self,
        id: &str,
        intent: &StoredIntent,
        from: &str,
        to: &str,
    ) -> Result<bool, ApiError> {
        let mut claimed = intent.clone();
        claimed.status.stage = to.into();
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&claimed).map_err(internal)?;
            let rows = pg
                .execute(
                    "UPDATE atlas_intents SET payload=$2,stage=$5,updated_at_ms=$3
                     WHERE intent_id=$1 AND stage=$4",
                    &[&id, &payload, &now_i64(), &from, &to],
                )
                .await
                .map_err(internal)?;
            return Ok(rows == 1);
        }
        let mut intents = self.intents.lock().map_err(internal)?;
        match intents.get_mut(id) {
            Some(stored) if stored.status.stage == from => {
                *stored = claimed;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl MarketState {
    // A plan of Base transactions the user sends in order; /signed and status check each against it.
    pub(super) async fn register_base_txs(
        &self,
        owner: String,
        wallet: String,
        txs: Vec<(String, String)>,
        topup: bool,
    ) -> Result<String, ApiError> {
        let intent_id = id("intent");
        let status = IntentStatus {
            intent_id: intent_id.clone(),
            stage: "validate".into(),
            state: "pending".into(),
            tx_ids: Vec::new(),
            error: None,
        };
        self.insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet,
                chain: "base".into(),
                expected: txs
                    .into_iter()
                    .map(|(to, data)| (to.to_ascii_lowercase(), data.to_ascii_lowercase()))
                    .collect(),
                request_id: None,
                own_swap: None,
                fee_preview: None,
                fee_reserve: None,
                status,
                trade: None,
                funding: None,
                buy_mint: None,
                gas_request_id: None,
                solana_transfer: false,
                base_topup: topup.then_some(BaseTopup {
                    uid: None,
                    permit: None,
                    order: None,
                }),
            },
        )
        .await?;
        Ok(intent_id)
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn now_i64() -> i64 {
    i64::try_from(now()).unwrap_or(i64::MAX)
}
fn id(prefix: &str) -> String {
    format!(
        "{prefix}-{:x}-{:x}",
        now(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
pub(super) fn unavailable(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}
pub(super) fn checked_currency(currency: &str) -> Result<(), ApiError> {
    if DISPLAY_CURRENCIES.contains(&currency) {
        Ok(())
    } else {
        Err(bad("unsupported display currency"))
    }
}
pub(super) fn parse_micros(s: &str) -> Result<u128, ApiError> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty()
        || !whole.bytes().all(|x| x.is_ascii_digit())
        || !frac.bytes().all(|x| x.is_ascii_digit())
        || frac.len() > 6
    {
        return Err(bad(
            "amount must be a positive decimal with at most six places",
        ));
    }
    let w: u128 = whole.parse().map_err(|_| bad("amount too large"))?;
    let f: u128 = format!("{:0<6}", frac)
        .parse()
        .map_err(|_| bad("invalid amount"))?;
    w.checked_mul(1_000_000)
        .and_then(|v| v.checked_add(f))
        .filter(|v| *v > 0)
        .ok_or_else(|| bad("amount must be positive"))
}
// Every buy, sell, send and savings move is between $0.10 and $10,000 (USDC units).
pub(super) const MIN_USDC: u128 = 100_000;
pub(super) const MAX_USDC: u128 = 10_000_000_000;

fn currency_symbol(currency: &str) -> &'static str {
    match currency {
        "NGN" => "₦",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "KES" => "KSh ",
        "GHS" => "GH₵",
        "ZAR" => "R",
        _ => "",
    }
}
fn group_digits(n: u128) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
// Money in a sentence people read, in their own currency: "₦1,504.69".
pub(super) fn say_money(usdc_units: u128, currency: &str, rate: u128) -> String {
    say_micros(usdc_units.saturating_mul(rate) / 1_000_000, currency)
}
// An amount already in their currency, in micros (6 decimals): "₦15,000.00".
pub(super) fn say_micros(micros: u128, currency: &str) -> String {
    format!(
        "{}{}.{:02}",
        currency_symbol(currency),
        group_digits(micros / 1_000_000),
        (micros % 1_000_000) / 10_000
    )
}
// A limit in whole units of their currency, rounded so it stays inside the limit: "₦151".
fn say_limit(usdc_units: u128, currency: &str, rate: u128, round_up: bool) -> String {
    let micros = usdc_units.saturating_mul(rate) / 1_000_000;
    let whole = if round_up {
        micros.div_ceil(1_000_000)
    } else {
        micros / 1_000_000
    };
    format!("{}{}", currency_symbol(currency), group_digits(whole))
}
// The $0.10–$10,000 range, said in the currency the user typed in.
pub(super) fn check_limits(usdc_units: u128, currency: &str, rate: u128) -> Result<(), ApiError> {
    if usdc_units < MIN_USDC {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "The smallest amount is {}",
                say_limit(MIN_USDC, currency, rate, true)
            ),
        ));
    }
    if usdc_units > MAX_USDC {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "The most at once is {}",
                say_limit(MAX_USDC, currency, rate, false)
            ),
        ));
    }
    Ok(())
}
// Short of cash: the balance is one balance, so say so plainly and point to adding money.
pub(super) fn short_of_cash() -> ApiError {
    (
        StatusCode::CONFLICT,
        "Not enough in your balance for this. Add money to continue.".into(),
    )
}
// A Base action the wallet can't pay gas for: no ETH, no USDC to spare for a top-up, no Solana cash
// to hop over with ETH. Atlas never pays gas, so they're asked for a little more.
pub(super) fn short_of_gas(currency: &str, rate: u128) -> ApiError {
    (
        StatusCode::CONFLICT,
        format!(
            "Add money to cover network fees (about {} more), then try again.",
            say_money(BASE_GAS_REFILL_USDC, currency, rate)
        ),
    )
}
// The same, with the cash they do have across every chain.
pub(super) fn not_enough_cash(cash: u128, currency: &str, rate: u128) -> ApiError {
    (
        StatusCode::CONFLICT,
        format!(
            "Not enough in your balance for this. You have {} to spend. Add money to continue.",
            say_money(cash, currency, rate)
        ),
    )
}

pub(super) fn format_units(units: u128, decimals: u32) -> String {
    let scale = 10u128.pow(decimals);
    let mut result = format!(
        "{}.{:0width$}",
        units / scale,
        units % scale,
        width = decimals as usize
    );
    while result.ends_with('0') {
        result.pop();
    }
    if result.ends_with('.') {
        result.pop();
    }
    result
}
fn money_from_usdc(units: u128, currency: &str, rate: u128) -> Result<Money, ApiError> {
    let scaled = units
        .checked_mul(rate)
        .ok_or_else(|| bad("amount too large"))?
        / 1_000_000;
    Ok(Money {
        amount: format_units(scaled, 6),
        currency: currency.into(),
    })
}
pub(super) fn unit_price(
    input_units: u128,
    output_units: u128,
    token_decimals: u32,
    currency: &str,
    rate: u128,
) -> Result<Money, ApiError> {
    if output_units == 0 {
        return Err(unavailable("venue returned zero output"));
    }
    let exact = input_units
        .checked_mul(rate)
        .and_then(|v| v.checked_mul(10u128.pow(token_decimals)))
        .zip(output_units.checked_mul(1_000_000))
        .and_then(|(n, d)| Some((n / d, (n % d).checked_mul(1_000_000)? / d)));
    let mut amount = match exact {
        Some((micros, tail)) => format!(
            "{}.{:06}{:06}",
            micros / 1_000_000,
            micros % 1_000_000,
            tail
        ),
        // 24-decimal coins (NEAR) priced in naira outgrow u128 here; a float keeps 15 significant
        // digits, more than any price shows.
        None => {
            let price = input_units as f64 * rate as f64 / 1e12 / output_units as f64
                * 10f64.powi(token_decimals as i32);
            if !price.is_finite() {
                return Err(bad("price overflow"));
            }
            format!("{price:.12}")
        }
    };
    while amount.ends_with('0') {
        amount.pop();
    }
    if amount.ends_with('.') {
        amount.pop();
    }
    Ok(Money {
        amount,
        currency: currency.into(),
    })
}
fn quote_request(a: &Asset, side: &str, input: u128) -> BaseSwapRequest {
    BaseSwapRequest {
        source_token: if side == "buy" { BASE_USDC } else { &a.token }.into(),
        destination_token: if side == "buy" { &a.token } else { BASE_USDC }.into(),
        amount_base_units: input,
    }
}

// A Base price: KyberSwap's search of every exchange, or one Uniswap pool when Kyber can't answer.
// `fee_ppm` is in millionths of what goes in: Kyber's network fee (paid in ETH), or the pool's fee.
pub(super) struct BaseQuote {
    pub(super) amount_in: u128,
    pub(super) amount_out: u128,
    pub(super) fee_ppm: u32,
    route: BaseRoute,
}
enum BaseRoute {
    Kyber(KyberRoute),
    Uniswap(BaseV3Quote),
}
pub(super) async fn base_quote(
    state: &MarketState,
    a: &Asset,
    side: &str,
    input: u128,
) -> Result<BaseQuote, ApiError> {
    let request = quote_request(a, side, input);
    match state
        .kyber
        .route(&request.source_token, &request.destination_token, input)
        .await
    {
        Ok(route) => {
            let fee_ppm = if route.amount_in_usd > 0.0 {
                (route.gas_usd / route.amount_in_usd * 1_000_000.0).min(1_000_000.0) as u32
            } else {
                0
            };
            return Ok(BaseQuote {
                amount_in: route.amount_in,
                amount_out: route.amount_out,
                fee_ppm,
                route: BaseRoute::Kyber(route),
            });
        }
        Err(error) => eprintln!("kyberswap {}: {error}; trying Uniswap", a.symbol),
    }
    uniswap_quote(state, &request).await
}
async fn uniswap_quote(
    state: &MarketState,
    request: &BaseSwapRequest,
) -> Result<BaseQuote, ApiError> {
    let q = state
        .base
        .quote_direct(request)
        .await
        .map_err(unavailable)?;
    Ok(BaseQuote {
        amount_in: q.amount_in,
        amount_out: q.amount_out,
        fee_ppm: q.fee,
        route: BaseRoute::Uniswap(q),
    })
}
// A built swap pays out within 1% of its route, and expires after this long: long enough to cover a
// gas top-up that runs first.
const BASE_SLIPPAGE_BPS: u16 = 100;
const BASE_SWAP_DEADLINE_SECS: u64 = 20 * 60;
// A Base swap from `wallet`, ready to send: an exact approval for its router when the allowance falls
// short, then the swap, which always pays `wallet`. If Kyber's build fails, a Uniswap pool's swap
// takes its place; the amounts are the ones built, for the caller's price checks.
pub(super) struct BaseSwap {
    pub(super) amount_in: u128,
    pub(super) amount_out: u128,
    pub(super) txs: Vec<(String, String)>,
}
async fn base_swap(
    state: &MarketState,
    a: &Asset,
    side: &str,
    input: u128,
    wallet: &str,
) -> Result<BaseSwap, ApiError> {
    let quote = base_quote(state, a, side, input).await?;
    let request = quote_request(a, side, input);
    let built = match &quote.route {
        BaseRoute::Kyber(route) => {
            let deadline = now() / 1000 + BASE_SWAP_DEADLINE_SECS;
            match state
                .kyber
                .build(route, wallet, BASE_SLIPPAGE_BPS, deadline)
                .await
            {
                Ok(swap) => Some((quote.amount_out, KYBER_ROUTER, (swap.to, swap.data))),
                Err(error) => {
                    eprintln!("kyberswap build {}: {error}; trying Uniswap", a.symbol);
                    None
                }
            }
        }
        BaseRoute::Uniswap(_) => None,
    };
    let (amount_out, spender, swap) = match built {
        Some(built) => built,
        None => {
            let fallback = match quote.route {
                BaseRoute::Uniswap(q) => q,
                BaseRoute::Kyber(_) => state
                    .base
                    .quote_direct(&request)
                    .await
                    .map_err(unavailable)?,
            };
            let swap = state
                .base
                .swap_transaction(&fallback, wallet, BASE_SLIPPAGE_BPS)
                .map_err(unavailable)?;
            (fallback.amount_out, SWAP_ROUTER_02, (swap.to, swap.data))
        }
    };
    let mut txs = Vec::new();
    let allowance = state
        .base
        .allowance_to(&request.source_token, wallet, spender)
        .await
        .map_err(unavailable)?;
    if allowance < input {
        let approval = state
            .base
            .approval_to(&request.source_token, wallet, spender, input)
            .map_err(unavailable)?;
        txs.push((approval.to, approval.data));
    }
    txs.push(swap);
    Ok(BaseSwap {
        amount_in: input,
        amount_out,
        txs,
    })
}
// xStocks track shares through a multiplier that drifts above 1 as dividends accrue; prices are per
// token, so that drift doesn't change what anyone pays or gets. A scheduled change (a split or
// reverse split, `newMultiplier`) does jump the token's value, so quotes pause while one is pending.
async fn ensure_stock_units(state: &MarketState, a: &Asset) -> Result<(), ApiError> {
    if !a.xstock {
        return Ok(());
    }
    if let Some((at, ok)) = state.multipliers.lock().map_err(internal)?.get(&a.symbol) {
        if at.elapsed() < MULTIPLIER_TTL {
            return if *ok {
                Ok(())
            } else {
                Err(paused_stock(&a.symbol))
            };
        }
    }
    let mut url =
        reqwest::Url::parse("https://api.xstocks.fi/api/v2/public/assets/").map_err(unavailable)?;
    url.path_segments_mut()
        .map_err(|_| unavailable("xStocks URL"))?
        .pop_if_empty()
        .push(&a.symbol)
        .push("multiplier");
    url.query_pairs_mut().append_pair("network", "Solana");
    let response: Value = state
        .http
        .get(url)
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(unavailable)?
        .json()
        .await
        .map_err(unavailable)?;
    let ok = response["currentMultiplier"]
        .as_f64()
        .is_some_and(|m| m.is_finite() && m > 0.0)
        && response["newMultiplier"].as_f64().unwrap_or(0.0) == 0.0;
    state
        .multipliers
        .lock()
        .map_err(internal)?
        .insert(a.symbol.clone(), (Instant::now(), ok));
    if ok {
        Ok(())
    } else {
        Err(paused_stock(&a.symbol))
    }
}
fn paused_stock(symbol: &str) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("{symbol} has a share split or similar change pending; trading resumes once it takes effect"),
    )
}
pub(super) async fn venue_quote(
    state: &MarketState,
    a: &Asset,
    side: &str,
    input: u128,
) -> Result<(u128, u128, u32), ApiError> {
    ensure_stock_units(state, a).await?;
    if a.chain == "base" {
        let q = base_quote(state, a, side, input).await?;
        Ok((q.amount_in, q.amount_out, q.fee_ppm))
    } else {
        let q = state
            .jupiter
            .order(&JupiterOrderRequest {
                input_mint: if side == "buy" { SOL_USDC } else { &a.token }.into(),
                output_mint: if side == "buy" { &a.token } else { SOL_USDC }.into(),
                amount_base_units: input.try_into().map_err(|_| bad("amount too large"))?,
                taker: None,
            })
            .await
            .map_err(unavailable)?;
        Ok((
            q.in_amount.parse().map_err(unavailable)?,
            q.out_amount.parse().map_err(unavailable)?,
            q.fee_bps.unwrap_or(0),
        ))
    }
}

pub(super) async fn assets(
    State(state): State<AppState>,
    Query(q): Query<AssetsQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let currency = q.currency.unwrap_or_else(|| "NGN".into());
    checked_currency(&currency)?;
    let category = q.category.unwrap_or_else(|| "popular".into());
    let kind = match category.as_str() {
        "popular" => None,
        "stocks" => Some("stock"),
        "memes" => Some("meme"),
        "crypto" => Some("crypto"),
        _ => return Err(bad("unsupported asset category")),
    };
    let raw = q.q.unwrap_or_default();
    let raw = raw.trim();
    // The sign-in check, the currency's rate and the coin list don't wait on each other.
    let ((), rate, catalog) = tokio::try_join!(
        app_balance::signed_in(&state, &headers),
        app_balance::fx_rate(&currency),
        catalog(&state.markets),
    )?;
    let part = q.part.as_deref().unwrap_or("all").to_owned();
    if !matches!(part.as_str(), "all" | "listed" | "other") {
        return Err(bad("part must be listed or other"));
    }
    let (want_listed, want_other) = (part != "other", part != "listed");
    let key = format!("{currency}|{category}|{part}|{raw}");
    if let Some(answer) = state.markets.recent_search(&key) {
        return Ok(Json(answer));
    }
    // Other chains (NEAR, Sui, Monad) are searched while Atlas's own list is priced, not after it.
    let other_chains = async {
        if want_other && !raw.is_empty() && kind.is_none_or(|k| k == "crypto") {
            Some(near_intents::search_assets(&state, raw, &currency, rate).await)
        } else {
            None
        }
    };
    let base_by_name = async {
        if want_other
            && raw.len() >= 2
            && !looks_like_evm_address(raw)
            && !looks_like_mint(raw)
            && kind.is_none_or(|k| k == "crypto")
        {
            base_search(&state, &catalog, raw, &currency, rate).await
        } else {
            Vec::new()
        }
    };
    let listed = async {
        if want_listed {
            listed_assets(&state, &catalog, raw, kind, &currency, rate).await
        } else {
            Ok((Vec::new(), true))
        }
    };
    let (listed, found, base_found) = tokio::join!(listed, other_chains, base_by_name);
    let (mut result, mut search_complete) = listed?;
    if let Some(found) = found {
        search_complete &= found.complete;
        result.extend(found.assets);
    }
    // Base coins found by name come last, and never as a second coin with a symbol already shown
    // (a Base "NEAR" next to NEAR itself is a copy or a look-alike).
    let symbol = |r: &Value| r["symbol"].as_str().unwrap_or("").to_ascii_uppercase();
    for row in base_found {
        if !result
            .iter()
            .any(|r| r["assetId"] == row["assetId"] || symbol(r) == symbol(&row))
        {
            result.push(row);
        }
    }
    let answer = json!({"assets":result,"searchComplete":search_complete});
    if search_complete {
        state.markets.keep_search(key, &answer);
    }
    Ok(Json(answer))
}

// Base coins found by name on DexScreener, beyond Atlas's own list: the few most liquid (at least
// $100k), verified only when CoinGecko lists that contract on Base. They go by their address, like
// a pasted one, so they can be bought from here. A busy DexScreener just adds nothing.
async fn base_search(
    state: &AppState,
    catalog: &[Asset],
    query: &str,
    currency: &str,
    rate: u128,
) -> Vec<Value> {
    let Ok(body) = near_intents::dex_search(&state.markets.http, query).await else {
        return Vec::new();
    };
    let needle = query.to_ascii_lowercase();
    let mut best: HashMap<String, (f64, &Value)> = HashMap::new();
    for pair in body["pairs"].as_array().into_iter().flatten() {
        let token = &pair["baseToken"];
        let (Some(address), Some(symbol), Some(name)) = (
            token["address"].as_str(),
            token["symbol"].as_str(),
            token["name"].as_str(),
        ) else {
            continue;
        };
        let liquidity = pair["liquidity"]["usd"].as_f64().unwrap_or(0.0);
        if pair["chainId"] != "base"
            || !looks_like_evm_address(address)
            || liquidity < 100_000.0
            || !(symbol.to_ascii_lowercase().contains(&needle)
                || name.to_ascii_lowercase().contains(&needle))
            || catalog
                .iter()
                .any(|a| a.chain == "base" && a.token.eq_ignore_ascii_case(address))
        {
            continue;
        }
        let entry = best
            .entry(address.to_ascii_lowercase())
            .or_insert((0.0, pair));
        if liquidity > entry.0 {
            *entry = (liquidity, pair);
        }
    }
    let mut found: Vec<_> = best.into_values().collect();
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    // Never waits for CoinGecko's list: unverified until it's warm.
    let listed = state.near.listed_for_search();
    found
        .into_iter()
        .take(5)
        .filter_map(|(_, pair)| {
            let token = &pair["baseToken"];
            let address = token["address"].as_str()?;
            let usd: f64 = pair["priceUsd"].as_str()?.parse().ok()?;
            if !usd.is_finite() || usd <= 0.0 {
                return None;
            }
            Some(json!({
                "assetId": base_token_id(address),
                "symbol": token["symbol"],
                "name": token["name"],
                "kind": "crypto",
                "chain": "base",
                "price": money_from_usd(usd, currency, rate).ok()?,
                "change24hPct": pair["priceChange"]["h24"].as_f64().map(|c| format!("{c:.2}")),
                "iconUrl": pair["info"]["imageUrl"].as_str().filter(|u| u.starts_with("https://")),
                "verified": listed.has("base", address),
            }))
        })
        .collect()
}

// Atlas's own coins (Solana and Base, plus a pasted address) matching a search, priced live, and
// whether every one of them got a price in time.
async fn listed_assets(
    state: &AppState,
    catalog: &[Asset],
    raw: &str,
    kind: Option<&str>,
    currency: &str,
    rate: u128,
) -> Result<(Vec<Value>, bool), ApiError> {
    let query = raw.to_ascii_lowercase();
    // A pasted token address finds that token, listed or not, whatever chip is selected: a Solana
    // mint, or a 0x address on Base (verified only if CoinGecko lists that exact contract there).
    let base_address = looks_like_evm_address(raw);
    let by_address = looks_like_mint(raw) || base_address;
    let pasted: Vec<Asset> = if by_address {
        match catalog.iter().find(|a| a.token.eq_ignore_ascii_case(raw)) {
            Some(listed) => vec![listed.clone()],
            None if base_address => {
                let listed = state.near.listed().await;
                pasted_base_token(&state.markets, raw)
                    .await?
                    .map(|a| Asset {
                        verified: listed.has("base", &a.token),
                        ..a
                    })
                    .into_iter()
                    .collect()
            }
            None => pasted_token(&state.markets, raw)
                .await?
                .into_iter()
                .collect(),
        }
    } else {
        Vec::new()
    };
    // Base coins trending on GeckoTerminal join the list (and its search), unless GeckoTerminal is
    // slow: search answers in seconds with what it has.
    let trending_base = if by_address {
        Arc::default()
    } else {
        tokio::time::timeout(SEARCH_SOURCE_LIMIT, base_trending(state))
            .await
            .unwrap_or_default()
    };
    let rank = |a: &Asset| -> u8 {
        let (symbol, name) = (a.symbol.to_ascii_lowercase(), a.name.to_ascii_lowercase());
        if symbol == query {
            0
        } else if symbol.starts_with(&query) {
            1
        } else if name.starts_with(&query) {
            2
        } else {
            3
        }
    };
    let mut picked: Vec<&Asset> = if by_address {
        pasted.iter().collect()
    } else {
        catalog
            .iter()
            .chain(trending_base.iter())
            .filter(|a| a.listed)
            .filter(|a| kind.is_none_or(|k| a.kind == k))
            .filter(|a| {
                query.is_empty()
                    || a.symbol.to_ascii_lowercase().contains(&query)
                    || a.name.to_ascii_lowercase().contains(&query)
            })
            .collect()
    };
    // Most traded first (a search puts close name matches ahead of that); Crypto goes by price,
    // Bitcoin at the top; Trending (no chip filter) goes by who's rising.
    let by_price = kind == Some("crypto") && query.is_empty();
    let trending = kind.is_none() && query.is_empty() && !by_address;
    picked.sort_by(|a, b| {
        let by_match = if query.is_empty() { 0 } else { rank(a) }.cmp(&if query.is_empty() {
            0
        } else {
            rank(b)
        });
        if by_price {
            b.ref_price.total_cmp(&a.ref_price)
        } else {
            by_match.then(b.volume_24h.total_cmp(&a.volume_24h))
        }
    });
    if trending {
        picked = trending_order(picked);
    }
    picked.truncate(if !query.is_empty() {
        30
    } else if kind.is_none() {
        40
    } else {
        100
    });
    // Solana prices (Jupiter) and Base prices (a live $1 quote each) are read at the same time.
    // Each lookup runs on its own task: one that misses the deadline still finishes and is cached,
    // so the next search (or the app's retry) has it.
    let mints: Vec<String> = picked
        .iter()
        .filter(|a| a.chain == "solana")
        .map(|a| a.token.clone())
        .collect();
    let solana = {
        let markets = state.markets.clone();
        tokio::spawn(async move { usd_prices(&markets, &mints).await })
    };
    let base: Vec<_> = picked
        .iter()
        .filter(|a| a.chain == "base")
        .map(|a| {
            let (markets, a) = (state.markets.clone(), (*a).clone());
            tokio::spawn(async move { (a.token.clone(), base_rate(&markets, &a).await) })
        })
        .collect();
    // Base prices that come back by the deadline are kept; the ones still out are left out.
    let mut base_rates = HashMap::new();
    let base = async {
        for quote in base {
            if let Ok((token, Some(rate))) = quote.await {
                base_rates.insert(token, rate);
            }
        }
    };
    // The plain Trade list has nothing to show without Solana prices, so it waits for them.
    let solana_limit = if query.is_empty() {
        Duration::from_secs(30)
    } else {
        SEARCH_SOURCE_LIMIT * 2
    };
    let (prices, _) = tokio::join!(
        tokio::time::timeout(solana_limit, solana),
        tokio::time::timeout(SEARCH_SOURCE_LIMIT * 2, base),
    );
    let prices = match prices {
        Ok(Ok(Ok(prices))) => prices,
        // The plain Trade list still says Jupiter is down; a search keeps whatever else it found.
        Ok(Ok(Err(error))) if query.is_empty() => return Err(error),
        _ if query.is_empty() => return Err(unavailable("Solana prices unavailable")),
        _ => HashMap::new(),
    };
    let picked_count = picked.len();
    let mut result = Vec::with_capacity(picked_count);
    for a in picked {
        let (price, change) = if a.chain == "solana" {
            // No live price, no row: never show a stale or guessed one.
            let Some((usd, change)) = prices.get(&a.token) else {
                continue;
            };
            (
                money_from_usd(*usd, currency, rate)?,
                change.map(|c| format!("{c:.2}")),
            )
        } else {
            // A Base coin whose price doesn't come back in time is left out rather than holding the list.
            let Some(out) = base_rates.get(&a.token) else {
                continue;
            };
            (
                json!(unit_price(1_000_000, *out, a.decimals, currency, rate)?),
                (a.change_24h != 0.0).then(|| format!("{:.2}", a.change_24h)),
            )
        };
        result.push(json!({"assetId":a.id,"symbol":a.symbol,"name":a.name,"kind":a.kind,"chain":a.chain,"price":price,"change24hPct":change,"iconUrl":a.icon_url,"verified":a.verified}));
    }
    // Each source owns its deadline; a slow source must not erase another source's matches.
    let complete = query.is_empty() || result.len() == picked_count;
    Ok((result, complete))
}

// How long one optional source (trending lists, prices for a few Base coins, other chains' search)
// may hold a search or a list before it's answered without that source.
const SEARCH_SOURCE_LIMIT: Duration = Duration::from_secs(2);
const SEARCH_TTL: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub(super) struct ChartQuery {
    range: Option<String>,
    currency: Option<String>,
}
const GECKOTERMINAL: &str = "https://api.geckoterminal.com/api/v2/networks/";
const JUPITER_CHARTS: &str = "https://datapi.jup.ag/v2/charts/";
// A second source for Base charts: CoinGecko's price history by contract address.
const COINGECKO: &str = "https://api.coingecko.com/api/v3/";
// A chain's own coin, which has no token address (native MON).
pub(super) const NATIVE_COIN: &str = "native";

// Jupiter chart candles: {"candles":[{"time": unix seconds, "close": usd, ...}]}, oldest first.
fn jupiter_closes(body: &Value) -> Vec<(u64, f64)> {
    let mut points: Vec<(u64, f64)> = body["candles"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let close = c["close"].as_f64().filter(|v| v.is_finite() && *v > 0.0)?;
                    Some((c["time"].as_u64()? * 1000, close))
                })
                .collect()
        })
        .unwrap_or_default();
    points.sort_by_key(|(ms, _)| *ms);
    points
}

// Price history for an asset's detail screen, from on-chain trades in its deepest pool
// (GeckoTerminal). Display only: `points` are [unix ms, price in the display currency], oldest first.
pub(super) async fn chart(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    Query(q): Query<ChartQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    let currency = q.currency.unwrap_or_else(|| "NGN".into());
    checked_currency(&currency)?;
    let range = q.range.unwrap_or_else(|| "1D".into());
    // Perps markets ("NEAR-PERP", "xyz:TSLA-PERP") chart from Hyperliquid's candles.
    if asset_id.ends_with("-PERP") {
        let closes = hl::closes(&state, &asset_id, &range).await?;
        if closes.is_empty() {
            return Err(unavailable("no price history for this market yet"));
        }
        let rate = app_balance::fx_rate(&currency).await?;
        let scale = rate as f64 / 1_000_000.0;
        let points: Vec<Value> = closes
            .iter()
            .map(|(ms, usd)| json!([ms, usd * scale]))
            .collect();
        return Ok(Json(
            json!({"assetId":asset_id,"range":range,"currency":currency,"points":points}),
        ));
    }
    // (Jupiter interval, GeckoTerminal timeframe + aggregate, candles, cache). Longer ranges change
    // slowly and are cached longer.
    let (interval, timeframe, aggregate, limit, ttl, days) = match range.as_str() {
        "1D" => ("15_MINUTE", "minute", 15, 96, Duration::from_secs(60), 1),
        "1W" => ("1_HOUR", "hour", 1, 168, Duration::from_secs(300), 7),
        "1M" => ("4_HOUR", "hour", 4, 180, Duration::from_secs(900), 30),
        "1Y" => ("1_DAY", "day", 1, 365, Duration::from_secs(3600), 365),
        _ => return Err(bad("range must be 1D, 1W, 1M or 1Y")),
    };
    // 1Click assets (Sui, NEAR, Monad…) chart from GeckoTerminal by their own address; the rest are
    // catalog assets on Solana or Base.
    // The coin's symbol and live price too, for an exchange's chart (not on Solana: Jupiter has it).
    let (asset_id, network, token, quote) = if asset_id.starts_with("near:") {
        let (found, quote) = tokio::join!(
            near_intents::chart_token(&state, &asset_id),
            near_intents::chart_quote(&state, &asset_id)
        );
        let (network, token) =
            found?.ok_or_else(|| unavailable("no price history for this asset yet"))?;
        (asset_id, network, token, quote)
    } else {
        let asset = find_asset(&state.markets, &asset_id).await?;
        if asset.chain == "base" {
            let quote = base_rate(&state.markets, &asset).await.map(|units| {
                (
                    asset.symbol.clone(),
                    10f64.powi(asset.decimals as i32) / units as f64,
                )
            });
            (asset.id, "base", asset.token, quote)
        } else {
            (asset.id, "solana", asset.token, None)
        }
    };
    // Sui coin types ("0x…::deep::DEEP") go into GeckoTerminal paths with their colons escaped.
    let token_path = token.replace(':', "%3A");
    let rate = app_balance::fx_rate(&currency).await?;
    let key = format!("{network}:{token}:{range}");
    let cached = state
        .markets
        .charts
        .lock()
        .map_err(internal)?
        .get(&key)
        .map(|(at, points)| (at.elapsed() < ttl, points.clone()));
    let source = ChartSource {
        asset_id: &asset_id,
        network,
        token: &token,
        token_path: &token_path,
    };
    let shape = (interval, timeframe, aggregate, limit, days);
    let points = match cached {
        Some((true, points)) => points,
        // A stale chart beats none when the sources are busy.
        Some((false, points)) => match chart_any(&state, &source, shape, &range, quote).await {
            Ok(fresh) => keep_chart(&state.markets, key, fresh)?,
            Err(_) => points,
        },
        None => {
            let fresh = chart_any(&state, &source, shape, &range, quote).await?;
            keep_chart(&state.markets, key, fresh)?
        }
    };
    let scale = rate as f64 / 1_000_000.0;
    let series: Vec<Value> = points
        .iter()
        .map(|(ms, usd)| json!([ms, usd * scale]))
        .collect();
    Ok(Json(
        json!({"assetId":asset_id,"range":range,"currency":currency,"points":series}),
    ))
}

type ChartPoints = Arc<Vec<(u64, f64)>>;

fn keep_chart(
    state: &MarketState,
    key: String,
    points: Vec<(u64, f64)>,
) -> Result<ChartPoints, ApiError> {
    let points = Arc::new(points);
    state
        .charts
        .lock()
        .map_err(internal)?
        .insert(key, (Instant::now(), points.clone()));
    Ok(points)
}

struct ChartSource<'a> {
    asset_id: &'a str,
    network: &'a str,
    token: &'a str,
    token_path: &'a str,
}

// A chart outside Solana: first an exchange that trades the coin by its symbol (Coinbase,
// Hyperliquid, Gate.io: free, and far more generous with requests than GeckoTerminal), used only
// when its latest price is within 3% of the coin's live price so a look-alike symbol never draws
// another coin's chart; then GeckoTerminal and CoinGecko.
async fn chart_any(
    state: &AppState,
    source: &ChartSource<'_>,
    shape: (&str, &str, u32, u32, u32),
    range: &str,
    quote: Option<(String, f64)>,
) -> Result<Vec<(u64, f64)>, ApiError> {
    if let Some((symbol, live)) = quote.filter(|_| source.network != "solana") {
        if let Some(points) = exchange_closes(state, &symbol, live, range).await {
            return Ok(points);
        }
    }
    chart_points(&state.markets, source, shape).await
}

async fn exchange_closes(
    state: &AppState,
    symbol: &str,
    live: f64,
    range: &str,
) -> Option<Vec<(u64, f64)>> {
    // wNEAR trades as NEAR.
    let symbol = symbol
        .strip_prefix('w')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
        .unwrap_or(symbol)
        .to_ascii_uppercase();
    if symbol.is_empty() || !symbol.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let matches = |points: &[(u64, f64)]| {
        points.len() >= 10
            && live.is_finite()
            && live > 0.0
            && points
                .last()
                .is_some_and(|(_, close)| (close / live - 1.0).abs() <= 0.03)
    };
    let limit = Duration::from_secs(3);
    let http = &state.markets.http;
    if let Ok(Some(points)) =
        tokio::time::timeout(limit, coinbase_closes(http, &symbol, range)).await
    {
        if matches(&points) {
            return Some(points);
        }
    }
    if let Ok(Ok(points)) =
        tokio::time::timeout(limit, hl::closes(state, &format!("{symbol}-PERP"), range)).await
    {
        if matches(&points) {
            return Some(points);
        }
    }
    if let Ok(Some(points)) = tokio::time::timeout(limit, gate_closes(http, &symbol, range)).await {
        if matches(&points) {
            return Some(points);
        }
    }
    None
}

// Coinbase Exchange candles for SYMBOL-USD: [time s, low, high, open, close, volume], newest first.
async fn coinbase_closes(
    http: &reqwest::Client,
    symbol: &str,
    range: &str,
) -> Option<Vec<(u64, f64)>> {
    let (granularity, span_ms): (u64, u64) = match range {
        "1D" => (900, 86_400_000),
        "1W" => (3600, 7 * 86_400_000),
        "1M" => (21_600, 30 * 86_400_000),
        // At most 300 candles per request.
        _ => (86_400, 300 * 86_400_000),
    };
    let end = now();
    let body: Value = http
        .get(format!(
            "https://api.exchange.coinbase.com/products/{symbol}-USD/candles"
        ))
        .query(&[
            ("granularity", granularity.to_string()),
            ("start", daya::format_time(end.saturating_sub(span_ms))),
            ("end", daya::format_time(end)),
        ])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let mut points: Vec<(u64, f64)> = body
        .as_array()?
        .iter()
        .filter_map(|c| Some((c[0].as_u64()? * 1000, c[4].as_f64()?)))
        .filter(|(_, close)| close.is_finite() && *close > 0.0)
        .collect();
    points.sort_by_key(|(ms, _)| *ms);
    Some(points)
}

// Gate.io candles for SYMBOL_USDT: [time s, quote volume, close, high, low, open, …], oldest first.
async fn gate_closes(http: &reqwest::Client, symbol: &str, range: &str) -> Option<Vec<(u64, f64)>> {
    let (interval, limit) = match range {
        "1D" => ("15m", 96),
        "1W" => ("1h", 168),
        "1M" => ("4h", 180),
        _ => ("1d", 365),
    };
    let body: Value = http
        .get("https://api.gateio.ws/api/v4/spot/candlesticks")
        .query(&[
            ("currency_pair", format!("{symbol}_USDT")),
            ("interval", interval.to_owned()),
            ("limit", limit.to_string()),
        ])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        body.as_array()?
            .iter()
            .filter_map(|c| {
                let seconds: u64 = c[0].as_str()?.parse().ok()?;
                let close: f64 = c[2].as_str()?.parse().ok()?;
                (close.is_finite() && close > 0.0).then_some((seconds * 1000, close))
            })
            .collect(),
    )
}

// A chart's closing prices in dollars, oldest first, from the source that has them. `shape` is
// (Jupiter interval, GeckoTerminal timeframe, aggregate, candles, CoinGecko days).
async fn chart_points(
    state: &MarketState,
    source: &ChartSource<'_>,
    shape: (&str, &str, u32, u32, u32),
) -> Result<Vec<(u64, f64)>, ApiError> {
    let (interval, timeframe, aggregate, limit, days) = shape;
    let ChartSource {
        asset_id,
        network,
        token,
        token_path,
    } = *source;
    // Solana: Jupiter's chart data (what jup.ag draws). Elsewhere GeckoTerminal, whose free limit
    // is per IP and often spent on shared hosts; Base then falls back to CoinGecko.
    // WETH and AAPLc track the same thing as Ether (Portal) and Apple xStock on Solana, whose
    // charts Jupiter has.
    let solana_twin = match asset_id {
        "weth-base" => Some("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs"),
        "aaplc-base" => Some("XsbEhLAtcf6HdfpFZ5xEMdqW8nfAvcsP5bdudRLJzJp"),
        _ => None,
    };
    let points = if let Some(mint) = (network == "solana").then_some(token).or(solana_twin) {
        let url = format!(
            "{JUPITER_CHARTS}{mint}?interval={interval}&to={}&candles={limit}&type=price",
            now()
        );
        let body: Value = state
            .http
            .get(url)
            .send()
            .await
            .map_err(unavailable)?
            .error_for_status()
            .map_err(|e| unavailable(format!("price history unavailable: {e}")))?
            .json()
            .await
            .map_err(unavailable)?;
        jupiter_closes(&body)
    } else {
        let from_gecko = async {
            if token == NATIVE_COIN {
                return Ok(Vec::new());
            }
            let pool = deepest_pool(state, network, token_path).await?;
            let body: Value = gecko(
                state,
                &format!(
                    "{network}/pools/{pool}/ohlcv/{timeframe}?aggregate={aggregate}&limit={limit}&currency=usd&token={token_path}"
                ),
            )
            .await?;
            Ok::<_, ApiError>(ohlcv_closes(&body))
        };
        // GeckoTerminal's free limit is per IP and often spent on Render's shared one: CoinGecko,
        // which counts separately, has the same coins by contract or coin id.
        match (from_gecko.await, coingecko_coin(network, token)) {
            (Ok(points), _) if !points.is_empty() => points,
            (_, Some(coin)) => coingecko_closes_for(state, &coin, days).await?,
            (other, None) => other?,
        }
    };
    if points.is_empty() {
        return Err(unavailable("no price history for this asset yet"));
    }
    Ok(points)
}

// Where CoinGecko keeps a coin's price history: by contract on its chain, or by coin id for a
// chain's own coin. None for a chain it isn't asked about.
fn coingecko_coin(network: &str, token: &str) -> Option<String> {
    let native = |id: &str| Some(format!("coins/{id}"));
    let contract = |platform: &str| Some(format!("coins/{platform}/contract/{token}"));
    match network {
        "sui-network"
            if token.trim_start_matches("0x").trim_start_matches('0') == "2::sui::SUI" =>
        {
            native("sui")
        }
        "sui-network" => contract("sui"),
        "near" if token == "wrap.near" => native("near"),
        "near" => contract("near-protocol"),
        "monad" if token == NATIVE_COIN => native("monad"),
        "monad" => contract("monad"),
        "base" => contract("base"),
        "eth" => contract("ethereum"),
        "arbitrum" => contract("arbitrum-one"),
        "bsc" => contract("binance-smart-chain"),
        _ => None,
    }
}

// CoinGecko's price history for a coin (a `coingecko_coin` path): (ms, price) pairs, oldest first.
async fn coingecko_closes_for(
    state: &MarketState,
    coin: &str,
    days: u32,
) -> Result<Vec<(u64, f64)>, ApiError> {
    let body: Value = state
        .http
        .get(format!(
            "{COINGECKO}{coin}/market_chart?vs_currency=usd&days={days}"
        ))
        .header("accept", "application/json")
        .header("user-agent", "Atlas/1.0")
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(|e| unavailable(format!("price history unavailable: {e}")))?
        .json()
        .await
        .map_err(unavailable)?;
    Ok(coingecko_closes(&body))
}

fn coingecko_closes(body: &Value) -> Vec<(u64, f64)> {
    body["prices"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|p| Some((p[0].as_f64()? as u64, p[1].as_f64()?)))
                .filter(|(_, price)| price.is_finite() && *price > 0.0)
                .collect()
        })
        .unwrap_or_default()
}

async fn gecko(state: &MarketState, path: &str) -> Result<Value, ApiError> {
    state
        .http
        .get(format!("{GECKOTERMINAL}{path}"))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(|e| unavailable(format!("price history unavailable: {e}")))?
        .json()
        .await
        .map_err(unavailable)
}

// The pool holding the most liquidity for a token: its trades are the truest price.
async fn deepest_pool(state: &MarketState, network: &str, token: &str) -> Result<String, ApiError> {
    let key = format!("{network}:{token}");
    if let Some((at, pool)) = state.chart_pools.lock().map_err(internal)?.get(&key) {
        if at.elapsed() < Duration::from_secs(3600) {
            return Ok(pool.clone());
        }
    }
    let body = gecko(state, &format!("{network}/tokens/{token}/pools?page=1")).await?;
    let pool = body["data"]
        .as_array()
        .and_then(|pools| {
            pools.iter().max_by(|a, b| {
                let reserve = |p: &Value| {
                    p["attributes"]["reserve_in_usd"]
                        .as_str()
                        .and_then(|r| r.parse::<f64>().ok())
                        .unwrap_or(0.0)
                };
                reserve(a).total_cmp(&reserve(b))
            })
        })
        .and_then(|p| p["attributes"]["address"].as_str())
        .ok_or_else(|| unavailable("no trading pool for this asset"))?
        .to_owned();
    state
        .chart_pools
        .lock()
        .map_err(internal)?
        .insert(key, (Instant::now(), pool.clone()));
    Ok(pool)
}

// GeckoTerminal candles are [unix seconds, open, high, low, close, volume], newest first.
fn ohlcv_closes(body: &Value) -> Vec<(u64, f64)> {
    let mut points: Vec<(u64, f64)> = body["data"]["attributes"]["ohlcv_list"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let close = c[4].as_f64().filter(|v| v.is_finite() && *v > 0.0)?;
                    Some((c[0].as_u64()? * 1000, close))
                })
                .collect()
        })
        .unwrap_or_default();
    points.sort_by_key(|(ms, _)| *ms);
    points
}

// An indicative unit price from a floating USD price (display only; trades use venue quotes).
pub(super) fn money_from_usd(usd: f64, currency: &str, rate: u128) -> Result<Value, ApiError> {
    if !usd.is_finite() || usd <= 0.0 {
        return Err(unavailable("invalid venue price"));
    }
    let amount = usd * rate as f64 / 1_000_000.0;
    let mut text = format!("{amount:.12}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    Ok(json!({"amount":text,"currency":currency}))
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.asset_id.starts_with("near:") {
        return near_intents::quote(state, headers, req).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let a = find_asset(&state.markets, &req.asset_id).await?;
    if req.side != "buy" && req.side != "sell" {
        return Err(bad("side must be buy or sell"));
    }
    checked_currency(&req.amount.currency)?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let display_micros = parse_micros(&req.amount.amount)?;
    let usdc_units = display_micros
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    check_limits(usdc_units, &req.amount.currency, rate)?;
    let mut input = if req.side == "buy" {
        usdc_units
    } else if req.all {
        // Everything held; SOL keeps its gas tank (0.01 SOL) for the fees ahead.
        let held = spot_available(&state, &a, "sell", &user)
            .await
            .ok_or_else(|| unavailable("Couldn't read your holding right now; try again"))?;
        if a.token == SOL_MINT {
            held.saturating_sub(10_000_000)
        } else {
            held
        }
    } else {
        let (_, out, _) = venue_quote(&state.markets, &a, "buy", 1_000_000).await?;
        usdc_units
            .checked_mul(out)
            .ok_or_else(|| bad("amount too large"))?
            / 1_000_000
    };
    if input == 0 {
        return Err(bad("amount too small for this asset"));
    }
    let fee_preview = if a.chain == "solana" {
        let owner = user
            .solana_wallet
            .as_deref()
            .filter(|w| !w.is_empty())
            .ok_or((StatusCode::CONFLICT, "Your wallet isn't ready yet.".into()))?;
        let funding_first = req.side == "buy" && solana_cash(&state, owner).await < input;
        // When the input is already here, simulate the exact route before buying any reserve.
        let needs_reserve = if funding_first {
            state
                .solana_mainnet
                .owner_sol_balance(owner)
                .await
                .map_err(unavailable)?
                < solana_fees::FLOOR
        } else {
            let request = JupiterOrderRequest {
                input_mint: if req.side == "buy" {
                    SOL_USDC
                } else {
                    &a.token
                }
                .into(),
                output_mint: if req.side == "buy" {
                    &a.token
                } else {
                    SOL_USDC
                }
                .into(),
                amount_base_units: input.try_into().map_err(|_| bad("amount too large"))?,
                taker: Some(owner.into()),
            };
            match state
                .markets
                .jupiter
                .user_paid_order(&request, &state.solana_mainnet)
                .await
            {
                Ok(_) => false,
                Err(engine_execution::swaps::jupiter::JupiterError::Preflight(
                    engine_execution::solana::SolanaPreflightError::InsufficientGas,
                )) => match state.markets.jupiter.gasless_order(&request).await {
                    Ok(_) => false,
                    Err(engine_execution::swaps::jupiter::JupiterError::Preflight(
                        engine_execution::solana::SolanaPreflightError::InsufficientGas,
                    ))
                    | Err(engine_execution::swaps::jupiter::JupiterError::NotExecutable(_)) => true,
                    Err(error) => return Err(swap_error(error)),
                },
                Err(error) => return Err(swap_error(error)),
            }
        };
        if needs_reserve {
            solana_fees::preview(&state, owner, true).await?
        } else {
            None
        }
    } else {
        None
    };
    let reserve_cash = fee_preview.as_ref().map_or(0, |p| p.cash());
    if req.side == "buy" {
        input = purchase_input(usdc_units, fee_preview.as_ref(), &req.amount.currency, rate)?;
    }
    let (actual_in, actual_out, fee_units) =
        venue_quote(&state.markets, &a, &req.side, input).await?;
    let (asset_units, stable_units) = if req.side == "buy" {
        (actual_out, actual_in)
    } else {
        (actual_in, actual_out)
    };
    // Above what they have: say so now, in their currency, rather than at the confirm. A buy can use
    // cash on the other chain too: it moves over first.
    let cash_needed = actual_in + if req.side == "buy" { reserve_cash } else { 0 };
    if req.side == "sell" && reserve_cash > 0 {
        let cash = solana_cash(&state, user.solana_wallet.as_deref().unwrap_or_default()).await;
        if cash < reserve_cash {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "Keep at least {} in cash to cover this sale and future network fees.",
                    say_money(reserve_cash, &req.amount.currency, rate)
                ),
            ));
        }
    }
    let mut funding_units = 0;
    let mut funding_fee = 0;
    if let Some(held) = spot_available(&state, &a, &req.side, &user).await {
        if held < cash_needed && req.side == "buy" && a.chain == "base" {
            (funding_units, funding_fee) =
                base_cash_from_solana(&state, &user, held, cash_needed, &req.amount.currency, rate)
                    .await?;
        } else if held < cash_needed && req.side == "buy" && a.chain == "solana" {
            let base_cash = match user.evm_wallet.as_deref().filter(|w| !w.is_empty()) {
                Some(evm) => state
                    .markets
                    .base
                    .balance_of(BASE_USDC, evm)
                    .await
                    .unwrap_or(0),
                None => 0,
            };
            let (send, fee) = funding_for(
                &state,
                user.evm_wallet.as_deref().filter(|w| !w.is_empty()),
                user.solana_wallet.as_deref().filter(|w| !w.is_empty()),
                cash_needed - held,
            )
            .await?;
            if base_cash < send {
                return Err(not_enough_cash(
                    held + base_cash,
                    &req.amount.currency,
                    rate,
                ));
            }
            funding_units = send;
            funding_fee = fee;
        } else if held < cash_needed {
            return Err(if req.side == "buy" {
                short_of_cash()
            } else {
                let worth = mul_div_units(held, stable_units, asset_units);
                (
                    StatusCode::CONFLICT,
                    format!(
                        "You only have {} {} ({}).",
                        format_units(held, a.decimals),
                        a.symbol,
                        say_money(worth, &req.amount.currency, rate)
                    ),
                )
            });
        }
    }
    let price = unit_price(
        stable_units,
        asset_units,
        a.decimals,
        &req.amount.currency,
        rate,
    )?;
    let (pay, receive) = if req.side == "buy" {
        (
            json!({"amount":format_units(cash_needed,6),"symbol":"USDC","value":money_from_usdc(cash_needed,&req.amount.currency,rate)?}),
            json!({"amount":format_units(actual_out,a.decimals),"symbol":a.symbol,"value":money_from_usdc(actual_in,&req.amount.currency,rate)?}),
        )
    } else {
        (
            json!({"amount":format_units(actual_in,a.decimals),"symbol":a.symbol,"value":money_from_usdc(actual_out,&req.amount.currency,rate)?}),
            json!({"amount":format_units(actual_out,6),"symbol":"USDC","value":money_from_usdc(actual_out,&req.amount.currency,rate)?}),
        )
    };
    let fee_usdc = if a.chain == "base" {
        usdc_units
            .checked_mul(u128::from(fee_units))
            .ok_or_else(|| bad("fee overflow"))?
            / 1_000_000
    } else {
        usdc_units
            .checked_mul(u128::from(fee_units))
            .ok_or_else(|| bad("fee overflow"))?
            / 10_000
    };
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.markets.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            asset: a.clone(),
            side: req.side.clone(),
            currency: req.amount.currency.clone(),
            input_units: actual_in,
            output_units: actual_out,
            funding_units,
            funding_fee,
            fee_preview: fee_preview.clone(),
            expires,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":a.id,"side":req.side,"pay":pay,"receive":receive,"price":price,"fee":money_from_usdc(fee_usdc + funding_fee + fee_preview.as_ref().map_or(0, |p| p.network_fee()),&req.amount.currency,rate)?,
            "funding":if funding_units > 0 {json!({"from":if a.chain == "base" {"Solana"} else {"Base"},"to":if a.chain == "base" {"Base"} else {"Solana"},"amount":money_from_usdc(funding_units,&req.amount.currency,rate)?,"fee":money_from_usdc(funding_fee,&req.amount.currency,rate)?})} else {Value::Null},
            "feeReserve":fee_preview.as_ref().map(|p| json!({"amount":money_from_usdc(p.cash(),&req.amount.currency,rate).ok(),"networkFee":money_from_usdc(p.network_fee(),&req.amount.currency,rate).ok(),"kept":money_from_usdc(p.gas_value,&req.amount.currency,rate).ok()})),
            "expiresAtUnixMs":expires}),
    ))
}

fn purchase_input(
    total: u128,
    preview: Option<&solana_fees::Preview>,
    currency: &str,
    rate: u128,
) -> Result<u128, ApiError> {
    let reserve = preview.map_or(0, |p| p.cash());
    let trade = total.saturating_sub(reserve);
    if trade < MIN_USDC {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "This purchase needs at least {} including a reserve for future network fees.",
                say_money(reserve + MIN_USDC, currency, rate)
            ),
        ));
    }
    Ok(trade)
}

// How much Base USDC to send so at least `shortfall` lands on Solana, and the fee in it. Relay lands
// exactly that (plus a cent) for a few cents and needs no Base gas. When Relay can't, Layerswap: its
// fee plus a 1% + $0.05 cushion, at least $1 (the extra stays in the user's Solana cash). Either
// way, what lands is `send - fee`.
async fn funding_for(
    state: &AppState,
    evm: Option<&str>,
    solana: Option<&str>,
    shortfall: u128,
) -> Result<(u128, u128), ApiError> {
    let land = shortfall + 10_000;
    if let (Some(evm), Some(solana)) = (evm, solana) {
        if let Ok(quote) = state.relay_link.base_to_solana(evm, solana, land).await {
            return Ok((quote.amount_in_units, quote.amount_in_units - land));
        }
    }
    let base = shortfall.max(1_000_000);
    let fee = state
        .layerswap
        .base_to_solana_fee(base)
        .await
        .map_err(|_| unavailable("Couldn't move your cash right now; try again shortly"))?;
    Ok((base + fee + base / 100 + 50_000, fee))
}

// ETH that rides along when cash moves to Base and the gas tank is empty: dollars (6 decimals).
// About 0.00005 ETH, dozens of Base transactions (selling what was bought, for one).
const BASE_GAS_BY_RELAY_USDC: u128 = 150_000;

// A Solana wallet with too little SOL for its transaction fee pays a $0.50 top-up first (gas_topup).
async fn solana_fee_reserve(state: &AppState, owner: &str) -> u128 {
    match state.solana_mainnet.owner_sol_balance(owner).await {
        Ok(lamports) if lamports >= GAS_FLOOR_LAMPORTS => 0,
        _ => GAS_TOPUP_USDC,
    }
}

// Solana USDC to send so a Base buy of `needed` has its cash on Base (which holds `held`), and the
// fee in it. Relay lands the shortfall (plus a cent) in seconds, with a little ETH on top for an
// empty gas tank. Short on both: the error says what they have to spend.
async fn base_cash_from_solana(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    held: u128,
    needed: u128,
    currency: &str,
    rate: u128,
) -> Result<(u128, u128), ApiError> {
    let wallets = (
        user.evm_wallet.as_deref().filter(|w| !w.is_empty()),
        user.solana_wallet.as_deref().filter(|w| !w.is_empty()),
    );
    let (Some(evm), Some(sol)) = wallets else {
        return Err(short_of_cash());
    };
    let sol_cash = solana_cash(state, sol).await;
    let land = needed - held + 10_000;
    let gas = if wallet_pays_gas(state, evm).await {
        0
    } else {
        BASE_GAS_BY_RELAY_USDC
    };
    let reserve = solana_fee_reserve(state, sol).await;
    if sol_cash < land + gas + reserve {
        return Err(not_enough_cash(held + sol_cash, currency, rate));
    }
    let moved = state
        .relay_link
        .solana_to_base(sol, evm, land, gas)
        .await
        .map_err(|_| unavailable("Couldn't move your cash right now; try again shortly"))?;
    if sol_cash < moved.amount_in_units + reserve {
        return Err(not_enough_cash(held + sol_cash, currency, rate));
    }
    Ok((moved.amount_in_units, moved.amount_in_units - land))
}

// Starts moving `send` of Base cash (`fee` of it the network fee) to the user's Solana wallet: a
// gasless Relay move when it can (the app sends nothing; the user's session signs it after the
// confirm), else one Layerswap transfer for the app to send, returned as (to, data).
async fn move_to_solana(
    state: &AppState,
    evm: &str,
    solana: &str,
    send: u128,
    fee: u128,
    intent_id: &str,
) -> Result<(CashMove, Option<(String, String)>), ApiError> {
    if let Ok(quote) = state
        .relay_link
        .base_to_solana(evm, solana, send - fee)
        .await
    {
        return Ok((
            CashMove {
                swap_id: quote.request_id,
                amount_units: quote.amount_in_units,
                tx_hash: None,
                authorization: Some(Authorization {
                    typed_data: quote.typed_data,
                    api: quote.api,
                }),
                relay: true,
            },
            None,
        ));
    }
    // Layerswap's deposit is a plain transfer: only when the wallet pays its own gas.
    if !wallet_pays_gas(state, evm).await {
        return Err(unavailable(
            "Couldn't move your cash right now; try again shortly",
        ));
    }
    let deposit = state
        .layerswap
        .base_to_solana(evm, solana, send, intent_id)
        .await
        .map_err(|_| unavailable("Couldn't move your cash right now; try again shortly"))?;
    Ok((
        CashMove {
            swap_id: deposit.swap_id,
            amount_units: deposit.amount_units,
            tx_hash: None,
            authorization: None,
            relay: false,
        },
        Some((deposit.to, deposit.data)),
    ))
}

// The gas tank. Every Solana transaction needs a little SOL for its fee unless Jupiter pays it
// (gasless swaps). Below this, a Solana step gets a top-up first.
const GAS_FLOOR_LAMPORTS: u128 = 1_000_000; // 0.001 SOL
                                            // What a top-up buys: about 0.004 SOL, hundreds of fees. The tank stays under the 0.01 SOL line
                                            // below which Jupiter keeps paying for swaps of about $10 and more (their fees and new token accounts).
const GAS_TOPUP_USDC: u128 = 500_000;

// A gasless $0.50 USDC → SOL swap (Jupiter's market makers pay its fee) when the wallet is below the
// floor and has the USDC to spare beyond `reserve` (what the main step spends). Returns the Jupiter
// request and its transaction, or None when no top-up is needed or none can be made.
pub(super) async fn gas_topup(
    state: &AppState,
    owner: &str,
    reserve: u128,
) -> Option<(String, String)> {
    gas_topup_below(state, owner, reserve, GAS_FLOOR_LAMPORTS).await
}

// The same, with a higher floor when the step also opens a token account (rent ~0.002 SOL).
async fn gas_topup_below(
    state: &AppState,
    owner: &str,
    reserve: u128,
    floor: u128,
) -> Option<(String, String)> {
    let sol = state.solana_mainnet.owner_sol_balance(owner).await.ok()?;
    if sol >= floor {
        return None;
    }
    if solana_cash(state, owner).await < reserve.saturating_add(GAS_TOPUP_USDC) {
        return None;
    }
    let order = state
        .markets
        .jupiter
        .order(&JupiterOrderRequest {
            input_mint: SOL_USDC.into(),
            output_mint: SOL_MINT.into(),
            amount_base_units: GAS_TOPUP_USDC as u64,
            taker: Some(owner.into()),
        })
        .await
        .ok()?;
    if !order.gasless {
        return None;
    }
    Some((order.request_id, order.transaction?))
}

// Lands a signed gas top-up by its Jupiter request (perps funding uses this directly).
pub(super) async fn land_gas_topup(state: &AppState, request_id: &str, signed_tx: &str) {
    if let Err(error) = state.markets.jupiter.execute(request_id, signed_tx).await {
        eprintln!("gas top-up didn't land: {error}");
    }
}

// Runs the gas top-up the user signed (index 0) before their main Solana transaction. A failed
// top-up isn't fatal here: the main transaction reports its own result.
async fn run_gas_topup(state: &AppState, intent: &StoredIntent, signed: &[Signed]) {
    if let (Some(request_id), Some(tx)) = (&intent.gas_request_id, signed.first()) {
        if let Err(error) = state
            .markets
            .jupiter
            .execute(request_id, &tx.transaction)
            .await
        {
            eprintln!("gas top-up didn't land: {error}");
        }
    }
}

fn swap_error(error: engine_execution::swaps::jupiter::JupiterError) -> ApiError {
    if matches!(
        error,
        engine_execution::swaps::jupiter::JupiterError::Preflight(
            engine_execution::solana::SolanaPreflightError::InsufficientGas
        )
    ) {
        return (StatusCode::CONFLICT,
            "There isn't enough gas for this swap. Add a little SOL for network fees, then try again. This swap hasn't been sent.".into());
    }
    unavailable(error)
}

// One Jupiter swap the user signs (Earn moving USDC in or out of Jupiter Lend), settled through the
// same /signed path as a Solana trade, with a gas top-up first when it needs one. Returns the
// intent, the transactions to sign and what Jupiter expects to deliver.
pub(super) async fn plan_jupiter_swap(
    state: &AppState,
    owner: String,
    wallet: String,
    input_mint: &str,
    output_mint: &str,
    amount: u128,
) -> Result<(String, Vec<Value>, u128), ApiError> {
    let (order, own_swap) = state
        .markets
        .jupiter
        .order_for_wallet(
            &JupiterOrderRequest {
                input_mint: input_mint.into(),
                output_mint: output_mint.into(),
                amount_base_units: amount.try_into().map_err(|_| bad("amount too large"))?,
                taker: Some(wallet.clone()),
            },
            &state.solana_mainnet,
        )
        .await
        .map_err(swap_error)?;
    let out: u128 = order.out_amount.parse().map_err(unavailable)?;
    let main_tx = order.transaction.ok_or((
        StatusCode::BAD_GATEWAY,
        "Jupiter returned no signable transaction".into(),
    ))?;
    let spends = if input_mint == SOL_USDC { amount } else { 0 };
    let gas = if order.gasless || own_swap.is_some() {
        None
    } else {
        gas_topup(state, &wallet, spends).await
    };
    let intent_id = id("intent");
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet,
                chain: "solana".into(),
                expected: Vec::new(),
                request_id: Some(order.request_id),
                own_swap,
                fee_preview: None,
                fee_reserve: None,
                status: IntentStatus {
                    intent_id: intent_id.clone(),
                    stage: "validate".into(),
                    state: "pending".into(),
                    tx_ids: Vec::new(),
                    error: None,
                },
                trade: None,
                funding: None,
                buy_mint: None,
                gas_request_id: gas.as_ref().map(|(id, _)| id.clone()),
                solana_transfer: false,
                base_topup: None,
            },
        )
        .await?;
    let mut transactions = Vec::new();
    if let Some((_, gas_tx)) = gas {
        transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
    }
    transactions.push(json!({"chain":"solana","transaction":main_tx,"submit":"engine"}));
    Ok((intent_id, transactions, out))
}

// A USDC transfer on Solana from one Atlas wallet to another (a friend send when the cash is on
// Solana): no bridge, lands in seconds. The sender pays the fee (gas tank topped up first if low).
pub(super) async fn plan_solana_transfer(
    state: &AppState,
    owner: String,
    from: String,
    to: &str,
    amount: u128,
) -> Result<(String, Vec<Value>), ApiError> {
    let (gas, transfer) = solana_usdc_transfer(state, &from, to, amount).await?;
    let intent_id = id("intent");
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet: from,
                chain: "solana".into(),
                expected: Vec::new(),
                request_id: None,
                own_swap: None,
                fee_preview: None,
                fee_reserve: None,
                status: IntentStatus {
                    intent_id: intent_id.clone(),
                    stage: "validate".into(),
                    state: "pending".into(),
                    tx_ids: Vec::new(),
                    error: None,
                },
                trade: None,
                funding: None,
                buy_mint: None,
                gas_request_id: gas.as_ref().map(|(id, _)| id.clone()),
                solana_transfer: true,
                base_topup: None,
            },
        )
        .await?;
    let mut transactions = Vec::new();
    if let Some((_, gas_tx)) = gas {
        transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
    }
    transactions.push(json!({"chain":"solana","transaction":transfer,"submit":"engine"}));
    Ok((intent_id, transactions))
}

// A USDC transfer from a Solana wallet that the engine lands (a friend send, a 1Click deposit): the
// gas top-up to run first when the wallet needs one (Jupiter request and transaction), and the
// transfer. Opening the receiver's USDC account costs rent, so the floor is higher then.
pub(super) async fn solana_usdc_transfer(
    state: &AppState,
    from: &str,
    to: &str,
    amount: u128,
) -> Result<(Option<(String, String)>, String), ApiError> {
    let (transfer, creates) = state
        .solana_mainnet
        .usdc_transfer_transaction(
            from,
            to,
            amount.try_into().map_err(|_| bad("amount too large"))?,
        )
        .await
        .map_err(unavailable)?;
    let floor = if creates {
        3_000_000
    } else {
        GAS_FLOOR_LAMPORTS
    };
    Ok((gas_topup_below(state, from, amount, floor).await, transfer))
}

// Rent for a new USDC account on Solana, which the sender pays and doesn't get back.
pub(super) const ACCOUNT_RENT_LAMPORTS: u128 = 2_039_280;

// Whether a Solana wallet can pay for a USDC transfer of `amount` that opens the receiver's account:
// SOL for the fee and rent already there, or USDC to spare for the gasless top-up.
pub(super) async fn solana_can_send(state: &AppState, owner: &str, amount: u128) -> bool {
    let sol = state
        .solana_mainnet
        .owner_sol_balance(owner)
        .await
        .unwrap_or(0);
    sol >= ACCOUNT_RENT_LAMPORTS + 100_000
        || solana_cash(state, owner).await >= amount.saturating_add(GAS_TOPUP_USDC)
}

// That rent in USD micros at SOL's live price (0 if the price is unavailable).
pub(super) async fn account_rent_usd(state: &AppState) -> u128 {
    let price = usd_prices(&state.markets, &[SOL_MINT.to_string()])
        .await
        .ok()
        .and_then(|p| p.get(SOL_MINT).map(|(price, _)| *price))
        .unwrap_or(0.0);
    (ACCOUNT_RENT_LAMPORTS as f64 / 1e9 * price * 1e6).ceil() as u128
}

// The Base gas tank, paid by the user, never by Atlas. At or above the floor the wallet pays its own
// gas (a Base transaction costs about 0.000002 ETH). Below the refill mark (but above the floor) a
// plan starts with a USDC → ETH refill the tank still pays for itself. An empty tank is filled first
// by a gasless CoW top-up from the user's Base USDC (run_base_topup), or by Layerswap's refuel when
// cash hops over from Solana anyway.
// 0.000005 ETH, and 0.00004 ETH.
const BASE_GAS_FLOOR_WEI: u128 = 5_000_000_000_000;
const BASE_GAS_REFILL_AT_WEI: u128 = 40_000_000_000_000;
// A refill: $0.50 of USDC becomes about 0.0002 ETH, enough for dozens of transactions.
pub(super) const BASE_GAS_REFILL_USDC: u128 = 500_000;
// What the user reads when the tank couldn't be filled: nothing else happened.
pub(super) const GAS_NOT_READY: &str =
    "Couldn't get your account ready for this, so nothing happened and nothing left your balance. Try again.";
// What Layerswap's refuel turns into ETH on an empty tank's first hop from Solana.
const BASE_REFUEL_USDC: u128 = 500_000;

pub(super) async fn base_eth(state: &AppState, wallet: &str) -> Option<u128> {
    let result = base_rpc(&state.markets, "eth_getBalance", json!([wallet, "latest"]))
        .await
        .ok()?;
    u128::from_str_radix(result.as_str()?.trim_start_matches("0x"), 16).ok()
}

// Whether the wallet's own ETH can pay for a transaction now.
pub(super) async fn wallet_pays_gas(state: &AppState, wallet: &str) -> bool {
    base_eth(state, wallet)
        .await
        .is_some_and(|wei| wei >= BASE_GAS_FLOOR_WEI)
}

// The Base transactions that refill the gas tank when it's low and the user has USDC to spare beyond
// `reserve`: approve the router if needed, swap USDC → WETH, unwrap to ETH. Empty otherwise.
pub(super) async fn base_gas_refill(
    state: &AppState,
    wallet: &str,
    reserve: u128,
) -> Vec<(String, String)> {
    // Only while the tank can still pay for its own refill, and only when it's getting low.
    let Some(eth) = base_eth(state, wallet).await else {
        return Vec::new();
    };
    if !(BASE_GAS_FLOOR_WEI..BASE_GAS_REFILL_AT_WEI).contains(&eth) {
        return Vec::new();
    }
    let usdc = state
        .markets
        .base
        .balance_of(BASE_USDC, wallet)
        .await
        .unwrap_or(0);
    if usdc < reserve.saturating_add(BASE_GAS_REFILL_USDC) {
        return Vec::new();
    }
    let request = BaseSwapRequest {
        source_token: BASE_USDC.into(),
        destination_token: BASE_WETH.into(),
        amount_base_units: BASE_GAS_REFILL_USDC,
    };
    let Ok(quote) = state.markets.base.quote_direct(&request).await else {
        return Vec::new();
    };
    let Ok(swap) = state.markets.base.swap_transaction(&quote, wallet, 100) else {
        return Vec::new();
    };
    let mut txs = Vec::new();
    let allowance = state
        .markets
        .base
        .allowance(BASE_USDC, wallet)
        .await
        .unwrap_or(0);
    if allowance < BASE_GAS_REFILL_USDC {
        if let Ok(approval) =
            state
                .markets
                .base
                .approval_transaction(BASE_USDC, wallet, BASE_GAS_REFILL_USDC)
        {
            txs.push((
                approval.to.to_ascii_lowercase(),
                approval.data.to_ascii_lowercase(),
            ));
        }
    }
    txs.push((swap.to.to_ascii_lowercase(), swap.data.to_ascii_lowercase()));
    // WETH.withdraw(the least the swap delivers): the ETH lands natively for gas.
    let unwrap = quote.amount_out.saturating_mul(99) / 100;
    txs.push((
        BASE_WETH.to_ascii_lowercase(),
        format!("0x2e1a7d4d{unwrap:064x}"),
    ));
    txs
}

// Whether a Base plan should fill an empty tank first: no ETH to pay gas, and USDC to spare beyond
// `spends` for the top-up.
pub(super) async fn base_topup_fits(state: &AppState, wallet: &str, spends: u128) -> bool {
    if wallet_pays_gas(state, wallet).await {
        return false;
    }
    state
        .markets
        .base
        .balance_of(BASE_USDC, wallet)
        .await
        .is_ok_and(|usdc| usdc >= spends.saturating_add(BASE_GAS_REFILL_USDC))
}

// Fills an empty tank without gas once the user has confirmed: their session signs a USDC permit
// for CoW and a $0.50 USDC → ETH order to themselves; a solver settles it and pays the gas. Returns
// CoW's order id.
pub(super) async fn prepare_base_topup(state: &AppState, evm: &str) -> Result<BaseTopup, ApiError> {
    let owner = evm.trim_start_matches("0x").to_ascii_lowercase();
    let nonce = base_rpc(
        &state.markets,
        "eth_call",
        json!([{"to":BASE_USDC,"data":format!("0x7ecebe00{owner:0>64}")},"latest"]),
    )
    .await?
    .as_str()
    .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
    .ok_or_else(|| unavailable("USDC nonce unavailable"))?;
    Ok(BaseTopup {
        uid: None,
        permit: Some(engine_execution::cow::permit_typed_data(
            evm,
            BASE_GAS_REFILL_USDC,
            nonce,
            now() / 1000 + 180,
        )),
        order: None,
    })
}
pub(super) fn topup_expired(topup: &BaseTopup) -> bool {
    let expires = topup.order.as_ref().map(|o| o.valid_to as u64).or_else(|| {
        topup
            .permit
            .as_ref()?
            .get("message")?
            .get("deadline")?
            .as_str()?
            .parse::<u64>()
            .ok()
    });
    expires.is_none_or(|at| at <= now() / 1000)
}
pub(super) fn topup_steps(topup: &BaseTopup) -> Vec<Value> {
    match (&topup.order, &topup.permit) {
        (Some(order), _) => vec![gasless::step(&order.typed_data(), "base")],
        (_, Some(permit)) => vec![gasless::step(permit, "base")],
        _ => Vec::new(),
    }
}
pub(super) async fn advance_base_topup(
    state: &AppState,
    headers: &HeaderMap,
    evm: &str,
    mut topup: BaseTopup,
    signature: &str,
) -> Result<BaseTopup, String> {
    if let Some(order) = &topup.order {
        let signed = gasless::verify(state, headers, evm, &order.typed_data(), signature).await?;
        topup.uid = Some(
            state
                .cow
                .place(order, &signed)
                .await
                .map_err(|e| e.to_string())?,
        );
        return Ok(topup);
    }
    let permit = topup.permit.as_ref().ok_or("Top-up preparation expired")?;
    let signed = gasless::verify(state, headers, evm, permit, signature).await?;
    let deadline = permit["message"]["deadline"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or("Permit deadline unavailable")?;
    let app_data =
        engine_execution::cow::permit_app_data(evm, BASE_GAS_REFILL_USDC, deadline, &signed)
            .map_err(|e| e.to_string())?;
    topup.order = Some(
        state
            .cow
            .gas_order(evm, BASE_GAS_REFILL_USDC, &app_data)
            .await
            .map_err(|e| e.to_string())?,
    );
    Ok(topup)
}

// USDC in a Solana wallet (0 if it can't be read).
pub(super) async fn solana_cash(state: &AppState, owner: &str) -> u128 {
    state
        .solana_mainnet
        .owner_token_balances(owner)
        .await
        .map(|held| {
            held.iter()
                .filter(|(m, _, _)| m == SOL_USDC)
                .map(|(_, u, _)| *u)
                .sum()
        })
        .unwrap_or(0)
}

// The unified balance for something paid on Base: None when Base cash covers `needed`, else how
// much USDC to move from Solana first (Layerswap's fee and a cushion included). Short on both: the
// error says what each chain holds.
pub(super) async fn cash_for_base(
    state: &AppState,
    evm: &str,
    solana: Option<&str>,
    needed: u128,
    currency: &str,
    rate: u128,
) -> Result<Option<(u128, u128)>, ApiError> {
    cash_for_base_with(state, evm, solana, needed, currency, rate, false).await
}

// The same; with `refuel` the hop always happens and carries ETH for an empty gas tank on top.
pub(super) async fn cash_for_base_with(
    state: &AppState,
    evm: &str,
    solana: Option<&str>,
    needed: u128,
    currency: &str,
    rate: u128,
    refuel: bool,
) -> Result<Option<(u128, u128)>, ApiError> {
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, evm)
        .await
        .map_err(unavailable)?;
    if base_cash >= needed && !refuel {
        return Ok(None);
    }
    let sol_cash = match solana {
        Some(owner) => solana_cash(state, owner).await,
        None => 0,
    };
    let shortfall = needed.saturating_sub(base_cash).max(1_000_000);
    let fee = state
        .layerswap
        .solana_to_base_fee(shortfall)
        .await
        .map_err(|_| unavailable("Couldn't move cash from Solana right now; try again shortly"))?;
    let refuel_cost = if refuel { BASE_REFUEL_USDC } else { 0 };
    let send = shortfall + fee + shortfall / 100 + 50_000 + refuel_cost;
    if sol_cash < send {
        return Err(not_enough_cash(base_cash + sol_cash, currency, rate));
    }
    Ok(Some((send, fee)))
}

// Base USDC to send so at least `needed` is on Solana (see funding_for); None when Solana already
// has it. Short on both: the error says what each chain holds.
pub(super) async fn cash_for_solana(
    state: &AppState,
    evm: Option<&str>,
    solana: Option<&str>,
    held_on_solana: u128,
    needed: u128,
    currency: &str,
    rate: u128,
) -> Result<Option<(u128, u128)>, ApiError> {
    if held_on_solana >= needed {
        return Ok(None);
    }
    let base_cash = match evm {
        Some(evm) => state
            .markets
            .base
            .balance_of(BASE_USDC, evm)
            .await
            .unwrap_or(0),
        None => 0,
    };
    let (send, fee) = funding_for(state, evm, solana, needed - held_on_solana).await?;
    if base_cash < send {
        return Err(not_enough_cash(held_on_solana + base_cash, currency, rate));
    }
    Ok(Some((send, fee)))
}

// Plans Base transactions paid from the unified balance. With enough on Base they go out as they
// are. Otherwise the plan is one Solana transaction moving the shortfall over (Layerswap) and the
// Base transactions are sent once it lands (GET /v1/intents/{id}/next), still one confirm.
// Returns the intent, what the app signs now, and the fee for moving cash (None when none moves).
#[allow(clippy::too_many_arguments)]
pub(super) async fn plan_base_with_cash(
    state: &AppState,
    owner: String,
    evm: String,
    solana: Option<String>,
    txs: Vec<(String, String)>,
    needed: u128,
    currency: &str,
    rate: u128,
) -> Result<(String, Vec<Value>, Option<u128>), ApiError> {
    let as_base = |txs: &[(String, String)]| -> Vec<Value> {
        txs.iter()
            .map(
                |(to, data)| json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}),
            )
            .collect()
    };
    // A low gas tank refills itself from the user's USDC first; an empty one gets ETH on a hop from
    // Solana (Layerswap refuel) when there's Solana cash to hop.
    let refill = base_gas_refill(state, &evm, needed).await;
    let txs = if refill.is_empty() {
        txs
    } else {
        refill.iter().cloned().chain(txs).collect()
    };
    // An empty tank with Base cash to spare fills itself first: a gasless CoW top-up from the user's
    // own USDC (a fraction of a cent), then the Base transactions (GET /v1/intents/{id}/next).
    if base_topup_fits(state, &evm, needed).await {
        let intent_id = state
            .markets
            .register_base_txs(owner, evm, txs, true)
            .await?;
        let mut intent = state
            .markets
            .get_intent(&intent_id)
            .await?
            .ok_or_else(|| unavailable("Intent unavailable"))?;
        let topup = prepare_base_topup(state, &intent.wallet).await?;
        let steps = topup_steps(&topup);
        intent.base_topup = Some(topup);
        state.markets.save_intent(&intent_id, &intent).await?;
        return Ok((intent_id, steps, None));
    }
    // Otherwise an empty tank gets ETH on a hop from Solana (Layerswap refuel). Atlas never pays gas:
    // with neither, the user is asked for a little more.
    let refuel = !wallet_pays_gas(state, &evm).await;
    let hop = match cash_for_base_with(
        state,
        &evm,
        solana.as_deref(),
        needed,
        currency,
        rate,
        refuel,
    )
    .await
    {
        Err(_)
            if refuel
                && state
                    .markets
                    .base
                    .balance_of(BASE_USDC, &evm)
                    .await
                    .is_ok_and(|cash| cash >= needed) =>
        {
            return Err(short_of_gas(currency, rate));
        }
        other => other?,
    };
    let Some((send, fee)) = hop else {
        let transactions = as_base(&txs);
        let intent_id = state
            .markets
            .register_base_txs(owner, evm, txs, false)
            .await?;
        return Ok((intent_id, transactions, None));
    };
    let sol = solana.ok_or((
        StatusCode::CONFLICT,
        "Privy Solana wallet is not ready".into(),
    ))?;
    let intent_id = id("intent");
    let deposit = state
        .layerswap
        .solana_to_base(&sol, &evm, send, &intent_id, refuel)
        .await
        .map_err(unavailable)?;
    // The Solana deposit pays its own fee; a low wallet gets its tank topped up first.
    let gas = gas_topup(state, &sol, send).await;
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet: evm,
                chain: "base".into(),
                expected: txs
                    .into_iter()
                    .map(|(to, data)| (to.to_ascii_lowercase(), data.to_ascii_lowercase()))
                    .collect(),
                request_id: None,
                own_swap: None,
                fee_preview: None,
                fee_reserve: None,
                status: IntentStatus {
                    intent_id: intent_id.clone(),
                    stage: "validate".into(),
                    state: "pending".into(),
                    tx_ids: Vec::new(),
                    error: None,
                },
                trade: None,
                funding: Some(CashMove {
                    swap_id: deposit.swap_id,
                    amount_units: deposit.amount_units,
                    tx_hash: None,
                    authorization: None,
                    relay: false,
                }),
                buy_mint: None,
                gas_request_id: gas.as_ref().map(|(id, _)| id.clone()),
                solana_transfer: false,
                base_topup: None,
            },
        )
        .await?;
    let mut transactions = Vec::new();
    if let Some((_, gas_tx)) = gas {
        transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
    }
    transactions
        .push(json!({"chain":"solana","transaction":deposit.transaction,"submit":"engine"}));
    Ok((intent_id, transactions, Some(fee)))
}

// A Solana swap from USDC into `buy_mint` (Earn into Jupiter Lend) paid with Base cash (`send`, `fee`
// of it the network fee): the cash moves first (see move_to_solana); /next makes the swap once it
// lands. Returns the intent and what the app sends now (nothing when the move is gasless).
#[allow(clippy::too_many_arguments)]
pub(super) async fn plan_solana_swap_with_base_cash(
    state: &AppState,
    owner: String,
    solana: String,
    evm: &str,
    buy_mint: &str,
    amount: u128,
    send: u128,
    fee: u128,
) -> Result<(String, Vec<Value>), ApiError> {
    let intent_id = id("intent");
    let (cash, transfer) = move_to_solana(state, evm, &solana, send, fee, &intent_id).await?;
    let mut transactions: Vec<Value> = transfer
        .iter()
        .map(|(to, data)| json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}))
        .collect();
    if let Some(auth) = &cash.authorization {
        transactions.push(gasless::step(&auth.typed_data, "base"));
    }
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet: solana,
                chain: "solana".into(),
                expected: transfer
                    .iter()
                    .map(|(to, data)| (to.to_ascii_lowercase(), data.to_ascii_lowercase()))
                    .collect(),
                request_id: None,
                own_swap: None,
                fee_preview: None,
                fee_reserve: None,
                status: IntentStatus {
                    intent_id: intent_id.clone(),
                    stage: "validate".into(),
                    state: "pending".into(),
                    tx_ids: Vec::new(),
                    error: None,
                },
                // Not a trade (never kept as a position): /next swaps `amount` into `buy_mint`.
                trade: Some(PlannedTrade {
                    asset_id: buy_mint.into(),
                    side: "buy".into(),
                    pay_units: amount,
                    get_units: 0,
                    receive_token: String::new(),
                    handed_nonce: None,
                }),
                funding: Some(cash),
                buy_mint: Some(buy_mint.into()),
                gas_request_id: None,
                solana_transfer: false,
                base_topup: None,
            },
        )
        .await?;
    Ok((intent_id, transactions))
}

// What the user can spend on this trade: USDC on the asset's chain for a buy, the token for a sell.
// None when the wallet can't be read (the confirm still checks before anything is signed).
async fn spot_available(
    state: &AppState,
    a: &Asset,
    side: &str,
    user: &app_balance::VerifiedWallets,
) -> Option<u128> {
    let token = if side == "buy" {
        if a.chain == "base" {
            BASE_USDC
        } else {
            SOL_USDC
        }
    } else {
        a.token.as_str()
    };
    if a.chain == "base" {
        let wallet = user.evm_wallet.as_deref().filter(|w| !w.is_empty())?;
        return state.markets.base.balance_of(token, wallet).await.ok();
    }
    let owner = user.solana_wallet.as_deref().filter(|w| !w.is_empty())?;
    let held = state
        .solana_mainnet
        .owner_token_balances(owner)
        .await
        .ok()?;
    let mut units: u128 = held
        .iter()
        .filter(|(m, _, _)| m == token)
        .map(|(_, u, _)| *u)
        .sum();
    if token == SOL_MINT {
        units = units.saturating_add(state.solana_mainnet.owner_sol_balance(owner).await.ok()?);
    }
    Some(units)
}
fn mul_div_units(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    a.checked_mul(b)
        .map(|n| n / c)
        .unwrap_or_else(|| (a as f64 * b as f64 / c as f64) as u128)
}

async fn execute_quote_inner(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if quote_id.starts_with("near-q-") {
        return near_intents::execute(state, headers, quote_id).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let stored = state
        .markets
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "quote not found".into()))?;
    if stored.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "quote belongs to another user".into(),
        ));
    }
    if stored.expires < now() {
        return Err((
            StatusCode::CONFLICT,
            "quote expired; request a fresh quote".into(),
        ));
    }
    let a = stored.asset;
    ensure_stock_units(&state.markets, &a).await?;
    let evm_wallet = user.evm_wallet.clone().filter(|w| !w.is_empty());
    let solana_wallet = user.solana_wallet.clone().filter(|w| !w.is_empty());
    let wallet = if a.chain == "base" {
        user.evm_wallet
    } else {
        user.solana_wallet
    }
    .filter(|w| !w.is_empty())
    .ok_or((StatusCode::CONFLICT, "Privy wallet is not ready".into()))?;
    let mut transactions = Vec::new();
    let mut expected = Vec::new();
    let mut request_id = None;
    let mut own_swap = None;
    let mut fee_reserve = None;
    let mut funding = None;
    let mut gas_request_id = None;
    let mut base_topup = None;
    let intent_id = id("intent");
    let output: u128;
    let mut network_fee = stored.funding_fee;
    if a.chain == "base" && stored.funding_units > 0 {
        // Cash on Solana: Relay moves it to Base (with a little ETH for an empty gas tank) in one
        // Solana transaction the engine lands; /next makes the swap once it's there, without asking
        // again.
        let sol = solana_wallet.ok_or((
            StatusCode::CONFLICT,
            "Privy Solana wallet is not ready".into(),
        ))?;
        let base_cash = state
            .markets
            .base
            .balance_of(BASE_USDC, &wallet)
            .await
            .map_err(unavailable)?;
        let land = stored.input_units.saturating_sub(base_cash) + 10_000;
        let gas = if wallet_pays_gas(&state, &wallet).await {
            0
        } else {
            BASE_GAS_BY_RELAY_USDC
        };
        let moved = state
            .relay_link
            .solana_to_base(&sol, &wallet, land, gas)
            .await
            .map_err(|_| unavailable("Couldn't move your cash right now; try again shortly"))?;
        if solana_cash(&state, &sol).await < moved.amount_in_units {
            return Err(short_of_cash());
        }
        let deposit = state
            .solana_mainnet
            .v0_transaction(&sol, &moved.instructions, &moved.lookup_tables)
            .await
            .map_err(unavailable)?;
        // A Solana wallet with no SOL pays the transaction fee from a gasless top-up first.
        if let Some((gas_id, gas_tx)) = gas_topup(&state, &sol, moved.amount_in_units).await {
            transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
            gas_request_id = Some(gas_id);
        }
        transactions.push(json!({"chain":"solana","transaction":deposit,"submit":"engine"}));
        network_fee = moved.amount_in_units - land;
        funding = Some(CashMove {
            swap_id: moved.request_id,
            amount_units: moved.amount_in_units,
            tx_hash: None,
            authorization: None,
            relay: true,
        });
        output = stored.output_units;
    } else if a.chain == "base" {
        let request = quote_request(&a, &stored.side, stored.input_units);
        let fresh = base_swap(
            &state.markets,
            &a,
            &stored.side,
            stored.input_units,
            &wallet,
        )
        .await?;
        // Refuse a changed route that degrades more than 1% from the preview.
        if fresh.amount_out < stored.output_units.saturating_mul(99) / 100 {
            return Err((
                StatusCode::CONFLICT,
                "market price changed; request a fresh quote".into(),
            ));
        }
        let balance = state
            .markets
            .base
            .balance_of(&request.source_token, &wallet)
            .await
            .map_err(unavailable)?;
        if balance < fresh.amount_in {
            return Err(if stored.side == "buy" {
                short_of_cash()
            } else {
                (
                    StatusCode::CONFLICT,
                    format!(
                        "You only have {} {}.",
                        format_units(balance, a.decimals),
                        a.symbol
                    ),
                )
            });
        }
        // An exact approval for the route's router when needed, then the swap.
        for (to, data) in &fresh.txs {
            expected.push((to.to_ascii_lowercase(), data.to_ascii_lowercase()));
            transactions
                .push(json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}));
        }
        // No ETH for gas: a gasless CoW top-up from their USDC first; the swap goes out once it lands.
        let spends = if stored.side == "buy" {
            fresh.amount_in
        } else {
            0
        };
        if base_topup_fits(&state, &wallet, spends).await {
            base_topup = Some(prepare_base_topup(&state, &wallet).await?);
            transactions = topup_steps(base_topup.as_ref().expect("prepared"));
        } else if !wallet_pays_gas(&state, &wallet).await {
            let rate = app_balance::fx_rate(&stored.currency).await?;
            return Err(short_of_gas(&stored.currency, rate));
        }
        output = fresh.amount_out;
    } else if stored.funding_units > 0 {
        // Base cash first (see move_to_solana); the Jupiter buy is signed once the USDC lands on
        // Solana (GET /v1/intents/{id}/next).
        let evm = evm_wallet.clone().ok_or((
            StatusCode::CONFLICT,
            "Privy Base wallet is not ready".into(),
        ))?;
        let base_cash = state
            .markets
            .base
            .balance_of(BASE_USDC, &evm)
            .await
            .map_err(unavailable)?;
        let (cash, transfer) = move_to_solana(
            &state,
            &evm,
            &wallet,
            stored.funding_units,
            stored.funding_fee,
            &intent_id,
        )
        .await?;
        if base_cash < cash.amount_units {
            return Err(short_of_cash());
        }
        if let Some((to, data)) = transfer {
            expected.push((to.to_ascii_lowercase(), data.to_ascii_lowercase()));
            transactions
                .push(json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}));
        }
        if let Some(auth) = &cash.authorization {
            transactions.push(gasless::step(&auth.typed_data, "base"));
        }
        funding = Some(cash);
        output = stored.output_units;
    } else if let Some(preview) = &stored.fee_preview {
        let required = preview.cash()
            + if stored.side == "buy" {
                stored.input_units
            } else {
                0
            };
        if stored.side == "sell" {
            let held = state
                .solana_mainnet
                .owner_mint_balance(&wallet, &a.token, u64::from(a.decimals))
                .await
                .map_err(unavailable)?;
            let held = if a.token == SOL_MINT {
                held + state
                    .solana_mainnet
                    .owner_sol_balance(&wallet)
                    .await
                    .map_err(unavailable)?
            } else {
                held
            };
            if held < stored.input_units {
                return Err((
                    StatusCode::CONFLICT,
                    "Your holding changed. Refresh the quote; nothing has been sent.".into(),
                ));
            }
        }
        let reserve = solana_fees::prepare(&state, &wallet, preview, required).await?;
        transactions.push(solana_fees::step(&reserve));
        fee_reserve = Some(reserve);
        output = stored.output_units;
    } else {
        let (order, prepared) = state
            .markets
            .jupiter
            .order_for_wallet(
                &JupiterOrderRequest {
                    input_mint: if stored.side == "buy" {
                        SOL_USDC
                    } else {
                        &a.token
                    }
                    .into(),
                    output_mint: if stored.side == "buy" {
                        &a.token
                    } else {
                        SOL_USDC
                    }
                    .into(),
                    amount_base_units: stored
                        .input_units
                        .try_into()
                        .map_err(|_| bad("amount too large"))?,
                    taker: Some(wallet.clone()),
                },
                &state.solana_mainnet,
            )
            .await
            .map_err(swap_error)?;
        output = order.out_amount.parse().map_err(unavailable)?;
        if output < stored.output_units.saturating_mul(99) / 100 {
            return Err((
                StatusCode::CONFLICT,
                "market price changed; request a fresh quote".into(),
            ));
        }
        let main_tx = order.transaction.ok_or((
            StatusCode::BAD_GATEWAY,
            "Jupiter returned no signable transaction".into(),
        ))?;
        // Jupiter pays the gas on many swaps; otherwise a low wallet gets its tank topped up first.
        if !order.gasless && prepared.is_none() {
            let spends = if stored.side == "buy" {
                stored.input_units
            } else {
                0
            };
            if let Some((gas_id, gas_tx)) = gas_topup(&state, &wallet, spends).await {
                transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
                gas_request_id = Some(gas_id);
            }
        }
        transactions.push(json!({"chain":"solana","transaction":main_tx,"submit":"engine"}));
        request_id = (!order.request_id.is_empty()).then_some(order.request_id);
        own_swap = prepared;
    }
    let expires = now()
        + if a.chain == "base" || funding.is_some() {
            120_000
        } else {
            45_000
        };
    // The confirm sheet speaks their currency: cash as money, the asset as tokens.
    let rate = app_balance::fx_rate(&stored.currency).await?;
    let mut summary = if funding.is_some() {
        json!([
            {"label":"You pay","value":say_money(stored.input_units + stored.fee_preview.as_ref().map_or(0,|p| p.cash()), &stored.currency, rate)},
            {"label":"You get (about)","value":format!("{} {}", format_units(output, a.decimals), a.symbol)},
            {"label":"Network fee","value":say_money(network_fee.max(1), &stored.currency, rate)},
        ])
    } else if stored.side == "buy" {
        json!([
            {"label":"You pay","value":say_money(stored.input_units + stored.fee_preview.as_ref().map_or(0,|p| p.cash()), &stored.currency, rate)},
            {"label":"You get (about)","value":format!("{} {}", format_units(output, a.decimals), a.symbol)},
        ])
    } else {
        json!([
            {"label":"You sell","value":format!("{} {}", format_units(stored.input_units, a.decimals), a.symbol)},
            {"label":"You get (about)","value":say_money(output, &stored.currency, rate)},
        ])
    };
    if let Some(p) = &stored.fee_preview {
        let lines = summary.as_array_mut().expect("summary array");
        if stored.side == "sell" {
            lines.push(
                json!({"label":"From your cash","value":say_money(p.cash(),&stored.currency,rate)}),
            );
        }
        lines.push(json!({"label":"Kept for future network fees","value":format!("About {}",say_money(p.gas_value,&stored.currency,rate))}));
        lines.push(json!({"label":"Reserve transfer fee","value":say_money(p.network_fee(),&stored.currency,rate)}));
    }
    let status = IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: Vec::new(),
        error: None,
    };
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner: user.user_id,
                wallet,
                chain: a.chain.clone(),
                expected,
                request_id,
                own_swap,
                fee_preview: stored.fee_preview,
                fee_reserve,
                status,
                trade: Some(PlannedTrade {
                    asset_id: a.id.clone(),
                    side: stored.side.clone(),
                    pay_units: stored.input_units,
                    get_units: output,
                    receive_token: if stored.side == "buy" {
                        a.token.to_ascii_lowercase()
                    } else {
                        BASE_USDC.to_ascii_lowercase()
                    },
                    handed_nonce: None,
                }),
                funding,
                buy_mint: None,
                gas_request_id,
                solana_transfer: false,
                base_topup,
            },
        )
        .await?;
    Ok(Json(
        json!({"intentId":intent_id,"kind":stored.side,"summary":summary,"transactions":transactions,"expiresAtUnixMs":expires}),
    ))
}

pub(super) async fn base_rpc(
    state: &MarketState,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    let body: Value = state
        .http
        .post(state.rpc.clone())
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(unavailable)?
        .json()
        .await
        .map_err(unavailable)?;
    if let Some(error) = body.get("error") {
        return Err(unavailable(error));
    }
    Ok(body
        .get("result")
        .cloned()
        .ok_or_else(|| unavailable("Base RPC omitted result"))?)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 66 && hash.starts_with("0x") && hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) async fn signed(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Submission>,
) -> Result<Json<IntentStatus>, ApiError> {
    if intent_id.starts_with("prediction-") {
        return predictions::signed(state, headers, intent_id, body).await;
    }
    if intent_id.starts_with("near-intent-") {
        return near_intents::signed(state, headers, intent_id, body).await;
    }
    if intent_id.starts_with("hl-") {
        return hl::signed(state, intent_id, headers, body).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
        .markets
        .get_intent(&intent_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    // A Solana buy paid with Base cash signs twice: the Base transfer (validate), then the Jupiter
    // buy once the cash has landed (sign). Anything else is a repeat: answer with the status.
    let second_step = (current.funding.is_some()
        || current.base_topup.is_some()
        || current.fee_reserve.is_some())
        && current.status.stage == "sign";
    if current.status.state != "pending" || (current.status.stage != "validate" && !second_step) {
        return Ok(Json(current.status));
    }
    if let Some(reserve) = current
        .fee_reserve
        .as_ref()
        .filter(|r| r.signature.is_none())
    {
        if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
            return Err(bad("signed report does not match the network-fee step"));
        }
        if reserve.expires < now()
            || state
                .solana_mainnet
                .block_height()
                .await
                .map_err(unavailable)?
                > reserve.transfer.last_valid_block_height
        {
            return expire_fee_submission(&state,&intent_id,&current,"The network-fee step expired. Your unspent cash is in your wallet; request a fresh quote.").await;
        }
        let signature = engine_execution::kora::checked(
            &reserve.transfer.transaction,
            &body.signed[0].transaction,
            &current.wallet,
            &reserve.transfer.payer,
            true,
        )
        .map_err(|_| bad("signed transaction does not match your network-fee step"))?;
        let from = current.status.stage.clone();
        let mut updated = current;
        updated.fee_reserve.as_mut().expect("reserve").signature = Some(signature.clone());
        updated.status.tx_ids.push(signature);
        updated.status.stage = "fund".into();
        if !state
            .markets
            .claim_execution_to(&intent_id, &updated, &from, "fund")
            .await?
        {
            return Ok(Json(
                state
                    .markets
                    .get_intent(&intent_id)
                    .await?
                    .map_or(updated.status, |i| i.status),
            ));
        }
        if state
            .solana_mainnet
            .send_signed(&body.signed[0].transaction)
            .await
            .is_err()
        {
            eprintln!("intent {intent_id}: network-fee transfer outcome pending");
        }
        return Ok(Json(updated.status));
    }
    // An empty Base gas tank: the app sent nothing; now that the user has confirmed, their session
    // signs the CoW top-up, and the Base transactions follow once it fills.
    if current.base_topup.as_ref().is_some_and(|t| t.uid.is_none()) {
        if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
            return Err(bad("signed report does not match the plan"));
        }
        if !state
            .markets
            .claim_execution(&intent_id, &current, &current.status.stage)
            .await?
        {
            let latest = state.markets.get_intent(&intent_id).await?;
            return Ok(Json(latest.map_or(current.status, |i| i.status)));
        }
        let mut updated = current;
        updated.status.stage = "fund".into();
        match advance_base_topup(
            &state,
            &headers,
            &updated.wallet,
            updated.base_topup.clone().expect("topup"),
            &body.signed[0].transaction,
        )
        .await
        {
            Ok(topup) => {
                updated.status.stage = if topup.uid.is_some() { "fund" } else { "sign" }.into();
                updated.base_topup = Some(topup);
            }
            Err(reason) => {
                eprintln!("intent {intent_id}: gas top-up not started: {reason}");
                updated.status.state = "failed".into();
                updated.status.error = Some(GAS_NOT_READY.into());
            }
        }
        state.markets.save_intent(&intent_id, &updated).await?;
        return Ok(Json(updated.status));
    }
    // Moved by Relay without Base gas: the app sent nothing; now that the user has confirmed, their
    // own session signs the authorization and Relay's solver does the rest.
    let authorization = current
        .funding
        .as_ref()
        .and_then(|c| c.authorization.clone());
    if let (Some(auth), false) = (authorization, second_step) {
        if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
            return Err(bad("signed report does not match the move"));
        }
        let evm = user.evm_wallet.clone().filter(|w| !w.is_empty()).ok_or((
            StatusCode::CONFLICT,
            "Privy Base wallet is not ready".into(),
        ))?;
        if !state
            .markets
            .claim_execution(&intent_id, &current, "validate")
            .await?
        {
            let latest = state.markets.get_intent(&intent_id).await?;
            return Ok(Json(latest.map_or(current.status, |i| i.status)));
        }
        let mut updated = current;
        let request_id = updated
            .funding
            .as_ref()
            .map(|c| c.swap_id.clone())
            .unwrap_or_default();
        let moved = match gasless::verify(
            &state,
            &headers,
            &evm,
            &auth.typed_data,
            &body.signed[0].transaction,
        )
        .await
        {
            Ok(signature) => state
                .relay_link
                .submit(&request_id, &auth.api, &signature)
                .await
                .map_err(|e| e.to_string()),
            Err(reason) => Err(reason),
        };
        updated.status.stage = "fund".into();
        if let Err(reason) = moved {
            eprintln!("intent {intent_id}: gasless move not started: {reason}");
            updated.status.state = "failed".into();
            updated.status.error = Some(gasless::NOT_MOVED.into());
        }
        state.markets.save_intent(&intent_id, &updated).await?;
        return Ok(Json(updated.status));
    }
    if let Some(reserve) = current
        .fee_reserve
        .as_ref()
        .filter(|r| r.signature.is_some())
    {
        if reserve.finish_by < now() || reserve.next_expires < now() {
            return expire_fee_submission(&state,&intent_id,&current,"The swap step expired. Your network-fee reserve and unspent tokens are in your wallet; request a fresh quote.").await;
        }
    }
    let mut status = current.status.clone();
    if current.funding.is_some() && !second_step {
        // Cash moving to Solana starts with a Base transfer the app sent; cash moving to Base with a
        // Solana transaction the app signed, which the engine lands.
        let hash = if current.chain == "solana" {
            if !body.signed.is_empty()
                || body.sent.len() != 1
                || body.sent[0].chain != "base"
                || !valid_hash(&body.sent[0].id)
            {
                return Err(bad("signed report does not match the transfer from Base"));
            }
            body.sent[0].id.clone()
        } else {
            let main = usize::from(current.gas_request_id.is_some());
            if !body.sent.is_empty()
                || body.signed.len() != main + 1
                || body.signed[main].index != main
            {
                return Err(bad("signed report does not match the transfer from Solana"));
            }
            run_gas_topup(&state, &current, &body.signed).await;
            state
                .solana_mainnet
                .send_signed(&body.signed[main].transaction)
                .await
                .map_err(|e| {
                    (
                        StatusCode::BAD_GATEWAY,
                        format!("Moving your cash from Solana didn't go through: {e}"),
                    )
                })?
        };
        let mut updated = current;
        if let Some(cash) = updated.funding.as_mut() {
            cash.tx_hash = Some(hash.clone());
        }
        updated.status.tx_ids = vec![hash];
        updated.status.stage = "fund".into();
        state.markets.save_intent(&intent_id, &updated).await?;
        return Ok(Json(updated.status));
    }
    if current.chain == "base" {
        if !body.signed.is_empty()
            || body.sent.len() != current.expected.len()
            || body
                .sent
                .iter()
                .any(|s| s.chain != "base" || !valid_hash(&s.id))
        {
            return Err(bad("signed report does not match Base execution plan"));
        }
        status.tx_ids = body.sent.iter().map(|s| s.id.clone()).collect();
        status.stage = "settle".into();
    } else {
        // With a gas top-up the plan is [top-up, swap]; the swap is the last one signed.
        let main = usize::from(current.gas_request_id.is_some());
        if !body.sent.is_empty() || body.signed.len() != main + 1 || body.signed[main].index != main
        {
            return Err(bad("signed report does not match Jupiter execution plan"));
        }
        if let Some(swap) = &current.own_swap {
            let signature = engine_execution::solana::checked_swap_signature(
                &swap.transaction,
                &body.signed[main].transaction,
                &current.wallet,
            )
            .map_err(|_| bad("signed transaction does not match your swap"))?;
            let from = current.status.stage.clone();
            let mut updated = current;
            updated.status.tx_ids.push(signature);
            updated.status.stage = "settle".into();
            // Atomically save both the claim and the signature before broadcasting. Even a
            // crash after submission can be recovered by polling, without a second purchase.
            if !state
                .markets
                .claim_execution_to(&intent_id, &updated, &from, "settle")
                .await?
            {
                let latest = state.markets.get_intent(&intent_id).await?;
                return Ok(Json(latest.map_or(updated.status, |i| i.status)));
            }
            // A transport error may mean it was sent. Keep polling the known signature;
            // never replace this transaction or call the purchase failed while it can land.
            if let Err(error) = state
                .solana_mainnet
                .send_signed(&body.signed[main].transaction)
                .await
            {
                eprintln!("swap {intent_id}: submission unresolved: {error}");
            }
            return Ok(Json(updated.status));
        }
        // A plain USDC transfer: the engine lands it and status follows its signature.
        if current.solana_transfer {
            if !state
                .markets
                .claim_execution(&intent_id, &current, "validate")
                .await?
            {
                let latest = state.markets.get_intent(&intent_id).await?;
                return Ok(Json(latest.map_or(current.status, |i| i.status)));
            }
            run_gas_topup(&state, &current, &body.signed).await;
            let mut updated = current;
            match state
                .solana_mainnet
                .send_signed(&body.signed[main].transaction)
                .await
            {
                Ok(signature) => {
                    updated.status.tx_ids = vec![signature];
                    updated.status.stage = "settle".into();
                }
                Err(error) => {
                    updated.status.stage = "settle".into();
                    updated.status.state = "failed".into();
                    updated.status.error = Some(format!("The transfer didn't go through: {error}"));
                }
            }
            state.markets.save_intent(&intent_id, &updated).await?;
            return Ok(Json(updated.status));
        }
        if !state
            .markets
            .claim_execution(&intent_id, &current, &current.status.stage)
            .await?
        {
            let latest = state.markets.get_intent(&intent_id).await?;
            return Ok(Json(latest.map_or(current.status, |i| i.status)));
        }
        let request_id = current
            .request_id
            .as_deref()
            .ok_or_else(|| unavailable("Jupiter request ID missing"))?;
        run_gas_topup(&state, &current, &body.signed).await;
        match state
            .markets
            .jupiter
            .execute(request_id, &body.signed[main].transaction)
            .await
        {
            Ok(result) => {
                let filled =
                    |amount: &Option<String>| amount.as_deref().and_then(|a| a.parse().ok());
                let (paid, got) = (
                    filled(&result.total_input_amount),
                    filled(&result.total_output_amount),
                );
                status.tx_ids.push(result.signature);
                status.stage = "settle".into();
                status.state = "filled".into();
                keep_trade(&state.trades, &intent_id, &current, &status, paid, got).await;
            }
            Err(error) => {
                status.stage = "settle".into();
                status.state = "failed".into();
                status.error = Some(error.to_string());
            }
        }
    }
    let mut updated = current;
    updated.status = status.clone();
    state.markets.save_intent(&intent_id, &updated).await?;
    Ok(Json(status))
}

async fn expire_fee_submission(
    state: &AppState,
    id: &str,
    current: &StoredIntent,
    reason: &str,
) -> Result<Json<IntentStatus>, ApiError> {
    let mut expired = current.clone();
    expired.status.state = "failed".into();
    expired.status.error = Some(reason.into());
    if !state.markets.replace_plan(id, current, &expired).await? {
        return Ok(Json(
            state
                .markets
                .get_intent(id)
                .await?
                .map_or(expired.status, |i| i.status),
        ));
    }
    Ok(Json(expired.status))
}

pub(super) async fn intent_status(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<IntentStatus>, ApiError> {
    if intent_id.starts_with("prediction-") {
        return predictions::status(state, headers, intent_id).await;
    }
    if intent_id.starts_with("near-intent-") {
        return near_intents::status(state, headers, intent_id).await;
    }
    if intent_id.starts_with("hl-") {
        return hl::status(state, intent_id, headers).await;
    }
    if intent_id.starts_with("cashlink-") {
        return cashlinks::status(state, headers, intent_id).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
        .markets
        .get_intent(&intent_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if current.status.state == "pending" && current.status.stage == "fund" {
        if let Some(reserve) = &current.fee_reserve {
            let result = solana_fees::progress(&state, &current.wallet, reserve).await?;
            let mut updated = current.clone();
            match result {
                solana_fees::Progress::Pending => return Ok(Json(updated.status)),
                solana_fees::Progress::Ready => updated.status.stage = "sign".into(),
                solana_fees::Progress::Failed(reason) => {
                    updated.status.state = "failed".into();
                    updated.status.error = Some(reason.into());
                }
            }
            if !state
                .markets
                .replace_plan(&intent_id, &current, &updated)
                .await?
            {
                return Ok(Json(
                    state
                        .markets
                        .get_intent(&intent_id)
                        .await?
                        .map_or(updated.status, |i| i.status),
                ));
            }
            return Ok(Json(updated.status));
        }
    }
    // Cash moving between chains: Relay or Layerswap says when it has landed; then the rest is signed.
    // A gas top-up filling: CoW says when; then the Base transactions can be sent.
    let topup = current.base_topup.as_ref().and_then(|t| t.uid.clone());
    if let (Some(uid), "pending", "fund") = (
        topup,
        current.status.state.as_str(),
        current.status.stage.as_str(),
    ) {
        let mut updated = current.clone();
        match state.cow.order_state(&uid).await {
            Ok(engine_execution::layerswap::SwapState::Completed) => {
                updated.status.stage = "sign".into();
            }
            Ok(engine_execution::layerswap::SwapState::Failed(reason)) => {
                eprintln!("intent {intent_id}: gas top-up {reason}");
                updated.status.state = "failed".into();
                updated.status.error = Some(GAS_NOT_READY.into());
            }
            _ => return Ok(Json(updated.status)),
        }
        if !state
            .markets
            .replace_plan(&intent_id, &current, &updated)
            .await?
        {
            return Ok(Json(
                state
                    .markets
                    .get_intent(&intent_id)
                    .await?
                    .map_or(updated.status, |i| i.status),
            ));
        }
        return Ok(Json(updated.status));
    }
    if current.status.state == "pending" && current.status.stage == "fund" {
        let Some(cash) = &current.funding else {
            return Ok(Json(current.status));
        };
        let moved = if cash.relay || cash.authorization.is_some() {
            state.relay_link.state(&cash.swap_id).await.ok()
        } else {
            state.layerswap.swap_state(&cash.swap_id).await.ok()
        };
        let next = match moved {
            Some(engine_execution::layerswap::SwapState::Completed) => {
                Some(("sign", "pending", None))
            }
            Some(engine_execution::layerswap::SwapState::Failed(reason)) => Some((
                "fund",
                "failed",
                Some(format!(
                    "Moving your cash didn't go through ({reason}). It goes back to your balance."
                )),
            )),
            // Still moving, or Layerswap didn't answer this time: ask again on the next poll.
            _ => None,
        };
        let Some((stage, state_now, error)) = next else {
            return Ok(Json(current.status));
        };
        let mut updated = current.clone();
        updated.status.stage = stage.into();
        updated.status.state = state_now.into();
        updated.status.error = error;
        if !state
            .markets
            .replace_plan(&intent_id, &current, &updated)
            .await?
        {
            return Ok(Json(
                state
                    .markets
                    .get_intent(&intent_id)
                    .await?
                    .map_or(updated.status, |i| i.status),
            ));
        }
        return Ok(Json(updated.status));
    }
    if let Some(swap) = &current.own_swap {
        if current.status.state == "pending" && current.status.stage == "settle" {
            let Some(signature) = current.status.tx_ids.last() else {
                return Ok(Json(current.status));
            };
            let landed = state
                .solana_mainnet
                .signature_status(signature)
                .await
                .map_err(unavailable)?;
            let mut status = current.status.clone();
            match landed {
                Some(Ok(())) => {
                    let Some((paid, got)) = state
                        .solana_mainnet
                        .swap_amounts(
                            signature,
                            &current.wallet,
                            &swap.input_mint,
                            &swap.output_mint,
                        )
                        .await
                        .map_err(unavailable)?
                    else {
                        return Ok(Json(status));
                    };
                    status.state = "filled".into();
                    keep_trade(
                        &state.trades,
                        &intent_id,
                        &current,
                        &status,
                        Some(paid),
                        Some(got),
                    )
                    .await;
                }
                Some(Err(_)) => {
                    status.state = "failed".into();
                    status.error = Some("The swap didn't go through. Your tokens are still in your wallet; only the network fee may have been spent.".into());
                }
                None => {
                    if state
                        .solana_mainnet
                        .block_height()
                        .await
                        .map_err(unavailable)?
                        <= swap.last_valid_block_height
                    {
                        return Ok(Json(status));
                    }
                    status.state = "failed".into();
                    status.error = Some(
                        "The swap expired without landing. Your tokens are still in your wallet."
                            .into(),
                    );
                }
            }
            let mut updated = current.clone();
            updated.status = status;
            if !state
                .markets
                .replace_plan(&intent_id, &current, &updated)
                .await?
            {
                return Ok(Json(
                    state
                        .markets
                        .get_intent(&intent_id)
                        .await?
                        .map_or(updated.status, |i| i.status),
                ));
            }
            return Ok(Json(updated.status));
        }
    }
    if current.solana_transfer
        && current.status.state == "pending"
        && current.status.stage == "settle"
    {
        let Some(signature) = current.status.tx_ids.first().cloned() else {
            return Ok(Json(current.status));
        };
        let landed = state
            .solana_mainnet
            .signature_status(&signature)
            .await
            .map_err(unavailable)?;
        let mut updated = current.clone();
        match landed {
            None => return Ok(Json(updated.status)),
            Some(Ok(())) => updated.status.state = "filled".into(),
            Some(Err(error)) => {
                updated.status.state = "failed".into();
                updated.status.error = Some(format!("The transfer didn't go through ({error})"));
            }
        }
        if !state
            .markets
            .replace_plan(&intent_id, &current, &updated)
            .await?
        {
            return Ok(Json(
                state
                    .markets
                    .get_intent(&intent_id)
                    .await?
                    .map_or(updated.status, |i| i.status),
            ));
        }
        return Ok(Json(updated.status));
    }
    if current.status.state != "pending"
        || current.status.stage != "settle"
        || current.chain != "base"
    {
        return Ok(Json(current.status));
    }
    let mut status = current.status.clone();
    let mut last_receipt = Value::Null;
    for (index, hash) in status.tx_ids.iter().enumerate() {
        let tx = base_rpc(&state.markets, "eth_getTransactionByHash", json!([hash])).await?;
        if tx.is_null() {
            return Ok(Json(status));
        }
        let to = tx["to"].as_str().unwrap_or_default().to_ascii_lowercase();
        let input = tx["input"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let from = tx["from"].as_str().unwrap_or_default();
        let (expected_to, expected_input) = &current.expected[index];
        if !from.eq_ignore_ascii_case(&current.wallet)
            || to != *expected_to
            || input != *expected_input
        {
            status.state = "failed".into();
            status.error = Some("reported transaction does not match the signed plan".into());
            break;
        }
        let receipt = base_rpc(&state.markets, "eth_getTransactionReceipt", json!([hash])).await?;
        if receipt.is_null() {
            return Ok(Json(status));
        }
        if receipt["status"].as_str() != Some("0x1") {
            status.state = "failed".into();
            status.error = Some("Base transaction reverted".into());
            break;
        }
        last_receipt = receipt;
    }
    if status.state == "pending" {
        status.state = "filled".into();
        // The swap is the last transaction; what reached the wallet is in its Transfer logs.
        if let Some(trade) = &current.trade {
            let got = received_units(&last_receipt, &trade.receive_token, &current.wallet);
            keep_trade(&state.trades, &intent_id, &current, &status, None, got).await;
        }
    }
    let mut updated = current.clone();
    updated.status = status.clone();
    if !state
        .markets
        .replace_plan(&intent_id, &current, &updated)
        .await?
    {
        return Ok(Json(
            state
                .markets
                .get_intent(&intent_id)
                .await?
                .map_or(updated.status, |i| i.status),
        ));
    }
    Ok(Json(status))
}

// The second step of a Solana buy paid with Base cash: once the cash has landed, a fresh Jupiter
// order for what arrived, for the app to sign without asking again (the user confirmed the whole
// buy). Asking again before it's signed makes a fresh order.
pub(super) async fn next_transactions(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    pin::require_intent(&state, &headers, &intent_id).await?;
    if intent_id.starts_with("prediction-") {
        return predictions::next(state, headers, intent_id).await;
    }
    if intent_id.starts_with("near-intent-") {
        return near_intents::next(state, headers, intent_id).await;
    }
    if intent_id.starts_with("hl-") {
        return hl::next(state, intent_id, headers).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut intent = state
        .markets
        .get_intent(&intent_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if intent.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if intent.status.state != "pending" || intent.status.stage != "sign" {
        return Err((
            StatusCode::CONFLICT,
            "nothing to sign for this intent".into(),
        ));
    }
    if let Some(topup) = intent.base_topup.as_ref().filter(|t| t.uid.is_none()) {
        if topup_expired(topup) {
            intent.base_topup = Some(prepare_base_topup(&state, &intent.wallet).await?);
            state.markets.save_intent(&intent_id, &intent).await?;
        }
        return Ok(Json(
            json!({"transactions":topup_steps(intent.base_topup.as_ref().expect("topup"))}),
        ));
    }
    // A Base buy whose cash came from Solana: a fresh swap for what landed.
    if intent.chain == "base" && intent.trade.is_some() {
        let transactions = base_buy_after_move(&state, &intent_id, &mut intent).await?;
        return Ok(Json(json!({ "transactions": transactions })));
    }
    // Cash landed on Base: the Base transactions planned at the start.
    if intent.chain == "base" {
        let txs: Vec<Value> = intent
            .expected
            .iter()
            .map(
                |(to, data)| json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}),
            )
            .collect();
        return Ok(Json(json!({"transactions":txs})));
    }
    if intent.fee_preview.is_some() {
        return next_fee_step(&state, &intent_id, intent).await;
    }
    let plan = intent
        .trade
        .clone()
        .ok_or_else(|| unavailable("intent has no trade"))?;
    let output_mint = match &intent.buy_mint {
        Some(mint) => mint.clone(),
        None => find_asset(&state.markets, &plan.asset_id).await?.token,
    };
    let cash: u128 = state
        .solana_mainnet
        .owner_token_balances(&intent.wallet)
        .await
        .map_err(unavailable)?
        .iter()
        .filter(|(m, _, _)| m == SOL_USDC)
        .map(|(_, u, _)| *u)
        .sum();
    let amount = plan.pay_units.min(cash);
    if amount == 0 {
        return Err((
            StatusCode::CONFLICT,
            "the cash hasn't reached Solana yet".into(),
        ));
    }
    let (order, own_swap) = state
        .markets
        .jupiter
        .order_for_wallet(
            &JupiterOrderRequest {
                input_mint: SOL_USDC.into(),
                output_mint,
                amount_base_units: amount.try_into().map_err(|_| bad("amount too large"))?,
                taker: Some(intent.wallet.clone()),
            },
            &state.solana_mainnet,
        )
        .await
        .map_err(swap_error)?;
    let out: u128 = order.out_amount.parse().map_err(unavailable)?;
    // Prices move while cash crosses over; more than 5% worse than the quote isn't what they agreed to.
    let expected = mul_div_units(plan.get_units, amount, plan.pay_units);
    if intent.buy_mint.is_none() && out < expected.saturating_mul(95) / 100 {
        intent.status.state = "failed".into();
        intent.status.error = Some(
            "The price moved more than 5% while your cash was moving. The cash is on Solana now; try again."
                .into(),
        );
        state.markets.save_intent(&intent_id, &intent).await?;
        return Err((
            StatusCode::CONFLICT,
            intent.status.error.unwrap_or_default(),
        ));
    }
    let transaction = order.transaction.ok_or((
        StatusCode::BAD_GATEWAY,
        "Jupiter returned no signable transaction".into(),
    ))?;
    let gas = if order.gasless || own_swap.is_some() {
        None
    } else {
        gas_topup(&state, &intent.wallet, amount).await
    };
    intent.request_id = (!order.request_id.is_empty()).then_some(order.request_id);
    intent.own_swap = own_swap;
    intent.gas_request_id = gas.as_ref().map(|(id, _)| id.clone());
    if let Some(trade) = intent.trade.as_mut() {
        trade.pay_units = amount;
        trade.get_units = out;
    }
    state.markets.save_intent(&intent_id, &intent).await?;
    let mut transactions = Vec::new();
    if let Some((_, gas_tx)) = gas {
        transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
    }
    transactions.push(json!({"chain":"solana","transaction":transaction,"submit":"engine"}));
    Ok(Json(json!({ "transactions": transactions })))
}

// Cash for the reserve is already here. Only the exact amount and fee confirmed earlier can leave.
async fn next_fee_step(
    state: &AppState,
    id: &str,
    current: StoredIntent,
) -> Result<Json<Value>, ApiError> {
    let mut intent = current.clone();
    if intent.fee_reserve.is_none() {
        let preview = intent
            .fee_preview
            .as_ref()
            .ok_or_else(|| unavailable("network-fee quote missing"))?;
        let required = preview.cash()
            + intent
                .trade
                .as_ref()
                .filter(|t| t.side == "buy")
                .map_or(0, |t| t.pay_units);
        let reserve = match solana_fees::prepare(state, &intent.wallet, preview,required).await {
            Ok(reserve) => reserve,
            Err(_) => return fail_fee_step(state,id,&intent,"Your cash move is recorded in history, but network fees couldn't be prepared. The purchase wasn't sent; check your balance before requesting a fresh quote.").await,
        };
        intent.fee_reserve = Some(reserve);
        if !state.markets.replace_plan(id, &current, &intent).await? {
            return Err((
                StatusCode::CONFLICT,
                "This step changed; refresh its status.".into(),
            ));
        }
    }
    let reserve = intent.fee_reserve.as_ref().expect("reserve");
    if reserve.signature.is_none() {
        if reserve.expires < now() {
            return fail_fee_step(state,id,&intent,"The network-fee step expired. Your unspent cash is in your wallet; request a fresh quote.").await;
        }
        return Ok(Json(json!({"transactions":[solana_fees::step(reserve)]})));
    }
    if reserve.finish_by < now() {
        return fail_fee_step(state,id,&intent,"The purchase quote expired. Your network-fee reserve and unspent cash are in your wallet; request a fresh quote.").await;
    }
    if let Some(transaction) = &reserve.next_transaction {
        if reserve.next_expires < now() {
            return fail_fee_step(state,id,&intent,"The swap step expired without being sent. Your network-fee reserve and unspent cash are in your wallet; request a fresh quote.").await;
        }
        return Ok(Json(
            json!({"transactions":[{"chain":"solana","transaction":transaction,"submit":"engine"}]}),
        ));
    }
    let plan = intent
        .trade
        .as_ref()
        .ok_or_else(|| unavailable("intent has no trade"))?;
    let asset = find_asset(&state.markets, &plan.asset_id).await?;
    let (input, output) = if plan.side == "buy" {
        (SOL_USDC, &*asset.token)
    } else {
        (&*asset.token, SOL_USDC)
    };
    let request = JupiterOrderRequest {
        input_mint: input.into(),
        output_mint: output.into(),
        amount_base_units: plan
            .pay_units
            .try_into()
            .map_err(|_| bad("amount too large"))?,
        taker: Some(intent.wallet.clone()),
    };
    let (order,own_swap) = match state.markets.jupiter.order_for_wallet(&request,&state.solana_mainnet).await {
        Ok(order) => order,
        Err(_) => return fail_fee_step(state,id,&intent,"The purchase couldn't be prepared. Your network-fee reserve and unspent tokens are in your wallet; request a fresh quote.").await,
    };
    let out: u128 = order.out_amount.parse().map_err(unavailable)?;
    if out == 0
        || !own_swap
            .as_ref()
            .is_some_and(|s| s.minimum_out >= plan.get_units.saturating_mul(99) / 100)
    {
        return fail_fee_step(state,id,&intent,"The price moved beyond your confirmed quote. Your network-fee reserve and unspent tokens are in your wallet; request a fresh quote.").await;
    }
    let transaction = order
        .transaction
        .ok_or_else(|| unavailable("no signable swap"))?;
    intent.own_swap = own_swap;
    intent.request_id = (!order.request_id.is_empty()).then_some(order.request_id);
    let reserve = intent.fee_reserve.as_mut().expect("reserve");
    reserve.next_transaction = Some(transaction.clone());
    reserve.next_expires = now() + 45_000;
    if !state.markets.replace_plan(id, &current, &intent).await? {
        return Err((
            StatusCode::CONFLICT,
            "This step changed; refresh its status.".into(),
        ));
    }
    Ok(Json(
        json!({"transactions":[{"chain":"solana","transaction":transaction,"submit":"engine"}]}),
    ))
}
async fn fail_fee_step(
    state: &AppState,
    id: &str,
    intent: &StoredIntent,
    reason: &str,
) -> Result<Json<Value>, ApiError> {
    let mut failed = intent.clone();
    failed.status.state = "failed".into();
    failed.status.error = Some(reason.into());
    state.markets.replace_plan(id, intent, &failed).await?;
    Err((StatusCode::CONFLICT, reason.into()))
}

// The wallet's next nonce on Base, counting transactions still waiting to be mined.
async fn wallet_nonce(state: &AppState, wallet: &str) -> Result<u64, ApiError> {
    let count = base_rpc(
        &state.markets,
        "eth_getTransactionCount",
        json!([wallet, "pending"]),
    )
    .await?;
    count
        .as_str()
        .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| unavailable("Base nonce unavailable"))
}

// The second step of a Base buy paid with Solana cash: once the cash has landed, a fresh swap (and an
// approval when the router needs one) for what arrived, for the app to send without asking again.
// The plan becomes these transactions, which /signed and status check as usual.
async fn base_buy_after_move(
    state: &AppState,
    intent_id: &str,
    intent: &mut StoredIntent,
) -> Result<Vec<Value>, ApiError> {
    let plan = intent
        .trade
        .clone()
        .ok_or_else(|| unavailable("intent has no trade"))?;
    let asset = find_asset(&state.markets, &plan.asset_id).await?;
    let wallet = intent.wallet.clone();
    // Handed out before and the wallet has sent since: those went out (the app's report of them
    // was lost). A fresh swap now would buy twice; the app reports what it sent instead.
    let nonce = wallet_nonce(state, &wallet).await?;
    if plan.handed_nonce.is_some_and(|handed| nonce > handed) {
        return Err((
            StatusCode::CONFLICT,
            "This buy was already sent; it's settling.".into(),
        ));
    }
    // Relay reports the move done as it fills; Base nodes can be a block behind, so give the cash
    // (and any gas with it) a few seconds to show.
    let mut cash = 0;
    for attempt in 0..6 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        cash = state
            .markets
            .base
            .balance_of(BASE_USDC, &wallet)
            .await
            .map_err(unavailable)?;
        if cash >= plan.pay_units && wallet_pays_gas(state, &wallet).await {
            break;
        }
    }
    let amount = plan.pay_units.min(cash);
    if amount == 0 {
        return Err((
            StatusCode::CONFLICT,
            "the cash hasn't reached Base yet".into(),
        ));
    }
    let fresh = base_swap(&state.markets, &asset, "buy", amount, &wallet).await?;
    // Prices move while cash crosses over; more than 5% worse than the quote isn't what they agreed
    // to. Without gas the swap can't go out (the move brings some to an empty tank).
    let expected = mul_div_units(plan.get_units, amount, plan.pay_units);
    let stopped = if fresh.amount_out < expected.saturating_mul(95) / 100 {
        Some("The price moved more than 5% while your cash was moving. The cash is in your balance now; try again.")
    } else if !wallet_pays_gas(state, &wallet).await {
        Some("Your cash is in your balance now, but the network fee couldn't be covered yet. Try again.")
    } else {
        None
    };
    if let Some(reason) = stopped {
        intent.status.state = "failed".into();
        intent.status.error = Some(reason.into());
        state.markets.save_intent(intent_id, intent).await?;
        return Err((StatusCode::CONFLICT, reason.into()));
    }
    let txs = fresh.txs;
    intent.expected = txs
        .iter()
        .map(|(to, data)| (to.to_ascii_lowercase(), data.to_ascii_lowercase()))
        .collect();
    if let Some(trade) = intent.trade.as_mut() {
        trade.pay_units = fresh.amount_in;
        trade.get_units = fresh.amount_out;
        trade.handed_nonce = Some(nonce);
    }
    state.markets.save_intent(intent_id, intent).await?;
    Ok(txs
        .iter()
        .map(|(to, data)| json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}))
        .collect())
}

// A filled spot trade goes into the trade book (the spot positions). Failing to keep it never
// fails the trade the user already made.
async fn keep_trade(
    book: &positions::TradeBook,
    intent_id: &str,
    intent: &StoredIntent,
    status: &IntentStatus,
    paid: Option<u128>,
    got: Option<u128>,
) {
    let Some(plan) = &intent.trade else {
        return;
    };
    let paid = paid.unwrap_or(plan.pay_units);
    let got = got.unwrap_or(plan.get_units);
    let (token_units, usdc_units) = if plan.side == "buy" {
        (got, paid)
    } else {
        (paid, got)
    };
    let trade = positions::Trade {
        intent_id: intent_id.into(),
        user_id: intent.owner.clone(),
        asset_id: plan.asset_id.clone(),
        side: plan.side.clone(),
        token_units,
        usdc_units,
        tx_id: status.tx_ids.last().cloned(),
        filled_at_ms: now(),
    };
    if let Err((_, error)) = book.record(&trade).await {
        eprintln!("could not keep trade {intent_id}: {error}");
    }
}

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

// Sum of `token` Transfer events into `wallet` in a receipt.
fn received_units(receipt: &Value, token: &str, wallet: &str) -> Option<u128> {
    let wallet = wallet.trim_start_matches("0x").to_ascii_lowercase();
    let mut total: Option<u128> = None;
    for log in receipt["logs"].as_array()? {
        let topics = log["topics"].as_array()?;
        let to = topics
            .get(2)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !log["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(token))
            || topics.first().and_then(Value::as_str) != Some(TRANSFER_TOPIC)
            || !to.ends_with(&wallet)
        {
            continue;
        }
        let data = log["data"].as_str()?.trim_start_matches("0x");
        let (high, low) = data.split_at(data.len().checked_sub(32)?);
        if high.bytes().any(|b| b != b'0') {
            return None;
        }
        let amount = u128::from_str_radix(low, 16).ok()?;
        total = Some(total.unwrap_or(0).checked_add(amount)?);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_24_decimal_coin_prices_in_naira_without_overflowing() {
        // 0.452112 wNEAR bought for $2, at ₦1,328.44 per dollar: about ₦5,876.60 per NEAR.
        let price = unit_price(
            2_000_000,
            452_112_000_000_000_000_000_000,
            24,
            "NGN",
            1_328_440_000,
        )
        .unwrap();
        assert!(price.amount.starts_with("5876.59"), "{}", price.amount);
        // Small decimals keep the exact integer path.
        assert_eq!(
            unit_price(1_000_000, 2_000_000, 6, "USD", 1_000_000)
                .unwrap()
                .amount,
            "0.5"
        );
    }
    #[test]
    fn stats_resolve_mints_and_base_coins_without_a_catalog_request() {
        let bonk = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
        assert_eq!(stats_token(bonk), Some(("solana", bonk.into())));
        assert_eq!(stats_token("brett-base"), Some(("base", BRETT.into())));
        assert_eq!(
            stats_token(&format!("base:{BRETT}")),
            Some(("base", BRETT.into()))
        );
        assert_eq!(stats_token("base:https://example.com"), None);
        assert_eq!(stats_token("unknown"), None);
    }

    #[test]
    fn charts_have_a_coingecko_twin() {
        let deep = "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP";
        assert_eq!(
            coingecko_coin("sui-network", "0x2::sui::SUI").as_deref(),
            Some("coins/sui")
        );
        assert_eq!(
            coingecko_coin("sui-network", deep),
            Some(format!("coins/sui/contract/{deep}"))
        );
        assert_eq!(
            coingecko_coin("near", "wrap.near").as_deref(),
            Some("coins/near")
        );
        assert_eq!(
            coingecko_coin("monad", NATIVE_COIN).as_deref(),
            Some("coins/monad")
        );
        assert_eq!(coingecko_coin("solana", "x"), None);
    }
    fn spot_intent(side: &str, pay: u128, get: u128) -> StoredIntent {
        StoredIntent {
            owner: "did:privy:a".into(),
            wallet: "0x1111111111111111111111111111111111111111".into(),
            chain: "solana".into(),
            expected: Vec::new(),
            request_id: None,
            own_swap: None,
            fee_preview: None,
            fee_reserve: None,
            status: IntentStatus {
                intent_id: format!("intent-{side}"),
                stage: "settle".into(),
                state: "filled".into(),
                tx_ids: vec!["sig".into()],
                error: None,
            },
            trade: Some(PlannedTrade {
                asset_id: "bonk".into(),
                side: side.into(),
                pay_units: pay,
                get_units: get,
                receive_token: String::new(),
                handed_nonce: None,
            }),
            funding: None,
            buy_mint: None,
            gas_request_id: None,
            solana_transfer: false,
            base_topup: None,
        }
    }

    #[test]
    fn stored_intents_survive_the_database_round_trip() {
        // Token units past u64 (memecoins with 9+ decimals) must come back exact.
        let mut intent = spot_intent("buy", 10_000_000, 40_000_000_000_000_000_000);
        intent.expected = vec![("0xpool".into(), "0xdata".into())];
        intent.request_id = Some("jup-req".into());
        let json = serde_json::to_string(&intent).unwrap();
        let back: StoredIntent = serde_json::from_str(&json).unwrap();
        let trade = back.trade.unwrap();
        assert_eq!(trade.get_units, 40_000_000_000_000_000_000);
        assert_eq!(trade.pay_units, 10_000_000);
        assert_eq!(back.expected, intent.expected);
        assert_eq!(back.request_id.as_deref(), Some("jup-req"));
        assert_eq!(back.status.tx_ids, vec!["sig".to_string()]);
    }

    #[tokio::test]
    async fn user_paid_swap_saves_its_signature_with_the_send_once_claim() {
        let markets = MarketState::new().unwrap();
        let mut intent = spot_intent("buy", 500_000, 4_000_000);
        intent.status.stage = "validate".into();
        intent.status.state = "pending".into();
        markets.insert_intent("own-swap", &intent).await.unwrap();
        let mut submitted = intent.clone();
        submitted.status.stage = "settle".into();
        submitted.status.tx_ids = vec!["known-before-send".into()];
        assert!(markets
            .claim_execution_to("own-swap", &submitted, "validate", "settle")
            .await
            .unwrap());
        assert!(!markets
            .claim_execution_to("own-swap", &submitted, "validate", "settle")
            .await
            .unwrap());
        let stored = markets.get_intent("own-swap").await.unwrap().unwrap();
        assert_eq!(stored.status.stage, "settle");
        assert_eq!(stored.status.tx_ids, vec!["known-before-send"]);
        let mut older = serde_json::to_value(&intent).unwrap();
        older.as_object_mut().unwrap().remove("own_swap");
        assert!(serde_json::from_value::<StoredIntent>(older)
            .unwrap()
            .own_swap
            .is_none());
    }

    #[tokio::test]
    async fn a_solana_order_is_claimed_for_execution_once() {
        let markets = MarketState::new().unwrap();
        let mut intent = spot_intent("buy", 1, 1);
        intent.status.stage = "validate".into();
        intent.status.state = "pending".into();
        markets.insert_intent("intent-1", &intent).await.unwrap();
        assert!(markets
            .claim_execution("intent-1", &intent, "validate")
            .await
            .unwrap());
        assert!(!markets
            .claim_execution("intent-1", &intent, "validate")
            .await
            .unwrap());
        assert!(!markets
            .claim_execution("missing", &intent, "validate")
            .await
            .unwrap());
        let stored = markets.get_intent("intent-1").await.unwrap().unwrap();
        assert_eq!(stored.status.stage, "execute");
        // A Base plan keeps the transactions the user sends (lowercased); an empty tank tops up first.
        let base_id = markets
            .register_base_txs(
                "did:privy:a".into(),
                "0xwallet".into(),
                vec![("0xPool".into(), "0xDATA".into())],
                false,
            )
            .await
            .unwrap();
        let topped = markets
            .register_base_txs(
                "did:privy:a".into(),
                "0xwallet".into(),
                vec![("0xPool".into(), "0xDATA".into())],
                true,
            )
            .await
            .unwrap();
        let filled = markets.get_intent(&topped).await.unwrap().unwrap();
        assert!(filled.base_topup.as_ref().is_some_and(|t| t.uid.is_none()));
        let planned = markets.get_intent(&base_id).await.unwrap().unwrap();
        assert_eq!(
            planned.expected,
            vec![("0xpool".to_string(), "0xdata".to_string())]
        );
        assert!(planned.base_topup.is_none());
        // A Solana buy paid with Base cash is claimed once, from the sign stage.
        let mut funded = spot_intent("buy", 5_000_000, 1_000);
        funded.status.stage = "validate".into();
        funded.status.state = "pending".into();
        funded.expected = vec![("0xusdc".into(), "0xtransfer".into())];
        funded.funding = Some(CashMove {
            swap_id: "swap".into(),
            amount_units: 5_400_000,
            tx_hash: None,
            authorization: None,
            relay: false,
        });
        markets
            .insert_intent("intent-funded", &funded)
            .await
            .unwrap();
        funded.status.stage = "sign".into();
        markets.save_intent("intent-funded", &funded).await.unwrap();
        assert!(!markets
            .claim_execution("intent-funded", &funded, "validate")
            .await
            .unwrap());
        assert!(markets
            .claim_execution("intent-funded", &funded, "sign")
            .await
            .unwrap());
        // A gasless move (Relay): the authorization kept for /signed, and moves saved before gasless
        // ones existed still load as transfers.
        let mut gasless = spot_intent("buy", 5_000_000, 1_000);
        gasless.status.stage = "validate".into();
        gasless.status.state = "pending".into();
        gasless.funding = Some(CashMove {
            swap_id: "0xrequest".into(),
            amount_units: 5_035_780,
            tx_hash: None,
            authorization: Some(Authorization {
                typed_data: serde_json::json!({"primaryType":"ReceiveWithAuthorization"}),
                api: "swap".into(),
            }),
            relay: true,
        });
        markets
            .insert_intent("intent-gasless", &gasless)
            .await
            .unwrap();
        let stored = markets.get_intent("intent-gasless").await.unwrap().unwrap();
        assert_eq!(stored.funding.unwrap().authorization.unwrap().api, "swap");
        let earlier: CashMove = serde_json::from_value(
            serde_json::json!({"swap_id":"s","amount_units":1,"tx_hash":null}),
        )
        .unwrap();
        assert!(earlier.authorization.is_none());
        // Intents saved before funding existed still load.
        let old: StoredIntent = serde_json::from_value(serde_json::json!({"owner":"o","wallet":"w","chain":"solana",
            "expected":[],"request_id":null,"status":{"intentId":"i","stage":"settle","state":"filled","txIds":[],"error":null},
            "trade":null})).unwrap();
        assert!(old.funding.is_none());
    }

    #[tokio::test]
    async fn kept_trades_put_tokens_and_usdc_on_the_right_side() {
        let book = positions::TradeBook::default();
        // A buy pays USDC and gets tokens; the venue's filled amounts beat the quote's.
        let buy = spot_intent("buy", 10_000_000, 900);
        keep_trade(&book, "intent-buy", &buy, &buy.status, None, Some(1_000)).await;
        // A sell pays tokens and gets USDC; with nothing reported, the quote stands.
        let sell = spot_intent("sell", 400, 5_000_000);
        keep_trade(&book, "intent-sell", &sell, &sell.status, None, None).await;
        // Kept once, however many times the status is polled.
        keep_trade(&book, "intent-sell", &sell, &sell.status, None, None).await;
        // Sends and Earn plans have no trade to keep.
        let mut send = spot_intent("buy", 1, 1);
        send.trade = None;
        keep_trade(&book, "intent-send", &send, &send.status, None, None).await;
        let p = &positions::fold(&book.for_user("did:privy:a").await.unwrap())["bonk"];
        assert_eq!(p.units, 600);
        assert_eq!(p.cost, 6_000_000);
        assert_eq!(p.realized, 1_000_000);
    }

    #[test]
    fn received_units_reads_transfers_into_the_wallet() {
        let wallet = "0x1111111111111111111111111111111111111111";
        let token = "0x532f27101965dd16442e59d40670faf5ebb142e4";
        let pad = |a: &str| format!("0x{:0>64}", a.trim_start_matches("0x"));
        let transfer = |address: &str, to: &str, amount: u128| json!({"address": address, "topics": [TRANSFER_TOPIC, pad("0x22"), pad(to)], "data": format!("0x{amount:064x}")});
        let receipt = json!({"logs": [
            transfer("0x532F27101965dd16442E59d40670FaF5eBB142E4", wallet, 700),
            // Other tokens and other recipients don't count.
            transfer(BASE_USDC, wallet, 5),
            transfer(token, "0x3333333333333333333333333333333333333333", 9),
            transfer(token, wallet, 300),
        ]});
        assert_eq!(received_units(&receipt, token, wallet), Some(1_000));
        assert_eq!(received_units(&json!({"logs": []}), token, wallet), None);
        assert_eq!(received_units(&Value::Null, token, wallet), None);
    }
    #[test]
    fn catalog_lists_only_verified_liquid_tradable_tokens() {
        let token = |mint: &str,
                     symbol: &str,
                     name: &str,
                     tags: &[&str],
                     verified: bool,
                     liquidity: f64| {
            json!({"id":mint,"symbol":symbol,"name":name,"decimals":8,"isVerified":verified,
                "liquidity":liquidity,"tags":tags,"icon":"https://example.com/i.png",
                "stats24h":{"buyVolume":1000.0,"sellVolume":500.0}})
        };
        let nvdax = solana_asset(&token(
            "NVDAmint",
            "NVDAx",
            "NVIDIA xStock",
            &["verified", "xstocks"],
            true,
            5e6,
        ))
        .unwrap();
        assert_eq!(
            (nvdax.kind.as_str(), nvdax.xstock, nvdax.volume_24h),
            ("stock", true, 1500.0)
        );
        // An impostor with the same symbol but no verification never lists.
        assert!(solana_asset(&token(
            "fake",
            "TSLAx",
            "Tesla xStock",
            &["unknown"],
            false,
            5e6
        ))
        .is_none());
        // Stablecoins and staking/yield tokens belong to cash and Earn, not Trade.
        assert!(solana_asset(&token(
            "usdg",
            "USDG",
            "Global Dollar",
            &["verified", "stable"],
            true,
            5e6
        ))
        .is_none());
        assert!(solana_asset(&token(
            "jito",
            "JitoSOL",
            "Jito Staked SOL",
            &["verified", "lst"],
            true,
            5e6
        ))
        .is_none());
        assert!(solana_asset(&token(
            "pyusd",
            "PYUSD",
            "PayPal USD",
            &["verified"],
            true,
            5e6
        ))
        .is_none());
        // Too thin to fill a real order.
        assert!(
            solana_asset(&token("thin", "THIN", "Thin", &["verified"], true, 5_000.0)).is_none()
        );
        assert_eq!(
            solana_asset(&token(
                "bonk",
                "BONK",
                "Bonk",
                &["verified", "meme"],
                true,
                5e6
            ))
            .unwrap()
            .kind,
            "meme"
        );
        let nflx = solana_asset(&token(
            "NFLXmint",
            "NFLX",
            "Netflix - Backpack Securities",
            &["verified"],
            true,
            5e5,
        ))
        .unwrap();
        assert_eq!(
            (nflx.kind.as_str(), nflx.name.as_str()),
            ("stock", "Netflix")
        );
        let djt = solana_asset(&token(
            "DJTmint",
            "DJT",
            "Trump Media & Technology Group Corp. Common Stock - Backpack Securities",
            &["verified", "backpack"],
            true,
            5e5,
        ))
        .unwrap();
        assert_eq!(djt.name, "Trump Media & Technology Group Corp.");
        let sol = solana_asset(&token(
            SOL_MINT,
            "SOL",
            "Wrapped SOL",
            &["verified"],
            true,
            5e8,
        ))
        .unwrap();
        assert_eq!((sol.name.as_str(), sol.kind.as_str()), ("Solana", "crypto"));
        assert_eq!(base_assets().len(), 3);
        // Pool shares and leveraged wrappers aren't listed; launchpad coins are memes.
        assert!(solana_asset(&token(
            "jlp",
            "JLP",
            "Jupiter Perps",
            &["verified", "defi"],
            true,
            5e8
        ))
        .is_none());
        assert!(solana_asset(&token(
            "xsol",
            "xSOL",
            "Hylo Leveraged SOL",
            &["verified"],
            true,
            5e6
        ))
        .is_none());
        let mut cate = token(
            "CATEmint1111111111111111111111pump",
            "CATE",
            "Cate",
            &["verified"],
            true,
            5e5,
        );
        assert_eq!(solana_asset(&cate).unwrap().kind, "meme");
        cate["id"] = json!("ZCATmint");
        cate["launchpad"] = json!("stonkfun");
        assert_eq!(solana_asset(&cate).unwrap().kind, "meme");
        assert_eq!(
            solana_asset(&token("doge", "DOGE", "Dogecoin", &["verified"], true, 5e6))
                .unwrap()
                .kind,
            "meme"
        );
        // A pasted address may be unverified (flagged), but never dead.
        let pasted = pasted_asset(&token(
            "anon",
            "ANON",
            "Anon",
            &["unknown"],
            false,
            20_000.0,
        ))
        .unwrap();
        assert!(!pasted.verified);
        assert!(pasted_asset(&token("dead", "DEAD", "Dead", &["unknown"], false, 50.0)).is_none());
        assert!(looks_like_mint(
            "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"
        ));
        assert!(!looks_like_mint("bonk"));
        assert!(!looks_like_mint(
            "0x532f27101965dd16442E59d40670FaF5eBB142E4"
        ));
    }
    // Network: the real Jupiter verified list through the real filter.
    // cargo test -p engine-service live_jupiter_catalog -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_jupiter_catalog() {
        let tokens: Vec<Value> = reqwest::Client::new()
            .get(JUPITER_VERIFIED)
            .header("user-agent", "atlas-engine")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let assets: Vec<Asset> = tokens.iter().filter_map(solana_asset).collect();
        let count = |k: &str| assets.iter().filter(|a| a.kind == k).count();
        println!(
            "{} listed of {}: {} crypto, {} stock, {} meme",
            assets.len(),
            tokens.len(),
            count("crypto"),
            count("stock"),
            count("meme")
        );
        let mut top: Vec<&Asset> = assets.iter().collect();
        top.sort_by(|a, b| b.volume_24h.total_cmp(&a.volume_24h));
        println!(
            "top: {:?}",
            top.iter()
                .take(15)
                .map(|a| a.symbol.as_str())
                .collect::<Vec<_>>()
        );
        let tslax: Vec<&Asset> = assets.iter().filter(|a| a.symbol == "TSLAx").collect();
        assert_eq!(tslax.len(), 1, "exactly one TSLAx, the verified one");
        assert_eq!(
            tslax[0].token,
            "XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB"
        );
        assert!(assets
            .iter()
            .any(|a| a.token == SOL_MINT && a.name == "Solana"));
        assert!(!assets
            .iter()
            .any(|a| a.symbol.to_ascii_uppercase().contains("USD")));
        // The real catalog: one Bitcoin and one Ethereum, by their real names, and Crypto by price.
        let catalog = curate(&tokens);
        let listed = |symbol: &str| {
            catalog
                .iter()
                .filter(|a| {
                    a.listed && a.kind == "crypto" && a.symbol.to_ascii_uppercase().contains(symbol)
                })
                .map(|a| format!("{} {} ${:.0}", a.symbol, a.name, a.ref_price))
                .collect::<Vec<_>>()
        };
        println!("BTC rows: {:?}", listed("BTC"));
        println!("ETH rows: {:?}", listed("ETH"));
        let mut crypto: Vec<&Asset> = catalog
            .iter()
            .filter(|a| a.listed && a.kind == "crypto")
            .collect();
        crypto.sort_by(|a, b| b.ref_price.total_cmp(&a.ref_price));
        println!(
            "crypto by price ({}): {:?}",
            crypto.len(),
            crypto
                .iter()
                .take(12)
                .map(|a| format!("{} ${:.2}", a.symbol, a.ref_price))
                .collect::<Vec<_>>()
        );
        let trending = trending_order(catalog.iter().filter(|a| a.listed).collect());
        println!(
            "trending: {:?}",
            trending
                .iter()
                .take(15)
                .map(|a| format!("{} {} {:+.0}%", a.kind, a.symbol, a.change_24h))
                .collect::<Vec<_>>()
        );
        assert_eq!(crypto[0].name, "Bitcoin");
        assert_eq!(crypto[1].name, "Ethereum");
        assert_eq!(
            catalog
                .iter()
                .filter(|a| a.listed && a.name == "Bitcoin")
                .count(),
            1
        );
        assert_eq!(
            catalog
                .iter()
                .filter(|a| a.listed && a.name == "Ethereum")
                .count(),
            1
        );
    }
    #[test]
    fn trending_puts_each_kinds_top_mover_first() {
        let asset = |id: &str, kind: &str, change: f64, volume: f64| Asset {
            id: id.into(),
            symbol: id.into(),
            name: id.into(),
            kind: kind.into(),
            chain: "solana".into(),
            token: id.into(),
            decimals: 6,
            icon_url: None,
            volume_24h: volume,
            xstock: false,
            verified: true,
            ref_price: 1.0,
            change_24h: change,
            listed: true,
        };
        let list = [
            asset("meme1", "meme", 900.0, 1.0),
            asset("meme2", "meme", 300.0, 1.0),
            asset("meme3", "meme", 40.0, 1.0),
            asset("stock1", "stock", 6.0, 1.0),
            asset("stock2", "stock", 1.0, 1.0),
            asset("coin1", "crypto", 12.0, 1.0),
            asset("flat", "crypto", 0.0, 50.0),
            asset("down", "meme", -20.0, 99.0),
        ];
        let order: Vec<&str> = trending_order(list.iter().collect())
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        // Each kind's top mover first (biggest move leading), then the next rank of each kind.
        assert_eq!(&order[..3], &["meme1", "coin1", "stock1"]);
        assert_eq!(&order[3..6], &["meme2", "stock2", "meme3"]);
        // Not rising: last, most traded first.
        assert_eq!(&order[6..], &["down", "flat"]);
    }
    #[test]
    fn reserve_is_inside_the_buy_budget_and_small_buys_stop_before_payment() {
        let preview = solana_fees::Preview {
            amount: 500_000,
            fee: 302_746,
            gas_value: 487_000,
            min_lamports: 3_955_581,
        };
        assert_eq!(
            purchase_input(10_000_000, Some(&preview), "USD", 1_000_000).unwrap(),
            9_197_254
        );
        assert_eq!(
            purchase_input(10_000_000, None, "USD", 1_000_000).unwrap(),
            10_000_000
        );
        let error = purchase_input(800_000, Some(&preview), "NGN", 1_504_690_000).unwrap_err();
        assert!(error.1.contains("₦"));
        assert!(!error.1.contains("USDC") && !error.1.contains("USD"));
    }
    #[tokio::test]
    async fn stale_next_and_poll_cannot_replace_a_sent_fee_step() {
        let state = MarketState::new().unwrap();
        let mut waiting = spot_intent("buy", 1_000_000, 10);
        waiting.status.stage = "sign".into();
        waiting.fee_preview = Some(solana_fees::Preview {
            amount: 500_000,
            fee: 302_746,
            gas_value: 487_000,
            min_lamports: 3_955_581,
        });
        state.insert_intent("reserve-race", &waiting).await.unwrap();
        let mut sent = waiting.clone();
        sent.status.stage = "fund".into();
        sent.status.tx_ids.push("known-reserve-signature".into());
        assert!(state
            .replace_plan("reserve-race", &waiting, &sent)
            .await
            .unwrap());
        assert!(!state
            .replace_plan("reserve-race", &waiting, &waiting)
            .await
            .unwrap());
        assert!(!state
            .claim_execution_to("reserve-race", &sent, "sign", "fund")
            .await
            .unwrap());
        let current = state.get_intent("reserve-race").await.unwrap().unwrap();
        assert_eq!(current.status.stage, "fund");
        assert_eq!(current.status.tx_ids, ["sig", "known-reserve-signature"]);
        let mut old = serde_json::to_value(spot_intent("buy", 1_000_000, 10)).unwrap();
        old.as_object_mut().unwrap().remove("fee_preview");
        old.as_object_mut().unwrap().remove("fee_reserve");
        let old: StoredIntent = serde_json::from_value(old).unwrap();
        assert!(old.fee_preview.is_none() && old.fee_reserve.is_none());
    }
    #[test]
    fn limits_and_shortfalls_speak_the_users_currency() {
        // ₦1,504.69 per dollar.
        let ngn = 1_504_690_000;
        assert_eq!(say_money(1_000_000, "NGN", ngn), "₦1,504.69");
        assert_eq!(say_money(12_345_678_900, "USD", 1_000_000), "$12,345.67");
        assert_eq!(say_money(0, "EUR", 900_000), "€0.00");
        let small = check_limits(33_000, "NGN", ngn).unwrap_err();
        assert_eq!(small.1, "The smallest amount is ₦151");
        let big = check_limits(MAX_USDC + 1, "NGN", ngn).unwrap_err();
        assert_eq!(big.1, "The most at once is ₦15,046,900");
        assert!(check_limits(MIN_USDC, "USD", 1_000_000).is_ok());
        assert_eq!(
            check_limits(MIN_USDC - 1, "USD", 1_000_000).unwrap_err().1,
            "The smallest amount is $1"
        );
        let short = not_enough_cash(2_500_000, "KES", 129_000_000);
        assert_eq!(
            short.1,
            "Not enough in your balance for this. You have KSh 322.50 to spend. Add money to continue."
        );
        assert_eq!(
            short_of_cash().1,
            "Not enough in your balance for this. Add money to continue."
        );
    }
    #[test]
    fn wrapped_copies_of_a_major_list_once_under_its_real_name() {
        let token = |mint: &str, symbol: &str, name: &str, price: f64, tags: &[&str]| {
            json!({"id":mint,"symbol":symbol,"name":name,"decimals":8,"isVerified":true,
                "liquidity":5e6,"usdPrice":price,"tags":tags,"stats24h":{"buyVolume":1000.0,"sellVolume":500.0}})
        };
        let tokens = [
            token(
                MAJORS[0].0,
                "cbBTC",
                "Coinbase Wrapped BTC",
                85_500.0,
                &["verified"],
            ),
            token(
                "wbtc",
                "WBTC",
                "Wrapped BTC (Portal)",
                85_490.0,
                &["verified"],
            ),
            token("xbtc", "xBTC", "OKX Wrapped BTC", 85_225.0, &["verified"]),
            // Different coins that happen to say Bitcoin stay.
            token("pbtc", "PBTC", "Purple Bitcoin", 0.17, &["verified"]),
            token(
                "ibit",
                "IBITon",
                "iShares Bitcoin Trust",
                47.7,
                &["verified", "stocks"],
            ),
            token(MAJORS[1].0, "ETH", "Ether (Portal)", 2_734.0, &["verified"]),
            token("jup", "JUP", "Jupiter", 0.5, &["verified"]),
        ];
        let catalog = curate(&tokens);
        let listed: Vec<(&str, &str)> = catalog
            .iter()
            .filter(|a| a.listed)
            .map(|a| (a.symbol.as_str(), a.name.as_str()))
            .collect();
        assert!(listed.contains(&("BTC", "Bitcoin")));
        assert!(listed.contains(&("ETH", "Ethereum")));
        assert!(listed.contains(&("PBTC", "Purple Bitcoin")));
        assert!(listed.contains(&("IBITon", "iShares Bitcoin Trust")));
        assert!(!listed
            .iter()
            .any(|(s, _)| *s == "WBTC" || *s == "xBTC" || *s == "WETH"));
        // Base WETH stays in the catalog (the balance values it) but off the Trade list.
        assert!(catalog.iter().any(|a| a.id == "weth-base" && !a.listed));
    }
    #[test]
    fn base_addresses_get_their_own_ids() {
        assert!(looks_like_evm_address(
            "0x4ed4E862860beD51a9570b96d89aF5E1B0Efefed"
        ));
        assert!(!looks_like_evm_address(
            "0x4ed4e862860bed51a9570b96d89af5e1b0efefe"
        ));
        assert!(!looks_like_evm_address(
            "0x4ed4e862860bed51a9570b96d89af5e1b0efefeg"
        ));
        assert!(!looks_like_evm_address(
            "So11111111111111111111111111111111111111112"
        ));
        assert!(!looks_like_mint(
            "0x4ed4e862860bed51a9570b96d89af5e1b0efefed"
        ));
        assert_eq!(
            base_token_id("0x4ed4E862860beD51a9570b96d89aF5E1B0Efefed"),
            "base:0x4ed4e862860bed51a9570b96d89af5e1b0efefed"
        );
    }

    #[test]
    fn trending_base_coins_are_listed_liquid_and_new_to_atlas() {
        // Captured from GeckoTerminal's Base trending pools on 2026-10-01 (trimmed): Doppler (xdp) in
        // two pools, boar, Aerodrome, and Solana bridged to Base.
        let body: Value = serde_json::from_str(r#"{"data": [{"attributes": {"name": "xdp / USDC 0.01%", "reserve_in_usd": "1738839.7394", "base_token_price_usd": "0.01987387892049", "volume_usd": {"h24": "180738711.092985"}, "price_change_percentage": {"h24": "-10.148"}}, "relationships": {"base_token": {"data": {"id": "base_0x07b3d902783c3c12b077508c3b5c00113d1291d0", "type": "token"}}}}, {"attributes": {"name": "boar / WETH", "reserve_in_usd": "307397.0505", "base_token_price_usd": "0.0000100124589837383", "volume_usd": {"h24": "542435.61549892"}, "price_change_percentage": {"h24": "22.328"}}, "relationships": {"base_token": {"data": {"id": "base_0x0cbf291ba052174879d90bf781df1a5f2bc5bb07", "type": "token"}}}}, {"attributes": {"name": "AERO / USDC", "reserve_in_usd": "41696707.6057", "base_token_price_usd": "0.809970138927019", "volume_usd": {"h24": "5346262.99400504"}, "price_change_percentage": {"h24": "0.148"}}, "relationships": {"base_token": {"data": {"id": "base_0x940181a94a35a4569e4529a3cdfb74e38fd98631", "type": "token"}}}}, {"attributes": {"name": "xdp / USDC 2%", "reserve_in_usd": "621726.7581", "base_token_price_usd": "0.0199908498069763", "volume_usd": {"h24": "21813099.1334268"}, "price_change_percentage": {"h24": "-10.013"}}, "relationships": {"base_token": {"data": {"id": "base_0x07b3d902783c3c12b077508c3b5c00113d1291d0", "type": "token"}}}}, {"attributes": {"name": "SOL / USDC 0.035%", "reserve_in_usd": "691608.3345", "base_token_price_usd": "118.4936366582", "volume_usd": {"h24": "7238471.92596665"}, "price_change_percentage": {"h24": "-0.525"}}, "relationships": {"base_token": {"data": {"id": "base_0x311935cd80b76769bf2ecc9d8ab7635b2139cf82", "type": "token"}}}}], "included": [{"id": "base_0x07b3d902783c3c12b077508c3b5c00113d1291d0", "attributes": {"address": "0x07b3d902783c3c12b077508c3b5c00113d1291d0", "name": "Doppler Finance", "symbol": "xdp", "decimals": 18, "image_url": "https://coin-images.coingecko.com/coins/images/102175227/large/03_Doppler_Symbol_Gradient_onDark_withBG.png?1785842913", "coingecko_coin_id": "doppler-finance"}}, {"id": "base_0x0cbf291ba052174879d90bf781df1a5f2bc5bb07", "attributes": {"address": "0x0cbf291ba052174879d90bf781df1a5f2bc5bb07", "name": "boar", "symbol": "boar", "decimals": 18, "image_url": "https://coin-images.coingecko.com/coins/images/102178848/large/w81p1dsba20f2viocdhmy2c5ek1n.?1790493250", "coingecko_coin_id": "boar"}}, {"id": "base_0x940181a94a35a4569e4529a3cdfb74e38fd98631", "attributes": {"address": "0x940181a94a35a4569e4529a3cdfb74e38fd98631", "name": "Aerodrome", "symbol": "AERO", "decimals": 18, "image_url": "https://coin-images.coingecko.com/coins/images/31745/large/token.png?1696530564", "coingecko_coin_id": "aerodrome-finance"}}, {"id": "base_0x311935cd80b76769bf2ecc9d8ab7635b2139cf82", "attributes": {"address": "0x311935cd80b76769bf2ecc9d8ab7635b2139cf82", "name": "Solana", "symbol": "SOL", "decimals": 9, "image_url": "https://coin-images.coingecko.com/coins/images/71099/large/solana.jpg?1765793164", "coingecko_coin_id": "base-bridged-sol-base"}}]}"#).unwrap();
        // boar isn't on CoinGecko here, and Atlas already lists AERO.
        let listed = |address: &str| address != "0x0cbf291ba052174879d90bf781df1a5f2bc5bb07";
        let taken = ["AERO".to_string()].into_iter().collect();
        let found = trending_base_assets(&body, listed, &taken);
        let symbols: Vec<&str> = found.iter().map(|(a, _)| a.symbol.as_str()).collect();
        // SOL is a bridged copy: never a second SOL row.
        assert_eq!(symbols, ["xdp"]);
        let (xdp, coingecko) = &found[0];
        assert_eq!(xdp.id, "base:0x07b3d902783c3c12b077508c3b5c00113d1291d0");
        assert_eq!(xdp.chain, "base");
        assert_eq!(xdp.decimals, 18);
        assert!(xdp.verified && xdp.listed);
        assert!(xdp
            .icon_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://")));
        assert!(xdp.change_24h < 0.0 && xdp.volume_24h > 0.0);
        assert_eq!(coingecko.as_deref(), Some("doppler-finance"));
        // Thin pools stay out, whatever else is true of them.
        let mut thin = body.clone();
        for pool in thin["data"].as_array_mut().unwrap() {
            pool["attributes"]["reserve_in_usd"] = json!("50000");
        }
        assert!(trending_base_assets(&thin, |_| true, &Default::default()).is_empty());
    }

    // Network: a Base token found by its address, priced and quoted both ways (nothing is sent).
    // cargo test -p engine-service live_base_tokens -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_base_tokens() {
        let markets = MarketState::new().unwrap();
        let degen = pasted_base_token(&markets, "0x4ed4E862860beD51a9570b96d89aF5E1B0Efefed")
            .await
            .unwrap()
            .unwrap();
        println!(
            "{} ({}), {} decimals, icon {:?}",
            degen.name, degen.symbol, degen.decimals, degen.icon_url
        );
        assert_eq!(degen.id, "base:0x4ed4e862860bed51a9570b96d89af5e1b0efefed");
        assert_eq!(degen.symbol, "DEGEN");
        assert_eq!(
            find_asset(&markets, &degen.id).await.unwrap().token,
            degen.token
        );
        let rate = base_rate(&markets, &degen).await.unwrap();
        println!("$1 buys {:.2} DEGEN", rate as f64 / 1e18);
        let back = base_quote(&markets, &degen, "sell", rate / 2)
            .await
            .unwrap();
        println!(
            "half of that sells for ${:.4} (fee {} ppm)",
            back.amount_out as f64 / 1e6,
            back.fee_ppm
        );
        assert!(back.amount_out > 400_000 && back.amount_out < 520_000);
        // A coin on the fixed list keeps its row; cash, gas and a wallet aren't tokens to trade.
        let brett = find_asset(&markets, "base:0x532f27101965dd16442e59d40670faf5ebb142e4")
            .await
            .unwrap();
        assert_eq!(brett.id, "brett-base");
        assert!(pasted_base_token(&markets, BASE_USDC)
            .await
            .unwrap()
            .is_none());
        assert!(pasted_base_token(&markets, BASE_WETH)
            .await
            .unwrap()
            .is_none());
        assert!(
            pasted_base_token(&markets, "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn jupiter_chart_candles_become_points() {
        let body = json!({"candles":[
            {"time":1790755200,"open":1.0,"high":1.0,"low":1.0,"close":354.57,"volume":1.0},
            {"time":1790754300,"close":"bad"},
            {"time":1790753400,"close":354.2}
        ]});
        assert_eq!(
            jupiter_closes(&body),
            vec![(1790753400000, 354.2), (1790755200000, 354.57)]
        );
    }
    #[test]
    fn chart_points_are_oldest_first_and_skip_bad_candles() {
        let body = json!({"data":{"attributes":{"ohlcv_list":[
            [1790756100, 354.0, 354.0, 353.7, 353.77, 10.0],
            [1790755200, 356.1, 356.2, 353.7, 0.0, 10.0],
            [1790754300, 358.9, 360.6, 358.9, 359.35, 10.0]
        ]}}});
        assert_eq!(
            ohlcv_closes(&body),
            vec![(1790754300000, 359.35), (1790756100000, 353.77)]
        );
    }
    #[test]
    fn coingecko_history_reads_as_closes() {
        // Shape from CoinGecko's market_chart for BRETT on Base, 2026-10-01.
        let body = json!({"prices":[[1790743800000u64, 0.005758461401963607],
            [1790744100000u64, 0.0], [1790830030000u64, 0.005925576268967545]]});
        assert_eq!(
            coingecko_closes(&body),
            vec![
                (1790743800000, 0.005758461401963607),
                (1790830030000, 0.005925576268967545)
            ]
        );
        assert!(coingecko_closes(&json!({"error":"rate limited"})).is_empty());
    }
    #[test]
    fn indicative_prices_keep_small_values() {
        assert_eq!(
            money_from_usd(118.5, "USD", 1_000_000).unwrap()["amount"],
            "118.5"
        );
        assert_eq!(
            money_from_usd(0.0000037958, "NGN", 1_328_000_000).unwrap()["amount"],
            "0.0050408224"
        );
        assert!(money_from_usd(0.0, "USD", 1_000_000).is_err());
    }
    #[test]
    fn money_and_price_keep_small_unit_precision() {
        assert_eq!(parse_micros("20.123456").unwrap(), 20_123_456);
        assert!(parse_micros("0.0000001").is_err());
        assert_eq!(format_units(283, 4), "0.0283");
        let price = unit_price(1_000_000, 1_000_000_000, 5, "NGN", 1_000_000_000).unwrap();
        assert_eq!(price.amount, "0.1");
    }
}

impl MarketState {
    pub(super) async fn history_rows(&self, owner: &str) -> Result<Vec<Value>, ApiError> {
        if let Some(pg) = &self.postgres {
            return pg
                .query(
                    "SELECT payload FROM atlas_intents WHERE owner=$1",
                    &[&owner],
                )
                .await
                .map_err(internal)?
                .into_iter()
                .map(|r| serde_json::from_str(r.get::<_, &str>(0)).map_err(internal))
                .collect();
        }
        self.intents
            .lock()
            .map_err(internal)?
            .values()
            .filter(|i| i.owner == owner)
            .map(|i| {
                serde_json::to_string(i)
                    .map_err(internal)
                    .and_then(|s| serde_json::from_str(&s).map_err(internal))
            })
            .collect()
    }
}

// Save the confirmation's receipt before the phone signs; this does not submit an action.
pub(super) async fn execute_quote(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;

    let quote = state
        .markets
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned();
    let plan =
        execute_quote_inner(State(state.clone()), Path(quote_id), headers, Json(body)).await?;
    let mut receipt = transactions::Receipt::plan(&owner, &plan.0);
    if let Some(q) = quote {
        receipt.symbol = q.asset.symbol;
        receipt.icon_url = q.asset.icon_url;
    }
    receipt.title = transactions::title(&receipt.kind, &receipt.symbol);
    state.history.put(&receipt).await?;
    Ok(plan)
}

impl MarketState {
    pub(super) fn history_asset(&self, id: &str) -> Option<Asset> {
        self.catalog
            .lock()
            .ok()
            .and_then(|c| {
                c.as_ref()
                    .and_then(|(_, assets)| assets.iter().find(|a| a.id == id).cloned())
            })
            .or_else(|| {
                self.pasted
                    .lock()
                    .ok()
                    .and_then(|c| c.get(id).and_then(|(_, a)| a.clone()))
            })
            .or_else(|| base_assets().into_iter().find(|a| a.id == id))
    }
}
