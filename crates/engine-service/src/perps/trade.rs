use super::*;
use axum::extract::Path;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(1);
const QUOTE_MS: u64 = 45_000;
// The confirmed submission runs after /signed has answered. It is a chain of venue and signer
// calls, each with its own timeout, so the whole chain gets one bound too.
const SUBMIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);
// Status only gives up on a submission once it cannot still be running: past the plan's expiry
// (the latest it could have been claimed) plus the submission bound, with a margin.
const SUBMIT_GRACE_MS: u64 = 180_000;

#[derive(Clone, Default)]
pub(crate) struct TradeState {
    quotes: Arc<Mutex<HashMap<String, Quote>>>,
    intents: Arc<Mutex<HashMap<String, Intent>>>,
    postgres: Option<Arc<tokio_postgres::Client>>,
}
impl TradeState {
    pub(crate) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut state = Self::default();
        if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("perps database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_perp_intents (
                    intent_id TEXT PRIMARY KEY,
                    owner TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    expires_at_ms BIGINT NOT NULL
                )",
                )
                .await?;
            state.postgres = Some(Arc::new(client));
        }
        Ok(state)
    }
    async fn insert_intent(&self, id: &str, intent: Intent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            pg.execute("INSERT INTO atlas_perp_intents (intent_id,owner,payload,stage,expires_at_ms) VALUES ($1,$2,$3,$4,$5)",
                &[&id,&intent.quote.owner,&payload,&intent.status.stage,&i64::try_from(intent.expires).map_err(internal)?])
                .await.map_err(internal)?;
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .insert(id.to_owned(), intent);
        }
        Ok(())
    }
    async fn get_intent(&self, id: &str) -> Result<Option<Intent>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT payload FROM atlas_perp_intents WHERE intent_id=$1",
                    &[&id],
                )
                .await
                .map_err(internal)?;
            return row
                .map(|r| serde_json::from_str::<Intent>(r.get::<_, &str>(0)).map_err(internal))
                .transpose();
        }
        Ok(self.intents.lock().map_err(internal)?.get(id).cloned())
    }
    async fn save_intent(&self, id: &str, intent: Intent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            let changed = pg
                .execute(
                    "UPDATE atlas_perp_intents SET payload=$2,stage=$3 WHERE intent_id=$1",
                    &[&id, &payload, &intent.status.stage],
                )
                .await
                .map_err(internal)?;
            if changed != 1 {
                return Err((StatusCode::NOT_FOUND, "perps intent not found".into()));
            }
        } else {
            let mut intents = self.intents.lock().map_err(internal)?;
            if !intents.contains_key(id) {
                return Err((StatusCode::NOT_FOUND, "perps intent not found".into()));
            }
            intents.insert(id.to_owned(), intent);
        }
        Ok(())
    }
    async fn claim_intent(&self, id: &str, owner: &str) -> Result<(Intent, bool), ApiError> {
        let mut current = self
            .get_intent(id)
            .await?
            .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
        if current.quote.owner != owner {
            return Err((
                StatusCode::FORBIDDEN,
                "intent belongs to another user".into(),
            ));
        }
        if current.status.stage != "validate" {
            return Ok((current, false));
        }
        if now() >= current.expires {
            return Err((StatusCode::GONE, "perps execution plan expired".into()));
        }
        current.status.stage = "execute".into();
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&current).map_err(internal)?;
            let changed=pg.execute(
                "UPDATE atlas_perp_intents SET payload=$2,stage='execute' WHERE intent_id=$1 AND owner=$3 AND stage='validate' AND expires_at_ms>$4",
                &[&id,&payload,&owner,&i64::try_from(now()).map_err(internal)?]
            ).await.map_err(internal)?;
            if changed == 1 {
                return Ok((current, true));
            }
            let latest = self
                .get_intent(id)
                .await?
                .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
            return Ok((latest, false));
        }
        let mut intents = self.intents.lock().map_err(internal)?;
        let stored = intents
            .get_mut(id)
            .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
        if stored.status.stage != "validate" {
            return Ok((stored.clone(), false));
        }
        stored.status.stage = "execute".into();
        Ok((stored.clone(), true))
    }
}
#[derive(Clone, Serialize, Deserialize)]
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
    kind: String,
    position_id: Option<String>,
    currency: String,
    margin: String,
}
#[derive(Clone, Serialize, Deserialize)]
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
fn fresh_bbo(
    bbo: &Value,
    summary: &Value,
    market: &str,
    side: &str,
) -> Result<(u128, u128), ApiError> {
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
    // This is the last order-book change, not the time the HTTP snapshot was served.
    // A resting executable order can remain unchanged for longer than 30 seconds.
    if updated > now().saturating_add(30_000) {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex BBO timestamp is in the future".into(),
        ));
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
    let mark = units(venue_str(summary, "mark_price")?)?;
    let deviation = p
        .abs_diff(mark)
        .checked_mul(100)
        .ok_or((StatusCode::BAD_GATEWAY, "Paradex price overflow".into()))?;
    let limit = mark.checked_mul(3).ok_or((
        StatusCode::BAD_GATEWAY,
        "Paradex mark price overflow".into(),
    ))?;
    if mark == 0 || deviation > limit {
        return Err(unavailable(
            "Paradex executable price is too far from mark price",
        ));
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
    let display = units(&body.margin.amount)
        .map_err(|_| bad("margin must be a positive decimal with at most twelve places"))?;
    let margin = display
        .checked_mul(1_000_000)
        .ok_or((StatusCode::BAD_REQUEST, "margin too large".into()))?
        / rate;
    if margin == 0 {
        return Err(bad("margin must be positive"));
    }
    let jwt = evm_jwt(&state, &headers, &user_id, &wallet, &account).await?;
    let (metadata, bbo, summary, account_data) = tokio::try_join!(
        state.paradex.market(&body.market_id),
        state.paradex.bbo(&body.market_id),
        state.paradex.summary(&body.market_id),
        state.paradex.account(&jwt),
    )
    .map_err(internal)?;
    let (increment, min, fee_rate) = market_constraints(&metadata, &body.market_id, body.leverage)?;
    let (price, liquidity) = fresh_bbo(&bbo, &summary, &body.market_id, &body.side)?;
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
            kind: "perp_open".into(),
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
    let (bbo, summary) = tokio::try_join!(
        state.paradex.bbo(&quote.market),
        state.paradex.summary(&quote.market),
    )
    .map_err(internal)?;
    let (price, liquidity) = fresh_bbo(&bbo, &summary, &quote.market, &quote.side)?;
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
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: vec![],
        error: None,
    };
    let expires = now() + QUOTE_MS;
    state
        .perps_trade
        .insert_intent(
            &intent_id,
            Intent {
                quote: quote.clone(),
                client_id: intent_id.clone(),
                order_id: None,
                status,
                expires,
            },
        )
        .await?;
    Ok(Json(
        json!({"intentId":intent_id,"kind":"perp_open","summary":[
        {"label":"Market","value":quote.market},{"label":"Side","value":quote.side},
        {"label":"Size","value":quote.size},{"label":"Leverage","value":format!("{}x",quote.leverage)},
        {"label":"Margin","value":format!("{} {}",quote.margin,quote.currency)},
        {"label":"Liquidation price","value":"Shown once open"}
    ],"transactions":[],"expiresAtUnixMs":expires}),
    ))
}
fn signed_number(value: &str) -> Result<i128, ApiError> {
    if let Some(rest) = value.strip_prefix('-') {
        let v = units(rest)?;
        Ok(-i128::try_from(v).map_err(internal)?)
    } else {
        i128::try_from(units(value)?).map_err(internal)
    }
}
fn signed_money(value: i128, currency: &str, rate: u128) -> Result<Value, ApiError> {
    let negative = value < 0;
    let magnitude = value
        .unsigned_abs()
        .checked_mul(rate)
        .ok_or((StatusCode::BAD_REQUEST, "amount too large".into()))?
        / 1_000_000;
    let amount = format!(
        "{}{}",
        if negative { "-" } else { "" },
        format_units(magnitude)
    );
    Ok(money(&amount, currency))
}
pub(crate) async fn close_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(position_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    if env::var("PARADEX_ENV").as_deref() != Ok("testnet") {
        return Err(unavailable("Paradex trading is enabled on testnet only"));
    }
    let (user_id, wallet, account) = owner(&state, &headers).await?;
    let jwt = evm_jwt(&state, &headers, &user_id, &wallet, &account).await?;
    let positions = state.paradex.positions(&jwt).await.map_err(internal)?;
    let position = positions
        .into_iter()
        .find(|p| p["id"].as_str() == Some(&position_id) && p["status"].as_str() == Some("OPEN"))
        .ok_or((
            StatusCode::NOT_FOUND,
            "open Paradex position not found".into(),
        ))?;
    let market = venue_str(&position, "market")?;
    if !MARKET_IDS.iter().any(|(m, _, _)| *m == market) {
        return Err(bad("unsupported Paradex market"));
    }
    let close_side = match venue_str(&position, "side")? {
        "LONG" => "sell",
        "SHORT" => "buy",
        _ => {
            return Err((
                StatusCode::BAD_GATEWAY,
                "invalid Paradex position side".into(),
            ))
        }
    };
    let size = venue_str(&position, "size")?.trim_start_matches('-');
    let size_units = units(size)?;
    if size_units == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex position size is zero".into(),
        ));
    }
    let venue_leverage = units(venue_str(&position, "leverage")?)?;
    if venue_leverage == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex position leverage is zero".into(),
        ));
    }
    let (metadata, bbo, summary) = tokio::try_join!(
        state.paradex.market(market),
        state.paradex.bbo(market),
        state.paradex.summary(market),
    )
    .map_err(internal)?;
    let (price, liquidity) = fresh_bbo(&bbo, &summary, market, close_side)?;
    if liquidity < size_units {
        return Err(unavailable(
            "Paradex order book cannot fill the full close now",
        ));
    }
    let fee_rate = units(venue_str(
        &metadata["fee_config"]["api_fee"]["taker_fee"],
        "fee",
    )?)?;
    let fee = mul(mul(size_units, price)?, fee_rate)?;
    let entry = units(venue_str(&position, "average_entry_price")?)?;
    let pnl_per_unit = if close_side == "sell" {
        i128::try_from(price).map_err(internal)? - i128::try_from(entry).map_err(internal)?
    } else {
        i128::try_from(entry).map_err(internal)? - i128::try_from(price).map_err(internal)?
    };
    let pnl = pnl_per_unit
        .checked_mul(i128::try_from(size_units).map_err(internal)?)
        .ok_or((StatusCode::BAD_REQUEST, "PnL overflow".into()))?
        / i128::try_from(SCALE).map_err(internal)?;
    let basis = signed_number(venue_str(&position, "cost_usd")?)?
        .unsigned_abs()
        .checked_mul(SCALE)
        .ok_or((
            StatusCode::BAD_GATEWAY,
            "Paradex position cost overflow".into(),
        ))?
        / venue_leverage;
    let receive = i128::try_from(basis)
        .map_err(internal)?
        .saturating_add(pnl)
        .saturating_sub(i128::try_from(fee).map_err(internal)?)
        .max(0);
    let currency = "USD";
    let quote_id = new_id("close-quote");
    let expires = now() + QUOTE_MS;
    state.perps_trade.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        Quote {
            owner: user_id,
            wallet,
            account,
            market: market.into(),
            side: close_side.into(),
            size: size.into(),
            price: format_units(price),
            leverage: 1,
            expires,
            kind: "perp_close".into(),
            position_id: Some(position_id.clone()),
            currency: currency.into(),
            margin: format_units(basis),
        },
    );
    Ok(Json(json!({"quoteId":quote_id,"positionId":position_id,
        "receive":money(&format_units(receive as u128),currency),
        "realizedPnl":signed_money(pnl,currency,1_000_000)?,
        "exitPrice":money(&format_units(price),currency),
        "fee":money(&format_units(fee),currency),"expiresAtUnixMs":expires
    })))
}
pub(crate) async fn execute_close(
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
        .ok_or((StatusCode::NOT_FOUND, "perps close quote not found".into()))?;
    if quote.kind != "perp_close" {
        return Err(bad("not a close quote"));
    }
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
        return Err((StatusCode::GONE, "perps close quote expired".into()));
    }
    let jwt = evm_jwt(&state, &headers, &user_id, &wallet, &account).await?;
    let positions = state.paradex.positions(&jwt).await.map_err(internal)?;
    let existing = positions
        .into_iter()
        .find(|p| {
            p["id"].as_str() == quote.position_id.as_deref() && p["status"].as_str() == Some("OPEN")
        })
        .ok_or((
            StatusCode::CONFLICT,
            "Paradex position is no longer open".into(),
        ))?;
    let expected_side = if quote.side == "sell" {
        "LONG"
    } else {
        "SHORT"
    };
    if existing["side"].as_str() != Some(expected_side)
        || existing["size"]
            .as_str()
            .map(|s| units(s.trim_start_matches('-')))
            .transpose()?
            != Some(units(&quote.size)?)
    {
        return Err((
            StatusCode::CONFLICT,
            "Paradex position size changed; request a new close quote".into(),
        ));
    }
    let (bbo, summary) = tokio::try_join!(
        state.paradex.bbo(&quote.market),
        state.paradex.summary(&quote.market),
    )
    .map_err(internal)?;
    let (price, liquidity) = fresh_bbo(&bbo, &summary, &quote.market, &quote.side)?;
    if price.abs_diff(units(&quote.price)?) > units(&quote.price)? / 100
        || liquidity < units(&quote.size)?
    {
        return Err((
            StatusCode::CONFLICT,
            "Paradex close price or liquidity changed; request a new quote".into(),
        ));
    }
    let intent_id = new_id("close");
    let expires = now() + QUOTE_MS;
    let status = markets::IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: vec![],
        error: None,
    };
    state
        .perps_trade
        .insert_intent(
            &intent_id,
            Intent {
                quote: quote.clone(),
                client_id: intent_id.clone(),
                order_id: None,
                status,
                expires,
            },
        )
        .await?;
    Ok(Json(
        json!({"intentId":intent_id,"kind":"perp_close","summary":[
        {"label":"Market","value":quote.market},{"label":"Action","value":"Close position"},
        {"label":"Size","value":quote.size},{"label":"Estimated exit price","value":format!("{} USD",quote.price)}
    ],"transactions":[],"expiresAtUnixMs":expires}),
    ))
}
fn access_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))
}
async fn signed_bridge(
    state: &AppState,
    headers: &HeaderMap,
    user: &str,
    wallet: &str,
    route: &str,
    details: Value,
) -> Result<Value, ApiError> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err(unavailable("Privy signer unavailable"));
    };
    let mut input = json!({"accessToken":access_token(headers)?,"walletAddress":wallet});
    if let (Some(to), Some(from)) = (input.as_object_mut(), details.as_object()) {
        for (k, v) in from {
            to.insert(k.clone(), v.clone());
        }
    }
    let response = http
        .post(format!("{bridge_url}/paradex/{route}"))
        .json(&input)
        .send()
        .await
        .map_err(|_| unavailable("Privy signing bridge unavailable"))?;
    if response.status() == reqwest::StatusCode::CONFLICT {
        return Err((
            StatusCode::CONFLICT,
            "Approve Atlas perps signer in the app".into(),
        ));
    }
    if !response.status().is_success() {
        return Err(unavailable("Privy could not sign Paradex request"));
    }
    let proof: Value = response.json().await.map_err(internal)?;
    if proof["userId"].as_str() != Some(user)
        || proof["walletAddress"]
            .as_str()
            .is_none_or(|v| !v.eq_ignore_ascii_case(wallet))
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Privy signing proof wallet mismatch".into(),
        ));
    }
    Ok(proof)
}
fn market_order_request(
    client_id: &str,
    market: &str,
    side: &str,
    size: &str,
    signature: &str,
    timestamp: u64,
    reduce_only: bool,
) -> Value {
    // Paradex's live API rejects price on MARKET orders. The signer still signs
    // price=0, but the submitted JSON must omit that field entirely.
    json!({
        "client_id":client_id,
        "instruction":"IOC",
        "market":market,
        "side":side,
        "size":size,
        "type":"MARKET",
        "signature":signature,
        "signature_timestamp":timestamp,
        "flags":if reduce_only {vec!["REDUCE_ONLY"]} else {vec![]}
    })
}
async fn submit_confirmed(
    state: &AppState,
    headers: &HeaderMap,
    intent: &Intent,
) -> Result<Value, ApiError> {
    let quote = &intent.quote;
    let (bbo, summary) = tokio::try_join!(
        state.paradex.bbo(&quote.market),
        state.paradex.summary(&quote.market),
    )
    .map_err(internal)?;
    let (latest, liquidity) = fresh_bbo(&bbo, &summary, &quote.market, &quote.side)?;
    let quoted = units(&quote.price)?;
    if latest.abs_diff(quoted) > quoted / 100 || liquidity < units(&quote.size)? {
        return Err((
            StatusCode::CONFLICT,
            "Paradex price or liquidity changed before submission".into(),
        ));
    }
    let evm = evm_jwt(state, headers, &quote.owner, &quote.wallet, &quote.account).await?;
    let config = state.paradex.system_config().await.map_err(internal)?;
    let chain = venue_str(&config, "starknet_chain_id")?;
    if quote.kind == "perp_open" {
        let margin = state
            .paradex
            .set_cross_margin(&evm, &quote.market, quote.leverage)
            .await
            .map_err(internal)?;
        if margin["leverage"].as_u64() != Some(quote.leverage)
            || margin["market"].as_str() != Some(&quote.market)
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Paradex did not confirm leverage".into(),
            ));
        }
    } else {
        let positions = state.paradex.positions(&evm).await.map_err(internal)?;
        let still_open = positions.iter().any(|p| {
            p["id"].as_str() == quote.position_id.as_deref()
                && p["status"].as_str() == Some("OPEN")
                && p["side"].as_str()
                    == Some(if quote.side == "sell" {
                        "LONG"
                    } else {
                        "SHORT"
                    })
                && p["size"]
                    .as_str()
                    .and_then(|s| units(s.trim_start_matches('-')).ok())
                    == units(&quote.size).ok()
        });
        if !still_open {
            return Err((
                StatusCode::CONFLICT,
                "Paradex position changed before close submission".into(),
            ));
        }
    }
    let registration = signed_bridge(
        state,
        headers,
        &quote.owner,
        &quote.wallet,
        "subkey-registration-signature",
        json!({}),
    )
    .await?;
    let key = venue_str(&registration, "publicKey")?;
    if !state
        .paradex
        .subkey_exists(&evm, key)
        .await
        .map_err(internal)?
    {
        state
            .paradex
            .register_subkey(
                &evm,
                key,
                venue_str(&registration, "signature")?,
                venue_str(&registration, "siweMessage")?,
            )
            .await
            .map_err(internal)?;
    }
    let auth = signed_bridge(
        state,
        headers,
        &quote.owner,
        &quote.wallet,
        "subkey-auth-signature",
        json!({
            "accountAddress":quote.account,"chainId":chain
        }),
    )
    .await?;
    if auth["publicKey"].as_str() != Some(key) {
        return Err((StatusCode::BAD_GATEWAY, "Paradex subkey mismatch".into()));
    }
    let timestamp = auth["timestamp"].as_u64().ok_or((
        StatusCode::BAD_GATEWAY,
        "Subkey auth timestamp missing".into(),
    ))?;
    let expiration = auth["expiration"]
        .as_u64()
        .ok_or((StatusCode::BAD_GATEWAY, "Subkey auth expiry missing".into()))?;
    let trade_jwt = state
        .paradex
        .authenticate_subkey(
            &quote.account,
            key,
            venue_str(&auth, "signature")?,
            timestamp,
            expiration,
        )
        .await
        .map_err(internal)?;
    // A stable client ID lets us reconcile a timed-out submit without placing another order.
    let existing = state
        .paradex
        .order_history(&trade_jwt, &intent.client_id)
        .await
        .map_err(internal)?;
    if let Some(order) = existing
        .into_iter()
        .find(|order| order["client_id"].as_str() == Some(&intent.client_id))
    {
        return Ok(order);
    }
    let side = if quote.side == "long" || quote.side == "buy" {
        "BUY"
    } else {
        "SELL"
    };
    let order =
        json!({"market":quote.market,"side":side,"type":"MARKET","size":quote.size,"price":"0"});
    let signature = signed_bridge(
        state,
        headers,
        &quote.owner,
        &quote.wallet,
        "order-signature",
        json!({
            "accountAddress":quote.account,"chainId":chain,"order":order
        }),
    )
    .await?;
    if signature["publicKey"].as_str() != Some(key) {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex order signer mismatch".into(),
        ));
    }
    let body = market_order_request(
        &intent.client_id,
        &quote.market,
        side,
        &quote.size,
        venue_str(&signature, "signature")?,
        signature["timestamp"]
            .as_u64()
            .ok_or((StatusCode::BAD_GATEWAY, "Order timestamp missing".into()))?,
        quote.kind == "perp_close",
    );
    match state.paradex.submit_order(&trade_jwt, &body).await {
        Ok(order) => Ok(order),
        Err(engine_execution::perps::ParadexError::Transport(_)) => {
            // The venue may have accepted the order before our HTTP connection failed.
            // Never submit a second order for this confirmed intent.
            let history = state
                .paradex
                .order_history(&trade_jwt, &intent.client_id)
                .await
                .map_err(internal)?;
            history
                .into_iter()
                .find(|order| order["client_id"].as_str() == Some(&intent.client_id))
                .ok_or((
                    StatusCode::ACCEPTED,
                    "Paradex submission outcome is pending reconciliation".into(),
                ))
        }
        Err(error) => Err(internal(error)),
    }
}
// What a finished submission means for the stored intent.
fn record_submission(mut stored: Intent, result: Result<Value, ApiError>) -> Intent {
    stored.status.stage = "settle".into();
    match result {
        Ok(order) => {
            if order["client_id"].as_str() != Some(&stored.client_id)
                || order["market"].as_str() != Some(&stored.quote.market)
            {
                stored.status.state = "failed".into();
                stored.status.error = Some("Paradex order identity mismatch".into());
            } else {
                stored.order_id = order["id"].as_str().map(str::to_owned);
                if let Some(id) = &stored.order_id {
                    stored.status.tx_ids = vec![id.clone()];
                }
            }
        }
        // ACCEPTED means the outcome is unknown: settlement reconciles it by client ID.
        Err(error) if error.0 == StatusCode::ACCEPTED => {}
        Err(error) => {
            stored.status.state = "failed".into();
            stored.status.error = Some(error.1);
        }
    }
    stored
}
pub(crate) async fn signed(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if !body.sent.is_empty() || !body.signed.is_empty() {
        return Err(bad("Paradex confirmation plan has no app transactions"));
    }
    let (current, claimed) = state
        .perps_trade
        .claim_intent(&intent_id, &user.user_id)
        .await?;
    if !claimed {
        return Ok(Json(current.status));
    }
    // Submitting takes a dozen venue and signer calls. Answer once the intent is claimed and let the
    // app poll GET /v1/intents/{id}; the stable client ID keeps the order single however this ends.
    let answer = current.status.clone();
    tokio::spawn(async move {
        let result =
            match tokio::time::timeout(SUBMIT_LIMIT, submit_confirmed(&state, &headers, &current))
                .await
            {
                Ok(result) => result,
                Err(_) => Err((
                    StatusCode::ACCEPTED,
                    "Paradex submission outcome is pending reconciliation".into(),
                )),
            };
        let stored = record_submission(current, result);
        if let Err(error) = state.perps_trade.save_intent(&intent_id, stored).await {
            eprintln!("perps intent {intent_id} could not be saved: {}", error.1);
        }
    });
    Ok(Json(answer))
}
pub(crate) async fn status(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut current = state
        .perps_trade
        .get_intent(&intent_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
    if current.quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    // Never claimed before its plan expired, so it can never be submitted.
    if current.status.stage == "validate" && now() >= current.expires {
        current.status.stage = "settle".into();
        current.status.state = "failed".into();
        current.status.error =
            Some("The confirmation didn't reach Atlas in time; nothing was ordered".into());
        state
            .perps_trade
            .save_intent(&intent_id, current.clone())
            .await?;
        return Ok(Json(current.status));
    }
    if current.status.stage == "execute" && current.status.state == "pending" {
        let jwt = evm_jwt(
            &state,
            &headers,
            &current.quote.owner,
            &current.quote.wallet,
            &current.quote.account,
        )
        .await?;
        let history = state
            .paradex
            .order_history(&jwt, &current.client_id)
            .await
            .map_err(internal)?;
        if let Some(order) = history
            .into_iter()
            .find(|v| v["client_id"].as_str() == Some(&current.client_id))
        {
            if order["market"].as_str() != Some(&current.quote.market)
                || order["size"].as_str().map(units).transpose()?
                    != Some(units(&current.quote.size)?)
            {
                current.status.state = "failed".into();
                current.status.error =
                    Some("Paradex order did not match the confirmed plan".into());
            } else {
                current.status.stage = "settle".into();
                current.order_id = order["id"].as_str().map(str::to_owned);
                if let Some(id) = &current.order_id {
                    current.status.tx_ids = vec![id.clone()];
                }
            }
            state
                .perps_trade
                .save_intent(&intent_id, current.clone())
                .await?;
        } else if now() > current.expires + SUBMIT_GRACE_MS {
            current.status.stage = "settle".into();
            current.status.state = "failed".into();
            current.status.error =
                Some("Paradex has not reported the order; check positions before retrying".into());
            state
                .perps_trade
                .save_intent(&intent_id, current.clone())
                .await?;
        } else {
            return Ok(Json(current.status));
        }
    }
    if current.status.stage != "settle" || current.status.state != "pending" {
        return Ok(Json(current.status));
    }
    let jwt = evm_jwt(
        &state,
        &headers,
        &current.quote.owner,
        &current.quote.wallet,
        &current.quote.account,
    )
    .await?;
    let history = state
        .paradex
        .order_history(&jwt, &current.client_id)
        .await
        .map_err(internal)?;
    let mut next = current.status.clone();
    if let Some(order) = history
        .into_iter()
        .find(|v| v["client_id"].as_str() == Some(&current.client_id))
    {
        if order["market"].as_str() != Some(&current.quote.market)
            || order["size"].as_str().map(units).transpose()? != Some(units(&current.quote.size)?)
        {
            next.state = "failed".into();
            next.error = Some("Paradex order did not match the confirmed plan".into());
        } else if order["status"].as_str() == Some("CLOSED") {
            let order_id = venue_str(&order, "id")?;
            let fills = state
                .paradex
                .fills(
                    &jwt,
                    &current.quote.market,
                    current.expires.saturating_sub(QUOTE_MS + 60_000),
                )
                .await
                .map_err(internal)?;
            let mut executed = 0u128;
            for fill in fills
                .iter()
                .filter(|fill| fill["order_id"].as_str() == Some(order_id))
            {
                executed = executed
                    .checked_add(units(venue_str(fill, "size")?)?)
                    .ok_or((StatusCode::BAD_GATEWAY, "Paradex fill size overflow".into()))?;
            }
            let expected = units(&current.quote.size)?;
            if executed >= expected {
                let positions = state.paradex.positions(&jwt).await.map_err(internal)?;
                let settled = if current.quote.kind == "perp_close" {
                    !positions.iter().any(|p| {
                        p["id"].as_str() == current.quote.position_id.as_deref()
                            && p["status"].as_str() == Some("OPEN")
                    })
                } else {
                    positions.iter().any(|p| {
                        p["market"].as_str() == Some(&current.quote.market)
                            && p["status"].as_str() == Some("OPEN")
                            && p["liquidation_price"]
                                .as_str()
                                .is_some_and(|value| !value.is_empty())
                    })
                };
                if settled {
                    next.state = "filled".into();
                } else if now() > current.expires + 60_000 {
                    next.state = "failed".into();
                    next.error=Some("Paradex filled the order but position settlement is not visible yet; check positions".into());
                }
            } else if executed > 0 || now() > current.expires + 60_000 {
                next.state = "failed".into();
                next.error = Some(if executed > 0 {
                    format!("Paradex partially filled {} of {}; check your position before another order",format_units(executed),current.quote.size)
                } else {
                    format!(
                        "Paradex order closed without a fill: {}",
                        order["cancel_reason"].as_str().unwrap_or("unfilled")
                    )
                });
            }
        }
        if order["status"].as_str() != Some("CLOSED")
            && next.state == "pending"
            && now() > current.expires + SUBMIT_GRACE_MS
        {
            next.state = "failed".into();
            next.error = Some(format!(
                "Paradex still shows the order as {}; check positions before retrying",
                order["status"].as_str().unwrap_or("open")
            ));
        }
        if next.tx_ids.is_empty() {
            if let Some(id) = order["id"].as_str() {
                next.tx_ids.push(id.to_owned());
            }
        }
    } else if now() > current.expires + SUBMIT_GRACE_MS {
        next.state = "failed".into();
        next.error = Some(
            "Paradex did not report the submitted order; check positions before retrying".into(),
        );
    }
    let mut updated = current;
    updated.status = next.clone();
    state.perps_trade.save_intent(&intent_id, updated).await?;
    Ok(Json(next))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample_intent() -> Intent {
        Intent {
            quote: Quote {
                owner: "did:privy:alice".into(),
                wallet: "0x0000000000000000000000000000000000000001".into(),
                account: "0x1".into(),
                market: "BTC-USD-PERP".into(),
                side: "long".into(),
                size: "0.001".into(),
                price: "83000".into(),
                leverage: 5,
                expires: 0,
                kind: "perp_open".into(),
                position_id: None,
                currency: "USD".into(),
                margin: "20".into(),
            },
            client_id: "perp-open-1".into(),
            order_id: None,
            status: markets::IntentStatus {
                intent_id: "perp-open-1".into(),
                stage: "execute".into(),
                state: "pending".into(),
                tx_ids: vec![],
                error: None,
            },
            expires: 0,
        }
    }
    #[test]
    fn submission_results_map_to_settlement_states() {
        let placed = record_submission(
            sample_intent(),
            Ok(json!({"client_id":"perp-open-1","market":"BTC-USD-PERP","id":"ord-9"})),
        );
        assert_eq!(
            (placed.status.stage.as_str(), placed.status.state.as_str()),
            ("settle", "pending")
        );
        assert_eq!(placed.status.tx_ids, vec!["ord-9".to_string()]);

        let unknown = record_submission(
            sample_intent(),
            Err((StatusCode::ACCEPTED, "pending reconciliation".into())),
        );
        assert_eq!(
            (unknown.status.stage.as_str(), unknown.status.state.as_str()),
            ("settle", "pending")
        );
        assert!(unknown.status.error.is_none());

        let rejected = record_submission(
            sample_intent(),
            Err((StatusCode::BAD_GATEWAY, "Paradex rejected the order".into())),
        );
        assert_eq!(rejected.status.state, "failed");
        assert_eq!(
            rejected.status.error.as_deref(),
            Some("Paradex rejected the order")
        );

        let wrong = record_submission(
            sample_intent(),
            Ok(json!({"client_id":"someone-else","market":"BTC-USD-PERP","id":"ord-1"})),
        );
        assert_eq!(wrong.status.state, "failed");
    }
    #[test]
    fn market_order_omits_price_for_open_and_close() {
        let open = market_order_request(
            "open-1",
            "BTC-USD-PERP",
            "BUY",
            "0.001",
            "[1,2]",
            123,
            false,
        );
        assert!(open.get("price").is_none());
        assert_eq!(open["type"], "MARKET");
        assert_eq!(open["flags"], json!([]));
        let close = market_order_request(
            "close-1",
            "BTC-USD-PERP",
            "SELL",
            "0.001",
            "[1,2]",
            124,
            true,
        );
        assert!(close.get("price").is_none());
        assert_eq!(close["flags"], json!(["REDUCE_ONLY"]));
    }
    #[test]
    fn unchanged_resting_book_is_quoted_only_at_sensible_venue_price() {
        let old_timestamp = now().saturating_sub(90 * 60 * 1_000);
        let bbo = json!({
            "market":"BTC-USD-PERP","last_updated_at":old_timestamp,
            "ask":"83000","ask_size":"0.00241","bid":"39593.8","bid_size":"0.00016"
        });
        let summary = json!({"mark_price":"83650.43441918"});
        assert_eq!(
            fresh_bbo(&bbo, &summary, "BTC-USD-PERP", "long").unwrap(),
            (units("83000").unwrap(), units("0.00241").unwrap())
        );
        assert!(fresh_bbo(&bbo, &summary, "BTC-USD-PERP", "short").is_err());
        assert!(fresh_bbo(
            &json!({"market":"ETH-USD-PERP","last_updated_at":old_timestamp,
                "ask":"0","ask_size":"0","bid":"777","bid_size":"12"}),
            &json!({"mark_price":"2100"}),
            "ETH-USD-PERP",
            "long"
        )
        .is_err());
    }
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
