//! Paradex-backed app perps reads. No paper orders or estimated liquidation prices.
use super::*;
use axum::extract::Query;
use serde_json::{json, Value};
use std::str::FromStr;

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
        let metadata = metadata.map_err(internal)?;
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

pub(super) async fn positions(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    checked_currency(q)?;
    let bound_did = env::var("PARADEX_ACCOUNT_USER_ID")
        .map_err(|_| unavailable("Paradex account is not linked to this Atlas user"))?;
    let bound_wallet = env::var("PARADEX_ACCOUNT_EVM_WALLET")
        .map_err(|_| unavailable("Paradex account wallet binding is not configured"))?;
    let jwt = env::var("PARADEX_READONLY_TOKEN")
        .map_err(|_| unavailable("Paradex account read token is not configured"))?;
    if user.user_id != bound_did
        || user
            .evm_wallet
            .as_deref()
            .is_none_or(|wallet| !wallet.eq_ignore_ascii_case(&bound_wallet))
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Paradex account belongs to another user".into(),
        ));
    }
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

pub(super) async fn quotes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable(
        "Paradex trade-only signer and venue-sourced pretrade risk quote are not configured",
    ))
}
pub(super) async fn execute_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable(
        "Paradex trade-only order signer is not configured",
    ))
}
pub(super) async fn close_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable(
        "Paradex trade-only order signer is not configured",
    ))
}
pub(super) async fn execute_close(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    Err(unavailable(
        "Paradex trade-only order signer is not configured",
    ))
}

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
