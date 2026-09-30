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
}

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
    };
    vec![
        base(
            "weth-base",
            "WETH",
            "Wrapped Ether",
            "crypto",
            BASE_WETH,
            18,
        ),
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
        .trim_end_matches(" - Backpack Securities")
        .trim_end_matches(" Common Stock")
        .trim();
    let (symbol, name) = if mint == SOL_MINT {
        ("SOL", "Solana")
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
    let mut solana: Vec<Asset> = tokens.iter().filter_map(solana_asset).collect();
    // One row per symbol within a kind (there are two WBTCs, for one): the most traded wins.
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
    let assets = Arc::new(assets);
    *state.catalog.lock().map_err(internal)? = Some((Instant::now(), assets.clone()));
    Ok(assets)
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
}
type ChartCache = Arc<Mutex<HashMap<String, (Instant, Arc<Vec<(u64, f64)>>)>>>;
#[derive(Clone)]
struct StoredQuote {
    owner: String,
    asset: Asset,
    side: String,
    currency: String,
    display_amount: String,
    input_units: u128,
    output_units: u128,

    expires: u64,
}
#[derive(Clone)]
struct StoredIntent {
    owner: String,
    wallet: String,
    chain: String,
    expected: Vec<(String, String)>,
    request_id: Option<String>,
    status: IntentStatus,
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
    asset_id: String,
    side: String,
    amount: Money,
}
#[derive(Clone, Deserialize, Serialize)]
struct Money {
    amount: String,
    currency: String,
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
    index: usize,
    transaction: String,
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
        })
    }
}

impl MarketState {
    // The Base transactions Atlas planned for this user's intent, while it's still awaiting them.
    pub(super) fn planned_base_txs(
        &self,
        intent_id: &str,
        owner: &str,
    ) -> Result<Vec<(String, String)>, ApiError> {
        Ok(self
            .intents
            .lock()
            .map_err(internal)?
            .get(intent_id)
            .filter(|i| i.owner == owner && i.chain == "base" && i.status.stage == "validate")
            .map(|i| i.expected.clone())
            .unwrap_or_default())
    }
    pub(super) fn register_base_transfer(
        &self,
        owner: String,
        wallet: String,
        to: String,
        data: String,
    ) -> Result<String, ApiError> {
        self.register_base_txs(owner, wallet, vec![(to, data)])
    }
    // A plan of Base transactions the user sends in order; /signed and status check each against it.
    pub(super) fn register_base_txs(
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
        self.intents.lock().map_err(internal)?.insert(
            intent_id.clone(),
            StoredIntent {
                owner,
                wallet,
                chain: "base".into(),
                expected: txs
                    .into_iter()
                    .map(|(to, data)| (to.to_ascii_lowercase(), data.to_ascii_lowercase()))
                    .collect(),
                request_id: None,
                status,
            },
        );
        Ok(intent_id)
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
fn checked_currency(currency: &str) -> Result<(), ApiError> {
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
fn unit_price(
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
            .filter(|a| kind.is_none_or(|k| a.kind == k))
            .filter(|a| {
                query.is_empty()
                    || a.symbol.to_ascii_lowercase().contains(&query)
                    || a.name.to_ascii_lowercase().contains(&query)
            })
            .collect()
    };
    // Most traded first (a search puts close name matches ahead of that).
    picked.sort_by(|a, b| {
        let by_match = if query.is_empty() { 0 } else { rank(a) }.cmp(&if query.is_empty() {
            0
        } else {
            rank(b)
        });
        by_match.then(b.volume_24h.total_cmp(&a.volume_24h))
    });
    picked.truncate(if !query.is_empty() {
        30
    } else if kind.is_none() {
        40
    } else {
        60
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
fn money_from_usd(usd: f64, currency: &str, rate: u128) -> Result<Value, ApiError> {
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
    if usdc_units < 100_000 || usdc_units > 10_000_000_000 {
        return Err(bad("amount must be between 0.10 and 10000 USD"));
    }
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
            display_amount: req.amount.amount,
            input_units: actual_in,
            output_units: actual_out,
            expires,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":a.id,"side":req.side,"pay":pay,"receive":receive,"price":price,"fee":money_from_usdc(fee_usdc,&req.amount.currency,rate)?,"expiresAtUnixMs":expires}),
    ))
}

pub(super) async fn execute_quote(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, ApiError> {
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
            return Err((
                StatusCode::CONFLICT,
                "insufficient Base mainnet token balance".into(),
            ));
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
    let intent_id = id("intent");
    let expires = now() + if a.chain == "base" { 120_000 } else { 45_000 };
    let receive = if stored.side == "buy" {
        format!("{} {}", format_units(output, a.decimals), a.symbol)
    } else {
        format!("{} USDC", format_units(output, 6))
    };
    let pay = if stored.side == "buy" {
        format!("{} USDC", format_units(stored.input_units, 6))
    } else {
        format!(
            "{} {}",
            format_units(stored.input_units, a.decimals),
            a.symbol
        )
    };
    let status = IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: Vec::new(),
        error: None,
    };
    state.markets.intents.lock().map_err(internal)?.insert(
        intent_id.clone(),
        StoredIntent {
            owner: user.user_id,
            wallet,
            chain: a.chain.clone(),
            expected,
            request_id,
            status,
        },
    );
    Ok(Json(
        json!({"intentId":intent_id,"kind":stored.side,"summary":[{"label":"Pay","value":pay},{"label":"Receive (estimated)","value":receive},{"label":"Display currency","value":stored.currency},{"label":"Requested value","value":stored.display_amount}],"transactions":transactions,"expiresAtUnixMs":expires}),
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
    if intent_id.starts_with("perp-") {
        return perps::trade::signed(state, intent_id, headers, body).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
        .markets
        .intents
        .lock()
        .map_err(internal)?
        .get(&intent_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if current.status.state != "pending" || current.status.stage != "validate" {
        return Ok(Json(current.status));
    }
    let mut status = current.status.clone();
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
        {
            let mut intents = state.markets.intents.lock().map_err(internal)?;
            let stored = intents
                .get_mut(&intent_id)
                .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
            if stored.status.stage != "validate" {
                return Ok(Json(stored.status.clone()));
            }
            stored.status.stage = "execute".into();
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
                status.tx_ids.push(result.signature);
                status.stage = "settle".into();
                status.state = "filled".into();
            }
            Err(error) => {
                status.stage = "settle".into();
                status.state = "failed".into();
                status.error = Some(error.to_string());
            }
        }
    }
    state
        .markets
        .intents
        .lock()
        .map_err(internal)?
        .get_mut(&intent_id)
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?
        .status = status.clone();
    Ok(Json(status))
}

pub(super) async fn intent_status(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<IntentStatus>, ApiError> {
    if intent_id.starts_with("perp-") {
        return perps::trade::status(state, intent_id, headers).await;
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
        .markets
        .intents
        .lock()
        .map_err(internal)?
        .get(&intent_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if current.status.state != "pending"
        || current.status.stage != "settle"
        || current.chain != "base"
    {
        return Ok(Json(current.status));
    }
    let mut status = current.status.clone();
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
    }
    if status.state == "pending" {
        status.state = "filled".into();
    }
    state
        .markets
        .intents
        .lock()
        .map_err(internal)?
        .get_mut(&intent_id)
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?
        .status = status.clone();
    Ok(Json(status))
}

#[cfg(test)]
mod tests {
    use super::*;
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
