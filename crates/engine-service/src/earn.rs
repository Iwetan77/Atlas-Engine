//! Earn: savings where the cash already is. USDC on Base goes to Aave v3 (plain Base transactions);
//! USDC on Solana goes to one of Jupiter Lend's dollar or euro markets (one Jupiter swap into its
//! share token and back out to USDC). All earn the venue's variable rate and settle through the same
//! intent flow as trades.
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
// Jupiter Lend markets Atlas offers: (share token, option id). Only cash-like assets Jupiter routes
// into from USDC and back out to USDC (checked 2026-09-30: jlWSOL can't be swapped into, jlUSDG had
// no way back out). Each share is worth more of its asset over time.
const LEND_MARKETS: &[(&str, &str)] = &[
    (
        "9BEcn9aPEmhSPbPQeFGjidRiEKki46fVQDyPpSQXPA2D",
        "jupiter-usdc-solana",
    ),
    (
        "Cmn4v2wipYV41dkakDvCgFJpxhtaaKt11NyWV8pjSE8A",
        "jupiter-usdt-solana",
    ),
    (
        "7GxATsNMnaC88vdwd2t3mwrFuQwwGvmYPrUQ4D6FotXk",
        "jupiter-jupusd-solana",
    ),
    (
        "j14XLJZSVMcUYpAfajdZRpnfHUpJieZHS4aPektLWvh",
        "jupiter-usds-solana",
    ),
    (
        "GcV9tEj62VncGithz4o4N9x6HWXARxuRgEAYk9zahNA8",
        "jupiter-eurc-solana",
    ),
];
const JUPITER_LEND_TOKENS: &str = "https://lite-api.jup.ag/lend/v1/earn/tokens";
// Logos the app shows for each venue.
const JUPITER_ICON: &str = "https://static.jup.ag/jup/icon.png";
const AAVE_ICON: &str =
    "https://coin-images.coingecko.com/coins/images/12645/large/aave-token-round.png";
const QUOTE_MS: u64 = 60_000;
const APY_TTL: Duration = Duration::from_secs(300);
const SECONDS_PER_YEAR: f64 = 31_536_000.0;

#[derive(Clone)]
struct EarnQuote {
    owner: String,
    // Jupiter Lend: the market's share token. None for Aave.
    share_mint: Option<&'static str>,
    // The wallet on the option's chain.
    wallet: String,
    deposit: bool,
    units: u128,
    // Jupiter Lend withdrawals: the shares to hand back.
    shares: u128,
    // Withdraw everything, principal and interest, rather than a fixed amount.
    all: bool,
    // What the confirm sheet speaks in.
    currency: String,
    rate: u128,
    expires: u64,
}

#[derive(Clone, Default)]
pub(super) struct EarnState {
    quotes: Arc<Mutex<HashMap<String, EarnQuote>>>,
    apy: Arc<Mutex<Option<(Instant, f64)>>>,
    lend: Arc<Mutex<Option<(Instant, Vec<Lend>)>>>,
    http: reqwest::Client,
}

// One Jupiter Lend market right now: its yearly rate and what a share is worth.
#[derive(Clone, Debug, PartialEq)]
struct Lend {
    option_id: &'static str,
    share_mint: &'static str,
    // The asset lent: "USDC", "USDT", "EURC"…
    asset: String,
    apy: f64,
    // Asset units per whole share (10^share_decimals share units).
    assets_per_share: u128,
    share_decimals: u32,
    asset_decimals: u32,
    // USD per whole asset; USDC is the cash unit, so exactly 1.
    asset_price: f64,
    icon_url: Option<String>,
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

fn lend_markets(tokens: &Value) -> Vec<Lend> {
    let Some(list) = tokens.as_array() else {
        return Vec::new();
    };
    LEND_MARKETS
        .iter()
        .filter_map(|(mint, option_id)| {
            let t = list.iter().find(|t| t["address"].as_str() == Some(mint))?;
            // totalRate is in basis points: the supply rate plus any rewards.
            let bps: f64 = t["totalRate"].as_str()?.parse().ok()?;
            let assets_per_share: u128 = t["convertToAssets"].as_str()?.parse().ok()?;
            let asset = t["asset"]["symbol"].as_str()?.to_owned();
            let asset_price = if t["assetAddress"].as_str() == Some(markets::SOL_USDC_MINT) {
                1.0
            } else {
                t["asset"]["price"].as_str()?.parse().ok()?
            };
            let lend = Lend {
                option_id,
                share_mint: mint,
                asset,
                apy: bps / 100.0,
                assets_per_share,
                share_decimals: u32::try_from(t["decimals"].as_u64()?).ok()?,
                asset_decimals: u32::try_from(t["asset"]["decimals"].as_u64()?).ok()?,
                asset_price,
                icon_url: t["asset"]["logoUrl"].as_str().map(str::to_owned),
            };
            (assets_per_share > 0 && bps.is_finite() && asset_price > 0.0).then_some(lend)
        })
        .collect()
}

async fn jupiter_lend(state: &AppState) -> Result<Vec<Lend>, ApiError> {
    if let Some((at, markets)) = state.earn.lend.lock().map_err(internal)?.as_ref() {
        if at.elapsed() < APY_TTL {
            return Ok(markets.clone());
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
    let markets = lend_markets(&tokens);
    if markets.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Jupiter Lend rates unreadable".into(),
        ));
    }
    *state.earn.lend.lock().map_err(internal)? = Some((Instant::now(), markets.clone()));
    Ok(markets)
}

// What `shares` of a market are worth, in USDC units.
fn lend_value(shares: u128, lend: &Lend) -> u128 {
    let assets = shares.saturating_mul(lend.assets_per_share) / 10u128.pow(lend.share_decimals);
    if lend.asset_price == 1.0 && lend.asset_decimals == 6 {
        return assets;
    }
    (assets as f64 / 10f64.powi(lend.asset_decimals as i32) * lend.asset_price * 1_000_000.0)
        as u128
}

// Shares worth at least `units` USDC, never more than the user has.
fn lend_shares_for(units: u128, held: u128, lend: &Lend) -> u128 {
    let per_share = lend_value(10u128.pow(lend.share_decimals), lend);
    if per_share == 0 {
        return held;
    }
    units
        .saturating_mul(10u128.pow(lend.share_decimals))
        .div_ceil(per_share)
        .min(held)
}

pub(super) fn is_lend_share(mint: &str) -> bool {
    LEND_MARKETS.iter().any(|(m, _)| *m == mint)
}

// What the user's Jupiter Lend shares (from a wallet read: mint, units, decimals) are worth in
// USDC units, for the balance.
pub(super) async fn lend_savings_units(
    state: &AppState,
    held: &[(String, u128, u32)],
) -> Result<u128, ApiError> {
    if !held
        .iter()
        .any(|(m, units, _)| *units > 0 && is_lend_share(m))
    {
        return Ok(0);
    }
    let markets = jupiter_lend(state).await?;
    Ok(markets
        .iter()
        .map(|lend| lend_value(shares_of(held, lend.share_mint), lend))
        .sum())
}

fn shares_of(held: &[(String, u128, u32)], mint: &str) -> u128 {
    held.iter()
        .filter(|(m, _, _)| m == mint)
        .map(|(_, units, _)| *units)
        .sum()
}

// Everything in the user's Solana wallet (mint, units, decimals).
async fn solana_wallet(
    state: &AppState,
    owner: &str,
) -> Result<Vec<(String, u128, u32)>, ApiError> {
    state
        .solana_mainnet
        .owner_token_balances(owner)
        .await
        .map_err(internal)
}

fn lend_about(lend: &Lend) -> String {
    match lend.asset.as_str() {
        "USDC" => "Your USDC on Solana, lent on Jupiter Lend. Take it out any time.".into(),
        "EURC" => "Your USDC on Solana becomes EURC (euro) lent on Jupiter Lend, so its value moves with the euro. It comes back as USDC.".into(),
        asset => format!("Your USDC on Solana becomes {asset}, a dollar coin, lent on Jupiter Lend. It comes back as USDC. Take it out any time."),
    }
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
    // One venue being down doesn't hide the others; all down is an error.
    let (aave, lend) = tokio::join!(usdc_apy(&state), jupiter_lend(&state));
    let mut options: Vec<(f64, Value)> = Vec::new();
    if let Ok(apy) = aave {
        options.push((apy, json!({
            "optionId":OPTION_ID,"name":"USDC savings","venue":"Aave","chain":"base","asset":"USDC",
            "apyPct":format!("{apy:.2}"),"iconUrl":null,"venueIconUrl":AAVE_ICON,
            "about":"Your USDC on Base, lent on Aave, the largest lending market. Take it out any time."
        })));
    }
    for market in lend.iter().flatten() {
        options.push((market.apy, json!({
            "optionId":market.option_id,"name":format!("{} savings",market.asset),"venue":"Jupiter Lend",
            "chain":"solana","asset":market.asset,"apyPct":format!("{:.2}",market.apy),"about":lend_about(market),
            "iconUrl":market.icon_url,"venueIconUrl":JUPITER_ICON
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
        let held = solana_wallet(&state, owner).await?;
        if held
            .iter()
            .any(|(m, units, _)| *units > 0 && is_lend_share(m))
        {
            for lend in jupiter_lend(&state).await? {
                let units = lend_value(shares_of(&held, lend.share_mint), &lend);
                if units > 0 {
                    positions.push(json!({"optionId":lend.option_id,"name":format!("{} savings",lend.asset),
                        "venue":"Jupiter Lend","amount":usdc(units),"value":money(units,&currency,rate),
                        "apyPct":format!("{:.2}",lend.apy)}));
                }
            }
        }
    }
    Ok(Json(json!({"positions":positions})))
}

pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    let share_mint = LEND_MARKETS
        .iter()
        .find(|(_, id)| *id == req.option_id)
        .map(|(mint, _)| *mint);
    if req.option_id != OPTION_ID && share_mint.is_none() {
        return Err((StatusCode::NOT_FOUND, "unknown earn option".into()));
    }
    let lend_option = share_mint.is_some();
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
    markets::check_limits(units, &currency, rate)?;
    // What can move: cash on the option's chain to put in, or what's in savings to take out.
    let (available, apy, held_shares, lend) = if let Some(mint) = share_mint {
        let (held, markets) =
            tokio::try_join!(solana_wallet(&state, &wallet), jupiter_lend(&state))?;
        let lend = markets.into_iter().find(|m| m.share_mint == mint).ok_or((
            StatusCode::BAD_GATEWAY,
            "Jupiter Lend market unavailable".into(),
        ))?;
        let shares = shares_of(&held, mint);
        let available = if deposit {
            shares_of(&held, markets::SOL_USDC_MINT)
        } else {
            lend_value(shares, &lend)
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
    // Putting cash in draws on the whole balance: short on this option's chain, the rest moves over
    // from the other chain first (checked, and the error names both, before anything is signed).
    let cash_ok = if deposit {
        let user = app_balance::verified_wallets(&state, &headers).await?;
        let solana = user.solana_wallet.filter(|w| !w.is_empty());
        let evm = user.evm_wallet.filter(|w| !w.is_empty());
        if lend_option {
            markets::cash_for_solana(&state, evm.as_deref(), available, units, &currency, rate)
                .await?;
        } else {
            markets::cash_for_base(&state, &wallet, solana.as_deref(), units, &currency, rate)
                .await?;
        }
        true
    } else {
        false
    };
    if !cash_ok && (units > available || available == 0) {
        return Err((
            StatusCode::CONFLICT,
            if deposit {
                format!(
                    "Not enough cash on {} for this. You have {} there.",
                    if lend_option { "Solana" } else { "Base" },
                    markets::say_money(available, &currency, rate)
                )
            } else {
                "That's more than you have in savings".into()
            },
        ));
    }
    let shares = match &lend {
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
            share_mint,
            wallet,
            deposit,
            units,
            shares,
            all,
            currency: currency.clone(),
            rate,
            expires,
        },
    );
    Ok(Json(json!({
        "quoteId":quote_id,"optionId":req.option_id,"action":req.action,
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
    let (owner, wallet) = if quote.share_mint.is_some() {
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
    if let Some(mint) = quote.share_mint {
        return execute_lend(&state, &headers, &quote, mint, owner, wallet).await;
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
    let solana = app_balance::verified_wallets(&state, &headers)
        .await?
        .solana_wallet
        .filter(|w| !w.is_empty());
    // A deposit may need Solana cash moved over first; a withdrawal only needs gas, which Atlas covers.
    let needed = if quote.deposit { quote.units } else { 0 };
    let (intent_id, transactions, fee) = markets::plan_base_with_cash(
        &state,
        owner,
        wallet,
        solana,
        txs,
        needed,
        &quote.currency,
        quote.rate,
    )
    .await?;
    let amount = if quote.all {
        "Everything in savings".into()
    } else {
        markets::say_money(quote.units, &quote.currency, quote.rate)
    };
    let mut summary = vec![
        json!({"label":if quote.deposit {"Put in savings"} else {"Take out of savings"},"value":amount}),
        json!({"label":"Where","value":"Aave, on Base"}),
        json!({"label":"Rate","value":"Variable, set by Aave"}),
    ];
    if let Some(fee) = fee {
        summary.push(json!({"label":"Network fee","value":markets::say_money(fee, &quote.currency, quote.rate)}));
    }
    Ok(Json(json!({
        "intentId":intent_id,
        "kind":if quote.deposit {"earn_deposit"} else {"earn_withdraw"},
        "summary":summary,
        "transactions":transactions,
        "expiresAtUnixMs":now() + 120_000
    })))
}

// Into Jupiter Lend is a Jupiter swap USDC → the market's share token (Jupiter routes it through the
// asset into a Lend deposit); out is shares → USDC. One Solana transaction either way.
async fn execute_lend(
    state: &AppState,
    headers: &HeaderMap,
    quote: &EarnQuote,
    share_mint: &str,
    owner: String,
    wallet: String,
) -> Result<Json<Value>, ApiError> {
    let (input, output, amount) = if quote.deposit {
        (markets::SOL_USDC_MINT, share_mint, quote.units)
    } else {
        (share_mint, markets::SOL_USDC_MINT, quote.shares)
    };
    // Short of cash on Solana: Base cash moves over first, and the deposit is made once it lands.
    if quote.deposit {
        let held = shares_of(
            &solana_wallet(state, &wallet).await?,
            markets::SOL_USDC_MINT,
        );
        let evm = app_balance::verified_wallets(state, headers)
            .await?
            .evm_wallet
            .filter(|w| !w.is_empty());
        if let Some((send, fee)) = markets::cash_for_solana(
            state,
            evm.as_deref(),
            held,
            quote.units,
            &quote.currency,
            quote.rate,
        )
        .await?
        {
            let evm = evm.ok_or((
                StatusCode::CONFLICT,
                "Privy Base wallet is not ready".into(),
            ))?;
            let (intent_id, tx) = markets::plan_solana_swap_with_base_cash(
                state,
                owner,
                wallet,
                &evm,
                share_mint,
                quote.units,
                send,
            )
            .await?;
            return Ok(Json(json!({
                "intentId":intent_id,"kind":"earn_deposit",
                "summary":[
                    {"label":"Put in savings","value":markets::say_money(quote.units, &quote.currency, quote.rate)},
                    {"label":"Where","value":"Jupiter Lend, on Solana"},
                    {"label":"Network fee","value":markets::say_money(fee, &quote.currency, quote.rate)}
                ],
                "transactions":[tx],
                "expiresAtUnixMs":now() + 120_000
            })));
        }
    }
    let asset = jupiter_lend(state)
        .await?
        .into_iter()
        .find(|m| m.share_mint == share_mint)
        .map(|m| m.asset)
        .unwrap_or_else(|| "USDC".into());
    if amount == 0 {
        return Err(bad("nothing to move"));
    }
    let (intent_id, transactions, _) =
        markets::plan_jupiter_swap(state, owner, wallet, input, output, amount).await?;
    let amount = if quote.all {
        "Everything in savings".into()
    } else {
        markets::say_money(quote.units, &quote.currency, quote.rate)
    };
    Ok(Json(json!({
        "intentId":intent_id,
        "kind":if quote.deposit {"earn_deposit"} else {"earn_withdraw"},
        "summary":[
            {"label":if quote.deposit {"Put in savings"} else {"Take out of savings"},"value":amount},
            {"label":"Where","value":format!("Jupiter Lend ({asset}), on Solana")},
            {"label":"Rate","value":"Variable, set by Jupiter Lend"}
        ],
        "transactions":transactions,
        "expiresAtUnixMs":now() + 45_000
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Jupiter's live Lend list still has every market Atlas offers, with readable rates.
    #[tokio::test]
    #[ignore]
    async fn live_jupiter_lend() {
        let tokens: Value = reqwest::get(JUPITER_LEND_TOKENS)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let markets = lend_markets(&tokens);
        for m in &markets {
            println!(
                "{} {:.2}% a year, 1 share = {} USDC",
                m.asset,
                m.apy,
                usdc(lend_value(10u128.pow(m.share_decimals), m))
            );
        }
        assert_eq!(markets.len(), LEND_MARKETS.len());
        assert!(markets.iter().all(|m| m.apy > 0.0 && m.apy < 50.0));
    }
    #[test]
    fn reads_jupiter_lend_and_converts_shares() {
        let market = |address: &str, asset: &str, asset_address: &str, price: &str| {
            json!({"address":address,"totalRate":"466","convertToAssets":"1062358","decimals":6,
                "assetAddress":asset_address,"asset":{"symbol":asset,"decimals":6,"price":price}})
        };
        let tokens = json!([
            {"address":"other","totalRate":"900","convertToAssets":"2000000"},
            market(LEND_MARKETS[0].0, "USDC", markets::SOL_USDC_MINT, "0.9998"),
            market(LEND_MARKETS[4].0, "EURC", "eurc", "1.10"),
        ]);
        let found = lend_markets(&tokens);
        assert_eq!(found.len(), 2);
        let usdc_market = &found[0];
        assert!((usdc_market.apy - 4.66).abs() < 1e-9);
        // USDC is the cash unit: 10 jlUSDC are worth exactly 10.62358 USDC, whatever the quoted price.
        assert_eq!(lend_value(10_000_000, usdc_market), 10_623_580);
        // A euro market is worth its euros at the euro's dollar price.
        assert_eq!(lend_value(10_000_000, &found[1]), 11_685_938);
        // Taking out 5 USDC hands back enough shares for at least 5 USDC, and never more than held.
        let shares = lend_shares_for(5_000_000, 10_000_000, usdc_market);
        assert!(lend_value(shares, usdc_market) >= 4_999_999);
        assert!(lend_value(shares - 1, usdc_market) < 5_000_000);
        assert_eq!(
            lend_shares_for(50_000_000, 10_000_000, usdc_market),
            10_000_000
        );
        assert!(lend_markets(&json!([])).is_empty());
        assert!(is_lend_share(LEND_MARKETS[2].0) && !is_lend_share("other"));
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
