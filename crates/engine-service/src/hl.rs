//! Perps on Hyperliquid, behind the same API the app already uses. The account is the user's own EVM
//! wallet. Margin moves in from the balance in the same confirmation (Relay: gasless from Base, or a
//! Solana transaction the user signs); then the user's Atlas agent (approved once, by their own
//! wallet, inside that confirmation) sets leverage and places an immediate-or-cancel order. The
//! agent can trade, never withdraw. A close sends the money it freed back to the user's cash in the
//! same confirmation (Relay, signed by the user's own session through the bridge).
use super::*;
use axum::extract::Query;
use engine_execution::hyperliquid::{order_price, order_size, Market};
use serde_json::{json, Value};

const QUOTE_MS: u64 = 45_000;
// Every market's list in one call; refreshed in the background, fetched on the spot when older.
const MARKETS_TTL: Duration = Duration::from_secs(30);
const REFRESH_EVERY: Duration = Duration::from_secs(10);
// Hyperliquid's base taker fee (0.045%), and the price room a market order allows (3%).
const TAKER_FEE: f64 = 0.00045;
const SLIPPAGE: f64 = 0.03;
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
}

#[derive(Clone, Serialize, Deserialize)]
struct HlIntent {
    quote: HlQuote,
    status: markets::IntentStatus,
    funding: Option<Funding>,
    expires: u64,
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
    async fn claim(&self, id: &str, intent: &HlIntent) -> Result<bool, ApiError> {
        let payload = serde_json::to_string(intent).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            let changed = pg
                .execute(
                    "UPDATE atlas_hl_intents SET stage=$2, payload=$3 WHERE intent_id=$1 AND stage='validate'",
                    &[&id, &intent.status.stage, &payload],
                )
                .await
                .map_err(internal)?;
            return Ok(changed == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        match memory.get(id) {
            Some(current) if current.status.stage == "validate" => {
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
            client: engine_execution::hyperliquid::HyperliquidClient::new()?,
            markets: Arc::new(Mutex::new(None)),
            quotes: Arc::new(Mutex::new(HashMap::new())),
            intents: IntentStore {
                postgres,
                memory: Arc::new(Mutex::new(HashMap::new())),
            },
        })
    }
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
    if MEMES.contains(&coin) {
        "meme"
    } else {
        "crypto"
    }
}

async fn markets_now(state: &AppState) -> Result<Arc<Vec<Market>>, ApiError> {
    if let Some((at, markets)) = state.hl.markets.lock().map_err(internal)?.clone() {
        if at.elapsed() < MARKETS_TTL {
            return Ok(markets);
        }
    }
    refresh_markets(state).await
}

async fn refresh_markets(state: &AppState) -> Result<Arc<Vec<Market>>, ApiError> {
    let markets = Arc::new(state.hl.client.markets().await.map_err(venue)?);
    if markets.is_empty() {
        return Err(venue("Hyperliquid listed no markets"));
    }
    *state.hl.markets.lock().map_err(internal)? = Some((Instant::now(), markets.clone()));
    Ok(markets)
}

// The market list stays warm, so opening Perps never waits.
pub(super) fn keep_warm(state: AppState) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = refresh_markets(&state).await {
                eprintln!("hyperliquid markets not refreshed: {}", error.1);
            }
            tokio::time::sleep(REFRESH_EVERY).await;
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
            let kind = category(&m.coin);
            let change =
                (m.prev_day > 0.0).then(|| format!("{:.2}", (m.mark / m.prev_day - 1.0) * 100.0));
            json!({"marketId":market_id(&m.coin),"symbol":m.coin,"name":m.coin,"category":kind,
                "iconUrl":perps::icon_url(&m.coin, kind),"markPrice":money(m.mark,&currency,rate),
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
    let (_, wallet) = wallet_of(&state, &headers).await?;
    currency_of(q.currency)?;
    let (account, fills, markets) = tokio::try_join!(
        async { state.hl.client.account(&wallet).await.map_err(venue) },
        async { state.hl.client.fills(&wallet).await.map_err(venue) },
        markets_now(&state)
    )?;
    let rows: Vec<Value> = account
        .positions
        .iter()
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
            json!({"positionId":p.coin,"openedAtUnixMs":opened,"marketId":market_id(&p.coin),
                "symbol":p.coin,"iconUrl":perps::icon_url(&p.coin, kind),
                "side":if p.size > 0.0 {"long"} else {"short"},"leverage":p.leverage,
                "size":trim(p.size.abs()),"entryPrice":usd(p.entry),"markPrice":usd(mark),
                "liquidationPrice":p.liquidation.map(usd),"margin":usd(p.margin),
                "unrealizedPnl":usd(p.unrealized_pnl),
                "unrealizedPnlPct":format!("{:.2}", p.return_on_equity * 100.0)})
        })
        .collect();
    Ok(Json(json!({"positions":rows})))
}

// The user's Hyperliquid account value in USDC units (6 decimals), for the Atlas balance.
pub(super) async fn account_value(state: &AppState, wallet: &str) -> Option<u128> {
    let account = state.hl.client.account(wallet).await.ok()?;
    (account.value > 0.0).then(|| (account.value * 1_000_000.0).floor() as u128)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OpenRequest {
    market_id: String,
    side: String,
    margin: Amount,
    leverage: u32,
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
    let fee = notional * TAKER_FEE;
    // What's already in the account covers it, or the rest moves in first (with a cushion for the fee).
    let account = state.hl.client.account(&wallet).await.unwrap_or_default();
    let needed = margin + fee * 2.0;
    let short_units = if account.withdrawable >= needed {
        0
    } else {
        ((needed - account.withdrawable) * 1_000_000.0).ceil() as u128 + 10_000
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
        "liquidationPrice":null,"fee":money(fee,&currency,rate),"expiresAtUnixMs":expires}),
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
    let account = state.hl.client.account(&wallet).await.map_err(venue)?;
    let position = account
        .positions
        .iter()
        .find(|p| p.coin == position_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "no such position".into()))?;
    let m = market(&state, &market_id(&position.coin)).await?;
    let trade_fee = position.size.abs() * m.mark * TAKER_FEE;
    let pnl = position.unrealized_pnl - trade_fee;
    let freed = (position.margin + pnl).max(0.0);
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
            size: trim(position.size.abs()),
            mark: m.mark,
            close: true,
            funding_units: 0,
            funding_from_solana: false,
            receive_usd: freed,
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
// top-up if the wallet needs one); from Base nothing is signed in the app (the user's session signs
// the gasless authorization at /signed); the rest happens after the confirm.
pub(super) async fn execute(
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
        transactions.push(json!({"chain":"solana","transaction":tx,"submit":"engine"}));
        Some(Funding {
            request_id: deposit.request_id,
            base_typed_data: None,
            base_api: None,
            gas_request_id: gas.map(|(id, _)| id),
        })
    } else {
        let deposit = state
            .relay_link
            .base_to_hyperliquid(&wallet, quote.funding_units)
            .await
            .map_err(|_| conflict("Couldn't move your margin right now. Try again shortly."))?;
        Some(Funding {
            request_id: deposit.request_id,
            base_typed_data: Some(deposit.typed_data),
            base_api: Some(deposit.api),
            gas_request_id: None,
        })
    };
    let currency_rate = |q: &HlQuote| (q.size.clone(), q.leverage);
    let (size, leverage) = currency_rate(&quote);
    let mut summary = vec![
        json!({"label":"Market","value":quote.coin}),
        json!({"label":"Action","value":if quote.close {"Close position".to_string()} else {
            format!("{} {}x", if quote.side == "long" {"Long"} else {"Short"}, leverage)}}),
        json!({"label":"Size","value":format!("{size} {}", quote.coin)}),
    ];
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
    if current.status.stage != "validate" {
        return Ok(Json(current.status));
    }
    let from_solana = current.funding.is_some() && current.quote.funding_from_solana;
    let main = usize::from(
        current
            .funding
            .as_ref()
            .is_some_and(|f| f.gas_request_id.is_some()),
    );
    if from_solana {
        if !body.sent.is_empty() || body.signed.len() != main + 1 || body.signed[main].index != main
        {
            return Err(bad("signed report does not match the plan"));
        }
    } else if !body.sent.is_empty() || !body.signed.is_empty() {
        return Err(bad("signed report does not match the plan"));
    }
    let mut claimed = current.clone();
    claimed.status.stage = if current.funding.is_some() {
        "fund"
    } else {
        "execute"
    }
    .into();
    if !state.hl.intents.claim(&intent_id, &claimed).await? {
        let latest = state.hl.intents.get(&intent_id).await?;
        return Ok(Json(latest.map_or(current.status, |i| i.status)));
    }
    let answer = claimed.status.clone();
    let signed = body.signed;
    tokio::spawn(async move {
        let mut intent = claimed;
        let result = run(&state, &headers, &mut intent, &signed, main).await;
        intent.status.stage = "settle".into();
        match result {
            Ok(()) => intent.status.state = "filled".into(),
            Err(message) => {
                intent.status.state = "failed".into();
                intent.status.error = Some(message);
            }
        }
        if let Err(error) = state.hl.intents.put(&intent_id, &intent).await {
            eprintln!("hyperliquid intent {intent_id} not saved: {}", error.1);
        }
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
    if let Some(funding) = intent.funding.clone() {
        let not_moved = |reason: String| {
            eprintln!("hyperliquid margin not moved: {reason}");
            "Moving your margin didn't go through, so nothing left your balance and nothing was ordered".to_string()
        };
        if let (Some(typed), Some(api)) = (&funding.base_typed_data, &funding.base_api) {
            let signature = gasless::sign(state, headers, &quote.wallet, typed)
                .await
                .map_err(not_moved)?;
            state
                .relay_link
                .submit(&funding.request_id, api, &signature)
                .await
                .map_err(|e| not_moved(e.to_string()))?;
        } else {
            if let (Some(gas_id), true) = (&funding.gas_request_id, main == 1) {
                markets::land_gas_topup(state, gas_id, &signed[0].transaction).await;
            }
            let signature = state
                .solana_mainnet
                .send_signed(&signed[main].transaction)
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
        expect_ok(
            bridge(state, headers, "approve", json!({})).await?,
            "approve",
        )?;
    }
    if !quote.close {
        expect_ok(
            bridge(
                state,
                headers,
                "leverage",
                json!({"asset":quote.asset,"leverage":quote.leverage}),
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
    if quote.close {
        cash_out(state, headers, intent).await;
    }
    Ok(())
}

// After a close: the money it freed goes back to cash. A cash-out that doesn't happen leaves the
// money in perps, where it still counts in the balance and pays for the next trade: the close stands.
async fn cash_out(state: &AppState, headers: &HeaderMap, intent: &mut HlIntent) {
    intent.status.stage = "settle".into();
    let id = intent.status.intent_id.clone();
    if let Err(error) = state.hl.intents.put(&id, intent).await {
        eprintln!("hyperliquid intent {id} not saved: {}", error.1);
    }
    if let Err(reason) = move_to_cash(state, headers, &intent.quote).await {
        eprintln!("hyperliquid cash-out for {id} not done: {reason}");
    }
}

// Relay pays it out to the user's own Solana wallet (Base without one); the bridge pins that.
// With nothing left open, everything in the account comes back, else only what this close freed.
async fn move_to_cash(
    state: &AppState,
    headers: &HeaderMap,
    quote: &HlQuote,
) -> Result<(), String> {
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
        "cashout",
        json!({"amount":units.to_string(),"to":to}),
    )
    .await?;
    let request_id = answer["result"]["requestId"]
        .as_str()
        .ok_or("Relay gave no request id")?;
    let deadline = tokio::time::Instant::now() + CASHOUT_LIMIT;
    loop {
        match state.relay_link.state(request_id).await {
            Ok(engine_execution::layerswap::SwapState::Completed) => return Ok(()),
            Ok(engine_execution::layerswap::SwapState::Failed(why)) => {
                return Err(format!("Relay says {why}"));
            }
            _ if tokio::time::Instant::now() > deadline => {
                return Err(format!("Relay request {request_id} still moving"));
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
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
    request["identityToken"] = json!(app_balance::identity_token(headers));
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
