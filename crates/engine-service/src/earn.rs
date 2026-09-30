//! Earn: savings where the cash already is. USDC on Base goes to Aave v3 (plain Base transactions);
//! USDC on Solana goes to Jupiter Lend (one Jupiter swap into jlUSDC and back out). Both earn the
//! venue's variable rate and settle through the same intent flow as trades.
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
const LEND_OPTION_ID: &str = "jupiter-usdc-solana";
// jlUSDC: Jupiter Lend's receipt for USDC supplied on Solana. Each share is worth more USDC over time.
pub(super) const JL_USDC: &str = "9BEcn9aPEmhSPbPQeFGjidRiEKki46fVQDyPpSQXPA2D";
const JUPITER_LEND_TOKENS: &str = "https://lite-api.jup.ag/lend/v1/earn/tokens";
const QUOTE_MS: u64 = 60_000;
const APY_TTL: Duration = Duration::from_secs(300);
const SECONDS_PER_YEAR: f64 = 31_536_000.0;

#[derive(Clone)]
struct EarnQuote {
    owner: String,
    option_id: &'static str,
    // The wallet on the option's chain.
    wallet: String,
    deposit: bool,
    units: u128,
    // Jupiter Lend withdrawals: the jlUSDC shares to hand back.
    shares: u128,
    // Withdraw everything, principal and interest, rather than a fixed amount.
    all: bool,
    expires: u64,
}

#[derive(Clone, Default)]
pub(super) struct EarnState {
    quotes: Arc<Mutex<HashMap<String, EarnQuote>>>,
    apy: Arc<Mutex<Option<(Instant, f64)>>>,
    lend: Arc<Mutex<Option<(Instant, Lend)>>>,
    http: reqwest::Client,
}

// Jupiter Lend's USDC market right now: its yearly rate and what one jlUSDC share is worth.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Lend {
    apy: f64,
    // USDC units per 1,000,000 shares (both have 6 decimals).
    assets_per_share: u128,
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

fn lend_rate(tokens: &Value) -> Option<Lend> {
    let t = tokens
        .as_array()?
        .iter()
        .find(|t| t["address"].as_str() == Some(JL_USDC))?;
    // totalRate is in basis points: the supply rate plus any rewards.
    let bps: f64 = t["totalRate"].as_str()?.parse().ok()?;
    let assets_per_share: u128 = t["convertToAssets"].as_str()?.parse().ok()?;
    (assets_per_share > 0 && bps.is_finite()).then_some(Lend {
        apy: bps / 100.0,
        assets_per_share,
    })
}

async fn jupiter_lend(state: &AppState) -> Result<Lend, ApiError> {
    if let Some((at, lend)) = *state.earn.lend.lock().map_err(internal)? {
        if at.elapsed() < APY_TTL {
            return Ok(lend);
        }
    }
    let tokens: Value = state
        .earn
        .http
        .get(JUPITER_LEND_TOKENS)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(internal)?
        .error_for_status()
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;
    let lend = lend_rate(&tokens).ok_or((
        StatusCode::BAD_GATEWAY,
        "Jupiter Lend rate unreadable".into(),
    ))?;
    *state.earn.lend.lock().map_err(internal)? = Some((Instant::now(), lend));
    Ok(lend)
}

fn lend_value(shares: u128, lend: Lend) -> u128 {
    shares.saturating_mul(lend.assets_per_share) / 1_000_000
}

// jlUSDC shares worth at least `units` USDC, never more than the user has.
fn lend_shares_for(units: u128, held: u128, lend: Lend) -> u128 {
    units
        .saturating_mul(1_000_000)
        .div_ceil(lend.assets_per_share)
        .min(held)
}

// What the user's jlUSDC is worth in USDC units, for the balance.
pub(super) async fn lend_savings_units(state: &AppState, shares: u128) -> Result<u128, ApiError> {
    Ok(lend_value(shares, jupiter_lend(state).await?))
}

// Plain USDC and jlUSDC shares in the user's Solana wallet.
async fn solana_holdings(state: &AppState, owner: &str) -> Result<(u128, u128), ApiError> {
    let held = state
        .solana_mainnet
        .owner_token_balances(owner)
        .await
        .map_err(internal)?;
    let sum = |mint: &str| {
        held.iter()
            .filter(|(m, _, _)| m == mint)
            .map(|(_, units, _)| *units)
            .sum::<u128>()
    };
    Ok((sum(markets::SOL_USDC_MINT), sum(JL_USDC)))
}

fn money(units: u128, currency: &str, rate: u128) -> Value {
    let value = units.saturating_mul(rate) / 1_000_000;
    json!({"amount":usdc(value),"currency":currency})
}

// The user and their Solana wallet.
async fn solana_wallet_of(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(String, String), ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    let wallet = user.solana_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Solana wallet is not ready".into(),
    ))?;
    Ok((user.user_id, wallet))
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
    // One venue being down doesn't hide the other; both down is an error.
    let (aave, lend) = tokio::join!(usdc_apy(&state), jupiter_lend(&state));
    let mut options: Vec<(f64, Value)> = Vec::new();
    if let Ok(apy) = aave {
        options.push((apy, json!({
            "optionId":OPTION_ID,"name":"USDC savings","venue":"Aave","chain":"base","asset":"USDC",
            "apyPct":format!("{apy:.2}"),
            "about":"Your USDC on Base, lent on Aave, the largest lending market. Take it out any time."
        })));
    }
    if let Ok(lend) = &lend {
        options.push((lend.apy, json!({
            "optionId":LEND_OPTION_ID,"name":"USDC savings","venue":"Jupiter Lend","chain":"solana","asset":"USDC",
            "apyPct":format!("{:.2}",lend.apy),
            "about":"Your USDC on Solana, lent on Jupiter Lend. Take it out any time."
        })));
    }
    if options.is_empty() {
        return Err(aave
            .err()
            .or(lend.err())
            .unwrap_or((StatusCode::BAD_GATEWAY, "no savings rate available".into())));
    }
    // Best rate first.
    options.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(Json(
        json!({"options": options.into_iter().map(|(_, o)| o).collect::<Vec<_>>()}),
    ))
}

pub(super) async fn positions(
    State(state): State<AppState>,
    Query(q): Query<CurrencyQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let currency = currency_of(q.currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let mut positions = Vec::new();
    if let Some(wallet) = user.evm_wallet.as_deref().filter(|w| !w.is_empty()) {
        let (units, apy) = tokio::try_join!(savings_units(&state, wallet), usdc_apy(&state))?;
        if units > 0 {
            positions.push(json!({"optionId":OPTION_ID,"name":"USDC savings","venue":"Aave","amount":usdc(units),
                "value":money(units,&currency,rate),"apyPct":format!("{apy:.2}")}));
        }
    }
    if let Some(owner) = user.solana_wallet.as_deref().filter(|w| !w.is_empty()) {
        let (_, shares) = solana_holdings(&state, owner).await?;
        if shares > 0 {
            let lend = jupiter_lend(&state).await?;
            let units = lend_value(shares, lend);
            positions.push(json!({"optionId":LEND_OPTION_ID,"name":"USDC savings","venue":"Jupiter Lend","amount":usdc(units),
                "value":money(units,&currency,rate),"apyPct":format!("{:.2}",lend.apy)}));
        }
    }
    Ok(Json(json!({"positions":positions})))
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    let option_id = match req.option_id.as_str() {
        OPTION_ID => OPTION_ID,
        LEND_OPTION_ID => LEND_OPTION_ID,
        _ => return Err((StatusCode::NOT_FOUND, "unknown earn option".into())),
    };
    let lend_option = option_id == LEND_OPTION_ID;
    let (owner, wallet) = if lend_option {
        solana_wallet_of(&state, &headers).await?
    } else {
        wallet_of(&state, &headers).await?
    };
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
    // What can move: cash on the option's chain to put in, or what's in savings to take out.
    let (available, apy, held_shares, lend) = if lend_option {
        let ((cash, shares), lend) =
            tokio::try_join!(solana_holdings(&state, &wallet), jupiter_lend(&state))?;
        let available = if deposit {
            cash
        } else {
            lend_value(shares, lend)
        };
        (available, lend.apy, shares, Some(lend))
    } else {
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
        (available, apy, 0, None)
    };
    // Asking for (nearly) all of the savings takes everything, interest included, leaving no dust.
    let all = !deposit && units >= available.saturating_mul(999) / 1000;
    if all {
        units = available;
    }
    if units > available || available == 0 {
        return Err((
            StatusCode::CONFLICT,
            if deposit {
                format!(
                    "Not enough in your balance on {} for this",
                    if lend_option { "Solana" } else { "Base" }
                )
            } else {
                "That's more than you have in savings".into()
            },
        ));
    }
    let shares = match lend {
        Some(_) if all => held_shares,
        Some(lend) if !deposit => lend_shares_for(units, held_shares, lend),
        _ => 0,
    };
    let quote_id = format!("earn-q-{:x}", now());
    let expires = now() + QUOTE_MS;
    state.earn.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        EarnQuote {
            owner,
            option_id,
            wallet,
            deposit,
            units,
            shares,
            all,
            expires,
        },
    );
    Ok(Json(json!({
        "quoteId":quote_id,"optionId":option_id,"action":req.action,
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
    let quote = state
        .earn
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "earn quote not found".into()))?;
    let (owner, wallet) = if quote.option_id == LEND_OPTION_ID {
        solana_wallet_of(&state, &headers).await?
    } else {
        wallet_of(&state, &headers).await?
    };
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
    if quote.option_id == LEND_OPTION_ID {
        return execute_lend(&state, quote, owner, wallet).await;
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
        .register_base_txs(owner, wallet, txs.clone())
        .await?;
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

// Into Jupiter Lend is a Jupiter swap USDC → jlUSDC (Jupiter routes it as a Lend deposit, no fee);
// out is jlUSDC → USDC. One Solana transaction either way.
async fn execute_lend(
    state: &AppState,
    quote: EarnQuote,
    owner: String,
    wallet: String,
) -> Result<Json<Value>, ApiError> {
    let (input, output, amount) = if quote.deposit {
        (markets::SOL_USDC_MINT, JL_USDC, quote.units)
    } else {
        (JL_USDC, markets::SOL_USDC_MINT, quote.shares)
    };
    if amount == 0 {
        return Err(bad("nothing to move"));
    }
    let (intent_id, transaction, _) = state
        .markets
        .plan_jupiter_swap(owner, wallet, input, output, amount)
        .await?;
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
            {"label":"Where","value":"Jupiter Lend, on Solana"},
            {"label":"Rate","value":"Variable, set by Jupiter Lend"}
        ],
        "transactions":[{"chain":"solana","transaction":transaction,"submit":"engine"}],
        "expiresAtUnixMs":now() + 45_000
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Jupiter's live Lend list still has jlUSDC with a rate and share price we can read.
    #[tokio::test]
    #[ignore]
    async fn live_jupiter_lend() {
        let tokens: Value = reqwest::get(JUPITER_LEND_TOKENS)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let lend = lend_rate(&tokens).unwrap();
        println!(
            "jlUSDC: {:.2}% a year, 1 share = {} USDC",
            lend.apy,
            usdc(lend.assets_per_share)
        );
        assert!(lend.apy > 0.0 && lend.apy < 50.0);
        assert!(lend.assets_per_share >= 1_000_000);
    }
    #[test]
    fn reads_jupiter_lend_and_converts_shares() {
        let tokens = json!([
            {"address":"other","totalRate":"900","convertToAssets":"2000000"},
            {"address":JL_USDC,"totalRate":"466","convertToAssets":"1062358"}
        ]);
        let lend = lend_rate(&tokens).unwrap();
        assert!((lend.apy - 4.66).abs() < 1e-9);
        // 10 jlUSDC are worth 10.62358 USDC.
        assert_eq!(lend_value(10_000_000, lend), 10_623_580);
        // Taking out 5 USDC hands back enough shares for at least 5 USDC, and never more than held.
        let shares = lend_shares_for(5_000_000, 10_000_000, lend);
        assert!(lend_value(shares, lend) >= 4_999_999);
        assert!(lend_value(shares - 1, lend) < 5_000_000);
        assert_eq!(lend_shares_for(50_000_000, 10_000_000, lend), 10_000_000);
        assert!(lend_rate(&json!([])).is_none());
        assert!(
            lend_rate(&json!([{"address":JL_USDC,"totalRate":"466","convertToAssets":"0"}]))
                .is_none()
        );
    }
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
