use super::*;
use axum::extract::Query;
use engine_execution::swaps::{
    jupiter::{JupiterClient, JupiterOrderRequest},
    oneinch::BaseSwapRequest,
    uniswap::{UniswapV3Client, BASE_USDC, BASE_WETH},
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
const JUPITER_VERIFIED: &str = "https://lite-api.jup.ag/tokens/v2/tag?query=verified";
const JUPITER_PRICES: &str = "https://lite-api.jup.ag/price/v3";
const JUPITER_SEARCH: &str = "https://lite-api.jup.ag/tokens/v2/search";
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
    if let Some((at, asset)) = state.pasted.lock().map_err(internal)?.get(mint) {
        if at.elapsed() < CATALOG_TTL {
            return Ok(asset.clone());
        }
    }
    let tokens: Vec<Value> = state
        .http
        .get(JUPITER_SEARCH)
        .query(&[("query", mint)])
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(unavailable)?
        .json()
        .await
        .map_err(unavailable)?;
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
        state
            .http
            .get(JUPITER_VERIFIED)
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
    if let Some(asset) = catalog(state).await?.iter().find(|a| a.id == id) {
        return Ok(asset.clone());
    }
    if looks_like_mint(id) {
        if let Some(asset) = pasted_token(state, id).await? {
            return Ok(asset);
        }
    }
    Err((StatusCode::NOT_FOUND, "unsupported asset".into()))
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
    for chunk in missing.chunks(50) {
        let body: Value = state
            .http
            .get(JUPITER_PRICES)
            .query(&[("ids", chunk.join(","))])
            .send()
            .await
            .map_err(unavailable)?
            .error_for_status()
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)?;
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
    Ok(result)
}

#[derive(Clone)]
pub(super) struct MarketState {
    pub(super) base: UniswapV3Client,
    jupiter: JupiterClient,
    rpc: reqwest::Url,
    http: reqwest::Client,
    quotes: Arc<Mutex<HashMap<String, StoredQuote>>>,
    intents: Arc<Mutex<HashMap<String, StoredIntent>>>,
    catalog: Arc<Mutex<Option<(Instant, Arc<Vec<Asset>>)>>>,
    pasted: Arc<Mutex<HashMap<String, (Instant, Option<Asset>)>>>,
    prices: Arc<Mutex<HashMap<String, (Instant, f64, Option<f64>)>>>,
    multipliers: Arc<Mutex<HashMap<String, (Instant, bool)>>>,
    chart_pools: Arc<Mutex<HashMap<String, (Instant, String)>>>,
    charts: ChartCache,
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
    // USDC (6 decimals) to move from Base to Solana before a Solana buy; 0 when Solana cash covers it.
    funding_units: u128,
    // Layerswap's fee inside funding_units, shown as the network fee.
    funding_fee: u128,
    expires: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct StoredIntent {
    owner: String,
    wallet: String,
    chain: String,
    expected: Vec<(String, String)>,
    request_id: Option<String>,
    status: IntentStatus,
    // Set for spot buys and sells, so the fill can be kept as a trade.
    trade: Option<PlannedTrade>,
    // A Solana buy paid with Base cash: Layerswap moves it first (the Base transfer is `expected`).
    #[serde(default)]
    funding: Option<CashMove>,
    // A funded Solana step that isn't a trade (Earn into Jupiter Lend): what /next swaps USDC into.
    #[serde(default)]
    buy_mint: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct CashMove {
    swap_id: String,
    amount_units: u128,
    tx_hash: Option<String>,
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
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuoteRequest {
    pub(super) asset_id: String,
    pub(super) side: String,
    pub(super) amount: Money,
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
        let rpc: reqwest::Url = env::var("ATLAS_BASE_MAINNET_RPC_URL")
            .unwrap_or_else(|_| "https://base-rpc.publicnode.com".into())
            .parse()?;
        Ok(Self {
            base: UniswapV3Client::new(rpc.clone())?,
            jupiter: JupiterClient::new(env::var("JUPITER_API_KEY").ok()),
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
            multipliers: Arc::new(Mutex::new(HashMap::new())),
            chart_pools: Arc::new(Mutex::new(HashMap::new())),
            charts: Arc::new(Mutex::new(HashMap::new())),
            postgres: None,
        })
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
        let mut claimed = intent.clone();
        claimed.status.stage = "execute".into();
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&claimed).map_err(internal)?;
            let rows = pg
                .execute(
                    "UPDATE atlas_intents SET payload=$2,stage='execute',updated_at_ms=$3
                     WHERE intent_id=$1 AND stage=$4",
                    &[&id, &payload, &now_i64(), &from],
                )
                .await
                .map_err(internal)?;
            return Ok(rows == 1);
        }
        let mut intents = self.intents.lock().map_err(internal)?;
        match intents.get_mut(id) {
            Some(stored) if stored.status.stage == from => {
                stored.status.stage = "execute".into();
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl MarketState {
    // The Base transactions Atlas planned for this user's intent, while it's still awaiting them.
    pub(super) async fn planned_base_txs(
        &self,
        intent_id: &str,
        owner: &str,
    ) -> Result<Vec<(String, String)>, ApiError> {
        Ok(self
            .get_intent(intent_id)
            .await?
            .filter(|i| {
                i.owner == owner
                    && match (i.chain.as_str(), i.funding.is_some()) {
                        // Base actions; paid with Solana cash, they're sent once it has landed.
                        ("base", false) => i.status.stage == "validate",
                        ("base", true) => i.status.stage == "sign",
                        // A Solana buy's transfer from Base.
                        (_, true) => i.status.stage == "validate",
                        _ => false,
                    }
            })
            .map(|i| i.expected)
            .unwrap_or_default())
    }
    // One Jupiter swap the user signs (Earn moving USDC in or out of Jupiter Lend), settled through the
    // same /signed path as a Solana trade. Returns the intent, the transaction and what Jupiter expects
    // to deliver.
    pub(super) async fn plan_jupiter_swap(
        &self,
        owner: String,
        wallet: String,
        input_mint: &str,
        output_mint: &str,
        amount: u128,
    ) -> Result<(String, String, u128), ApiError> {
        let order = self
            .jupiter
            .order(&JupiterOrderRequest {
                input_mint: input_mint.into(),
                output_mint: output_mint.into(),
                amount_base_units: amount.try_into().map_err(|_| bad("amount too large"))?,
                taker: Some(wallet.clone()),
            })
            .await
            .map_err(unavailable)?;
        let out: u128 = order.out_amount.parse().map_err(unavailable)?;
        let transaction = order.transaction.ok_or((
            StatusCode::BAD_GATEWAY,
            "Jupiter returned no signable transaction".into(),
        ))?;
        let intent_id = id("intent");
        self.insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet,
                chain: "solana".into(),
                expected: Vec::new(),
                request_id: Some(order.request_id),
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
            },
        )
        .await?;
        Ok((intent_id, transaction, out))
    }
    // A plan of Base transactions the user sends in order; /signed and status check each against it.
    pub(super) async fn register_base_txs(
        &self,
        owner: String,
        wallet: String,
        txs: Vec<(String, String)>,
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
                status,
                trade: None,
                funding: None,
                buy_mint: None,
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
fn unavailable(error: impl std::fmt::Display) -> ApiError {
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
    let n = input_units
        .checked_mul(rate)
        .and_then(|v| v.checked_mul(10u128.pow(token_decimals)))
        .ok_or_else(|| bad("price overflow"))?;
    let d = output_units
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("price overflow"))?;
    let micros = n / d;
    let tail = (n % d)
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("price overflow"))?
        / d;
    let mut amount = format!(
        "{}.{:06}{:06}",
        micros / 1_000_000,
        micros % 1_000_000,
        tail
    );
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
        let q = state
            .base
            .quote_direct(&quote_request(a, side, input))
            .await
            .map_err(unavailable)?;
        Ok((q.amount_in, q.amount_out, q.fee))
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
    app_balance::verified_wallets(&state, &headers).await?;
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
    let rate = app_balance::fx_rate(&currency).await?;
    let raw = q.q.unwrap_or_default();
    let raw = raw.trim();
    let query = raw.to_ascii_lowercase();
    let catalog = catalog(&state.markets).await?;
    // A pasted token address finds that token, listed or not, whatever chip is selected.
    let by_address = looks_like_mint(raw);
    let pasted: Vec<Asset> = if by_address {
        match catalog.iter().find(|a| a.token == raw) {
            Some(listed) => vec![listed.clone()],
            None => pasted_token(&state.markets, raw)
                .await?
                .into_iter()
                .collect(),
        }
    } else {
        Vec::new()
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
    let mints: Vec<String> = picked
        .iter()
        .filter(|a| a.chain == "solana")
        .map(|a| a.token.clone())
        .collect();
    let prices = usd_prices(&state.markets, &mints).await?;
    let mut result = Vec::with_capacity(picked.len());
    for a in picked {
        let (price, change) = if a.chain == "solana" {
            // No live price, no row: never show a stale or guessed one.
            let Some((usd, change)) = prices.get(&a.token) else {
                continue;
            };
            (
                money_from_usd(*usd, &currency, rate)?,
                change.map(|c| format!("{c:.2}")),
            )
        } else {
            let (_, out, _) = venue_quote(&state.markets, a, "buy", 1_000_000).await?;
            (
                json!(unit_price(1_000_000, out, a.decimals, &currency, rate)?),
                None,
            )
        };
        result.push(json!({"assetId":a.id,"symbol":a.symbol,"name":a.name,"kind":a.kind,"price":price,"change24hPct":change,"iconUrl":a.icon_url,"verified":a.verified}));
    }
    if !raw.is_empty() && kind.is_none_or(|k| k == "crypto") {
        result.extend(near_intents::search_assets(&state, raw, &currency, rate).await?);
    }
    Ok(Json(json!({"assets":result})))
}

#[derive(Deserialize)]
pub(super) struct ChartQuery {
    range: Option<String>,
    currency: Option<String>,
}
const GECKOTERMINAL: &str = "https://api.geckoterminal.com/api/v2/networks/";
const JUPITER_CHARTS: &str = "https://datapi.jup.ag/v2/charts/";

// Jupiter chart candles: {"candles":[{"time": unix seconds, "close": usd, ...}]}, oldest first.
const HOUR_MS: u64 = 3_600_000;

// Paradex candles for a perps market, as closing prices. Paradex serves up to hourly candles and 720
// per request, so longer ranges are fetched in 30-day windows and thinned to about one point per
// 4 hours (1M) or per day (1Y).
async fn perp_chart(
    state: &AppState,
    market: &str,
    range: &str,
    currency: &str,
) -> Result<Json<Value>, ApiError> {
    let (resolution, span_ms, step, ttl) = match range {
        "1D" => (15, 24 * HOUR_MS, 1, Duration::from_secs(60)),
        "1W" => (60, 7 * 24 * HOUR_MS, 1, Duration::from_secs(300)),
        "1M" => (60, 30 * 24 * HOUR_MS, 4, Duration::from_secs(900)),
        "1Y" => (60, 365 * 24 * HOUR_MS, 24, Duration::from_secs(3600)),
        _ => return Err(bad("range must be 1D, 1W, 1M or 1Y")),
    };
    let host = if perps::trading_env()? == "prod" {
        "https://api.prod.paradex.trade"
    } else {
        "https://api.testnet.paradex.trade"
    };
    let key = format!("paradex:{market}:{range}");
    let cached = state
        .markets
        .charts
        .lock()
        .map_err(internal)?
        .get(&key)
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, points)| points.clone());
    let points = match cached {
        Some(points) => points,
        None => {
            let end = now();
            let mut start = end.saturating_sub(span_ms);
            let mut closes = Vec::new();
            while start < end {
                let window_end = (start + 30 * 24 * HOUR_MS).min(end);
                let body: Value = state
                    .markets
                    .http
                    .get(format!("{host}/v1/markets/klines"))
                    .query(&[
                        ("symbol", market.to_string()),
                        ("resolution", resolution.to_string()),
                        ("start_at", start.to_string()),
                        ("end_at", window_end.to_string()),
                    ])
                    .send()
                    .await
                    .map_err(unavailable)?
                    .error_for_status()
                    .map_err(|e| unavailable(format!("price history unavailable: {e}")))?
                    .json()
                    .await
                    .map_err(unavailable)?;
                closes.extend(paradex_closes(&body));
                start = window_end;
            }
            closes.sort_by_key(|(ms, _)| *ms);
            closes.dedup_by_key(|(ms, _)| *ms);
            let thinned: Vec<(u64, f64)> = closes.iter().copied().step_by(step).collect();
            if thinned.is_empty() {
                return Err(unavailable("no price history for this market yet"));
            }
            let points = Arc::new(thinned);
            state
                .markets
                .charts
                .lock()
                .map_err(internal)?
                .insert(key, (Instant::now(), points.clone()));
            points
        }
    };
    let rate = app_balance::fx_rate(currency).await?;
    let scale = rate as f64 / 1_000_000.0;
    let series: Vec<Value> = points
        .iter()
        .map(|(ms, usd)| json!([ms, usd * scale]))
        .collect();
    Ok(Json(
        json!({"assetId":market,"range":range,"currency":currency,"points":series}),
    ))
}

// Paradex klines: [[time_ms, open, high, low, close, volume], ...] → (time, close).
fn paradex_closes(body: &Value) -> Vec<(u64, f64)> {
    body["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let ms = row.get(0)?.as_u64()?;
                    let close = row.get(4)?.as_f64().filter(|c| c.is_finite() && *c > 0.0)?;
                    Some((ms, close))
                })
                .collect()
        })
        .unwrap_or_default()
}

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
    // Perps markets chart from Paradex's own candles.
    if asset_id.ends_with("-PERP") {
        return perp_chart(&state, &asset_id, &range, &currency).await;
    }
    // (Jupiter interval, GeckoTerminal timeframe + aggregate, candles, cache). Longer ranges change
    // slowly and are cached longer.
    let (interval, timeframe, aggregate, limit, ttl) = match range.as_str() {
        "1D" => ("15_MINUTE", "minute", 15, 96, Duration::from_secs(60)),
        "1W" => ("1_HOUR", "hour", 1, 168, Duration::from_secs(300)),
        "1M" => ("4_HOUR", "hour", 4, 180, Duration::from_secs(900)),
        "1Y" => ("1_DAY", "day", 1, 365, Duration::from_secs(3600)),
        _ => return Err(bad("range must be 1D, 1W, 1M or 1Y")),
    };
    let asset = find_asset(&state.markets, &asset_id).await?;
    let rate = app_balance::fx_rate(&currency).await?;
    let network = if asset.chain == "base" {
        "base"
    } else {
        "solana"
    };
    let key = format!("{network}:{}:{range}", asset.token);
    let cached = state
        .markets
        .charts
        .lock()
        .map_err(internal)?
        .get(&key)
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, points)| points.clone());
    let points = match cached {
        Some(points) => points,
        None => {
            // Solana: Jupiter's chart data (what jup.ag draws). Base: GeckoTerminal, whose free
            // limit is per IP and often spent on shared hosts, so it may be unavailable.
            // WETH and AAPLc track the same thing as Ether (Portal) and Apple xStock on Solana,
            // whose charts Jupiter has.
            let solana_twin = match asset.id.as_str() {
                "weth-base" => Some("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs"),
                "aaplc-base" => Some("XsbEhLAtcf6HdfpFZ5xEMdqW8nfAvcsP5bdudRLJzJp"),
                _ => None,
            };
            let points = if let Some(mint) = (network == "solana")
                .then_some(asset.token.as_str())
                .or(solana_twin)
            {
                let url = format!(
                    "{JUPITER_CHARTS}{mint}?interval={interval}&to={}&candles={limit}&type=price",
                    now()
                );
                let body: Value = state
                    .markets
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
                let pool = deepest_pool(&state.markets, network, &asset.token).await?;
                let body: Value = gecko(
                    &state.markets,
                    &format!(
                        "{network}/pools/{pool}/ohlcv/{timeframe}?aggregate={aggregate}&limit={limit}&currency=usd&token={}",
                        asset.token
                    ),
                )
                .await?;
                ohlcv_closes(&body)
            };
            if points.is_empty() {
                return Err(unavailable("no price history for this asset yet"));
            }
            let points = Arc::new(points);
            state
                .markets
                .charts
                .lock()
                .map_err(internal)?
                .insert(key, (Instant::now(), points.clone()));
            points
        }
    };
    let scale = rate as f64 / 1_000_000.0;
    let series: Vec<Value> = points
        .iter()
        .map(|(ms, usd)| json!([ms, usd * scale]))
        .collect();
    Ok(Json(
        json!({"assetId":asset.id,"range":range,"currency":currency,"points":series}),
    ))
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
    let input = if req.side == "buy" {
        usdc_units
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
    let (actual_in, actual_out, fee_units) =
        venue_quote(&state.markets, &a, &req.side, input).await?;
    let (asset_units, stable_units) = if req.side == "buy" {
        (actual_out, actual_in)
    } else {
        (actual_in, actual_out)
    };
    // Above what they have: say so now, in their currency, rather than at the confirm. A Solana buy
    // can use Base cash too: Layerswap moves the shortfall over first.
    let mut funding_units = 0;
    let mut funding_fee = 0;
    if let Some(held) = spot_available(&state, &a, &req.side, &user).await {
        if held < actual_in && req.side == "buy" && a.chain == "solana" {
            let base_cash = match user.evm_wallet.as_deref().filter(|w| !w.is_empty()) {
                Some(evm) => state
                    .markets
                    .base
                    .balance_of(BASE_USDC, evm)
                    .await
                    .unwrap_or(0),
                None => 0,
            };
            let (send, fee) = funding_for(&state, actual_in - held).await?;
            if base_cash < send {
                return Err(not_enough_cash(
                    held + base_cash,
                    &req.amount.currency,
                    rate,
                ));
            }
            funding_units = send;
            funding_fee = fee;
        } else if held < actual_in {
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
            json!({"amount":format_units(actual_in,6),"symbol":"USDC","value":money_from_usdc(actual_in,&req.amount.currency,rate)?}),
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
            expires,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":a.id,"side":req.side,"pay":pay,"receive":receive,"price":price,"fee":money_from_usdc(fee_usdc,&req.amount.currency,rate)?,
            "funding":if funding_units > 0 {json!({"from":"Base","to":"Solana","amount":money_from_usdc(funding_units,&req.amount.currency,rate)?,"fee":money_from_usdc(funding_fee,&req.amount.currency,rate)?})} else {Value::Null},
            "expiresAtUnixMs":expires}),
    ))
}

// How much Base USDC to send so at least `shortfall` lands on Solana, and Layerswap's fee in it:
// its fee plus a 1% + $0.05 cushion. Layerswap wants at least $1, so smaller top-ups send $1 (the
// extra stays in the user's Solana cash).
async fn funding_for(state: &AppState, shortfall: u128) -> Result<(u128, u128), ApiError> {
    let base = shortfall.max(1_000_000);
    let fee = state
        .layerswap
        .base_to_solana_fee(base)
        .await
        .map_err(|_| unavailable("Couldn't move cash from Base right now; try again shortly"))?;
    Ok((base + fee + base / 100 + 50_000, fee))
}

// USDC in a Solana wallet (0 if it can't be read).
async fn solana_cash(state: &AppState, owner: &str) -> u128 {
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
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, evm)
        .await
        .map_err(unavailable)?;
    if base_cash >= needed {
        return Ok(None);
    }
    let sol_cash = match solana {
        Some(owner) => solana_cash(state, owner).await,
        None => 0,
    };
    let shortfall = (needed - base_cash).max(1_000_000);
    let fee = state
        .layerswap
        .solana_to_base_fee(shortfall)
        .await
        .map_err(|_| unavailable("Couldn't move cash from Solana right now; try again shortly"))?;
    let send = shortfall + fee + shortfall / 100 + 50_000;
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
    let (send, fee) = funding_for(state, needed - held_on_solana).await?;
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
    let Some((send, fee)) =
        cash_for_base(state, &evm, solana.as_deref(), needed, currency, rate).await?
    else {
        let transactions = as_base(&txs);
        let intent_id = state.markets.register_base_txs(owner, evm, txs).await?;
        return Ok((intent_id, transactions, None));
    };
    let sol = solana.ok_or((
        StatusCode::CONFLICT,
        "Privy Solana wallet is not ready".into(),
    ))?;
    let intent_id = id("intent");
    let deposit = state
        .layerswap
        .solana_to_base(&sol, &evm, send, &intent_id)
        .await
        .map_err(unavailable)?;
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
                }),
                buy_mint: None,
            },
        )
        .await?;
    let transactions =
        vec![json!({"chain":"solana","transaction":deposit.transaction,"submit":"engine"})];
    Ok((intent_id, transactions, Some(fee)))
}

// A Solana swap from USDC into `buy_mint` (Earn into Jupiter Lend) paid with Base cash: the plan is
// one Base transfer; /next makes the swap once the cash lands. Returns the intent and the Base tx.
pub(super) async fn plan_solana_swap_with_base_cash(
    state: &AppState,
    owner: String,
    solana: String,
    evm: &str,
    buy_mint: &str,
    amount: u128,
    send: u128,
) -> Result<(String, Value), ApiError> {
    let intent_id = id("intent");
    let deposit = state
        .layerswap
        .base_to_solana(evm, &solana, send, &intent_id)
        .await
        .map_err(unavailable)?;
    let tx = json!({"chain":"base","chainId":8453,"to":deposit.to,"data":deposit.data,"value":"0"});
    state
        .markets
        .insert_intent(
            &intent_id,
            &StoredIntent {
                owner,
                wallet: solana,
                chain: "solana".into(),
                expected: vec![(
                    deposit.to.to_ascii_lowercase(),
                    deposit.data.to_ascii_lowercase(),
                )],
                request_id: None,
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
                }),
                funding: Some(CashMove {
                    swap_id: deposit.swap_id,
                    amount_units: deposit.amount_units,
                    tx_hash: None,
                }),
                buy_mint: Some(buy_mint.into()),
            },
        )
        .await?;
    Ok((intent_id, tx))
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

pub(super) async fn execute_quote(
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
    let mut funding = None;
    let intent_id = id("intent");
    let output: u128;
    if a.chain == "base" {
        let request = quote_request(&a, &stored.side, stored.input_units);
        let fresh = state
            .markets
            .base
            .quote_direct(&request)
            .await
            .map_err(unavailable)?;
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
        let allowance = state
            .markets
            .base
            .allowance(&request.source_token, &wallet)
            .await
            .map_err(unavailable)?;
        if allowance < fresh.amount_in {
            let approval = state
                .markets
                .base
                .approval_transaction(&request.source_token, &wallet, fresh.amount_in)
                .map_err(unavailable)?;
            expected.push((
                approval.to.to_ascii_lowercase(),
                approval.data.to_ascii_lowercase(),
            ));
            transactions
                .push(json!({"chain":"base","chainId":8453,"to":approval.to,"data":approval.data,"value":"0"}));
        }
        let swap = state
            .markets
            .base
            .swap_transaction(&fresh, &wallet, 100)
            .map_err(unavailable)?;
        expected.push((swap.to.to_ascii_lowercase(), swap.data.to_ascii_lowercase()));
        transactions
            .push(json!({"chain":"base","chainId":8453,"to":swap.to,"data":swap.data,"value":"0"}));
        output = fresh.amount_out;
    } else if stored.funding_units > 0 {
        // Base cash first: one Base transfer to Layerswap now; the Jupiter buy is signed once the
        // USDC lands on Solana (GET /v1/intents/{id}/next).
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
        if base_cash < stored.funding_units {
            return Err(short_of_cash());
        }
        let deposit = state
            .layerswap
            .base_to_solana(&evm, &wallet, stored.funding_units, &intent_id)
            .await
            .map_err(unavailable)?;
        expected.push((
            deposit.to.to_ascii_lowercase(),
            deposit.data.to_ascii_lowercase(),
        ));
        transactions.push(
            json!({"chain":"base","chainId":8453,"to":deposit.to,"data":deposit.data,"value":"0"}),
        );
        funding = Some(CashMove {
            swap_id: deposit.swap_id,
            amount_units: deposit.amount_units,
            tx_hash: None,
        });
        output = stored.output_units;
    } else {
        let order = state
            .markets
            .jupiter
            .order(&JupiterOrderRequest {
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
            })
            .await
            .map_err(unavailable)?;
        output = order.out_amount.parse().map_err(unavailable)?;
        if output < stored.output_units.saturating_mul(99) / 100 {
            return Err((
                StatusCode::CONFLICT,
                "market price changed; request a fresh quote".into(),
            ));
        }
        transactions.push(json!({"chain":"solana","transaction":order.transaction.ok_or((StatusCode::BAD_GATEWAY,"Jupiter returned no signable transaction".into()))?,"submit":"engine"}));
        request_id = Some(order.request_id);
    }
    let expires = now()
        + if a.chain == "base" || funding.is_some() {
            120_000
        } else {
            45_000
        };
    // The confirm sheet speaks their currency: cash as money, the asset as tokens.
    let rate = app_balance::fx_rate(&stored.currency).await?;
    let summary = if funding.is_some() {
        json!([
            {"label":"You pay","value":say_money(stored.input_units, &stored.currency, rate)},
            {"label":"You get (about)","value":format!("{} {}", format_units(output, a.decimals), a.symbol)},
            {"label":"Network fee","value":say_money(stored.funding_fee.max(1), &stored.currency, rate)},
        ])
    } else if stored.side == "buy" {
        json!([
            {"label":"You pay","value":say_money(stored.input_units, &stored.currency, rate)},
            {"label":"You get (about)","value":format!("{} {}", format_units(output, a.decimals), a.symbol)},
        ])
    } else {
        json!([
            {"label":"You sell","value":format!("{} {}", format_units(stored.input_units, a.decimals), a.symbol)},
            {"label":"You get (about)","value":say_money(output, &stored.currency, rate)},
        ])
    };
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
                }),
                funding,
                buy_mint: None,
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
    if intent_id.starts_with("near-intent-") {
        return near_intents::signed(state, headers, intent_id, body).await;
    }
    if intent_id.starts_with("perp-") {
        return perps::trade::signed(state, intent_id, headers, body).await;
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
    let second_step = current.funding.is_some() && current.status.stage == "sign";
    if current.status.state != "pending" || (current.status.stage != "validate" && !second_step) {
        return Ok(Json(current.status));
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
            if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
                return Err(bad("signed report does not match the transfer from Solana"));
            }
            state
                .solana_mainnet
                .send_signed(&body.signed[0].transaction)
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
        if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
            return Err(bad("signed report does not match Jupiter execution plan"));
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
        match state
            .markets
            .jupiter
            .execute(request_id, &body.signed[0].transaction)
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

pub(super) async fn intent_status(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<IntentStatus>, ApiError> {
    if intent_id.starts_with("near-intent-") {
        return near_intents::status(state, headers, intent_id).await;
    }
    if intent_id.starts_with("perp-") {
        return perps::trade::status(state, intent_id, headers).await;
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
    // Base cash moving to Solana: Layerswap says when it has landed; then the buy can be signed.
    if current.status.state == "pending" && current.status.stage == "fund" {
        let Some(cash) = &current.funding else {
            return Ok(Json(current.status));
        };
        let next = match state.layerswap.swap_state(&cash.swap_id).await {
            Ok(engine_execution::layerswap::SwapState::Completed) => {
                Some(("sign", "pending", None))
            }
            Ok(engine_execution::layerswap::SwapState::Failed(reason)) => {
                let (from, to) = if current.chain == "solana" {
                    ("Base", "Solana")
                } else {
                    ("Solana", "Base")
                };
                Some((
                    "fund",
                    "failed",
                    Some(format!(
                        "Moving your cash from {from} to {to} didn't go through ({reason}). Layerswap returns it to {from}."
                    )),
                ))
            }
            // Still moving, or Layerswap didn't answer this time: ask again on the next poll.
            _ => None,
        };
        let Some((stage, state_now, error)) = next else {
            return Ok(Json(current.status));
        };
        let mut updated = current;
        updated.status.stage = stage.into();
        updated.status.state = state_now.into();
        updated.status.error = error;
        state.markets.save_intent(&intent_id, &updated).await?;
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
    let mut updated = current;
    updated.status = status.clone();
    state.markets.save_intent(&intent_id, &updated).await?;
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
    let order = state
        .markets
        .jupiter
        .order(&JupiterOrderRequest {
            input_mint: SOL_USDC.into(),
            output_mint,
            amount_base_units: amount.try_into().map_err(|_| bad("amount too large"))?,
            taker: Some(intent.wallet.clone()),
        })
        .await
        .map_err(unavailable)?;
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
    intent.request_id = Some(order.request_id);
    if let Some(trade) = intent.trade.as_mut() {
        trade.pay_units = amount;
        trade.get_units = out;
    }
    state.markets.save_intent(&intent_id, &intent).await?;
    Ok(Json(
        json!({"transactions":[{"chain":"solana","transaction":transaction,"submit":"engine"}]}),
    ))
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
    fn spot_intent(side: &str, pay: u128, get: u128) -> StoredIntent {
        StoredIntent {
            owner: "did:privy:a".into(),
            wallet: "0x1111111111111111111111111111111111111111".into(),
            chain: "solana".into(),
            expected: Vec::new(),
            request_id: None,
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
            }),
            funding: None,
            buy_mint: None,
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
        // Base plans are readable by the relay only while they still await the user's transactions.
        let base_id = markets
            .register_base_txs(
                "did:privy:a".into(),
                "0xwallet".into(),
                vec![("0xPool".into(), "0xDATA".into())],
            )
            .await
            .unwrap();
        let planned = markets
            .planned_base_txs(&base_id, "did:privy:a")
            .await
            .unwrap();
        assert_eq!(planned, vec![("0xpool".to_string(), "0xdata".to_string())]);
        assert!(markets
            .planned_base_txs(&base_id, "did:privy:b")
            .await
            .unwrap()
            .is_empty());
        // A Solana buy paid with Base cash: the relay sees its Base transfer while it awaits it, and
        // the buy is claimed once, from the sign stage.
        let mut funded = spot_intent("buy", 5_000_000, 1_000);
        funded.status.stage = "validate".into();
        funded.status.state = "pending".into();
        funded.expected = vec![("0xusdc".into(), "0xtransfer".into())];
        funded.funding = Some(CashMove {
            swap_id: "swap".into(),
            amount_units: 5_400_000,
            tx_hash: None,
        });
        markets
            .insert_intent("intent-funded", &funded)
            .await
            .unwrap();
        assert_eq!(
            markets
                .planned_base_txs("intent-funded", "did:privy:a")
                .await
                .unwrap(),
            vec![("0xusdc".to_string(), "0xtransfer".to_string())]
        );
        funded.status.stage = "sign".into();
        markets.save_intent("intent-funded", &funded).await.unwrap();
        assert!(markets
            .planned_base_txs("intent-funded", "did:privy:a")
            .await
            .unwrap()
            .is_empty());
        assert!(!markets
            .claim_execution("intent-funded", &funded, "validate")
            .await
            .unwrap());
        assert!(markets
            .claim_execution("intent-funded", &funded, "sign")
            .await
            .unwrap());
        // Intents saved before funding existed still load.
        let old: StoredIntent = serde_json::from_value(serde_json::json!({"owner":"o","wallet":"w","chain":"solana",
            "expected":[],"request_id":null,"status":{"intentId":"i","stage":"settle","state":"filled","txIds":[],"error":null},
            "trade":null})).unwrap();
        assert!(old.funding.is_none());
        let mut sent = markets.get_intent(&base_id).await.unwrap().unwrap();
        sent.status.stage = "settle".into();
        markets.save_intent(&base_id, &sent).await.unwrap();
        assert!(markets
            .planned_base_txs(&base_id, "did:privy:a")
            .await
            .unwrap()
            .is_empty());
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
    fn paradex_candles_become_closes() {
        let body = json!({"results":[[1790186400000u64,84312.9,84478.8,84162.4,84372.7,195400],
            [1790190000000u64,84372.7,84569.2,84267.2,0,34140],"bad"]});
        assert_eq!(paradex_closes(&body), vec![(1790186400000, 84372.7)]);
        assert!(paradex_closes(&json!({"error":"x"})).is_empty());
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
