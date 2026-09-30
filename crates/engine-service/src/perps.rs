//! Paradex-backed app perps reads. No paper orders or estimated liquidation prices.
use super::*;
pub(super) mod trade;
use axum::extract::Query;
use serde_json::{json, Value};
use std::{str::FromStr, time::Duration};
pub(super) use trade::TradeState;

const MARKET_IDS: [(&str, &str, &str); 3] = [
    ("BTC-USD-PERP", "BTC", "Bitcoin Perpetual"),
    ("ETH-USD-PERP", "ETH", "Ethereum Perpetual"),
    ("SOL-USD-PERP", "SOL", "Solana Perpetual"),
];
const SCALE: u128 = 1_000_000_000_000;

#[derive(Deserialize)]
pub(super) struct CurrencyQuery {
    currency: Option<String>,
}

// Trading and onboarding run on whichever Paradex network PARADEX_ENV names. It must be set
// explicitly: market reads default to prod, but nothing places or registers anything by default.
pub(super) fn trading_env() -> Result<&'static str, ApiError> {
    match env::var("PARADEX_ENV").as_deref() {
        Ok("testnet") => Ok("testnet"),
        Ok("prod") => Ok("prod"),
        _ => Err(unavailable(
            "Paradex trading needs PARADEX_ENV set to testnet or prod",
        )),
    }
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn unavailable(message: &str) -> ApiError {
    (StatusCode::SERVICE_UNAVAILABLE, message.into())
}
fn checked_currency(query: CurrencyQuery) -> Result<String, ApiError> {
    let currency = query.currency.unwrap_or_else(|| "USD".into());
    if matches!(currency.as_str(), "USD" | "NGN" | "KES" | "GHS" | "ZAR") {
        Ok(currency)
    } else {
        Err(bad("unsupported display currency"))
    }
}
fn venue_str<'a>(data: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    data[field]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or((StatusCode::BAD_GATEWAY, format!("Paradex omitted {field}")))
}
fn decimal_units(raw: &str) -> Result<u128, ApiError> {
    let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty()
        || frac.len() > 12
        || !whole.bytes().all(|v| v.is_ascii_digit())
        || !frac.bytes().all(|v| v.is_ascii_digit())
    {
        return Err((StatusCode::BAD_GATEWAY, "invalid Paradex decimal".into()));
    }
    let whole: u128 = whole.parse().map_err(internal)?;
    let frac: u128 = format!("{:0<12}", frac).parse().map_err(internal)?;
    whole
        .checked_mul(SCALE)
        .and_then(|v| v.checked_add(frac))
        .ok_or((StatusCode::BAD_GATEWAY, "Paradex decimal overflow".into()))
}
fn format_units(units: u128) -> String {
    let mut text = format!("{}.{:012}", units / SCALE, units % SCALE);
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}
fn percent(raw: &str) -> Result<String, ApiError> {
    let (negative, digits) = raw.strip_prefix('-').map_or((false, raw), |v| (true, v));
    let value = decimal_units(digits)?.checked_mul(100).ok_or((
        StatusCode::BAD_GATEWAY,
        "Paradex percentage overflow".into(),
    ))?;
    Ok(format!(
        "{}{}",
        if negative { "-" } else { "" },
        format_units(value)
    ))
}
fn display_money(usd_raw: &str, currency: &str, fx_micros: u128) -> Result<Value, ApiError> {
    let value = decimal_units(usd_raw)?
        .checked_mul(fx_micros)
        .ok_or((StatusCode::BAD_GATEWAY, "price conversion overflow".into()))?
        / 1_000_000;
    Ok(json!({"amount":format_units(value),"currency":currency}))
}
fn max_leverage(imf: &str) -> Result<u64, ApiError> {
    let fraction = decimal_units(imf)?;
    if fraction == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex returned zero margin fraction".into(),
        ));
    }
    (SCALE / fraction).try_into().map_err(internal)
}

pub(super) async fn onboarding(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let wallet = user.evm_wallet.filter(|wallet| !wallet.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let status = state
        .paradex
        .onboarding_status(&wallet)
        .await
        .map_err(internal)?;
    let signer_status = match &state.auth {
        AuthMode::Privy { bridge_url, http } => {
            let token = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or((
                    StatusCode::UNAUTHORIZED,
                    "Privy access token required".into(),
                ))?;
            let response = http
                .post(format!("{bridge_url}/paradex/signer-status"))
                .json(&json!({"accessToken":token,"walletAddress":wallet}))
                .send()
                .await
                .map_err(|_| unavailable("Privy signer check unavailable"))?;
            if !response.status().is_success() {
                return Err(unavailable("Privy signer check unavailable"));
            }
            let signer_status: Value = response.json().await.map_err(internal)?;
            if signer_status["userId"].as_str() != Some(&user.user_id)
                || signer_status["walletAddress"]
                    .as_str()
                    .is_none_or(|address| !address.eq_ignore_ascii_case(&wallet))
                || !signer_status["signerAuthorized"].is_boolean()
                || !(signer_status["signer"].is_null()
                    || (signer_status["signer"]["signerId"].is_string()
                        && signer_status["signer"]["policyIds"].is_array()))
            {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    "Privy signer status did not match the verified wallet".into(),
                ));
            }
            signer_status
        }
        AuthMode::LocalDemo => json!({"signer":null,"signerAuthorized":false}),
    };
    Ok(Json(json!({
        "walletAddress": status.wallet_address,
        "accountAddress": status.account_address,
        "onboarded": status.exists,
        "signer": signer_status["signer"],
        "signerAuthorized": signer_status["signerAuthorized"],
    })))
}
pub(super) async fn onboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    trading_env()?;
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let wallet = user.evm_wallet.filter(|wallet| !wallet.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let current = state
        .paradex
        .onboarding_status(&wallet)
        .await
        .map_err(internal)?;
    if current.exists {
        return Ok(Json(json!({
            "walletAddress": current.wallet_address,
            "accountAddress": current.account_address,
            "onboarded": true,
        })));
    }
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err(unavailable(
            "Privy server signing is unavailable in demo mode",
        ));
    };
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    let signed_response = http
        .post(format!("{bridge_url}/paradex/onboarding-signature"))
        .json(&json!({"accessToken": token, "walletAddress": wallet}))
        .send()
        .await
        .map_err(|_| unavailable("Privy signing bridge is unavailable"))?;
    if signed_response.status() == reqwest::StatusCode::CONFLICT {
        return Err((
            StatusCode::CONFLICT,
            "Approve Atlas perps access for this wallet in the app".into(),
        ));
    }
    if !signed_response.status().is_success() {
        return Err(unavailable("Privy could not authorize Paradex onboarding"));
    }
    let proof: Value = signed_response.json().await.map_err(internal)?;
    if proof["userId"].as_str() != Some(&user.user_id)
        || proof["walletAddress"]
            .as_str()
            .is_none_or(|address| !address.eq_ignore_ascii_case(&wallet))
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Privy signing response did not match the verified wallet".into(),
        ));
    }
    let signature = venue_str(&proof, "signature")?;
    let message = venue_str(&proof, "siweMessageBase64")?;
    let public_key = venue_str(&proof, "publicKey")?;
    state
        .paradex
        .onboard_evm(&current.account_address, signature, message, public_key)
        .await
        .map_err(internal)?;
    let confirmed = state
        .paradex
        .onboarding_status(&wallet)
        .await
        .map_err(internal)?;
    if !confirmed.exists || confirmed.account_address != current.account_address {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Paradex has not confirmed the new account yet".into(),
        ));
    }
    Ok(Json(json!({
        "walletAddress": confirmed.wallet_address,
        "accountAddress": confirmed.account_address,
        "onboarded": true,
    })))
}
pub(super) async fn markets(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    let currency = checked_currency(q)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let mut result = Vec::with_capacity(MARKET_IDS.len());
    for (market_id, symbol, name) in MARKET_IDS {
        let (metadata, summary, funding) = tokio::join!(
            state.paradex.market(market_id),
            state.paradex.summary(market_id),
            state.paradex.funding(market_id),
        );
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(engine_execution::perps::ParadexError::Rejected(
                reqwest::StatusCode::NOT_FOUND,
            )) => {
                continue;
            }
            Err(error) => return Err(internal(error)),
        };
        let summary = summary.map_err(internal)?;
        let funding = funding.map_err(internal)?;
        if venue_str(&metadata, "symbol")? != market_id
            || venue_str(&metadata, "asset_kind")? != "PERP"
            || venue_str(&summary, "symbol")? != market_id
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Paradex market identity mismatch".into(),
            ));
        }
        let mark = venue_str(&summary, "mark_price")?;
        let imf = venue_str(&metadata["delta1_cross_margin_params"], "imf_base")?;
        let change = summary["price_change_rate_24h"]
            .as_str()
            .map(percent)
            .transpose()?;
        let funding_pct = funding
            .as_ref()
            .and_then(|v| v["funding_rate_8h"].as_str())
            .map(percent)
            .transpose()?;
        result.push(json!({"marketId":market_id,"symbol":symbol,"name":name,"markPrice":display_money(mark,&currency,rate)?,"change24hPct":change,"maxLeverage":max_leverage(imf)?,"fundingRate8hPct":funding_pct}));
    }
    Ok(Json(json!({"markets":result})))
}

async fn evm_jwt(
    state: &AppState,
    headers: &HeaderMap,
    user_id: &str,
    wallet: &str,
    account_address: &str,
) -> Result<String, ApiError> {
    let cache_key = format!("{}:{}", user_id, wallet.to_ascii_lowercase());
    let cached = state
        .paradex_tokens
        .lock()
        .map_err(internal)?
        .get(&cache_key)
        .filter(|(_, until)| *until > Instant::now())
        .map(|(token, _)| token.clone());
    let jwt = if let Some(token) = cached {
        token
    } else {
        let AuthMode::Privy { bridge_url, http } = &state.auth else {
            return Err(unavailable(
                "Privy server signing is unavailable in demo mode",
            ));
        };
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or((
                StatusCode::UNAUTHORIZED,
                "Privy access token required".into(),
            ))?;
        let signed_response = http
            .post(format!("{bridge_url}/paradex/auth-signature"))
            .json(&json!({"accessToken": token, "walletAddress": wallet}))
            .send()
            .await
            .map_err(|_| unavailable("Privy signing bridge is unavailable"))?;
        if signed_response.status() == reqwest::StatusCode::CONFLICT {
            return Err((
                StatusCode::CONFLICT,
                "Approve Atlas perps access for this wallet in the app".into(),
            ));
        }
        if !signed_response.status().is_success() {
            return Err(unavailable(
                "Privy could not authorize Paradex account reads",
            ));
        }
        let proof: Value = signed_response.json().await.map_err(internal)?;
        if proof["userId"].as_str() != Some(&user_id)
            || proof["walletAddress"]
                .as_str()
                .is_none_or(|address| !address.eq_ignore_ascii_case(&wallet))
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Privy signing response did not match the verified wallet".into(),
            ));
        }
        let jwt = state
            .paradex
            .authenticate_evm(
                &account_address,
                venue_str(&proof, "signature")?,
                venue_str(&proof, "siweMessageBase64")?,
            )
            .await
            .map_err(internal)?;
        state.paradex_tokens.lock().map_err(internal)?.insert(
            cache_key,
            (jwt.clone(), Instant::now() + Duration::from_secs(30)),
        );
        jwt
    };
    Ok(jwt)
}

pub(super) async fn positions(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    checked_currency(q)?;
    let wallet = user.evm_wallet.filter(|wallet| !wallet.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let current = state
        .paradex
        .onboarding_status(&wallet)
        .await
        .map_err(internal)?;
    if !current.exists {
        return Ok(Json(json!({"positions": []})));
    }
    let jwt = evm_jwt(
        &state,
        &headers,
        &user.user_id,
        &wallet,
        &current.account_address,
    )
    .await?;
    let venue_positions = state.paradex.positions(&jwt).await.map_err(internal)?;
    let mut result = Vec::new();
    for position in venue_positions {
        if position["status"].as_str() != Some("OPEN") {
            continue;
        }
        let market_id = venue_str(&position, "market")?;
        let symbol = market_id
            .split('-')
            .next()
            .ok_or((StatusCode::BAD_GATEWAY, "invalid Paradex market".into()))?;
        let side = match venue_str(&position, "side")? {
            "LONG" => "long",
            "SHORT" => "short",
            _ => return Err((StatusCode::BAD_GATEWAY, "invalid Paradex side".into())),
        };
        let leverage = Value::Number(
            serde_json::Number::from_str(venue_str(&position, "leverage")?).map_err(internal)?,
        );
        let summary = state.paradex.summary(market_id).await.map_err(internal)?;
        let mark = venue_str(&summary, "mark_price")?;
        let size = venue_str(&position, "size")?.trim_start_matches('-');
        // These prices stay in venue USD so liquidationPrice.amount is byte-for-byte
        // the current Paradex liquidation_price field; the app must not recompute it.
        result.push(json!({
            "positionId":venue_str(&position,"id")?,
            "openedAtUnixMs":position["created_at"].as_u64().filter(|value| *value > 0).ok_or((StatusCode::BAD_GATEWAY, "Paradex omitted created_at".into()))?,
            "marketId":market_id,
            "symbol":symbol,
            "side":side,
            "leverage":leverage,
            "size":size,
            "entryPrice":{"amount":venue_str(&position,"average_entry_price")?,"currency":"USD"},
            "markPrice":{"amount":mark,"currency":"USD"},
            "liquidationPrice":{"amount":venue_str(&position,"liquidation_price")?,"currency":"USD"},
            "margin":null,
            "unrealizedPnl":{"amount":venue_str(&position,"unrealized_pnl")?,"currency":"USD"},
            "unrealizedPnlPct":null
        }));
    }
    Ok(Json(json!({"positions":result})))
}

pub(super) use trade::{close_quote, execute_close, execute_quote, quotes};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn market_rate_conversion_preserves_precision() {
        assert_eq!(percent("0.01233").unwrap(), "1.233");
        assert_eq!(max_leverage("0.02").unwrap(), 50);
        assert_eq!(
            display_money("84029.57255803", "NGN", 1_500_000_000).unwrap()["amount"],
            "126044358.837045"
        );
    }
}
