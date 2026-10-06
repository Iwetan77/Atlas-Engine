//! Perps on Hyperliquid, behind the same API the app already uses. The account is the user's own EVM
//! wallet. Margin moves in from the balance in the same confirmation (Relay: gasless from Base, or a
//! Solana transaction the user signs); then the user's Atlas agent (approved once, by their own
//! wallet, inside that confirmation) sets leverage and places an immediate-or-cancel order. The
//! agent can trade, never withdraw. A close sends the money it freed back to the user's cash in the
//! same confirmation (Relay, signed by the user's own session through the bridge). Stocks,
//! commodities, indices and currencies come from the `xyz` dex, which keeps its own margin: the agent
//! moves margin between the user's own balances there (`agentSendAsset` can't pay anyone else).
use super::*;
use axum::extract::Query;
use engine_execution::hyperliquid::{order_price, order_size, Market, PositionTrigger, DEXES};
use serde_json::{json, Value};

const QUOTE_MS: u64 = 45_000;
// Every market's list in one call; refreshed in the background, fetched on the spot when older.
const MARKETS_TTL: Duration = Duration::from_secs(30);
const REFRESH_EVERY: Duration = Duration::from_secs(20);
// Hyperliquid limits requests per address and shared hosting shares addresses: when it says "too
// many", the last good list keeps serving for a while, and refreshes slow down instead of piling on.
pub(super) fn usage(state: &AppState) -> Value {
    let u = state.hl.client.usage();
    json!({"requestsLastMinute": u.requests, "refusedLastMinute": u.refused, "lastStatus": u.last_status,
        "relay": env::var("HYPERLIQUID_RELAY_URL").is_ok_and(|v| !v.trim().is_empty())})
}

// HYPERLIQUID_RELAY_URL + HYPERLIQUID_RELAY_SECRET: Atlas's Cloudflare Worker that asks Hyperliquid
// from Cloudflare's addresses (Render's shared ones get "429 Too Many Requests"). Without them, or
// with a bad pair, requests go direct.
fn relayed(
    client: engine_execution::hyperliquid::HyperliquidClient,
) -> engine_execution::hyperliquid::HyperliquidClient {
    let (Ok(url), Ok(secret)) = (
        env::var("HYPERLIQUID_RELAY_URL"),
        env::var("HYPERLIQUID_RELAY_SECRET"),
    ) else {
        return client;
    };
    match client.clone().with_relay(url.trim(), secret.trim()) {
        Ok(relayed) => relayed,
        Err(error) => {
            eprintln!("hyperliquid relay not used: {error}");
            client
        }
    }
}

const STALE_OK: Duration = Duration::from_secs(5 * 60);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
// Hyperliquid's base taker fee (0.045%), and the price room a market order allows (3%).
const TAKER_FEE: f64 = 0.00045;
const SLIPPAGE: f64 = 0.03;
// A take-profit or stop-loss closes at market: it may fill up to 10% past its trigger (Hyperliquid's
// own setting for these).
const TPSL_SLIPPAGE: f64 = 0.10;
// Hyperliquid's smallest order, in dollars of notional.
const MIN_NOTIONAL: f64 = 10.0;
// How long margin may take to arrive (Relay usually takes seconds).
const FUNDING_LIMIT: Duration = Duration::from_secs(300);
// Less than $1 stays in perps for the next trade; a cash-out is watched for up to 2 minutes.
const CASHOUT_MIN_UNITS: u128 = 1_000_000;
const CASHOUT_LIMIT: Duration = Duration::from_secs(120);
// Coins Hyperliquid lists as memes (the rest are crypto).
const MEMES: [&str; 22] = [
    "kPEPE", "kBONK", "kSHIB", "kFLOKI", "kNEIRO", "DOGE", "WIF", "POPCAT", "FARTCOIN", "PENGU",
    "TRUMP", "MOODENG", "PNUT", "BRETT", "GOAT", "SPX", "MEW", "BOME", "TURBO", "MOG", "MEME",
    "PUMP",
];
// Names for Hyperliquid's own coins where one is known, so search finds them ("Bitcoin").
const COIN_NAMES: &[(&str, &str)] = &[
    ("BTC", "Bitcoin"),
    ("ETH", "Ethereum"),
    ("SOL", "Solana"),
    ("HYPE", "Hyperliquid"),
    ("BNB", "BNB"),
    ("SUI", "Sui"),
    ("XRP", "XRP"),
    ("AVAX", "Avalanche"),
    ("LINK", "Chainlink"),
    ("NEAR", "NEAR"),
    ("TAO", "Bittensor"),
    ("ADA", "Cardano"),
    ("LTC", "Litecoin"),
    ("UNI", "Uniswap"),
    ("AAVE", "Aave"),
    ("ENA", "Ethena"),
    ("ETHFI", "ether.fi"),
    ("JTO", "Jito"),
    ("JUP", "Jupiter"),
    ("KAITO", "Kaito"),
    ("LDO", "Lido DAO"),
    ("MORPHO", "Morpho"),
    ("ONDO", "Ondo"),
    ("PENDLE", "Pendle"),
    ("PYTH", "Pyth Network"),
    ("STRK", "Starknet"),
    ("TRX", "TRON"),
    ("XMR", "Monero"),
    ("ZEC", "Zcash"),
    ("MON", "Monad"),
    ("XPL", "Plasma"),
    ("WLFI", "World Liberty Financial"),
    ("DOGE", "Dogecoin"),
    ("PUMP", "Pump.fun"),
    ("TRUMP", "Official Trump"),
    ("kPEPE", "Pepe (per 1,000)"),
    ("kSHIB", "Shiba Inu (per 1,000)"),
    ("kBONK", "Bonk (per 1,000)"),
];
// The `xyz` dex: names we can give with confidence, and every market that isn't a stock (anything
// not here keeps its ticker and counts as a stock).
const XYZ_MARKETS: &[(&str, &str, &str)] = &[
    ("XYZ100", "Nasdaq-100 (XYZ100)", "index"),
    ("SP500", "S&P 500", "index"),
    ("JP225", "Nikkei 225", "index"),
    ("KR200", "KOSPI 200", "index"),
    ("NIFTY", "Nifty 50", "index"),
    ("IBOV", "Ibovespa", "index"),
    ("VIX", "VIX volatility index", "index"),
    ("GOLD", "Gold", "commodity"),
    ("SILVER", "Silver", "commodity"),
    ("PLATINUM", "Platinum", "commodity"),
    ("PALLADIUM", "Palladium", "commodity"),
    ("COPPER", "Copper", "commodity"),
    ("ALUMINIUM", "Aluminium", "commodity"),
    ("URANIUM", "Uranium", "commodity"),
    ("CL", "Crude oil (WTI)", "commodity"),
    ("BRENTOIL", "Brent crude", "commodity"),
    ("NATGAS", "Natural gas", "commodity"),
    ("HO", "Heating oil", "commodity"),
    ("TTF", "Dutch TTF gas", "commodity"),
    ("CORN", "Corn", "commodity"),
    ("WHEAT", "Wheat", "commodity"),
    ("EUR", "Euro", "currency"),
    ("GBP", "British pound", "currency"),
    ("JPY", "Japanese yen", "currency"),
    ("KRW", "Korean won", "currency"),
    ("DXY", "US dollar index", "currency"),
    ("TSLA", "Tesla", "stock"),
    ("NVDA", "Nvidia", "stock"),
    ("AAPL", "Apple", "stock"),
    ("MSFT", "Microsoft", "stock"),
    ("GOOGL", "Alphabet", "stock"),
    ("AMZN", "Amazon", "stock"),
    ("META", "Meta Platforms", "stock"),
    ("NFLX", "Netflix", "stock"),
    ("ORCL", "Oracle", "stock"),
    ("AMD", "AMD", "stock"),
    ("INTC", "Intel", "stock"),
    ("MU", "Micron", "stock"),
    ("TSM", "TSMC", "stock"),
    ("AVGO", "Broadcom", "stock"),
    ("QCOM", "Qualcomm", "stock"),
    ("AMAT", "Applied Materials", "stock"),
    ("ASML", "ASML", "stock"),
    ("ARM", "Arm Holdings", "stock"),
    ("MRVL", "Marvell", "stock"),
    ("SNDK", "Sandisk", "stock"),
    ("WDC", "Western Digital", "stock"),
    ("DELL", "Dell", "stock"),
    ("IBM", "IBM", "stock"),
    ("NOW", "ServiceNow", "stock"),
    ("NET", "Cloudflare", "stock"),
    ("CRWD", "CrowdStrike", "stock"),
    ("PLTR", "Palantir", "stock"),
    ("HOOD", "Robinhood", "stock"),
    ("COIN", "Coinbase", "stock"),
    ("MSTR", "Strategy", "stock"),
    ("CRCL", "Circle", "stock"),
    ("CRWV", "CoreWeave", "stock"),
    ("NBIS", "Nebius", "stock"),
    ("RDDT", "Reddit", "stock"),
    ("RIVN", "Rivian", "stock"),
    ("RKLB", "Rocket Lab", "stock"),
    ("GME", "GameStop", "stock"),
    ("BABA", "Alibaba", "stock"),
    ("COST", "Costco", "stock"),
    ("LLY", "Eli Lilly", "stock"),
    ("MRNA", "Moderna", "stock"),
    ("HIMS", "Hims & Hers", "stock"),
    ("DKNG", "DraftKings", "stock"),
    ("EBAY", "eBay", "stock"),
    ("ZM", "Zoom", "stock"),
    ("BX", "Blackstone", "stock"),
    ("CVX", "Chevron", "stock"),
    ("GEV", "GE Vernova", "stock"),
    ("NOK", "Nokia", "stock"),
    ("BB", "BlackBerry", "stock"),
    ("BE", "Bloom Energy", "stock"),
    ("LITE", "Lumentum", "stock"),
    ("SMSN", "Samsung Electronics", "stock"),
    ("SKHX", "SK Hynix", "stock"),
    ("SOFTBANK", "SoftBank", "stock"),
    ("HYUNDAI", "Hyundai Motor", "stock"),
    ("KIOXIA", "Kioxia", "stock"),
    ("SPCX", "SpaceX", "stock"),
    ("EWY", "iShares MSCI South Korea ETF", "stock"),
    ("EWJ", "iShares MSCI Japan ETF", "stock"),
    ("EWZ", "iShares MSCI Brazil ETF", "stock"),
    ("EWT", "iShares MSCI Taiwan ETF", "stock"),
    ("SMH", "VanEck Semiconductor ETF", "stock"),
    ("TLT", "iShares 20+ Year Treasury Bond ETF", "stock"),
];

#[derive(Clone)]
pub(super) struct HlState {
    client: engine_execution::hyperliquid::HyperliquidClient,
    markets: Arc<Mutex<Option<(Instant, Arc<Vec<Market>>)>>>,
    quotes: Arc<Mutex<HashMap<String, HlQuote>>>,
    intents: IntentStore,
}

#[derive(Clone, Serialize, Deserialize)]
struct HlQuote {
    owner: String,
    wallet: String,
    coin: String,
    asset: u32,
    sz_decimals: u32,
    // Open: "long"/"short"; close: the side being closed.
    side: String,
    leverage: u32,
    size: String,
    mark: f64,
    close: bool,
    // USDC units (6 decimals) to move in first, and from where.
    funding_units: u128,
    #[serde(default)]
    funding_from_solana: bool,
    // A close: what it frees (margin ± PnL − fee), in dollars.
    #[serde(default)]
    receive_usd: f64,
    // The dex holding the market's margin ("" for Hyperliquid's own), USDC units the agent moves
    // into it before an open, and whether its margin is per position only.
    #[serde(default)]
    dex: String,
    #[serde(default)]
    dex_units: u128,
    #[serde(default)]
    isolated: bool,
    // An open's take-profit and stop-loss, as a gain or loss on its margin (+50 → +50%).
    #[serde(default)]
    take_profit_pct: Option<f64>,
    #[serde(default)]
    stop_loss_pct: Option<f64>,
    expires: u64,
}

// What moves margin in: Relay's request, and how it's started (by the user's session signing a Base
// authorization, or the Solana transaction the app signed).
#[derive(Clone, Serialize, Deserialize)]
struct Funding {
    request_id: String,
    #[serde(default)]
    base_typed_data: Option<Value>,
    #[serde(default)]
    base_api: Option<String>,
    #[serde(default)]
    gas_request_id: Option<String>,
    #[serde(default)]
    main_index: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct HlIntent {
    quote: HlQuote,
    status: markets::IntentStatus,
    funding: Option<Funding>,
    expires: u64,
    #[serde(default)]
    approvals: Vec<HlApproval>,
    #[serde(default)]
    cash_request: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct HlApproval {
    index: usize,
    kind: String,
    prepare_id: String,
    typed: Value,
}

#[derive(Clone)]
struct IntentStore {
    postgres: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<String, HlIntent>>>,
}

impl IntentStore {
    async fn get(&self, id: &str) -> Result<Option<HlIntent>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT payload FROM atlas_hl_intents WHERE intent_id=$1",
                    &[&id],
                )
                .await
                .map_err(internal)?;
            return row
                .map(|r| serde_json::from_str(r.get::<_, &str>("payload")).map_err(internal))
                .transpose();
        }
        Ok(self.memory.lock().map_err(internal)?.get(id).cloned())
    }
    async fn put(&self, id: &str, intent: &HlIntent) -> Result<(), ApiError> {
        let payload = serde_json::to_string(intent).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_hl_intents (intent_id, owner, stage, payload) VALUES ($1,$2,$3,$4)
                 ON CONFLICT (intent_id) DO UPDATE SET stage=$3, payload=$4",
                &[&id, &intent.quote.owner, &intent.status.stage, &payload],
            )
            .await
            .map_err(internal)?;
            return Ok(());
        }
        self.memory
            .lock()
            .map_err(internal)?
            .insert(id.into(), intent.clone());
        Ok(())
    }
    // Moves an intent out of `validate` once: a repeated /signed can't start it twice.
    async fn claim(&self, id: &str, intent: &HlIntent, stage: &str) -> Result<bool, ApiError> {
        let payload = serde_json::to_string(intent).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            let changed = pg
                .execute(
                    "UPDATE atlas_hl_intents SET stage=$2, payload=$3 WHERE intent_id=$1 AND stage=$4",
                    &[&id, &intent.status.stage, &payload, &stage],
                )
                .await
                .map_err(internal)?;
            return Ok(changed == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        match memory.get(id) {
            Some(current) if current.status.stage == stage => {
                memory.insert(id.into(), intent.clone());
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl HlState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("hyperliquid database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_hl_intents (
                    intent_id TEXT PRIMARY KEY,
                    owner TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    payload TEXT NOT NULL
                )",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            client: relayed(engine_execution::hyperliquid::HyperliquidClient::new()?),
            markets: Arc::new(Mutex::new(None)),
            quotes: Arc::new(Mutex::new(HashMap::new())),
            intents: IntentStore {
                postgres,
                memory: Arc::new(Mutex::new(HashMap::new())),
            },
        })
    }
}

// For the perps alerts: the shared Hyperliquid client, and a market's latest mark price.
pub(super) fn client(state: &AppState) -> &engine_execution::hyperliquid::HyperliquidClient {
    &state.hl.client
}
pub(super) fn mark_of(state: &AppState, coin: &str) -> Option<f64> {
    let markets = state.hl.markets.lock().ok()?.clone()?.1;
    markets.iter().find(|m| m.coin == coin).map(|m| m.mark)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn conflict(message: &str) -> ApiError {
    (StatusCode::CONFLICT, message.into())
}
fn venue(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}
fn market_id(coin: &str) -> String {
    format!("{coin}-PERP")
}
// "xyz:TSLA" → ("xyz", "TSLA"); Hyperliquid's own "BTC" → ("", "BTC").
fn split_coin(coin: &str) -> (&str, &str) {
    coin.split_once(':').unwrap_or(("", coin))
}
fn coin_of(market_id: &str) -> Option<&str> {
    market_id.strip_suffix("-PERP").filter(|c| !c.is_empty())
}
// Dollars (venue) to money in the user's currency.
fn money(usd: f64, currency: &str, rate: u128) -> Value {
    let display = usd * rate as f64 / 1_000_000.0;
    json!({"amount":format!("{display:.2}"),"currency":currency})
}
fn usd(value: f64) -> Value {
    json!({"amount":trim(value),"currency":"USD"})
}
fn trim(value: f64) -> String {
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}
// Relay's fee for moving money out of Hyperliquid to cash: about 2.4¢ plus 0.04% (live quotes,
// 2026-10-01: $0.50 → 2.5¢, $5 → 2.6¢, $50 → 4.5¢).
fn cashout_fee(usd: f64) -> f64 {
    0.025 + usd * 0.0004
}
fn category(coin: &str) -> &'static str {
    describe(coin).1
}
// A market's display name and category.
fn describe(coin: &str) -> (String, &'static str) {
    let named = |ticker: &str| {
        COIN_NAMES
            .iter()
            .find(|(t, _)| *t == ticker)
            .map_or(ticker.into(), |(_, name)| (*name).into())
    };
    match split_coin(coin) {
        ("", ticker) if MEMES.contains(&ticker) => (named(ticker), "meme"),
        ("", ticker) => (named(ticker), "crypto"),
        (_, ticker) => XYZ_MARKETS
            .iter()
            .find(|(t, _, _)| *t == ticker)
            .map_or((ticker.into(), "stock"), |(_, name, kind)| {
                ((*name).into(), *kind)
            }),
    }
}
// Public logo CDNs: CoinCap by ticker for coins, FMP for stock tickers. Commodities, indices and
// currencies have no logo; the app falls back to initials (and does the same if a URL fails to load).
fn icon_url(symbol: &str, category: &str) -> Option<String> {
    // kPEPE / kSHIB quote 1,000 tokens; the logo is the token's.
    let token = symbol
        .strip_prefix('k')
        .filter(|rest| rest.chars().all(|c| c.is_ascii_uppercase()))
        .unwrap_or(symbol);
    match category {
        "crypto" | "meme" => Some(format!(
            "https://assets.coincap.io/assets/icons/{}@2x.png",
            token.to_ascii_lowercase()
        )),
        "stock" => Some(format!(
            "https://financialmodelingprep.com/image-stock/{symbol}.png"
        )),
        _ => None,
    }
}
// Margin in, from what's already there: the USDC units (6 decimals) still to bring, with 1¢ to spare.
fn shortfall(needed: f64, have: f64) -> u128 {
    if have >= needed {
        0
    } else {
        ((needed - have) * 1_000_000.0).ceil() as u128 + 10_000
    }
}

fn held_markets(state: &AppState, fresh_for: Duration) -> Option<Arc<Vec<Market>>> {
    let held = state.hl.markets.lock().ok()?.clone()?;
    (held.0.elapsed() < fresh_for).then_some(held.1)
}

async fn markets_now(state: &AppState) -> Result<Arc<Vec<Market>>, ApiError> {
    if let Some(markets) = held_markets(state, MARKETS_TTL) {
        return Ok(markets);
    }
    match refresh_markets(state).await {
        Ok(markets) => Ok(markets),
        Err(error) => held_markets(state, STALE_OK).ok_or(error),
    }
}

// One refresh at a time: requests that arrive while one runs share its answer.
static REFRESHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn refresh_markets(state: &AppState) -> Result<Arc<Vec<Market>>, ApiError> {
    let _turn = REFRESHING.lock().await;
    if let Some(markets) = held_markets(state, Duration::from_secs(5)) {
        return Ok(markets);
    }
    let markets = Arc::new(state.hl.client.markets().await.map_err(venue)?);
    if markets.is_empty() {
        return Err(venue("Hyperliquid listed no markets"));
    }
    *state.hl.markets.lock().map_err(internal)? = Some((Instant::now(), markets.clone()));
    Ok(markets)
}

// The market list stays warm, so opening Perps never waits. A refused refresh waits twice as long
// before the next (up to five minutes); a good one goes back to the usual pace.
pub(super) fn keep_warm(state: AppState) {
    tokio::spawn(async move {
        let mut wait = REFRESH_EVERY;
        loop {
            match refresh_markets(&state).await {
                Ok(_) => wait = REFRESH_EVERY,
                Err(error) => {
                    wait = (wait * 2).min(BACKOFF_MAX);
                    eprintln!(
                        "hyperliquid markets not refreshed (next try in {}s): {}",
                        wait.as_secs(),
                        error.1
                    );
                }
            }
            tokio::time::sleep(wait).await;
        }
    });
}

async fn market(state: &AppState, market_id: &str) -> Result<Market, ApiError> {
    let coin = coin_of(market_id).ok_or_else(|| bad("unknown market"))?;
    markets_now(state)
        .await?
        .iter()
        .find(|m| m.coin == coin)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "unknown market".into()))
}

#[derive(Deserialize)]
pub(super) struct CurrencyQuery {
    currency: Option<String>,
    // Closing: how much of the position, in percent (25, 50, 75; all of it when absent).
    percent: Option<u32>,
}

fn currency_of(q: Option<String>) -> Result<String, ApiError> {
    let currency = q.unwrap_or_else(|| "NGN".into());
    markets::checked_currency(&currency)?;
    Ok(currency)
}

async fn wallet_of(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(app_balance::VerifiedWallets, String), ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    let wallet = user
        .evm_wallet
        .clone()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| conflict("Your account is still being set up. Try again in a moment."))?;
    Ok((user, wallet))
}

// No setup step on Hyperliquid: the account is the wallet, and the agent is approved inside the
// first trade's confirmation.
pub(super) async fn onboarding(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (_, wallet) = wallet_of(&state, &headers).await?;
    Ok(Json(
        json!({"walletAddress":wallet,"accountAddress":wallet,"onboarded":true,
        "signer":null,"signerAuthorized":true}),
    ))
}

pub(super) async fn markets(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    let currency = currency_of(q.currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let mut list: Vec<Market> = markets_now(&state).await?.as_ref().clone();
    // Most traded first: those can actually fill an order.
    list.sort_by(|a, b| b.day_volume.total_cmp(&a.day_volume));
    let rows: Vec<Value> = list
        .iter()
        .map(|m| {
            let (name, kind) = describe(&m.coin);
            let symbol = split_coin(&m.coin).1;
            let change =
                (m.prev_day > 0.0).then(|| format!("{:.2}", (m.mark / m.prev_day - 1.0) * 100.0));
            json!({"marketId":market_id(&m.coin),"symbol":symbol,"name":name,"category":kind,
                "iconUrl":icon_url(symbol, kind),"markPrice":money(m.mark,&currency,rate),
                "change24hPct":change,"maxLeverage":m.max_leverage,
                // Hyperliquid funds hourly; the app shows the 8-hour rate.
                "fundingRate8hPct":format!("{:.4}", m.funding * 8.0 * 100.0),
                "volume24hUsd":format!("{:.2}", m.day_volume)})
        })
        .collect();
    Ok(Json(json!({"markets":rows})))
}

pub(super) async fn positions(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (user, wallet) = wallet_of(&state, &headers).await?;
    currency_of(q.currency)?;
    let (main, on_dex, fills, markets) = tokio::try_join!(
        async { state.hl.client.account(&wallet).await.map_err(venue) },
        async {
            state
                .hl
                .client
                .account_on(&wallet, DEXES[0].0)
                .await
                .map_err(venue)
        },
        async { state.hl.client.fills(&wallet).await.map_err(venue) },
        markets_now(&state)
    )?;
    // Take-profits and stop-losses, from each dex that has something open (none is never fatal).
    let triggers_on = |dex: &'static str, open: bool| {
        let client = state.hl.client.clone();
        let wallet = wallet.clone();
        async move {
            if !open {
                return Vec::new();
            }
            client
                .position_triggers(&wallet, dex)
                .await
                .unwrap_or_default()
        }
    };
    let (main_triggers, dex_triggers) = tokio::join!(
        triggers_on("", !main.positions.is_empty()),
        triggers_on(DEXES[0].0, !on_dex.positions.is_empty())
    );
    let triggers: Vec<PositionTrigger> = main_triggers.into_iter().chain(dex_triggers).collect();
    let rows: Vec<Value> = main
        .positions
        .iter()
        .chain(&on_dex.positions)
        .map(|p| {
            let mark = markets
                .iter()
                .find(|m| m.coin == p.coin)
                .map_or(p.entry, |m| m.mark);
            // Opened by the latest fill that started from nothing.
            let opened = fills
                .iter()
                .filter(|f| f.coin == p.coin && f.start_position == 0.0)
                .map(|f| f.time_ms)
                .max()
                .or_else(|| {
                    fills
                        .iter()
                        .filter(|f| f.coin == p.coin)
                        .map(|f| f.time_ms)
                        .min()
                })
                .unwrap_or_else(now);
            let kind = category(&p.coin);
            let symbol = split_coin(&p.coin).1;
            let mine: Vec<PositionTrigger> = triggers
                .iter()
                .filter(|t| t.coin == p.coin)
                .cloned()
                .collect();
            let tpsl = tpsl_view(&mine, p.entry, p.size > 0.0, p.leverage);
            json!({"positionId":p.coin,"takeProfit":tpsl["takeProfit"],"stopLoss":tpsl["stopLoss"],"openedAtUnixMs":opened,"marketId":market_id(&p.coin),
                "symbol":symbol,"iconUrl":icon_url(symbol, kind),
                "side":if p.size > 0.0 {"long"} else {"short"},"leverage":p.leverage,
                "size":trim(p.size.abs()),"entryPrice":usd(p.entry),"markPrice":usd(mark),
                "liquidationPrice":p.liquidation.map(usd),"margin":usd(p.margin),
                "unrealizedPnl":usd(p.unrealized_pnl),
                "unrealizedPnlPct":format!("{:.2}", p.return_on_equity * 100.0)})
        })
        .collect();
    if !rows.is_empty() {
        // Something open: watched for liquidation and take-profit / stop-loss emails.
        let _ = state.emails.watch_perps(&user.user_id).await;
    }
    Ok(Json(json!({"positions":rows})))
}

// The user's Hyperliquid account value in USDC units (6 decimals), for the Atlas balance: their own
// perps and the `xyz` dex together.
// What the account is worth in USDC units: zero with nothing there, an error when it can't be read.
pub(super) async fn account_value(state: &AppState, wallet: &str) -> Result<u128, ApiError> {
    let (main, on_dex) = tokio::join!(
        state.hl.client.account(wallet),
        state.hl.client.account_on(wallet, DEXES[0].0)
    );
    let value = main.map_err(internal)?.value + on_dex.map_or(0.0, |a| a.value);
    Ok(if value.is_finite() && value > 0.0 {
        (value * 1_000_000.0).floor() as u128
    } else {
        0
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OpenRequest {
    market_id: String,
    side: String,
    margin: Amount,
    leverage: u32,
    #[serde(default)]
    take_profit_pct: Option<f64>,
    #[serde(default)]
    stop_loss_pct: Option<f64>,
}
#[derive(Deserialize)]
struct Amount {
    amount: String,
    currency: String,
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpenRequest>,
) -> Result<Json<Value>, ApiError> {
    let (user, wallet) = wallet_of(&state, &headers).await?;
    if !matches!(req.side.as_str(), "long" | "short") {
        return Err(bad("side must be long or short"));
    }
    let currency = currency_of(Some(req.margin.currency.clone()))?;
    let rate = app_balance::fx_rate(&currency).await?;
    let m = market(&state, &req.market_id).await?;
    if req.leverage < 1 || req.leverage > m.max_leverage {
        return Err(bad(&format!("leverage must be 1 to {}", m.max_leverage)));
    }
    let long = req.side == "long";
    check_tpsl(req.take_profit_pct, req.stop_loss_pct, long, req.leverage)?;
    let margin_units = markets::parse_micros(&req.margin.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(margin_units, &currency, rate)?;
    let margin = margin_units as f64 / 1_000_000.0;
    let notional = margin * f64::from(req.leverage);
    if notional < MIN_NOTIONAL {
        return Err(conflict(&format!(
            "The smallest trade here is {} with leverage (Hyperliquid's minimum). Add margin or raise the leverage.",
            markets::say_money(10_000_000, &currency, rate)
        )));
    }
    let size = order_size(notional / m.mark, m.sz_decimals);
    if size.parse::<f64>().unwrap_or(0.0) <= 0.0 {
        return Err(conflict("That's too small for this market. Add margin."));
    }
    let fee = notional * TAKER_FEE * m.fee_scale;
    // What's already in the account covers it, or the rest moves in first (with a cushion for the
    // fee). A dex's market takes its margin from that dex: what it lacks comes from the main balance.
    let account = state.hl.client.account(&wallet).await.unwrap_or_default();
    let needed = margin + fee * 2.0;
    let (dex_units, short_units) = if m.dex.is_empty() {
        (0, shortfall(needed, account.withdrawable))
    } else {
        let on_dex = state
            .hl
            .client
            .account_on(&wallet, &m.dex)
            .await
            .unwrap_or_default();
        let dex_units = shortfall(needed, on_dex.withdrawable);
        let from_main = dex_units as f64 / 1_000_000.0;
        let short_units = if dex_units == 0 {
            0
        } else {
            shortfall(from_main, account.withdrawable)
        };
        (dex_units, short_units)
    };
    let (funding_units, funding_from_solana) = if short_units == 0 {
        (0, false)
    } else {
        let base_cash = state
            .markets
            .base
            .balance_of(engine_execution::swaps::uniswap::BASE_USDC, &wallet)
            .await
            .unwrap_or(0);
        let solana = user.solana_wallet.as_deref().filter(|w| !w.is_empty());
        let solana_cash = match solana {
            Some(owner) => markets::solana_cash(&state, owner).await,
            None => 0,
        };
        // Relay's fee is a few cents: leave room for it.
        let with_fee = short_units + short_units / 50 + 100_000;
        if base_cash >= with_fee {
            (short_units, false)
        } else if solana_cash >= with_fee {
            (short_units, true)
        } else {
            return Err(markets::not_enough_cash(
                base_cash + solana_cash,
                &currency,
                rate,
            ));
        }
    };
    let quote_id = format!("hlq-{:x}-{:x}", now(), rand_suffix());
    let expires = now() + QUOTE_MS;
    state.hl.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        HlQuote {
            owner: user.user_id,
            wallet,
            coin: m.coin.clone(),
            asset: m.asset,
            sz_decimals: m.sz_decimals,
            side: req.side.clone(),
            leverage: req.leverage,
            size: size.clone(),
            mark: m.mark,
            close: false,
            funding_units,
            funding_from_solana,
            receive_usd: 0.0,
            dex: m.dex.clone(),
            dex_units,
            isolated: m.isolated_only,
            take_profit_pct: req.take_profit_pct,
            stop_loss_pct: req.stop_loss_pct,
            expires,
        },
    );
    let funding = if funding_units > 0 {
        json!({"amount":money(funding_units as f64 / 1_000_000.0,&currency,rate)})
    } else {
        Value::Null
    };
    Ok(Json(
        json!({"quoteId":quote_id,"marketId":req.market_id,"side":req.side,
        "leverage":req.leverage,"funding":funding,"margin":money(margin,&currency,rate),
        "size":size,"notional":money(notional,&currency,rate),"entryPrice":money(m.mark,&currency,rate),
        "liquidationPrice":null,"fee":money(fee,&currency,rate),
        "takeProfitPrice":req.take_profit_pct.map(|p| money(tpsl_price(m.mark, long, req.leverage, p),&currency,rate)),
        "stopLossPrice":req.stop_loss_pct.map(|p| money(tpsl_price(m.mark, long, req.leverage, -p),&currency,rate)),
        "expiresAtUnixMs":expires}),
    ))
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(super) async fn close_quote(
    State(state): State<AppState>,
    Path(position_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<CurrencyQuery>,
) -> Result<Json<Value>, ApiError> {
    let (user, wallet) = wallet_of(&state, &headers).await?;
    let currency = currency_of(q.currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let dex = split_coin(&position_id).0;
    if !dex.is_empty() && !DEXES.iter().any(|(d, _)| *d == dex) {
        return Err((StatusCode::NOT_FOUND, "no such position".into()));
    }
    let account = state
        .hl
        .client
        .account_on(&wallet, dex)
        .await
        .map_err(venue)?;
    let position = account
        .positions
        .iter()
        .find(|p| p.coin == position_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "no such position".into()))?;
    let m = market(&state, &market_id(&position.coin)).await?;
    // Part of the position: that share of its size (rounded down to what Hyperliquid trades), its
    // profit or loss and its margin.
    let share = match q.percent {
        None | Some(100) => 1.0,
        Some(p @ 1..=99) => f64::from(p) / 100.0,
        Some(_) => return Err(bad("close between 1% and 100%")),
    };
    let size = if share < 1.0 {
        let step = 10f64.powi(m.sz_decimals as i32);
        (position.size.abs() * share * step).floor() / step
    } else {
        position.size.abs()
    };
    if size <= 0.0 {
        return Err(bad("That's too small a part of this position to close"));
    }
    let share = size / position.size.abs();
    let trade_fee = size * m.mark * TAKER_FEE * m.fee_scale;
    let pnl = position.unrealized_pnl * share - trade_fee;
    let freed = (position.margin * share + pnl).max(0.0);
    // What it frees comes back to cash (less Relay's cents); under $1 it stays in perps.
    let cashout = if freed * 1_000_000.0 >= CASHOUT_MIN_UNITS as f64 {
        cashout_fee(freed)
    } else {
        0.0
    };
    let quote_id = format!("hlc-{:x}-{:x}", now(), rand_suffix());
    let expires = now() + QUOTE_MS;
    state.hl.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        HlQuote {
            owner: user.user_id,
            wallet,
            coin: m.coin.clone(),
            asset: m.asset,
            sz_decimals: m.sz_decimals,
            side: if position.size > 0.0 { "long" } else { "short" }.into(),
            leverage: position.leverage,
            size: trim(size),
            mark: m.mark,
            close: true,
            funding_units: 0,
            funding_from_solana: false,
            receive_usd: freed,
            dex: m.dex.clone(),
            dex_units: 0,
            isolated: m.isolated_only,
            take_profit_pct: None,
            stop_loss_pct: None,
            expires,
        },
    );
    let signed = |value: f64| {
        let display = value * rate as f64 / 1_000_000.0;
        json!({"amount":format!("{display:.2}"),"currency":currency})
    };
    Ok(Json(json!({"quoteId":quote_id,"positionId":position_id,
        "receive":money((freed - cashout).max(0.0),&currency,rate),
        "realizedPnl":signed(pnl),"exitPrice":money(m.mark,&currency,rate),
        "fee":money(trade_fee + cashout,&currency,rate),"expiresAtUnixMs":expires})))
}

// Execute (open or close): the plan. Margin from Solana is a transaction the app signs (after a gas
// top-up if needed); from Base the phone signs the gasless authorization. A newly funded account
// approves its agent in the next phone step, covered by the same confirmation.
async fn execute_inner(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (user, wallet) = wallet_of(&state, &headers).await?;
    let quote = state
        .hl
        .quotes
        .lock()
        .map_err(internal)?
        .remove(&quote_id)
        .ok_or((
            StatusCode::NOT_FOUND,
            "quote not found or already used".into(),
        ))?;
    if quote.owner != user.user_id || quote.wallet != wallet {
        return Err((
            StatusCode::FORBIDDEN,
            "quote belongs to another user".into(),
        ));
    }
    if quote.expires < now() {
        return Err(conflict("quote expired; request a fresh quote"));
    }
    let intent_id = format!("hl-{:x}-{:x}", now(), rand_suffix());
    let mut transactions = Vec::new();
    let mut approvals = Vec::new();
    let agent = bridge(&state, &headers, "agent", json!({}))
        .await
        .map_err(venue)?;
    let address = agent["agentAddress"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if quote.funding_units == 0
        && !state
            .hl
            .client
            .agents(&wallet)
            .await
            .map_err(venue)?
            .contains(&address)
    {
        let prepared = bridge(&state, &headers, "approve_prepare", json!({}))
            .await
            .map_err(venue)?;
        let result = &prepared["result"];
        let typed = result["typedData"][0].clone();
        approvals.push(HlApproval {
            index: 0,
            kind: "agent".into(),
            prepare_id: result["prepareId"]
                .as_str()
                .ok_or_else(|| venue("Agent approval unavailable"))?
                .into(),
            typed: typed.clone(),
        });
        transactions.push(gasless::step(&typed, "hyperliquid"));
    }
    let funding = if quote.funding_units == 0 {
        None
    } else if quote.funding_from_solana {
        let owner = user
            .solana_wallet
            .clone()
            .filter(|w| !w.is_empty())
            .ok_or_else(|| {
                conflict("Your account is still being set up. Try again in a moment.")
            })?;
        let deposit = state
            .relay_link
            .solana_to_hyperliquid(&owner, &wallet, quote.funding_units)
            .await
            .map_err(|_| conflict("Couldn't move your margin right now. Try again shortly."))?;
        let tx = state
            .solana_mainnet
            .v0_transaction(&owner, &deposit.instructions, &deposit.lookup_tables)
            .await
            .map_err(venue)?;
        let gas = markets::gas_topup(&state, &owner, deposit.amount_in_units).await;
        if let Some((_, gas_tx)) = &gas {
            transactions.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
        }
        let main_index = transactions.len();
        transactions.push(json!({"chain":"solana","transaction":tx,"submit":"engine"}));
        Some(Funding {
            request_id: deposit.request_id,
            base_typed_data: None,
            base_api: None,
            gas_request_id: gas.map(|(id, _)| id),
            main_index,
        })
    } else {
        let deposit = state
            .relay_link
            .base_to_hyperliquid(&wallet, quote.funding_units)
            .await
            .map_err(|_| conflict("Couldn't move your margin right now. Try again shortly."))?;
        let index = transactions.len();
        transactions.push(gasless::step(&deposit.typed_data, "base"));
        approvals.push(HlApproval {
            index,
            kind: "funding".into(),
            prepare_id: String::new(),
            typed: deposit.typed_data.clone(),
        });
        Some(Funding {
            request_id: deposit.request_id,
            base_typed_data: Some(deposit.typed_data),
            base_api: Some(deposit.api),
            gas_request_id: None,
            main_index: 0,
        })
    };
    let currency_rate = |q: &HlQuote| (q.size.clone(), q.leverage);
    let (size, leverage) = currency_rate(&quote);
    let symbol = split_coin(&quote.coin).1;
    let mut summary = vec![
        json!({"label":"Market","value":symbol}),
        json!({"label":"Action","value":if quote.close {"Close position".to_string()} else {
            format!("{} {}x", if quote.side == "long" {"Long"} else {"Short"}, leverage)}}),
        json!({"label":"Size","value":format!("{size} {symbol}")}),
    ];
    let long = quote.side == "long";
    if let Some(pct) = quote.take_profit_pct {
        let price = tpsl_price(quote.mark, long, quote.leverage, pct);
        summary.push(json!({"label":"Take profit","value":format!("+{} at {}", say_pct(pct), say_usd(price))}));
    }
    if let Some(pct) = quote.stop_loss_pct {
        let price = tpsl_price(quote.mark, long, quote.leverage, -pct);
        summary.push(
            json!({"label":"Stop loss","value":format!("−{} at {}", say_pct(pct), say_usd(price))}),
        );
    }
    if quote.funding_units > 0 {
        summary.push(json!({"label":"Margin moved to Hyperliquid",
            "value":format!("${:.2}", quote.funding_units as f64 / 1_000_000.0)}));
    }
    let intent = HlIntent {
        status: markets::IntentStatus {
            intent_id: intent_id.clone(),
            stage: "validate".into(),
            state: "pending".into(),
            tx_ids: Vec::new(),
            error: None,
        },
        funding,
        expires: now() + 120_000,
        approvals,
        cash_request: None,
        quote,
    };
    state.hl.intents.put(&intent_id, &intent).await?;
    Ok(Json(
        json!({"intentId":intent_id,"kind":if intent.quote.close {"perp_close"} else {"perp_open"},
        "summary":summary,"transactions":transactions,"expiresAtUnixMs":intent.expires}),
    ))
}

pub(super) async fn signed(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let (user, _) = wallet_of(&state, &headers).await?;
    let current = state
        .hl
        .intents
        .get(&intent_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if current.status.stage == "sign" {
        return signed_cashout(state, intent_id, headers, current, body)
            .await
            .map(Json);
    }
    if current.status.stage != "validate" {
        return Ok(Json(current.status));
    }
    if current.expires <= now() {
        return Err(conflict("Confirmation expired; request a fresh quote"));
    }
    let from_solana = current.funding.is_some() && current.quote.funding_from_solana;
    let main = current.funding.as_ref().map_or(0, |f| f.main_index);
    let sol_count = if from_solana {
        1 + usize::from(
            current
                .funding
                .as_ref()
                .is_some_and(|f| f.gas_request_id.is_some()),
        )
    } else {
        0
    };
    if !body.sent.is_empty()
        || body.signed.iter().enumerate().any(|(n, s)| s.index != n)
        || body.signed.len() != sol_count + current.approvals.len()
        || current
            .approvals
            .iter()
            .any(|a| !body.signed.iter().any(|s| s.index == a.index))
        || (from_solana && !body.signed.iter().any(|s| s.index == main))
    {
        return Err(bad("Approvals do not match the plan"));
    }
    let mut claimed = current.clone();
    claimed.status.stage = if current.funding.is_some() {
        "fund"
    } else {
        "execute"
    }
    .into();
    if !state
        .hl
        .intents
        .claim(&intent_id, &claimed, "validate")
        .await?
    {
        let latest = state.hl.intents.get(&intent_id).await?;
        return Ok(Json(latest.map_or(current.status, |i| i.status)));
    }
    let answer = claimed.status.clone();
    let signed = body.signed;
    tokio::spawn(async move {
        let mut intent = claimed;
        let result = run(&state, &headers, &mut intent, &signed, main).await;
        match result {
            Ok(()) if intent.status.stage == "sign" => {}
            Ok(()) => {
                intent.status.stage = "settle".into();
                intent.status.state = "filled".into();
            }
            Err(message) => {
                intent.status.state = "failed".into();
                intent.status.error = Some(message);
            }
        }
        if let Err(error) = state.hl.intents.put(&intent_id, &intent).await {
            eprintln!("hyperliquid intent {intent_id} not saved: {}", error.1);
        }
        emails::after_status(&state, &headers, &intent.status);
    });
    Ok(Json(answer))
}

// After the confirm: margin in, agent approved, leverage set, order placed. The error is what the
// user reads.
async fn run(
    state: &AppState,
    headers: &HeaderMap,
    intent: &mut HlIntent,
    signed: &[markets::Signed],
    main: usize,
) -> Result<(), String> {
    let quote = intent.quote.clone();
    if let Some(approval) = intent.approvals.iter().find(|a| a.kind == "agent") {
        let signature = signed
            .iter()
            .find(|s| s.index == approval.index)
            .ok_or("Agent approval missing")?;
        expect_ok(
            bridge(
                state,
                headers,
                "approve",
                json!({"prepareId":approval.prepare_id,"signatures":[signature.transaction]}),
            )
            .await?,
            "approve",
        )?;
    }
    if let Some(funding) = intent.funding.clone() {
        let not_moved = |reason: String| {
            eprintln!("hyperliquid margin not moved: {reason}");
            "Moving your margin didn't go through, so nothing left your balance and nothing was ordered".to_string()
        };
        if let (Some(typed), Some(api)) = (&funding.base_typed_data, &funding.base_api) {
            let signature = gasless::verify(
                state,
                headers,
                &quote.wallet,
                typed,
                &signed
                    .iter()
                    .find(|s| {
                        s.index
                            == intent
                                .approvals
                                .iter()
                                .find(|a| a.kind == "funding")
                                .map_or(usize::MAX, |a| a.index)
                    })
                    .ok_or("Funding approval missing")?
                    .transaction,
            )
            .await
            .map_err(not_moved)?;
            state
                .relay_link
                .submit(&funding.request_id, api, &signature)
                .await
                .map_err(|e| not_moved(e.to_string()))?;
        } else {
            if let Some(gas_id) = &funding.gas_request_id {
                let gas = signed
                    .iter()
                    .find(|s| s.index + 1 == main)
                    .ok_or("Gas transaction missing")?;
                markets::land_gas_topup(state, gas_id, &gas.transaction).await;
            }
            let signature = state
                .solana_mainnet
                .send_signed(
                    &signed
                        .iter()
                        .find(|s| s.index == main)
                        .ok_or("Funding transaction missing")?
                        .transaction,
                )
                .await
                .map_err(|e| not_moved(e.to_string()))?;
            intent.status.tx_ids.push(signature);
        }
        let deadline = tokio::time::Instant::now() + FUNDING_LIMIT;
        loop {
            match state.relay_link.state(&funding.request_id).await {
                Ok(engine_execution::layerswap::SwapState::Completed) => break,
                Ok(engine_execution::layerswap::SwapState::Failed(_)) => {
                    return Err("Moving your margin didn't go through; it's on its way back to your balance. Nothing was ordered".into());
                }
                _ if tokio::time::Instant::now() > deadline => {
                    return Err("Your margin is taking longer than usual to arrive. It will show in your balance; nothing was ordered".into());
                }
                _ => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        intent.funding = None;
        intent.approvals.clear();
        intent.status.stage = "execute".into();
    }
    let agent = bridge(state, headers, "agent", json!({})).await?;
    let agent = agent["agentAddress"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let approved = state
        .hl
        .client
        .agents(&quote.wallet)
        .await
        .map_err(|e| e.to_string())?
        .contains(&agent);
    if !approved {
        prepare_agent_step(state, headers, intent).await?;
        state
            .hl
            .intents
            .put(&intent.status.intent_id, intent)
            .await
            .map_err(|e| e.1)?;
        return Ok(());
    }
    if !quote.close {
        if quote.dex_units > 0 {
            expect_ok(
                bridge(
                    state,
                    headers,
                    "move",
                    json!({"from":"","to":quote.dex,"amount":quote.dex_units.to_string()}),
                )
                .await?,
                "margin move",
            )?;
        }
        expect_ok(
            bridge(
                state,
                headers,
                "leverage",
                json!({"asset":quote.asset,"leverage":quote.leverage,"isolated":quote.isolated}),
            )
            .await?,
            "leverage",
        )?;
    }
    // A market order: immediate-or-cancel, within 3% of the price seen.
    let buy = (quote.side == "long") != quote.close;
    let limit = if buy {
        quote.mark * (1.0 + SLIPPAGE)
    } else {
        quote.mark * (1.0 - SLIPPAGE)
    };
    let placed = bridge(
        state,
        headers,
        "order",
        json!({"asset":quote.asset,"isBuy":buy,"price":order_price(limit, quote.sz_decimals),
            "size":quote.size,"reduceOnly":quote.close}),
    )
    .await?;
    filled(&placed)?;
    let (dex, _) = split_coin(&quote.coin);
    if quote.close {
        // Nothing left open: its take-profit and stop-loss go too.
        let gone = state
            .hl
            .client
            .account_on(&quote.wallet, dex)
            .await
            .is_ok_and(|a| a.positions.iter().all(|p| p.coin != quote.coin));
        if gone {
            if let Err(reason) =
                set_tpsl(state, headers, &quote.wallet, &quote.coin, None, None).await
            {
                eprintln!("hyperliquid {} tp/sl not cancelled: {reason}", quote.coin);
            }
        }
        cash_out(state, headers, intent).await;
    } else {
        let _ = state.emails.watch_perps(&quote.owner).await;
    }
    if !quote.close && (quote.take_profit_pct.is_some() || quote.stop_loss_pct.is_some()) {
        // The trade stands either way; one that couldn't be set shows as unset on the position.
        if let Err(reason) = set_tpsl(
            state,
            headers,
            &quote.wallet,
            &quote.coin,
            quote.take_profit_pct,
            quote.stop_loss_pct,
        )
        .await
        {
            eprintln!("hyperliquid {} tp/sl not set: {reason}", quote.coin);
        }
    }
    Ok(())
}

// Take-profit and stop-loss as a gain or loss on margin: +50% at 5x is a 10% move in the trade's
// favour. Take-profit from 1% up, stop-loss 1% to 90% (past that, liquidation comes first).
fn check_tpsl(tp: Option<f64>, sl: Option<f64>, long: bool, leverage: u32) -> Result<(), ApiError> {
    if let Some(tp) = tp {
        // A short's price can't fall below zero.
        let most = if long {
            1000.0
        } else {
            (99.0 * f64::from(leverage)).min(1000.0)
        };
        if !tp.is_finite() || tp < 1.0 || tp > most {
            return Err(bad(&format!("take profit must be +1% to +{most:.0}%")));
        }
    }
    if let Some(sl) = sl {
        if !sl.is_finite() || !(1.0..=90.0).contains(&sl) {
            return Err(bad("stop loss must be −1% to −90%"));
        }
    }
    Ok(())
}

// The price at which the position has gained (or, negative, lost) `pct` percent of its margin.
fn tpsl_price(entry: f64, long: bool, leverage: u32, pct: f64) -> f64 {
    let moved = pct / 100.0 / f64::from(leverage.max(1));
    entry * if long { 1.0 + moved } else { 1.0 - moved }
}

// The other way round: what reaching `price` gains (or loses) on margin, in percent.
fn tpsl_pct(entry: f64, long: bool, leverage: u32, price: f64) -> f64 {
    if entry <= 0.0 {
        return 0.0;
    }
    let moved = price / entry - 1.0;
    (if long { moved } else { -moved }) * f64::from(leverage) * 100.0
}

fn say_pct(pct: f64) -> String {
    format!("{}%", trim((pct * 10.0).round() / 10.0))
}

// "$120,000" or "$0.5321": a price as people read it.
fn say_usd(price: f64) -> String {
    let text = order_price(price, 0);
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if fraction.is_empty() {
        format!("${grouped}")
    } else {
        format!("${grouped}.{fraction}")
    }
}

// Replaces a position's take-profit and stop-loss (None removes one). They're priced off the
// position's entry and leverage as Hyperliquid has them now, and must still be ahead of the price:
// one already passed would close the position on the spot. If the new ones can't be placed, the old
// ones go back.
async fn set_tpsl(
    state: &AppState,
    headers: &HeaderMap,
    wallet: &str,
    coin: &str,
    tp: Option<f64>,
    sl: Option<f64>,
) -> Result<Vec<PositionTrigger>, String> {
    let (dex, _) = split_coin(coin);
    let m = market(state, &market_id(coin)).await.map_err(|e| e.1)?;
    let (account, existing) = tokio::try_join!(
        async {
            state
                .hl
                .client
                .account_on(wallet, dex)
                .await
                .map_err(|e| e.to_string())
        },
        async {
            state
                .hl
                .client
                .position_triggers(wallet, dex)
                .await
                .map_err(|e| e.to_string())
        }
    )?;
    let existing: Vec<PositionTrigger> = existing.into_iter().filter(|t| t.coin == coin).collect();
    let position = account.positions.iter().find(|p| p.coin == coin);
    let mut orders = Vec::new();
    if let Some(p) = position.filter(|_| tp.is_some() || sl.is_some()) {
        let long = p.size > 0.0;
        let mark = m.mark;
        for (kind, pct) in [("tp", tp), ("sl", sl.map(|v| -v))] {
            let Some(pct) = pct else { continue };
            let trigger = tpsl_price(p.entry, long, p.leverage, pct);
            // A long's take-profit sits above the price and its stop below; a short's the other way.
            let ahead = (kind == "tp") == long;
            if (ahead && trigger <= mark) || (!ahead && trigger >= mark) {
                return Err(if kind == "tp" {
                    "Your position is already past that take profit. Pick a higher one.".into()
                } else {
                    "Your position is already past that stop loss. Pick a lower one.".into()
                });
            }
            orders.push((kind, trigger));
        }
    } else if tp.is_some() || sl.is_some() {
        return Err("That position isn't open anymore.".into());
    }
    let long = position.is_some_and(|p| p.size > 0.0);
    let place = |orders: &[(&str, f64)]| {
        let rows: Vec<Value> = orders
            .iter()
            .map(|(kind, trigger)| {
                // A long closes by selling, so its worst fill is below the trigger.
                let worst = if long {
                    trigger * (1.0 - TPSL_SLIPPAGE)
                } else {
                    trigger * (1.0 + TPSL_SLIPPAGE)
                };
                json!({"tpsl":kind,"trigger":order_price(*trigger, m.sz_decimals),
                    "price":order_price(worst, m.sz_decimals)})
            })
            .collect();
        json!({"asset":m.asset,"isBuy":!long,"orders":rows})
    };
    if !existing.is_empty() {
        let oids: Vec<u64> = existing.iter().map(|t| t.oid).collect();
        expect_ok(
            bridge(
                state,
                headers,
                "cancel",
                json!({"asset":m.asset,"oids":oids}),
            )
            .await?,
            "cancel",
        )?;
    }
    if orders.is_empty() {
        return Ok(Vec::new());
    }
    let placed = bridge(state, headers, "tpsl", place(&orders)).await;
    let placed = placed.and_then(|answer| {
        expect_ok(answer.clone(), "take profit / stop loss")?;
        let statuses = answer["result"]["response"]["data"]["statuses"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        match statuses.iter().find_map(|s| s["error"].as_str()) {
            Some(reason) => Err(format!("Hyperliquid didn't take it ({reason})")),
            None => Ok(()),
        }
    });
    if let Err(reason) = placed {
        if position.is_some() && !existing.is_empty() {
            let old: Vec<(&str, f64)> =
                existing.iter().map(|t| (t.kind, t.trigger_price)).collect();
            let _ = bridge(state, headers, "tpsl", place(&old)).await;
        }
        return Err(reason);
    }
    Ok(orders
        .into_iter()
        .map(|(kind, trigger_price)| PositionTrigger {
            coin: coin.into(),
            oid: 0,
            kind,
            trigger_price,
        })
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TpslRequest {
    take_profit_pct: Option<f64>,
    stop_loss_pct: Option<f64>,
}

// Sets (or clears, with nulls) an open position's take-profit and stop-loss. The user's Atlas agent
// signs it, as it did the trade.
pub(super) async fn update_tpsl(
    State(state): State<AppState>,
    Path(position_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<TpslRequest>,
) -> Result<Json<Value>, ApiError> {
    pin::require_action(
        &state,
        &headers,
        pin::Action::Tpsl {
            position_id: position_id.clone(),
            take_profit_pct: req.take_profit_pct,
            stop_loss_pct: req.stop_loss_pct,
        },
        true,
    )
    .await?;
    let (_, wallet) = wallet_of(&state, &headers).await?;
    let dex = split_coin(&position_id).0;
    if !dex.is_empty() && !DEXES.iter().any(|(d, _)| *d == dex) {
        return Err((StatusCode::NOT_FOUND, "no such position".into()));
    }
    let position = state
        .hl
        .client
        .account_on(&wallet, dex)
        .await
        .map_err(venue)?
        .positions
        .into_iter()
        .find(|p| p.coin == position_id)
        .ok_or((StatusCode::NOT_FOUND, "no such position".into()))?;
    let long = position.size > 0.0;
    check_tpsl(
        req.take_profit_pct,
        req.stop_loss_pct,
        long,
        position.leverage,
    )?;
    let agent = bridge(&state, &headers, "agent", json!({}))
        .await
        .map_err(venue)?;
    let agent = agent["agentAddress"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !state
        .hl
        .client
        .agents(&wallet)
        .await
        .map_err(venue)?
        .contains(&agent)
    {
        return Err(conflict(
            "Make a trade first: your account needs its one-time approval.",
        ));
    }
    let set = set_tpsl(
        &state,
        &headers,
        &wallet,
        &position_id,
        req.take_profit_pct,
        req.stop_loss_pct,
    )
    .await
    .map_err(|reason| conflict(&reason))?;
    Ok(Json(tpsl_view(
        &set,
        position.entry,
        long,
        position.leverage,
    )))
}

// A position's take-profit and stop-loss for the app: price, and the gain or loss on margin there.
fn tpsl_view(triggers: &[PositionTrigger], entry: f64, long: bool, leverage: u32) -> Value {
    let view = |kind: &str| {
        triggers.iter().find(|t| t.kind == kind).map(|t| {
            json!({"price":usd(t.trigger_price),
                "pct":format!("{:.0}", tpsl_pct(entry, long, leverage, t.trigger_price))})
        })
    };
    json!({"takeProfit":view("tp"),"stopLoss":view("sl")})
}

// After a close: the money it freed goes back to cash. A cash-out that doesn't happen leaves the
// money in perps, where it still counts in the balance and pays for the next trade: the close stands.
async fn cash_out(state: &AppState, headers: &HeaderMap, intent: &mut HlIntent) {
    intent.status.stage = "settle".into();
    let id = intent.status.intent_id.clone();
    if let Err(error) = state.hl.intents.put(&id, intent).await {
        eprintln!("hyperliquid intent {id} not saved: {}", error.1);
    }
    if let Err(reason) = move_to_cash(state, headers, intent).await {
        intent.status.stage = "sign".into();
        intent.status.error=Some(format!("The position closed. Cash is still in your perps account; cash return needs attention ({reason})."));
    }
}

// Relay pays it out to the user's own Solana wallet (Base without one); the bridge pins that.
// With nothing left open, everything in the account comes back, else only what this close freed.
async fn move_to_cash(
    state: &AppState,
    headers: &HeaderMap,
    intent: &mut HlIntent,
) -> Result<(), String> {
    let quote = intent.quote.clone();
    // A dex's margin comes back to the main balance first (the agent, to the same account only).
    if !quote.dex.is_empty() {
        let on_dex = state
            .hl
            .client
            .account_on(&quote.wallet, &quote.dex)
            .await
            .map_err(|e| e.to_string())?;
        let units = cashout_units(&on_dex, quote.receive_usd);
        if units >= 10_000 {
            expect_ok(
                bridge(
                    state,
                    headers,
                    "move",
                    json!({"from":quote.dex,"to":"","amount":units.to_string()}),
                )
                .await?,
                "margin move",
            )?;
        }
    }
    let account = state
        .hl
        .client
        .account(&quote.wallet)
        .await
        .map_err(|e| e.to_string())?;
    let units = cashout_units(&account, quote.receive_usd);
    if units < CASHOUT_MIN_UNITS {
        return Ok(());
    }
    let user = app_balance::verified_wallets(state, headers)
        .await
        .map_err(|e| e.1)?;
    let to = if user.solana_wallet.is_some_and(|w| !w.is_empty()) {
        "solana"
    } else {
        "base"
    };
    let answer = bridge(
        state,
        headers,
        "cashout_prepare",
        json!({"amount":units.to_string(),"to":to}),
    )
    .await?;
    let prepared = &answer["result"];
    let prepare_id = prepared["prepareId"]
        .as_str()
        .ok_or("Cash return approval unavailable")?;
    intent.approvals = prepared["typedData"]
        .as_array()
        .ok_or("Cash return requests unavailable")?
        .iter()
        .enumerate()
        .map(|(index, typed)| HlApproval {
            index,
            kind: "cashout".into(),
            prepare_id: prepare_id.into(),
            typed: typed.clone(),
        })
        .collect();
    intent.expires = prepared["expiresAtUnixMs"]
        .as_u64()
        .unwrap_or(now() + 180_000);
    intent.status.stage = "sign".into();
    intent.status.error = None;
    Ok(())
}

fn cashout_units(account: &engine_execution::hyperliquid::Account, freed_usd: f64) -> u128 {
    let usd = if account.positions.is_empty() {
        account.withdrawable
    } else {
        account.withdrawable.min(freed_usd)
    };
    (usd.max(0.0) * 1_000_000.0).floor() as u128
}

async fn bridge(
    state: &AppState,
    headers: &HeaderMap,
    route: &str,
    body: Value,
) -> Result<Value, String> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err("Hyperliquid trading needs Privy".into());
    };
    let wallet = app_balance::verified_wallets(state, headers)
        .await
        .map_err(|e| e.1)?
        .evm_wallet
        .unwrap_or_default();
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or("Privy access token required")?;
    let mut request = body;
    request["accessToken"] = json!(token);
    request["walletAddress"] = json!(wallet);
    let response = http
        .post(format!("{bridge_url}/hyperliquid/{route}"))
        .json(&request)
        .send()
        .await
        .map_err(|_| "Hyperliquid signing unavailable".to_string())?;
    let ok = response.status().is_success();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !ok {
        return Err(format!(
            "Hyperliquid didn't take it ({})",
            body["error"].as_str().unwrap_or("signing failed")
        ));
    }
    Ok(body)
}

fn expect_ok(answer: Value, what: &str) -> Result<(), String> {
    if answer["result"]["status"].as_str() == Some("ok") {
        return Ok(());
    }
    eprintln!("hyperliquid {what} refused: {}", answer["result"]);
    Err(format!(
        "Hyperliquid didn't accept the {what} ({})",
        answer["result"]["response"]
            .as_str()
            .unwrap_or("no reason given")
    ))
}

// An order's outcome: filled, or the venue's reason in words.
fn filled(answer: &Value) -> Result<(), String> {
    let result = &answer["result"];
    if result["status"].as_str() != Some("ok") {
        return Err(format!(
            "Hyperliquid didn't take the order ({})",
            result["response"].as_str().unwrap_or("no reason given")
        ));
    }
    let status = &result["response"]["data"]["statuses"][0];
    if status["filled"].is_object() {
        return Ok(());
    }
    let reason = status["error"].as_str().unwrap_or("not filled");
    Err(if reason.contains("could not immediately match") {
        "Nobody on Hyperliquid filled this near the price you saw. Nothing was ordered; try again."
            .into()
    } else {
        format!("Hyperliquid didn't fill the order ({reason})")
    })
}

pub(super) async fn status(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let (user, _) = wallet_of(&state, &headers).await?;
    let current = state
        .hl
        .intents
        .get(&intent_id)
        .await?
        .filter(|i| i.quote.owner == user.user_id)
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    Ok(Json(current.status))
}

async fn prepare_agent_step(
    state: &AppState,
    headers: &HeaderMap,
    intent: &mut HlIntent,
) -> Result<(), String> {
    let prepared = bridge(state, headers, "approve_prepare", json!({})).await?;
    let data = &prepared["result"];
    intent.approvals = vec![HlApproval {
        index: 0,
        kind: "agent".into(),
        prepare_id: data["prepareId"]
            .as_str()
            .ok_or("Agent approval unavailable")?
            .into(),
        typed: data["typedData"][0].clone(),
    }];
    intent.expires = data["expiresAtUnixMs"]
        .as_u64()
        .ok_or("Agent approval expiry unavailable")?;
    intent.status.stage = "sign".into();
    intent.status.state = "pending".into();
    Ok(())
}

pub(super) async fn next(
    state: AppState,
    id: String,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (user, _) = wallet_of(&state, &headers).await?;
    let mut intent = state
        .hl
        .intents
        .get(&id)
        .await?
        .filter(|i| i.quote.owner == user.user_id)
        .ok_or((StatusCode::NOT_FOUND, "Intent not found".into()))?;
    if intent.status.stage != "sign" || intent.status.state != "pending" {
        return Err(conflict("Nothing waiting to sign"));
    }
    if intent.expires <= now() || intent.approvals.is_empty() {
        if intent.approvals.first().is_some_and(|a| a.kind == "agent") || !intent.quote.close {
            prepare_agent_step(&state, &headers, &mut intent)
                .await
                .map_err(venue)?;
        } else {
            move_to_cash(&state, &headers, &mut intent)
                .await
                .map_err(venue)?;
        }
        state.hl.intents.put(&id, &intent).await?;
    }
    Ok(Json(
        json!({"kind":if intent.quote.close{"perp_close"}else{"perp_open"},"transactions":intent.approvals.iter().map(|a|gasless::step(&a.typed,"hyperliquid")).collect::<Vec<_>>()}),
    ))
}
// Home's Finish only returns a closed position's cash; an opening that stopped is never finished
// later, at a price nobody agreed to.
pub(super) async fn resume_close(
    state: AppState,
    id: String,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (user, _) = wallet_of(&state, &headers).await?;
    let close = state
        .hl
        .intents
        .get(&id)
        .await?
        .filter(|i| i.quote.owner == user.user_id)
        .is_some_and(|i| i.quote.close);
    if !close {
        return Err(conflict(
            "This trade stopped earlier and won't be finished now. Nothing more was spent.",
        ));
    }
    next(state, id, headers).await
}
async fn signed_cashout(
    state: AppState,
    id: String,
    headers: HeaderMap,
    mut intent: HlIntent,
    body: markets::Submission,
) -> Result<markets::IntentStatus, ApiError> {
    if intent.expires <= now() {
        return Err(conflict("Cash return approval expired; request it again"));
    }
    if !body.sent.is_empty()
        || body.signed.len() != intent.approvals.len()
        || body.signed.iter().enumerate().any(|(n, s)| s.index != n)
    {
        return Err(bad("Cash return approvals do not match"));
    }
    let prepare_id = intent
        .approvals
        .first()
        .ok_or_else(|| conflict("Cash return needs preparation"))?
        .prepare_id
        .clone();
    intent.status.stage = "execute".into();
    if !state.hl.intents.claim(&id, &intent, "sign").await? {
        return Ok(state
            .hl
            .intents
            .get(&id)
            .await?
            .ok_or_else(|| conflict("Intent changed"))?
            .status);
    }
    let answer = intent.status.clone();
    if intent.approvals.first().is_some_and(|a| a.kind == "agent") {
        tokio::spawn(async move {
            match run(&state, &headers, &mut intent, &body.signed, 0).await {
                Ok(()) if intent.status.stage == "sign" => {}
                Ok(()) => {
                    intent.status.stage = "settle".into();
                    intent.status.state = "filled".into();
                }
                Err(reason) => {
                    intent.status.state = "failed".into();
                    intent.status.error = Some(reason);
                }
            }
            let _ = state.hl.intents.put(&id, &intent).await;
            emails::after_status(&state, &headers, &intent.status);
        });
        return Ok(answer);
    }
    tokio::spawn(async move {
        let result = async {
            let signatures: Vec<_> = body.signed.iter().map(|s| s.transaction.clone()).collect();
            let sent = bridge(
                &state,
                &headers,
                "cashout",
                json!({"prepareId":prepare_id,"signatures":signatures}),
            )
            .await?;
            let request = sent["result"]["requestId"]
                .as_str()
                .ok_or("Cash return request unavailable")?
                .to_string();
            intent.cash_request = Some(request.clone());
            state.hl.intents.put(&id, &intent).await.map_err(|e| e.1)?;
            let deadline = tokio::time::Instant::now() + CASHOUT_LIMIT;
            loop {
                match state.relay_link.state(&request).await {
                    Ok(engine_execution::layerswap::SwapState::Completed) => break,
                    Ok(engine_execution::layerswap::SwapState::Failed(reason)) => {
                        return Err(reason)
                    }
                    _ if tokio::time::Instant::now() > deadline => {
                        return Err("Cash is still moving; check your balance shortly".into())
                    }
                    _ => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
            Ok::<(), String>(())
        }
        .await;
        intent.status.stage = "settle".into();
        match result {
            Ok(()) => {
                intent.status.state = "filled".into();
                intent.status.error = None;
            }
            Err(reason) => {
                intent.status.state = "failed".into();
                intent.status.error=Some(format!("The position closed, but cash return could not be verified ({reason}). Check cash and perps balances."));
            }
        }
        let _ = state.hl.intents.put(&id, &intent).await;
        emails::after_status(&state, &headers, &intent.status);
    });
    Ok(answer)
}

// Price history for a Hyperliquid market: (ms, USD close), oldest first.
pub(super) async fn closes(
    state: &AppState,
    market_id: &str,
    range: &str,
) -> Result<Vec<(u64, f64)>, ApiError> {
    let coin = coin_of(market_id).ok_or_else(|| bad("unknown market"))?;
    let (interval, span_ms): (&str, u64) = match range {
        "1D" => ("15m", 86_400_000),
        "1W" => ("1h", 7 * 86_400_000),
        "1M" => ("4h", 30 * 86_400_000),
        "1Y" => ("1d", 365 * 86_400_000),
        _ => return Err(bad("range must be 1D, 1W, 1M or 1Y")),
    };
    let end = now();
    state
        .hl
        .client
        .closes(coin, interval, end.saturating_sub(span_ms), end)
        .await
        .map_err(venue)
}

pub(super) async fn pending_rows(state: &AppState, owner: &str) -> Result<Vec<Value>, ApiError> {
    let intents: Vec<HlIntent> = if let Some(pg) = &state.hl.intents.postgres {
        pg.query(
            "SELECT payload FROM atlas_hl_intents WHERE owner=$1 AND stage='sign'",
            &[&owner],
        )
        .await
        .map_err(internal)?
        .into_iter()
        .map(|row| serde_json::from_str(row.get::<_, &str>(0)).map_err(internal))
        .collect::<Result<_, _>>()?
    } else {
        state
            .hl
            .intents
            .memory
            .lock()
            .map_err(internal)?
            .values()
            .filter(|i| i.quote.owner == owner)
            .cloned()
            .collect()
    };
    Ok(intents.into_iter().filter(|i|i.quote.close&&i.status.state=="pending"&&i.status.stage=="sign").map(|i|
        json!({"intentId":i.status.intent_id,"symbol":split_coin(&i.quote.coin).1,"kind":if i.quote.close{"perp_close"}else{"perp_open"},"stage":"sign","error":i.status.error})).collect())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_ids_round_trip() {
        assert_eq!(market_id("NEAR"), "NEAR-PERP");
        assert_eq!(coin_of("NEAR-PERP"), Some("NEAR"));
        assert_eq!(coin_of("-PERP"), None);
        assert_eq!(coin_of("NEAR"), None);
        assert_eq!(category("kPEPE"), "meme");
        assert_eq!(category("BTC"), "crypto");
        assert_eq!(market_id("xyz:TSLA"), "xyz:TSLA-PERP");
        assert_eq!(coin_of("xyz:TSLA-PERP"), Some("xyz:TSLA"));
        assert_eq!(split_coin("xyz:TSLA"), ("xyz", "TSLA"));
        assert_eq!(split_coin("BTC"), ("", "BTC"));
        assert_eq!(describe("BTC"), ("Bitcoin".into(), "crypto"));
        assert_eq!(describe("kPEPE"), ("Pepe (per 1,000)".into(), "meme"));
        assert_eq!(
            icon_url("kPEPE", "meme").as_deref(),
            Some("https://assets.coincap.io/assets/icons/pepe@2x.png")
        );
        assert_eq!(
            icon_url("TSLA", "stock").as_deref(),
            Some("https://financialmodelingprep.com/image-stock/TSLA.png")
        );
        assert_eq!(icon_url("GOLD", "commodity"), None);
        assert_eq!(describe("xyz:TSLA"), ("Tesla".into(), "stock"));
        assert_eq!(describe("xyz:GOLD"), ("Gold".into(), "commodity"));
        assert_eq!(describe("xyz:EUR"), ("Euro".into(), "currency"));
        assert_eq!(describe("xyz:SP500").1, "index");
        // Unknown tickers on the dex keep their name and count as stocks.
        assert_eq!(describe("xyz:NEWCO"), ("NEWCO".into(), "stock"));
    }

    #[test]
    fn order_outcomes_read_in_words() {
        let ok = json!({"result":{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"filled":{"totalSz":"2","avgPx":"5.35","oid":1}}]}}}});
        assert!(filled(&ok).is_ok());
        let missed = json!({"result":{"status":"ok","response":{"type":"order","data":{"statuses":[
            {"error":"Order could not immediately match against any resting orders. asset=74"}]}}}});
        assert!(filled(&missed)
            .unwrap_err()
            .contains("Nobody on Hyperliquid"));
        let refused =
            json!({"result":{"status":"err","response":"Must deposit before performing actions."}});
        assert!(filled(&refused).unwrap_err().contains("Must deposit"));
        assert!(expect_ok(json!({"result":{"status":"ok"}}), "approve").is_ok());
        assert!(expect_ok(
            json!({"result":{"status":"err","response":"no"}}),
            "approve"
        )
        .is_err());
    }

    #[test]
    fn take_profit_and_stop_loss_are_gains_and_losses_on_margin() {
        // +50% at 5x: a 10% move in the trade's favour; -25%: 5% against.
        assert!((tpsl_price(100.0, true, 5, 50.0) - 110.0).abs() < 1e-9);
        assert!((tpsl_price(100.0, true, 5, -25.0) - 95.0).abs() < 1e-9);
        assert!((tpsl_price(100.0, false, 5, 50.0) - 90.0).abs() < 1e-9);
        assert!((tpsl_price(100.0, false, 5, -25.0) - 105.0).abs() < 1e-9);
        assert!((tpsl_pct(100.0, true, 5, 110.0) - 50.0).abs() < 1e-9);
        assert!((tpsl_pct(100.0, false, 5, 105.0) + 25.0).abs() < 1e-9);
        assert!(check_tpsl(Some(50.0), Some(25.0), true, 5).is_ok());
        assert!(check_tpsl(None, None, true, 5).is_ok());
        assert!(check_tpsl(Some(0.5), None, true, 5).is_err());
        assert!(check_tpsl(None, Some(95.0), true, 5).is_err());
        assert!(check_tpsl(Some(f64::NAN), None, true, 5).is_err());
        // A 1x short can't make more than 99%: the price would have to go below zero.
        assert!(check_tpsl(Some(100.0), None, false, 1).is_err());
        assert!(check_tpsl(Some(100.0), None, false, 2).is_ok());
        assert_eq!(say_usd(120_000.0), "$120,000");
        assert_eq!(say_usd(0.53214), "$0.53214");
        assert_eq!(say_usd(4_321.5), "$4,321.5");
        assert_eq!(say_pct(50.0), "50%");
        let view = tpsl_view(
            &[PositionTrigger {
                coin: "BTC".into(),
                oid: 1,
                kind: "sl",
                trigger_price: 95.0,
            }],
            100.0,
            true,
            5,
        );
        assert_eq!(view["takeProfit"], Value::Null);
        assert_eq!(view["stopLoss"]["pct"], "-25");
        assert_eq!(view["stopLoss"]["price"]["amount"], "95");
    }

    #[test]
    fn margin_moves_in_only_for_what_is_missing() {
        assert_eq!(shortfall(10.0, 12.0), 0);
        assert_eq!(shortfall(10.0, 10.0), 0);
        assert_eq!(shortfall(10.0, 4.5), 5_510_000);
    }

    #[test]
    fn a_close_sends_back_what_it_freed_or_everything_once_nothing_is_open() {
        use engine_execution::hyperliquid::{Account, Position};
        let mut account = Account {
            value: 12.5,
            withdrawable: 12.5,
            positions: Vec::new(),
        };
        // Nothing left open: the whole account, leftovers included.
        assert_eq!(cashout_units(&account, 10.0), 12_500_000);
        // Another position still open: only what this close freed, never past what can leave.
        account.positions.push(Position {
            coin: "BTC".into(),
            size: 0.001,
            entry: 100_000.0,
            value: 100.0,
            unrealized_pnl: 0.0,
            return_on_equity: 0.0,
            liquidation: None,
            margin: 20.0,
            leverage: 5,
        });
        assert_eq!(cashout_units(&account, 10.0), 10_000_000);
        account.withdrawable = 4.2;
        assert_eq!(cashout_units(&account, 10.0), 4_200_000);
        account.withdrawable = -1.0;
        assert_eq!(cashout_units(&account, 10.0), 0);
        // Relay's cents, as measured.
        assert!((cashout_fee(5.0) - 0.027).abs() < 0.002);
        assert!((cashout_fee(50.0) - 0.045).abs() < 0.002);
    }

    #[test]
    fn money_is_shown_in_the_users_currency() {
        // $10 at ₦1,500 / $ (rate in micros).
        assert_eq!(
            money(10.0, "NGN", 1_500_000_000),
            json!({"amount":"15000.00","currency":"NGN"})
        );
        assert_eq!(usd(5.30), json!({"amount":"5.3","currency":"USD"}));
    }
}

impl HlState {
    pub(super) async fn history_rows(&self, owner: &str) -> Result<Vec<Value>, ApiError> {
        if let Some(pg) = &self.intents.postgres {
            return pg
                .query(
                    "SELECT payload FROM atlas_hl_intents WHERE owner=$1",
                    &[&owner],
                )
                .await
                .map_err(internal)?
                .into_iter()
                .map(|r| history_value(r.get::<_, &str>(0)))
                .collect();
        }
        self.intents
            .memory
            .lock()
            .map_err(internal)?
            .values()
            .filter(|i| i.quote.owner == owner)
            .map(|i| {
                serde_json::to_string(i)
                    .map_err(internal)
                    .and_then(|s| serde_json::from_str(&s).map_err(internal))
            })
            .collect()
    }
}

// Save the confirmation's receipt before the phone signs; this does not submit an action.
pub(super) async fn execute(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;

    let plan = execute_inner(State(state.clone()), Path(quote_id), headers).await?;
    let mut receipt = transactions::Receipt::plan(&owner, &plan.0);

    receipt.title = transactions::title(&receipt.kind, &receipt.symbol);
    state.history.put(&receipt).await?;
    Ok(plan)
}

fn history_value(payload: &str) -> Result<Value, ApiError> {
    let i: HlIntent = serde_json::from_str(payload).map_err(internal)?;
    let mut v = serde_json::to_value(&i).map_err(internal)?;
    v["historyIcon"] = json!(icon_url(
        split_coin(&i.quote.coin).1,
        category(&i.quote.coin)
    ));
    Ok(v)
}
