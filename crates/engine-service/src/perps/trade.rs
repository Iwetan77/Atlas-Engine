use super::*;
use axum::extract::Path;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
const QUOTE_MS: u64 = 45_000;

#[derive(Clone, Default)]
pub(crate) struct TradeState {
    quotes: Arc<Mutex<HashMap<String, Quote>>>,
    intents: Arc<Mutex<HashMap<String, Intent>>>,
}
#[derive(Clone)]
struct Quote {
    owner: String,
    wallet: String,
    account: String,
    market: String,
    side: String,
    size: String,
    price: String,
    leverage: u64,
    expires: u64,
    kind: &'static str,
    position_id: Option<String>,
    currency: String,
    margin: String,
}
#[derive(Clone)]
struct Intent {
    quote: Quote,
    client_id: String,
    order_id: Option<String>,
    status: markets::IntentStatus,
    expires: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenRequest {
    market_id: String,
    side: String,
    margin: Amount,
    leverage: u64,
}
#[derive(Deserialize)]
struct Amount {
    amount: String,
    currency: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn new_id(prefix: &str) -> String {
    format!(
        "perp-{prefix}-{:x}-{:x}",
        now(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn money(amount: &str, currency: &str) -> Value {
    json!({"amount":amount,"currency":currency})
}
fn units(raw: &str) -> Result<u128, ApiError> {
    decimal_units(raw)
}
fn mul(a: u128, b: u128) -> Result<u128, ApiError> {
    a.checked_mul(b)
        .map(|v| v / SCALE)
        .ok_or((StatusCode::BAD_REQUEST, "amount too large".into()))
}
fn rate_money(usd_units: u128, currency: &str, rate: u128) -> Result<Value, ApiError> {
    let value = usd_units
        .checked_mul(rate)
        .ok_or((StatusCode::BAD_REQUEST, "amount too large".into()))?
        / 1_000_000;
    Ok(money(&format_units(value), currency))
}
fn fresh_bbo(bbo: &Value, market: &str, side: &str) -> Result<(u128, u128), ApiError> {
    if bbo["market"].as_str() != Some(market) {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex BBO market mismatch".into(),
        ));
    }
    let updated = bbo["last_updated_at"].as_u64().ok_or((
        StatusCode::BAD_GATEWAY,
        "Paradex BBO timestamp missing".into(),
    ))?;
    if now().abs_diff(updated) > 30_000 {
        return Err(unavailable("Paradex market price is stale"));
    }
    let (price, size) = if side == "long" || side == "buy" {
        ("ask", "ask_size")
    } else {
        ("bid", "bid_size")
    };
    let p = units(venue_str(bbo, price)?)?;
    let s = units(venue_str(bbo, size)?)?;
    if p == 0 || s == 0 {
        return Err(unavailable("Paradex market has no fillable liquidity"));
    }
    Ok((p, s))
}
fn market_constraints(
    metadata: &Value,
    market: &str,
    leverage: u64,
) -> Result<(u128, u128, u128), ApiError> {
    if venue_str(metadata, "symbol")? != market || venue_str(metadata, "asset_kind")? != "PERP" {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex market identity mismatch".into(),
        ));
    }
    let max = max_leverage(venue_str(
        &metadata["delta1_cross_margin_params"],
        "imf_base",
    )?)?;
    if leverage == 0 || leverage > max {
        return Err(bad("leverage exceeds Paradex market limit"));
    }
    let increment = units(venue_str(metadata, "order_size_increment")?)?;
    let min = units(venue_str(metadata, "min_notional")?)?;
    let fee = units(venue_str(
        &metadata["fee_config"]["api_fee"]["taker_fee"],
        "fee",
    )?)?;
    if increment == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex order increment is zero".into(),
        ));
    }
    Ok((increment, min, fee))
}
fn order_size(
    notional: u128,
    price: u128,
    increment: u128,
    min: u128,
    liquidity: u128,
) -> Result<u128, ApiError> {
    let size = notional
        .checked_mul(SCALE)
        .ok_or((StatusCode::BAD_REQUEST, "notional too large".into()))?
        / price;
    let size = size / increment * increment;
    if size == 0 || mul(size, price)? < min {
        return Err(bad("amount is below Paradex minimum order"));
    }
    if size > liquidity {
        return Err(unavailable(
            "Paradex order book cannot fill this amount now",
        ));
    }
    Ok(size)
}
async fn owner(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(String, String, String), ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    let wallet = user.evm_wallet.filter(|v| !v.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let onboard = state
        .paradex
        .onboarding_status(&wallet)
        .await
        .map_err(internal)?;
    if !onboard.exists {
        return Err((StatusCode::CONFLICT, "Set up Paradex account first".into()));
    }
    Ok((user.user_id, wallet, onboard.account_address))
}
pub(crate) async fn quotes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OpenRequest>,
) -> Result<Json<Value>, ApiError> {
    if env::var("PARADEX_ENV").as_deref() != Ok("testnet") {
        return Err(unavailable("Paradex trading is enabled on testnet only"));
    }
    let (user_id, wallet, account) = owner(&state, &headers).await?;
    if !MARKET_IDS.iter().any(|(m, _, _)| *m == body.market_id) {
        return Err(bad("unsupported Paradex market"));
    }
    if !matches!(body.side.as_str(), "long" | "short") {
        return Err(bad("side must be long or short"));
    }
    let currency = checked_currency(CurrencyQuery {
        currency: Some(body.margin.currency.clone()),
    })?;
    let rate = app_balance::fx_rate(&currency).await?;
    let display = units(&body.margin.amount)?;
    let margin = display
        .checked_mul(1_000_000)
        .ok_or((StatusCode::BAD_REQUEST, "margin too large".into()))?
        / rate;
    if margin == 0 {
        return Err(bad("margin must be positive"));
    }
    let jwt = evm_jwt(&state, &headers, &user_id, &wallet, &account).await?;
    let (metadata, bbo, account_data) = tokio::try_join!(
        state.paradex.market(&body.market_id),
        state.paradex.bbo(&body.market_id),
        state.paradex.account(&jwt),
    )
    .map_err(internal)?;
    let (increment, min, fee_rate) = market_constraints(&metadata, &body.market_id, body.leverage)?;
    let (price, liquidity) = fresh_bbo(&bbo, &body.market_id, &body.side)?;
    let notional = margin
        .checked_mul(body.leverage as u128)
        .ok_or((StatusCode::BAD_REQUEST, "notional too large".into()))?;
    let size = order_size(notional, price, increment, min, liquidity)?;
    let fee = mul(mul(size, price)?, fee_rate)?;
    let collateral = units(venue_str(&account_data, "free_collateral")?)?;
    if collateral
        < margin
            .checked_add(fee)
            .ok_or((StatusCode::BAD_REQUEST, "margin too large".into()))?
    {
        return Err((
            StatusCode::CONFLICT,
            "Insufficient Paradex free collateral".into(),
        ));
    }
    let quote_id = new_id("quote");
    let expires = now() + QUOTE_MS;
    state.perps_trade.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        Quote {
            owner: user_id,
            wallet,
            account,
            market: body.market_id.clone(),
            side: body.side.clone(),
            size: format_units(size),
            price: format_units(price),
            leverage: body.leverage,
            expires,
            kind: "perp_open",
            position_id: None,
            currency: currency.clone(),
            margin: body.margin.amount.clone(),
        },
    );
    Ok(Json(json!({
        "quoteId":quote_id,"marketId":body.market_id,"side":body.side,"leverage":body.leverage,
        "margin":money(&body.margin.amount,&currency),"size":format_units(size),
        "notional":rate_money(mul(size,price)?,&currency,rate)?,
        "entryPrice":rate_money(price,&currency,rate)?,
        "liquidationPrice":null,
        "fee":rate_money(fee,&currency,rate)?,
        "expiresAtUnixMs":expires
    })))
}
pub(crate) async fn execute_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let (user_id, wallet, account) = owner(&state, &headers).await?;
    let quote = state
        .perps_trade
        .quotes
        .lock()
        .map_err(internal)?
        .get(&id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "perps quote not found".into()))?;
    if quote.owner != user_id
        || !quote.wallet.eq_ignore_ascii_case(&wallet)
        || quote.account != account
    {
        return Err((
            StatusCode::FORBIDDEN,
            "quote belongs to another wallet".into(),
        ));
    }
    if now() >= quote.expires {
        return Err((StatusCode::GONE, "perps quote expired".into()));
    }
    if quote.kind != "perp_open" {
        return Err(bad("not an open quote"));
    }
    let bbo = state.paradex.bbo(&quote.market).await.map_err(internal)?;
    let (price, liquidity) = fresh_bbo(&bbo, &quote.market, &quote.side)?;
    let previous = units(&quote.price)?;
    let tolerance = previous / 100;
    if price.abs_diff(previous) > tolerance || units(&quote.size)? > liquidity {
        return Err((
            StatusCode::CONFLICT,
            "Paradex price or liquidity changed; request a new quote".into(),
        ));
    }
    let intent_id = new_id("open");
    let status = markets::IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate",
        state: "pending",
        tx_ids: vec![],
        error: None,
    };
    state.perps_trade.intents.lock().map_err(internal)?.insert(
        intent_id.clone(),
        Intent {
            quote: quote.clone(),
            client_id: intent_id.clone(),
            order_id: None,
            status,
            expires: now() + QUOTE_MS,
        },
    );
    Ok(Json(
        json!({"intentId":intent_id,"kind":"perp_open","summary":[
        {"label":"Market","value":quote.market},{"label":"Side","value":quote.side},
        {"label":"Size","value":quote.size},{"label":"Leverage","value":format!("{}x",quote.leverage)},
        {"label":"Margin","value":format!("{} {}",quote.margin,quote.currency)},
        {"label":"Liquidation price","value":"Shown once open"}
    ],"transactions":[],"expiresAtUnixMs":now()+QUOTE_MS}),
    ))
}
pub(crate) async fn close_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable("Paradex close quotes are not enabled yet"))
}
pub(crate) async fn execute_close(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable("Paradex close execution is not enabled yet"))
}
pub(crate) async fn signed(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    let _ = (intent_id, body);
    Err(unavailable("Paradex order submission is not enabled yet"))
}
pub(crate) async fn status(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let stored = state
        .perps_trade
        .intents
        .lock()
        .map_err(internal)?
        .get(&intent_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
    if stored.quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    Ok(Json(stored.status))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size_respects_venue_minimum_and_depth() {
        assert_eq!(
            order_size(
                units("100").unwrap(),
                units("50000").unwrap(),
                units("0.00001").unwrap(),
                units("10").unwrap(),
                units("0.01").unwrap()
            )
            .unwrap(),
            units("0.002").unwrap()
        );
        assert!(order_size(
            units("1").unwrap(),
            units("50000").unwrap(),
            units("0.00001").unwrap(),
            units("10").unwrap(),
            units("0.01").unwrap()
        )
        .is_err());
    }
}
