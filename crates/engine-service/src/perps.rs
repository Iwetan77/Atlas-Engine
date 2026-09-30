//! Paradex-backed app perps reads. No paper orders or estimated liquidation prices.
use super::*;
pub(super) mod trade;
use axum::extract::Query;
use serde_json::{json, Value};
use std::{str::FromStr, time::Duration};
pub(super) use trade::TradeState;

const SCALE: u128 = 1_000_000_000_000;

// Every perp Paradex lists is offered. Names for the ones we can name with confidence; anything else
// keeps its ticker. Categories here override Paradex's tags, which lump stocks, commodities and
// indices together as RWA (and call PAXG, tokenized gold, DEFI).
const KNOWN_MARKETS: &[(&str, &str, &str)] = &[
    ("BTC", "Bitcoin", "crypto"),
    ("ETH", "Ethereum", "crypto"),
    ("SOL", "Solana", "crypto"),
    ("HYPE", "Hyperliquid", "crypto"),
    ("BNB", "BNB", "crypto"),
    ("SUI", "Sui", "crypto"),
    ("XRP", "XRP", "crypto"),
    ("AVAX", "Avalanche", "crypto"),
    ("LINK", "Chainlink", "crypto"),
    ("NEAR", "NEAR", "crypto"),
    ("TAO", "Bittensor", "crypto"),
    ("ADA", "Cardano", "crypto"),
    ("LTC", "Litecoin", "crypto"),
    ("UNI", "Uniswap", "crypto"),
    ("AAVE", "Aave", "crypto"),
    ("ENA", "Ethena", "crypto"),
    ("ETHFI", "ether.fi", "crypto"),
    ("JTO", "Jito", "crypto"),
    ("JUP", "Jupiter", "crypto"),
    ("KAITO", "Kaito", "crypto"),
    ("LDO", "Lido DAO", "crypto"),
    ("MORPHO", "Morpho", "crypto"),
    ("ONDO", "Ondo", "crypto"),
    ("PENDLE", "Pendle", "crypto"),
    ("PYTH", "Pyth Network", "crypto"),
    ("STRK", "Starknet", "crypto"),
    ("TRX", "TRON", "crypto"),
    ("XMR", "Monero", "crypto"),
    ("ZEC", "Zcash", "crypto"),
    ("MON", "Monad", "crypto"),
    ("XPL", "Plasma", "crypto"),
    ("WLFI", "World Liberty Financial", "crypto"),
    ("DOGE", "Dogecoin", "meme"),
    ("PUMP", "Pump.fun", "meme"),
    ("TRUMP", "Official Trump", "meme"),
    ("kPEPE", "Pepe (per 1,000)", "meme"),
    ("kSHIB", "Shiba Inu (per 1,000)", "meme"),
    ("GOOGL", "Alphabet", "stock"),
    ("META", "Meta Platforms", "stock"),
    ("MSFT", "Microsoft", "stock"),
    ("MSTR", "Strategy", "stock"),
    ("INTC", "Intel", "stock"),
    ("CRCL", "Circle", "stock"),
    ("MU", "Micron", "stock"),
    ("MRVL", "Marvell", "stock"),
    ("SNDK", "Sandisk", "stock"),
    ("EWY", "iShares MSCI South Korea ETF", "stock"),
    ("XAU", "Gold", "commodity"),
    ("PAXG", "PAX Gold", "commodity"),
    ("XAG", "Silver", "commodity"),
    ("XPT", "Platinum", "commodity"),
    ("XCU", "Copper", "commodity"),
    ("CL", "Crude oil (WTI)", "commodity"),
    ("BZ", "Brent crude", "commodity"),
    ("NG", "Natural gas", "commodity"),
    ("US100", "Nasdaq 100", "index"),
    ("US500", "S&P 500", "index"),
];

fn classify(symbol: &str, tags: &[&str]) -> (String, &'static str) {
    if let Some((_, name, category)) = KNOWN_MARKETS.iter().find(|(s, _, _)| *s == symbol) {
        return ((*name).into(), category);
    }
    let category = if tags.contains(&"RWA") {
        "stock"
    } else if tags.contains(&"MEME") {
        "meme"
    } else {
        "crypto"
    };
    (symbol.into(), category)
}

// Public logo CDNs: CoinCap by ticker for tokens, FMP for stock tickers. Commodities and indices
// have no logo; the app falls back to initials (and does the same if a URL fails to load).
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
        _ if symbol == "PAXG" => Some("https://assets.coincap.io/assets/icons/paxg@2x.png".into()),
        _ => None,
    }
}

#[derive(Clone)]
pub(super) struct PerpMeta {
    market_id: String,
    symbol: String,
    name: String,
    category: &'static str,
    max_leverage: u64,
}

// The catalog barely changes; prices are shared by every viewer for a few seconds.
const CATALOG_TTL: Duration = Duration::from_secs(60 * 60);
const PRICES_TTL: Duration = Duration::from_secs(15);
const PRICE_FETCHES_AT_ONCE: usize = 16;

type Cached<T> = Arc<Mutex<Option<(Instant, Arc<T>)>>>;

#[derive(Clone, Default)]
pub(super) struct PerpCache {
    catalog: Cached<Vec<PerpMeta>>,
    prices: Cached<HashMap<String, Value>>,
}

fn fresh<T>(slot: &Cached<T>, ttl: Duration) -> Result<Option<Arc<T>>, ApiError> {
    Ok(slot
        .lock()
        .map_err(internal)?
        .as_ref()
        .filter(|(at, _)| at.elapsed() < ttl)
        .map(|(_, value)| value.clone()))
}

async fn perp_catalog(state: &AppState) -> Result<Arc<Vec<PerpMeta>>, ApiError> {
    if let Some(catalog) = fresh(&state.perp_cache.catalog, CATALOG_TTL)? {
        return Ok(catalog);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut catalog = Vec::new();
    for market in state.paradex.perp_markets().await.map_err(internal)? {
        let (Some(market_id), Some(symbol)) =
            (market["symbol"].as_str(), market["base_currency"].as_str())
        else {
            continue;
        };
        // Not open yet or no longer trading normally: nothing to offer.
        if market["open_at"].as_u64().unwrap_or(0) > now_ms
            || market["trading_mode"].as_str() != Some("STANDARD")
        {
            continue;
        }
        let Some(max_leverage) = market["delta1_cross_margin_params"]["imf_base"]
            .as_str()
            .and_then(|imf| max_leverage(imf).ok())
        else {
            continue;
        };
        let tags: Vec<&str> = market["tags"]
            .as_array()
            .map(|tags| tags.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let (name, category) = classify(symbol, &tags);
        catalog.push(PerpMeta {
            market_id: market_id.into(),
            symbol: symbol.into(),
            name,
            category,
            max_leverage,
        });
    }
    if catalog.is_empty() {
        return Err(unavailable("Paradex listed no perpetual markets"));
    }
    let catalog = Arc::new(catalog);
    *state.perp_cache.catalog.lock().map_err(internal)? = Some((Instant::now(), catalog.clone()));
    Ok(catalog)
}

// One small summary request per market, a few at a time: Paradex's all-markets summary is ~3 MB
// of options data we'd throw away.
async fn perp_prices(
    state: &AppState,
    catalog: &[PerpMeta],
) -> Result<Arc<HashMap<String, Value>>, ApiError> {
    if let Some(prices) = fresh(&state.perp_cache.prices, PRICES_TTL)? {
        return Ok(prices);
    }
    let limit = Arc::new(tokio::sync::Semaphore::new(PRICE_FETCHES_AT_ONCE));
    let mut tasks = tokio::task::JoinSet::new();
    for meta in catalog {
        let (paradex, limit, market_id) =
            (state.paradex.clone(), limit.clone(), meta.market_id.clone());
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await.ok()?;
            let summary = paradex.summary(&market_id).await.ok()?;
            Some((market_id, summary))
        });
    }
    let mut prices = HashMap::new();
    while let Some(done) = tasks.join_next().await {
        if let Ok(Some((market_id, summary))) = done {
            prices.insert(market_id, summary);
        }
    }
    if prices.is_empty() {
        return Err(unavailable("Paradex market prices are unavailable"));
    }
    let prices = Arc::new(prices);
    *state.perp_cache.prices.lock().map_err(internal)? = Some((Instant::now(), prices.clone()));
    Ok(prices)
}

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
    let (rate, catalog) = tokio::try_join!(app_balance::fx_rate(&currency), perp_catalog(&state))?;
    let prices = perp_prices(&state, &catalog).await?;
    let mut rows = Vec::with_capacity(catalog.len());
    for meta in catalog.iter() {
        // A market whose price didn't come back this round is left out rather than failing the list.
        let Some(summary) = prices.get(&meta.market_id) else {
            continue;
        };
        if summary["symbol"].as_str() != Some(meta.market_id.as_str()) {
            continue;
        }
        let Some(mark) = summary["mark_price"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        let change = summary["price_change_rate_24h"]
            .as_str()
            .map(percent)
            .transpose()?;
        // Every Paradex perp funds every 8 hours, so the current rate is the 8h rate.
        let funding_pct = summary["funding_rate"].as_str().map(percent).transpose()?;
        let volume = summary["volume_24h"].as_str().unwrap_or("0");
        rows.push((
            volume.parse::<f64>().unwrap_or(0.0),
            json!({
                "marketId":meta.market_id,"symbol":meta.symbol,"name":meta.name,
                "category":meta.category,"iconUrl":icon_url(&meta.symbol, meta.category),
                "markPrice":display_money(mark,&currency,rate)?,"change24hPct":change,
                "maxLeverage":meta.max_leverage,"fundingRate8hPct":funding_pct,
                "volume24hUsd":volume
            }),
        ));
    }
    // Most traded first: those are the markets that can actually fill an order.
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(Json(
        json!({"markets":rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>()}),
    ))
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
    let catalog = perp_catalog(&state).await.ok();
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
            "iconUrl":catalog.as_ref().and_then(|c| c.iter().find(|m| m.market_id == market_id)).and_then(|m| icon_url(&m.symbol, m.category)),
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
    fn markets_get_names_categories_and_icons() {
        assert_eq!(classify("XAU", &["RWA"]), ("Gold".into(), "commodity"));
        assert_eq!(classify("US500", &["RWA"]), ("S&P 500".into(), "index"));
        assert_eq!(classify("PAXG", &["DEFI"]).1, "commodity");
        assert_eq!(classify("NEWCO", &["RWA"]), ("NEWCO".into(), "stock"));
        assert_eq!(classify("FROG", &["MEME"]).1, "meme");
        assert_eq!(classify("NEWL1", &["LAYER-1"]).1, "crypto");
        assert_eq!(
            icon_url("kPEPE", "meme").as_deref(),
            Some("https://assets.coincap.io/assets/icons/pepe@2x.png")
        );
        assert_eq!(
            icon_url("GOOGL", "stock").as_deref(),
            Some("https://financialmodelingprep.com/image-stock/GOOGL.png")
        );
        assert_eq!(icon_url("XAU", "commodity"), None);
        assert_eq!(icon_url("US100", "index"), None);
    }
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
