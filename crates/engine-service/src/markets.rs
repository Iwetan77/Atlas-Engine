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
    time::{SystemTime, UNIX_EPOCH},
};

const SOL_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const BONK_MINT: &str = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
const TSLAX_MINT: &str = "XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB";
const BRETT: &str = "0x532f27101965dd16442E59d40670FaF5eBB142E4";
const AAPLC: &str = "0xb200000000000000000000C2e324d24d7eEcd1fb";
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Asset {
    id: &'static str,
    symbol: &'static str,
    name: &'static str,
    kind: &'static str,
    chain: &'static str,
    token: &'static str,
    decimals: u32,
}
const ASSETS: [Asset; 6] = [
    Asset {
        id: "weth-base",
        symbol: "WETH",
        name: "Wrapped Ether",
        kind: "crypto",
        chain: "base",
        token: BASE_WETH,
        decimals: 18,
    },
    Asset {
        id: "brett-base",
        symbol: "BRETT",
        name: "Brett",
        kind: "meme",
        chain: "base",
        token: BRETT,
        decimals: 18,
    },
    Asset {
        id: "aaplc-base",
        symbol: "AAPLc",
        name: "Coinbase Wrapped Apple",
        kind: "stock",
        chain: "base",
        token: AAPLC,
        decimals: 8,
    },
    Asset {
        id: "sol-solana",
        symbol: "SOL",
        name: "Solana",
        kind: "crypto",
        chain: "solana",
        token: SOL_MINT,
        decimals: 9,
    },
    Asset {
        id: "tslax-solana",
        symbol: "TSLAx",
        name: "Tesla xStock",
        kind: "stock",
        chain: "solana",
        token: TSLAX_MINT,
        decimals: 8,
    },
    Asset {
        id: "bonk-solana",
        symbol: "BONK",
        name: "Bonk",
        kind: "meme",
        chain: "solana",
        token: BONK_MINT,
        decimals: 5,
    },
];

#[derive(Clone)]
pub(super) struct MarketState {
    base: UniswapV3Client,
    jupiter: JupiterClient,
    rpc: reqwest::Url,
    http: reqwest::Client,
    quotes: Arc<Mutex<HashMap<String, StoredQuote>>>,
    intents: Arc<Mutex<HashMap<String, StoredIntent>>>,
}
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
    expires: u64,
    status: IntentStatus,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IntentStatus {
    intent_id: String,
    stage: &'static str,
    state: &'static str,
    tx_ids: Vec<String>,
    error: Option<String>,
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
    sent: Vec<Sent>,
    signed: Vec<Signed>,
}
#[derive(Deserialize)]
struct Sent {
    chain: String,
    id: String,
}
#[derive(Deserialize)]
struct Signed {
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
            http: reqwest::Client::new(),
            quotes: Arc::new(Mutex::new(HashMap::new())),
            intents: Arc::new(Mutex::new(HashMap::new())),
        })
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
fn asset(id: &str) -> Result<Asset, ApiError> {
    ASSETS
        .iter()
        .find(|a| a.id == id)
        .copied()
        .ok_or((StatusCode::NOT_FOUND, "unsupported asset".into()))
}
fn checked_currency(currency: &str) -> Result<(), ApiError> {
    if matches!(currency, "USD" | "NGN" | "KES" | "GHS" | "ZAR") {
        Ok(())
    } else {
        Err(bad("unsupported display currency"))
    }
}
fn parse_micros(s: &str) -> Result<u128, ApiError> {
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
fn format_units(units: u128, decimals: u32) -> String {
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
fn quote_request(a: Asset, side: &str, input: u128) -> BaseSwapRequest {
    BaseSwapRequest {
        source_token: if side == "buy" { BASE_USDC } else { a.token }.into(),
        destination_token: if side == "buy" { a.token } else { BASE_USDC }.into(),
        amount_base_units: input,
    }
}
async fn ensure_stock_units(state: &MarketState, a: Asset) -> Result<(), ApiError> {
    if a.id != "tslax-solana" {
        return Ok(());
    }
    let response: Value = state
        .http
        .get("https://api.xstocks.fi/api/v2/public/assets/TSLAx/multiplier?network=Solana")
        .send()
        .await
        .map_err(unavailable)?
        .error_for_status()
        .map_err(unavailable)?
        .json()
        .await
        .map_err(unavailable)?;
    if response["currentMultiplier"] != 1 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "TSLAx display multiplier changed; quotes are paused until adjusted".into(),
        ));
    }
    Ok(())
}
async fn venue_quote(
    state: &MarketState,
    a: Asset,
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
                input_mint: if side == "buy" { SOL_USDC } else { a.token }.into(),
                output_mint: if side == "buy" { a.token } else { SOL_USDC }.into(),
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
    if !matches!(category.as_str(), "popular" | "stocks" | "memes" | "crypto") {
        return Err(bad("unsupported asset category"));
    }
    let rate = app_balance::fx_rate(&currency).await?;
    let query = q.q.unwrap_or_default().to_ascii_lowercase();
    let mut result = Vec::new();
    for a in ASSETS {
        if category != "popular"
            && category != format!("{}s", a.kind)
            && !(category == "crypto" && a.kind == "crypto")
        {
            continue;
        }
        if !query.is_empty()
            && !a.name.to_ascii_lowercase().contains(&query)
            && !a.symbol.to_ascii_lowercase().contains(&query)
        {
            continue;
        }
        let (_, out, _) = venue_quote(&state.markets, a, "buy", 1_000_000).await?;
        result.push(json!({"assetId":a.id,"symbol":a.symbol,"name":a.name,"kind":a.kind,"price":unit_price(1_000_000,out,a.decimals,&currency,rate)?,"change24hPct":null,"iconUrl":null}));
    }
    Ok(Json(json!({"assets":result})))
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let a = asset(&req.asset_id)?;
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
        let (_, out, _) = venue_quote(&state.markets, a, "buy", 1_000_000).await?;
        usdc_units
            .checked_mul(out)
            .ok_or_else(|| bad("amount too large"))?
            / 1_000_000
    };
    if input == 0 {
        return Err(bad("amount too small for this asset"));
    }
    let (actual_in, actual_out, fee_units) =
        venue_quote(&state.markets, a, &req.side, input).await?;
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
            asset: a,
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
    ensure_stock_units(&state.markets, a).await?;
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
        let request = quote_request(a, &stored.side, stored.input_units);
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
                .push(json!({"chain":"base","to":approval.to,"data":approval.data,"value":"0"}));
        }
        let swap = state
            .markets
            .base
            .swap_transaction(&fresh, &wallet, 100)
            .map_err(unavailable)?;
        expected.push((swap.to.to_ascii_lowercase(), swap.data.to_ascii_lowercase()));
        transactions.push(json!({"chain":"base","to":swap.to,"data":swap.data,"value":"0"}));
        output = fresh.amount_out;
    } else {
        let order = state
            .markets
            .jupiter
            .order(&JupiterOrderRequest {
                input_mint: if stored.side == "buy" {
                    SOL_USDC
                } else {
                    a.token
                }
                .into(),
                output_mint: if stored.side == "buy" {
                    a.token
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
        stage: "validate",
        state: "pending",
        tx_ids: Vec::new(),
        error: None,
    };
    state.markets.intents.lock().map_err(internal)?.insert(
        intent_id.clone(),
        StoredIntent {
            owner: user.user_id,
            wallet,
            chain: a.chain.into(),
            expected,
            request_id,
            expires,
            status,
        },
    );
    Ok(Json(
        json!({"intentId":intent_id,"kind":stored.side,"summary":[{"label":"Pay","value":pay},{"label":"Receive (estimated)","value":receive},{"label":"Display currency","value":stored.currency},{"label":"Requested value","value":stored.display_amount}],"transactions":transactions,"expiresAtUnixMs":expires}),
    ))
}

async fn base_rpc(state: &MarketState, method: &str, params: Value) -> Result<Value, ApiError> {
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
    if current.expires < now() {
        return Err((StatusCode::CONFLICT, "execution plan expired".into()));
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
        status.stage = "settle";
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
            stored.status.stage = "execute";
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
                status.stage = "settle";
                status.state = "filled";
            }
            Err(error) => {
                status.stage = "settle";
                status.state = "failed";
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
            status.state = "failed";
            status.error = Some("reported transaction does not match the signed plan".into());
            break;
        }
        let receipt = base_rpc(&state.markets, "eth_getTransactionReceipt", json!([hash])).await?;
        if receipt.is_null() {
            return Ok(Json(status));
        }
        if receipt["status"].as_str() != Some("0x1") {
            status.state = "failed";
            status.error = Some("Base transaction reverted".into());
            break;
        }
    }
    if status.state == "pending" {
        status.state = "filled";
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
    fn money_and_price_keep_small_unit_precision() {
        assert_eq!(parse_micros("20.123456").unwrap(), 20_123_456);
        assert!(parse_micros("0.0000001").is_err());
        assert_eq!(format_units(283, 4), "0.0283");
        let price = unit_price(1_000_000, 1_000_000_000, 5, "NGN", 1_000_000_000).unwrap();
        assert_eq!(price.amount, "0.1");
    }
}
