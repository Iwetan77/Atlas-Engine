//! Earn: the user's USDC on Base supplied to Aave v3, earning Aave's variable rate. Plain Base
//! transactions, planned here and settled through the same Base intent flow as trades.
use super::*;
use axum::extract::{Path, Query};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

const AAVE_POOL: &str = "0xa238dd80c259a72e81d7e4664a9801593f98d1c5";
// aBasUSDC: Aave's receipt for supplied USDC. Its balance grows as interest accrues.
const AAVE_USDC: &str = "0x4e65fe4dba92790696d040ac24aa414708f5c0ab";
const OPTION_ID: &str = "aave-usdc-base";
const QUOTE_MS: u64 = 60_000;
const APY_TTL: Duration = Duration::from_secs(300);
const SECONDS_PER_YEAR: f64 = 31_536_000.0;

#[derive(Clone)]
struct EarnQuote {
    owner: String,
    wallet: String,
    deposit: bool,
    units: u128,
    // Withdraw everything, principal and interest, rather than a fixed amount.
    all: bool,
    expires: u64,
}

#[derive(Clone, Default)]
pub(super) struct EarnState {
    quotes: Arc<Mutex<HashMap<String, EarnQuote>>>,
    apy: Arc<Mutex<Option<(Instant, f64)>>>,
}

#[derive(Deserialize)]
pub(super) struct CurrencyQuery {
    currency: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct Amount {
    amount: String,
    currency: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuoteRequest {
    option_id: String,
    action: String,
    amount: Amount,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn currency_of(query: Option<String>) -> Result<String, ApiError> {
    let currency = query.unwrap_or_else(|| "NGN".into());
    if DISPLAY_CURRENCIES.contains(&currency.as_str()) {
        Ok(currency)
    } else {
        Err(bad("unsupported display currency"))
    }
}
fn word_address(address: &str) -> String {
    format!(
        "{:0>64}",
        address.trim_start_matches("0x").to_ascii_lowercase()
    )
}
fn word_units(units: u128) -> String {
    format!("{units:064x}")
}
fn usdc(units: u128) -> String {
    format!("{}.{:06}", units / 1_000_000, units % 1_000_000)
}

async fn eth_call(state: &AppState, to: &str, data: &str) -> Result<String, ApiError> {
    let result = markets::base_rpc(
        &state.markets,
        "eth_call",
        json!([{"to":to,"data":data}, "latest"]),
    )
    .await?;
    result
        .as_str()
        .map(str::to_owned)
        .ok_or((StatusCode::BAD_GATEWAY, "Base call returned nothing".into()))
}

// Aave's current USDC supply rate, as a yearly percentage yield (it varies with demand).
async fn usdc_apy(state: &AppState) -> Result<f64, ApiError> {
    if let Some((at, apy)) = *state.earn.apy.lock().map_err(internal)? {
        if at.elapsed() < APY_TTL {
            return Ok(apy);
        }
    }
    // getReserveData(USDC): the third word is currentLiquidityRate, in ray (1e27) per year.
    let data = format!(
        "0x35ea6a75{}",
        word_address(engine_execution::swaps::uniswap::BASE_USDC)
    );
    let raw = eth_call(state, AAVE_POOL, &data).await?;
    let apy = supply_apy(&raw).ok_or((StatusCode::BAD_GATEWAY, "Aave rate unreadable".into()))?;
    *state.earn.apy.lock().map_err(internal)? = Some((Instant::now(), apy));
    Ok(apy)
}

fn supply_apy(reserve_data: &str) -> Option<f64> {
    let hex = reserve_data.strip_prefix("0x")?;
    let word = hex.get(128..192)?;
    let digits = word.trim_start_matches('0');
    let rate = u128::from_str_radix(if digits.is_empty() { "0" } else { digits }, 16).ok()?;
    let per_second = rate as f64 / 1e27 / SECONDS_PER_YEAR;
    Some(((1.0 + per_second).powf(SECONDS_PER_YEAR) - 1.0) * 100.0)
}

async fn usdc_allowance(state: &AppState, owner: &str) -> Result<u128, ApiError> {
    let data = format!(
        "0xdd62ed3e{}{}",
        word_address(owner),
        word_address(AAVE_POOL)
    );
    let raw = eth_call(state, engine_execution::swaps::uniswap::BASE_USDC, &data).await?;
    let hex = raw.trim_start_matches("0x").trim_start_matches('0');
    // An allowance past u128 (an unlimited approval) is more than any deposit needs.
    Ok(if hex.len() > 32 {
        u128::MAX
    } else {
        u128::from_str_radix(if hex.is_empty() { "0" } else { hex }, 16).map_err(internal)?
    })
}

// The user's Aave savings in USDC units (6 decimals), principal plus interest so far.
pub(super) async fn savings_units(state: &AppState, wallet: &str) -> Result<u128, ApiError> {
    state
        .markets
        .base
        .balance_of(AAVE_USDC, wallet)
        .await
        .map_err(internal)
}

fn money(units: u128, currency: &str, rate: u128) -> Value {
    let value = units.saturating_mul(rate) / 1_000_000;
    json!({"amount":usdc(value),"currency":currency})
}

async fn wallet_of(state: &AppState, headers: &HeaderMap) -> Result<(String, String), ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    let wallet = user.evm_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    Ok((user.user_id, wallet))
}

pub(super) async fn options(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    currency_of(q.currency)?;
    let apy = usdc_apy(&state).await?;
    Ok(Json(json!({"options":[{
        "optionId":OPTION_ID,"name":"USDC savings","venue":"Aave","chain":"base","asset":"USDC",
        "apyPct":format!("{apy:.2}"),
        "about":"Your USDC lent on Aave, the largest lending market, earning its variable rate. Take it out any time."
    }]})))
}

pub(super) async fn positions(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (_, wallet) = wallet_of(&state, &headers).await?;
    let currency = currency_of(q.currency)?;
    let (units, apy, rate) = tokio::try_join!(
        savings_units(&state, &wallet),
        usdc_apy(&state),
        app_balance::fx_rate(&currency)
    )?;
    let positions: Vec<Value> = (units > 0)
        .then(|| {
            json!({"optionId":OPTION_ID,"name":"USDC savings","venue":"Aave","amount":usdc(units),
            "value":money(units,&currency,rate),"apyPct":format!("{apy:.2}")})
        })
        .into_iter()
        .collect();
    Ok(Json(json!({"positions":positions})))
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    let (owner, wallet) = wallet_of(&state, &headers).await?;
    if req.option_id != OPTION_ID {
        return Err((StatusCode::NOT_FOUND, "unknown earn option".into()));
    }
    let deposit = match req.action.as_str() {
        "deposit" => true,
        "withdraw" => false,
        _ => return Err(bad("action must be deposit or withdraw")),
    };
    let currency = currency_of(Some(req.amount.currency.clone()))?;
    let rate = app_balance::fx_rate(&currency).await?;
    let display = markets::parse_micros(&req.amount.amount)?;
    let mut units = display.saturating_mul(1_000_000) / rate;
    if units < 100_000 {
        return Err(bad("amount must be at least 0.10 USD"));
    }
    let (available, apy) = tokio::try_join!(
        async {
            if deposit {
                state
                    .markets
                    .base
                    .balance_of(engine_execution::swaps::uniswap::BASE_USDC, &wallet)
                    .await
                    .map_err(internal)
            } else {
                savings_units(&state, &wallet).await
            }
        },
        usdc_apy(&state)
    )?;
    // Asking for (nearly) all of the savings takes everything, interest included, leaving no dust.
    let all = !deposit && units >= available.saturating_mul(999) / 1000;
    if all {
        units = available;
    }
    if units > available || available == 0 {
        return Err((
            StatusCode::CONFLICT,
            if deposit {
                "Not enough in your balance on Base for this".into()
            } else {
                "That's more than you have in savings".into()
            },
        ));
    }
    let quote_id = format!("earn-q-{:x}", now());
    let expires = now() + QUOTE_MS;
    state.earn.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        EarnQuote {
            owner,
            wallet,
            deposit,
            units,
            all,
            expires,
        },
    );
    Ok(Json(json!({
        "quoteId":quote_id,"optionId":OPTION_ID,"action":req.action,
        "amount":money(units,&currency,rate),"usdc":usdc(units),"all":all,
        "apyPct":format!("{apy:.2}"),"expiresAtUnixMs":expires
    })))
}

pub(super) async fn execute(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let (owner, wallet) = wallet_of(&state, &headers).await?;
    let quote = state
        .earn
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "earn quote not found".into()))?;
    if quote.owner != owner || !quote.wallet.eq_ignore_ascii_case(&wallet) {
        return Err((
            StatusCode::FORBIDDEN,
            "quote belongs to another user".into(),
        ));
    }
    if now() >= quote.expires {
        return Err((
            StatusCode::GONE,
            "quote expired; request a fresh one".into(),
        ));
    }
    let usdc_token = engine_execution::swaps::uniswap::BASE_USDC.to_ascii_lowercase();
    let mut txs: Vec<(String, String)> = Vec::new();
    if quote.deposit {
        if usdc_allowance(&state, &wallet).await? < quote.units {
            // approve(pool, amount): exactly this deposit, never an open-ended allowance.
            txs.push((
                usdc_token.clone(),
                format!(
                    "0x095ea7b3{}{}",
                    word_address(AAVE_POOL),
                    word_units(quote.units)
                ),
            ));
        }
        // supply(asset, amount, onBehalfOf, referralCode)
        txs.push((
            AAVE_POOL.into(),
            format!(
                "0x617ba037{}{}{}{}",
                word_address(&usdc_token),
                word_units(quote.units),
                word_address(&wallet),
                word_units(0)
            ),
        ));
    } else {
        // withdraw(asset, amount, to): type(uint256).max takes everything, interest included.
        let amount = if quote.all {
            "f".repeat(64)
        } else {
            word_units(quote.units)
        };
        txs.push((
            AAVE_POOL.into(),
            format!(
                "0x69328dec{}{}{}",
                word_address(&usdc_token),
                amount,
                word_address(&wallet)
            ),
        ));
    }
    let intent_id = state
        .markets
        .register_base_txs(owner, wallet, txs.clone())?;
    let transactions: Vec<Value> = txs
        .iter()
        .map(|(to, data)| json!({"chain":"base","chainId":8453,"to":to,"data":data,"value":"0"}))
        .collect();
    let amount = if quote.all {
        "Everything in savings".into()
    } else {
        format!("{} USDC", usdc(quote.units))
    };
    Ok(Json(json!({
        "intentId":intent_id,
        "kind":if quote.deposit {"earn_deposit"} else {"earn_withdraw"},
        "summary":[
            {"label":if quote.deposit {"Put in savings"} else {"Take out of savings"},"value":amount},
            {"label":"Where","value":"Aave, on Base"},
            {"label":"Rate","value":"Variable, set by Aave"}
        ],
        "transactions":transactions,
        "expiresAtUnixMs":now() + 120_000
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_aave_rate_and_encodes_calls() {
        // currentLiquidityRate 4.292% APR in ray, as the third word of getReserveData.
        let rate = (0.04292_f64 * 1e27) as u128;
        let data = format!(
            "0x{}{}{}{}",
            word_units(1),
            word_units(2),
            word_units(rate),
            word_units(0)
        );
        let apy = supply_apy(&data).unwrap();
        assert!((apy - 4.385).abs() < 0.01, "{apy}");
        assert_eq!(word_address("0xAbC"), format!("{:0>64}", "abc"));
        assert_eq!(word_units(20_000_000).len(), 64);
        assert!(word_units(20_000_000).ends_with("1312d00"));
        assert_eq!(usdc(20_500_001), "20.500001");
    }
}
