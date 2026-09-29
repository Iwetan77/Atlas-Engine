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
async fn submit_confirmed(
    state: &AppState,
    headers: &HeaderMap,
    intent: &Intent,
) -> Result<Value, ApiError> {
    let quote = &intent.quote;
    let evm = evm_jwt(state, headers, &quote.owner, &quote.wallet, &quote.account).await?;
    let config = state.paradex.system_config().await.map_err(internal)?;
    let chain = venue_str(&config, "starknet_chain_id")?;
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
    let body = json!({"client_id":intent.client_id,"instruction":"IOC","market":quote.market,
        "price":"0","side":side,"size":quote.size,"type":"MARKET",
        "signature":venue_str(&signature,"signature")?,
        "signature_timestamp":signature["timestamp"].as_u64().ok_or((StatusCode::BAD_GATEWAY,"Order timestamp missing".into()))?,
        "flags":if quote.kind=="perp_close" {vec!["REDUCE_ONLY"]} else {vec![]}
    });
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
    let current = {
        let mut intents = state.perps_trade.intents.lock().map_err(internal)?;
        let intent = intents
            .get_mut(&intent_id)
            .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
        if intent.quote.owner != user.user_id {
            return Err((
                StatusCode::FORBIDDEN,
                "intent belongs to another user".into(),
            ));
        }
        if intent.status.stage != "validate" {
            return Ok(Json(intent.status.clone()));
        }
        if now() >= intent.expires {
            return Err((StatusCode::GONE, "perps execution plan expired".into()));
        }
        intent.status.stage = "execute";
        intent.clone()
    };
    let result = submit_confirmed(&state, &headers, &current).await;
    let mut intents = state.perps_trade.intents.lock().map_err(internal)?;
    let stored = intents
        .get_mut(&intent_id)
        .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
    match result {
        Ok(order) => {
            if order["client_id"].as_str() != Some(&stored.client_id)
                || order["market"].as_str() != Some(&stored.quote.market)
            {
                stored.status.stage = "settle";
                stored.status.state = "failed";
                stored.status.error = Some("Paradex order identity mismatch".into());
            } else {
                stored.order_id = order["id"].as_str().map(str::to_owned);
                if let Some(id) = &stored.order_id {
                    stored.status.tx_ids = vec![id.clone()];
                }
                stored.status.stage = "settle";
            }
        }
        Err(error) => {
            stored.status.stage = "settle";
            if error.0 != StatusCode::ACCEPTED {
                stored.status.state = "failed";
                stored.status.error = Some(error.1);
            }
        }
    }
    Ok(Json(stored.status.clone()))
}
pub(crate) async fn status(
    state: AppState,
    intent_id: String,
    headers: HeaderMap,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
        .perps_trade
        .intents
        .lock()
        .map_err(internal)?
        .get(&intent_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?;
    if current.quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
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
            next.state = "failed";
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
                next.state = "filled";
            } else if executed > 0 || now() > current.expires + 60_000 {
                next.state = "failed";
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
        if next.tx_ids.is_empty() {
            if let Some(id) = order["id"].as_str() {
                next.tx_ids.push(id.to_owned());
            }
        }
    } else if now() > current.expires + 60_000 {
        next.state = "failed";
        next.error = Some("Paradex did not report the submitted order within two minutes".into());
    }
    state
        .perps_trade
        .intents
        .lock()
        .map_err(internal)?
        .get_mut(&intent_id)
        .ok_or((StatusCode::NOT_FOUND, "perps intent not found".into()))?
        .status = next.clone();
    Ok(Json(next))
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
