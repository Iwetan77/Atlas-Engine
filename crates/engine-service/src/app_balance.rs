//! App-facing read API. Wallet addresses come from the verified Privy user.

use super::*;
use axum::{extract::Query, http::header::AUTHORIZATION};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
pub(super) struct BalanceQuery {
    currency: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VerifiedWallets {
    pub(super) user_id: String,
    pub(super) evm_wallet: Option<String>,
    pub(super) solana_wallet: Option<String>,
    // The sign-in email and Google name: bank transfers register the user with Daya by these.
    #[serde(default)]
    pub(super) email: Option<String>,
    #[serde(default)]
    pub(super) name: Option<String>,
}

#[derive(Serialize)]
struct Money {
    amount: String,
    currency: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Holding {
    asset_id: String,
    symbol: String,
    name: String,
    kind: String,
    chain: String,
    amount: String,
    value: Money,
    value_usd: String,
    location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_url: Option<String>,
    // Gain or loss on what Atlas bought of this coin, in percent, from the same read as `value`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pnl_pct: Option<String>,
}

// A holding as read from its chain or venue, before it's shown in a currency. The last confirmed
// set is remembered per user (`BalanceMemory`), so a read that fails can't drop it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Held {
    pub(super) asset_id: String,
    pub(super) symbol: String,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) chain: String,
    pub(super) location: String,
    #[serde(default)]
    pub(super) icon_url: Option<String>,
    // As shown: whole tokens, or shares for a prediction.
    pub(super) amount: String,
    // Base units and their decimals (a prediction carries its value here, in USDC units).
    #[serde(with = "u128_text")]
    pub(super) units: u128,
    pub(super) decimals: u32,
    #[serde(with = "u128_text")]
    pub(super) value_usdc: u128,
    // When a read last confirmed it.
    #[serde(default)]
    pub(super) seen_ms: u64,
}

// Token amounts outgrow JSON numbers; they're kept as text.
mod u128_text {
    use serde::{Deserialize, Deserializer, Serializer};
    pub(super) fn serialize<S: Serializer>(value: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppBalanceResponse {
    total: Money,
    total_usd: String,
    pending: Option<Money>,
    holdings: Vec<Holding>,
    // The gas tanks: what pays network fees on each chain. Not counted in `total`; Profile shows it.
    gas: Vec<GasTank>,
    as_of_unix_ms: u128,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GasTank {
    chain: &'static str,
    symbol: &'static str,
    amount: String,
    value: Money,
}

// Ether's price, read through its Solana twin (Portal ETH) the way SOL's is read.
const PORTAL_ETH_MINT: &str = "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs";

// SOL the user bought through Atlas and still holds on net: the rest of the native SOL is the gas
// tank Atlas fills for Solana fees.
fn sol_bought(trades: &[positions::Trade], catalog: &[markets::Asset]) -> u128 {
    let Some(sol) = catalog
        .iter()
        .find(|a| a.chain == "solana" && a.token == markets::SOL_MINT)
    else {
        return 0;
    };
    trades
        .iter()
        .filter(|t| t.asset_id == sol.id)
        .fold(0u128, |net, t| {
            if t.side == "buy" {
                net.saturating_add(t.token_units)
            } else {
                net.saturating_sub(t.token_units)
            }
        })
}

const ARC_USDC: &str = "0x3600000000000000000000000000000000000000";

async fn arc_balance(wallet: &str) -> Result<u128, ApiError> {
    if wallet.len() != 42
        || !wallet.starts_with("0x")
        || !wallet[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err((StatusCode::BAD_REQUEST, "Wallet address is invalid".into()));
    }
    let (default_rpc, expected_chain) = ("https://rpc.mainnet.arc.io", "0x13b2");
    let rpc = env_url("ATLAS_ARC_RPC_URL", default_rpc);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(internal)?;
    let chain: Value = client
        .post(&rpc)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}))
        .send()
        .await
        .map_err(internal)?
        .error_for_status()
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;
    if chain["result"].as_str() != Some(expected_chain) {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Cash balance temporarily unavailable".into(),
        ));
    }
    let data = format!("0x70a08231{:0>64}", &wallet[2..].to_ascii_lowercase());
    let balance: Value = client
        .post(&rpc)
        .json(
            &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"eth_call",
            "params":[{"to":ARC_USDC,"data":data},"latest"]}),
        )
        .send()
        .await
        .map_err(internal)?
        .error_for_status()
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;
    let hex = balance["result"]
        .as_str()
        .and_then(|v| v.strip_prefix("0x"))
        .ok_or((
            StatusCode::BAD_GATEWAY,
            "Cash balance temporarily unavailable".into(),
        ))?;
    u128::from_str_radix(hex, 16).map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "Cash balance temporarily unavailable".into(),
        )
    })
}
pub(super) async fn balance(
    State(state): State<AppState>,
    Query(query): Query<BalanceQuery>,
    headers: HeaderMap,
) -> Result<Json<AppBalanceResponse>, ApiError> {
    let currency = query.currency.unwrap_or_else(|| "USD".into());
    if !DISPLAY_CURRENCIES.contains(&currency.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            "unsupported display currency".into(),
        ));
    }
    let user = verified_wallets(&state, &headers).await?;
    // Their email and currency, for emails about their money (never fatal to the balance).
    let _ = state.emails.remember(&user, Some(&currency)).await;
    let portfolio = portfolio(&state, &headers, &user).await?;
    let rate = fx_rate(&currency).await?;
    let open = positions::open_positions(&portfolio.trades);
    let mut total = 0u128;
    let mut holdings = Vec::with_capacity(portfolio.holdings.len());
    for h in &portfolio.holdings {
        total = total
            .checked_add(h.value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
        holdings.push(Holding {
            asset_id: h.asset_id.clone(),
            symbol: h.symbol.clone(),
            name: h.name.clone(),
            kind: h.kind.clone(),
            chain: h.chain.clone(),
            amount: h.amount.clone(),
            value: money(h.value_usdc, &currency, rate)?,
            value_usd: usd(h.value_usdc),
            location: h.location.clone(),
            icon_url: h.icon_url.clone(),
            pnl_pct: positions::pnl_pct(h, &open),
        });
    }
    let gas = gas_tanks(&state, portfolio.gas_sol, &portfolio.evm, &currency, rate).await?;
    let as_of_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    Ok(Json(AppBalanceResponse {
        total: money(total, &currency, rate)?,
        total_usd: usd(total),
        pending: None,
        holdings,
        gas,
        as_of_unix_ms,
    }))
}

// Everything the user owns, read once for the balance and the profit and loss alike, so the two
// always agree.
pub(super) struct Portfolio {
    pub(super) holdings: Vec<Held>,
    pub(super) trades: Vec<positions::Trade>,
    gas_sol: u128,
    evm: String,
}

// Home asks for the balance and the profit and loss together: they share one read. A request that
// arrives while a read is running waits for it instead of starting another.
const PORTFOLIO_FRESH: Duration = Duration::from_secs(2);
type PortfolioSlot = Arc<tokio::sync::Mutex<Option<(Instant, Arc<Portfolio>)>>>;
static PORTFOLIOS: std::sync::LazyLock<Mutex<HashMap<String, PortfolioSlot>>> =
    std::sync::LazyLock::new(Default::default);

pub(super) async fn portfolio(
    state: &AppState,
    headers: &HeaderMap,
    user: &VerifiedWallets,
) -> Result<Arc<Portfolio>, ApiError> {
    let slot = PORTFOLIOS
        .lock()
        .map_err(internal)?
        .entry(user.user_id.clone())
        .or_default()
        .clone();
    let mut slot = slot.lock().await;
    if let Some((at, portfolio)) = slot.as_ref() {
        if at.elapsed() < PORTFOLIO_FRESH {
            return Ok(portfolio.clone());
        }
    }
    let fresh = Arc::new(read_portfolio(state, headers, user).await?);
    *slot = Some((Instant::now(), fresh.clone()));
    Ok(fresh)
}

// What a failed read leaves unknown. A remembered holding it covers stays in the balance at its last
// confirmed amount and value; a holding a read confirmed gone (or zero) leaves at once.
#[derive(Clone, Debug)]
enum Unsure {
    // One holding: location, chain and asset id.
    One(&'static str, &'static str, String),
    // Every coin (not cash) in the wallet on a chain.
    Coins(&'static str),
    // Everything in a location (Predictions).
    Place(&'static str),
}

impl Unsure {
    fn covers(&self, h: &Held) -> bool {
        match self {
            Unsure::One(location, chain, id) => {
                h.location == *location && h.chain == *chain && same_asset(&h.asset_id, id)
            }
            Unsure::Coins(chain) => h.location == "wallet" && h.chain == *chain && h.kind != "cash",
            Unsure::Place(location) => h.location == *location,
        }
    }
}

// Base addresses come in either case.
fn same_asset(a: &str, b: &str) -> bool {
    a == b || (a.starts_with("base:") && a.eq_ignore_ascii_case(b))
}

fn same_holding(a: &Held, b: &Held) -> bool {
    a.location == b.location && a.chain == b.chain && same_asset(&a.asset_id, &b.asset_id)
}

// How long a holding nobody could read stays at its last confirmed amount.
const KEEP_UNREAD_MS: u64 = 24 * 60 * 60 * 1000;

// This read, completed by what the failed lookups couldn't confirm.
fn settle(mut read: Vec<Held>, unsure: &[Unsure], remembered: &[Held], now: u64) -> Vec<Held> {
    for h in &mut read {
        h.seen_ms = now;
    }
    let kept: Vec<Held> = remembered
        .iter()
        .filter(|old| {
            now.saturating_sub(old.seen_ms) < KEEP_UNREAD_MS
                && unsure.iter().any(|u| u.covers(old))
                && !read.iter().any(|h| same_holding(h, old))
        })
        .cloned()
        .collect();
    read.extend(kept);
    read
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn read_portfolio(
    state: &AppState,
    headers: &HeaderMap,
    user: &VerifiedWallets,
) -> Result<Portfolio, ApiError> {
    let evm = user.evm_wallet.clone().filter(|v| !v.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let solana_owner = user
        .solana_wallet
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or((
            StatusCode::CONFLICT,
            "Privy Solana wallet is not ready".into(),
        ))?;
    state
        .solana_mainnet
        .assert_network()
        .await
        .map_err(internal)?;
    let remembered = state.balances.recall(&user.user_id).await;
    let mut read = Vec::new();
    let mut unsure = Vec::new();
    // Cash on Base and Solana, the trade book and the catalog are the core: without them there's no
    // honest balance, so the app keeps showing its last one.
    let (base, solana) = tokio::join!(
        state
            .markets
            .base
            .balance_of(engine_execution::swaps::uniswap::BASE_USDC, &evm),
        state.solana_mainnet.owner_mint_balance(
            &solana_owner,
            engine_execution::solana::MAINNET_USDC_MINT,
            6,
        ),
    );
    read.extend(cash_held("base", "wallet", base.map_err(internal)?));
    read.extend(cash_held("solana", "wallet", solana.map_err(internal)?));
    let trades = state.trades.for_user(&user.user_id).await?;
    let catalog = markets::catalog(&state.markets).await?;

    // Every other source is read at once; one that fails leaves only its own holdings unread.
    let (cash, perps, predicted, base, (solana, gas_sol), sui, near, monad) = tokio::join!(
        other_cash(state, &evm),
        perps_margin(state, &evm),
        predictions_held(state, headers, &user.user_id),
        async {
            let mut found = Found::default();
            base_coins(state, &evm, &catalog, &trades, &mut found).await;
            found
        },
        async {
            let mut found = Found::default();
            let gas = solana_coins(
                state,
                &solana_owner,
                &catalog,
                &trades,
                &remembered,
                &mut found,
            )
            .await;
            (found, gas)
        },
        sui_coins(state, headers, user),
        near_coins(state, headers, user),
        monad_coins(state, &user.user_id, &evm),
    );
    for found in [cash, perps, predicted, base, solana, sui, near, monad] {
        read.extend(found.read);
        unsure.extend(found.unsure);
    }
    let holdings = settle(read, &unsure, &remembered, now_ms());
    state.balances.remember(&user.user_id, &holdings);
    Ok(Portfolio {
        holdings,
        trades,
        gas_sol,
        evm,
    })
}

// What one source read: its holdings, and what it couldn't confirm.
#[derive(Default)]
struct Found {
    read: Vec<Held>,
    unsure: Vec<Unsure>,
}

// Arc cash and savings on Base.
async fn other_cash(state: &AppState, evm: &str) -> Found {
    let mut found = Found::default();
    match arc_balance(evm).await {
        Ok(units) => found.read.extend(cash_held("arc", "wallet", units)),
        Err((_, error)) => {
            eprintln!("Arc balance unavailable: {error}");
            found
                .unsure
                .push(Unsure::One("wallet", "arc", "usdc".into()));
        }
    }
    // Savings on Aave and Morpho are cash that's earning: they count, labelled as being in Earn.
    match earn::base_savings_units(state, evm).await {
        Ok(units) => found.read.extend(cash_held("base", "earn", units)),
        Err(_) => found
            .unsure
            .push(Unsure::One("earn", "base", "usdc".into())),
    }
    found
}

async fn perps_margin(state: &AppState, evm: &str) -> Found {
    let mut found = Found::default();
    // Margin on Hyperliquid is still the user's money: it counts, as perps.
    match hl::account_value(state, evm).await {
        Ok(units) => found.read.extend(cash_held("hyperliquid", "perps", units)),
        Err(_) => found
            .unsure
            .push(Unsure::One("perps", "hyperliquid", "usdc".into())),
    }
    found
}

async fn predictions_held(state: &AppState, headers: &HeaderMap, owner: &str) -> Found {
    let mut found = Found::default();
    // Cash and outcome shares in Predictions remain part of what the user owns.
    match predictions::portfolio(state, headers, owner)
        .await
        .and_then(|rows| prediction_holdings(&rows))
    {
        Ok(rows) => found.read.extend(rows),
        Err(_) => found.unsure.push(Unsure::Place("predictions")),
    }
    found
}

async fn sui_coins(state: &AppState, headers: &HeaderMap, user: &VerifiedWallets) -> Found {
    let mut found = Found::default();
    // Sui coins bought through Atlas, read from the user's own Sui wallet.
    match near_intents::sui_holdings(state, headers, user).await {
        Ok((coins, unpriced)) => {
            for (asset_id, symbol, name, decimals, units, value_usdc, icon_url) in coins {
                found.read.push(Held {
                    asset_id,
                    symbol,
                    name,
                    kind: "crypto".into(),
                    chain: "sui".into(),
                    location: "wallet".into(),
                    icon_url,
                    amount: markets::format_units(units, decimals),
                    units,
                    decimals,
                    value_usdc,
                    seen_ms: 0,
                });
            }
            found.unsure.extend(
                unpriced
                    .into_iter()
                    .map(|id| Unsure::One("wallet", "sui", id)),
            );
        }
        Err(_) => found.unsure.push(Unsure::Coins("sui")),
    }
    found
}

async fn near_coins(state: &AppState, headers: &HeaderMap, user: &VerifiedWallets) -> Found {
    let mut found = Found::default();
    // A NEAR buy is held in the user's own Privy wallet; the current on-chain amount is authoritative.
    match near_intents::near_holdings(state, headers, user).await {
        Ok((coins, unread)) => {
            found.unsure.extend(
                unread
                    .into_iter()
                    .map(|id| Unsure::One("wallet", "near", id)),
            );
            for (asset, units) in coins {
                let id = format!("near:{}", asset.asset_id);
                match priced_units(&id, asset.price.as_ref(), units, asset.decimals) {
                    Some(value_usdc) => found.read.push(Held {
                        asset_id: id,
                        symbol: asset.symbol.clone(),
                        name: asset.symbol.clone(),
                        kind: "crypto".into(),
                        chain: "near".into(),
                        location: "wallet".into(),
                        icon_url: state.near.icon_for(&asset),
                        amount: markets::format_units(units, asset.decimals),
                        units,
                        decimals: asset.decimals,
                        value_usdc,
                        seen_ms: 0,
                    }),
                    None => found.unsure.push(Unsure::One("wallet", "near", id)),
                }
            }
        }
        Err(_) => found.unsure.push(Unsure::Coins("near")),
    }
    found
}

async fn monad_coins(state: &AppState, owner: &str, evm: &str) -> Found {
    let mut found = Found::default();
    // Once a 1Click buy settles, read the destination wallet on Monad itself.
    match state.near.monad_holdings(owner, evm).await {
        Ok(coins) => {
            for (asset, units) in coins {
                let id = format!("near:{}", asset.asset_id);
                match priced_units(&id, asset.price.as_ref(), units, asset.decimals) {
                    Some(value_usdc) => found.read.push(Held {
                        asset_id: id,
                        symbol: asset.symbol.clone(),
                        name: format!("{} on Monad", asset.symbol),
                        kind: "crypto".into(),
                        chain: "monad".into(),
                        location: "wallet".into(),
                        icon_url: state.near.icon_for(&asset),
                        amount: markets::format_units(units, asset.decimals),
                        units,
                        decimals: asset.decimals,
                        value_usdc,
                        seen_ms: 0,
                    }),
                    None => found.unsure.push(Unsure::One("wallet", "monad", id)),
                }
            }
        }
        Err(_) => found.unsure.push(Unsure::Coins("monad")),
    }
    found
}

// A 1Click-listed coin's value in USDC units at its live (or recently seen) price.
fn priced_units(asset_id: &str, live: Option<&Value>, units: u128, decimals: u32) -> Option<u128> {
    let price = live_or_recent(asset_id, live)?;
    let value = units as f64 / 10f64.powi(decimals as i32) * price * 1_000_000.0;
    (value.is_finite() && value >= 0.0 && value < u128::MAX as f64).then_some(value as u128)
}

fn prediction_holdings(rows: &[Value]) -> Result<Vec<Held>, ApiError> {
    rows.iter()
        .map(|p| {
            let units = p["units"]
                .as_str()
                .and_then(|v| v.parse::<u128>().ok())
                .ok_or_else(|| internal("Predictions value unavailable"))?;
            Ok(Held {
                asset_id: p["assetId"].as_str().unwrap_or_default().into(),
                symbol: p["symbol"].as_str().unwrap_or_default().into(),
                name: p["name"].as_str().unwrap_or_default().into(),
                kind: p["kind"].as_str().unwrap_or("crypto").into(),
                chain: "polygon".into(),
                location: "predictions".into(),
                icon_url: p["iconUrl"].as_str().map(str::to_string),
                amount: p["amount"].as_str().unwrap_or_default().into(),
                units,
                decimals: 6,
                value_usdc: units,
                seen_ms: 0,
            })
        })
        .collect()
}

// Base assets: the fixed list, plus every other Base token they bought through Atlas (found by its
// address). Each is read and valued through the same live route as the trade preview.
async fn base_coins(
    state: &AppState,
    evm: &str,
    catalog: &[markets::Asset],
    trades: &[positions::Trade],
    found: &mut Found,
) {
    let mut assets: Vec<markets::Asset> = catalog
        .iter()
        .filter(|a| a.chain == "base")
        .cloned()
        .collect();
    for id in base_tokens_bought(trades) {
        match markets::find_asset(&state.markets, id).await {
            Ok(asset) => {
                if !assets
                    .iter()
                    .any(|a| a.token.eq_ignore_ascii_case(&asset.token))
                {
                    assets.push(asset);
                }
            }
            // Nobody lists it any more: there's nothing to value it by.
            Err((StatusCode::NOT_FOUND, _)) => {}
            Err(_) => found
                .unsure
                .push(Unsure::One("wallet", "base", id.to_owned())),
        }
    }
    for asset in &assets {
        let units = match state.markets.base.balance_of(&asset.token, evm).await {
            Ok(units) => units,
            Err(_) => {
                found
                    .unsure
                    .push(Unsure::One("wallet", "base", asset.id.clone()));
                continue;
            }
        };
        if units == 0 {
            continue;
        }
        let value_usdc = match markets::base_rate(&state.markets, asset).await {
            Some(one_dollar_units) if one_dollar_units > 0 => {
                remember_price(
                    &asset.id,
                    10f64.powi(asset.decimals as i32) / one_dollar_units as f64,
                );
                indicative_usdc_value(units, one_dollar_units).ok()
            }
            _ => recent_price(&asset.id).map(|usd| worth(units, asset.decimals, usd)),
        };
        match value_usdc {
            Some(value_usdc) => found.read.push(coin_held(asset, units, value_usdc)),
            None => found
                .unsure
                .push(Unsure::One("wallet", "base", asset.id.clone())),
        }
    }
}

// Solana: everything the wallet holds that Atlas lists (unlisted tokens, e.g. airdropped spam, are
// left out), valued at Jupiter's live USD price. SOL counts native plus wrapped. Returns the gas
// tank: native SOL beyond what they bought.
async fn solana_coins(
    state: &AppState,
    owner: &str,
    catalog: &[markets::Asset],
    trades: &[positions::Trade],
    remembered: &[Held],
    found: &mut Found,
) -> u128 {
    let (tokens, native) = tokio::join!(
        state.solana_mainnet.owner_token_balances(owner),
        state.solana_mainnet.owner_sol_balance(owner),
    );
    let (Ok(mut held), Ok(native_sol)) = (tokens, native) else {
        found.unsure.extend([
            Unsure::Coins("solana"),
            Unsure::One("earn", "solana", "usdc".into()),
            Unsure::One("earn", "solana", earn::JITO_MINT.into()),
        ]);
        return 0;
    };
    // Native SOL beyond what they bought is gas: off the asset list and out of the total.
    let gas_sol = native_sol.saturating_sub(sol_bought(trades, catalog));
    let native_sol = native_sol - gas_sol;
    if native_sol > 0 {
        match held
            .iter_mut()
            .find(|(mint, _, _)| mint == markets::SOL_MINT)
        {
            Some(entry) => entry.1 = entry.1.saturating_add(native_sol),
            None => held.push((markets::SOL_MINT.into(), native_sol, 9)),
        }
    }
    // Savings on Jupiter Lend are cash that's earning, like Aave on Base.
    match earn::lend_savings_units(state, &held).await {
        Ok(units) => found.read.extend(cash_held("solana", "earn", units)),
        Err(_) => found
            .unsure
            .push(Unsure::One("earn", "solana", "usdc".into())),
    }
    let jito_units = held
        .iter()
        .filter(|(mint, _, _)| mint == earn::JITO_MINT)
        .map(|(_, units, _)| *units)
        .sum::<u128>();
    if jito_units > 0 {
        match earn::jito_staked_value(state, &held).await {
            Ok(value_usdc) => found.read.push(Held {
                asset_id: earn::JITO_MINT.into(),
                symbol: "JitoSOL".into(),
                name: "SOL staking".into(),
                kind: "crypto".into(),
                chain: "solana".into(),
                location: "earn".into(),
                icon_url: Some(earn::JITO_ICON.into()),
                amount: markets::format_units(jito_units, 9),
                units: jito_units,
                decimals: 9,
                value_usdc,
                seen_ms: 0,
            }),
            Err(_) => found
                .unsure
                .push(Unsure::One("earn", "solana", earn::JITO_MINT.into())),
        }
    }
    let mut listed: Vec<(markets::Asset, u128)> = Vec::new();
    let mut unlisted = Vec::new();
    for (mint, units, decimals) in &held {
        if earn::is_lend_share(mint) {
            continue;
        }
        match catalog
            .iter()
            .find(|a| a.chain == "solana" && &a.token == mint && a.decimals == *decimals)
        {
            Some(asset) => listed.push((asset.clone(), *units)),
            None if mint != markets::SOL_USDC_MINT => {
                unlisted.push((mint.clone(), *units, *decimals))
            }
            None => {}
        }
    }
    // Tokens bought by pasting their address aren't in the catalog. Ones they bought here or held
    // last time are always looked up; a few others too, with the same liquidity bar as pasting, so
    // worthless airdrops stay out. A wallet full of spam can't push a real coin off the list.
    let known = |mint: &str| {
        trades.iter().any(|t| t.asset_id == mint)
            || remembered
                .iter()
                .any(|h| h.chain == "solana" && h.asset_id == mint)
    };
    let mut others = 0;
    for (mint, units, decimals) in unlisted {
        if !known(&mint) {
            if others == 15 {
                continue;
            }
            others += 1;
        }
        match markets::pasted_token(&state.markets, &mint).await {
            Ok(Some(asset)) if asset.decimals == decimals => listed.push((asset, units)),
            Ok(_) => {}
            Err(_) => found.unsure.push(Unsure::One("wallet", "solana", mint)),
        }
    }
    let mints: Vec<String> = listed.iter().map(|(a, _)| a.token.clone()).collect();
    let prices = markets::usd_prices(&state.markets, &mints)
        .await
        .unwrap_or_default();
    for (asset, units) in &listed {
        let usd = match prices.get(&asset.token) {
            Some((usd, _)) => {
                remember_price(&asset.id, *usd);
                Some(*usd)
            }
            None => recent_price(&asset.id),
        };
        match usd {
            Some(usd) => {
                found
                    .read
                    .push(coin_held(asset, *units, worth(*units, asset.decimals, usd)))
            }
            None => found
                .unsure
                .push(Unsure::One("wallet", "solana", asset.id.clone())),
        }
    }
    gas_sol
}

// Each user's last confirmed holdings, in memory and in Postgres, so neither a failed lookup nor a
// restart can make a coin vanish from the balance for a moment.
#[derive(Clone, Default)]
pub(super) struct BalanceMemory {
    postgres: Option<Arc<tokio_postgres::Client>>,
    users: Arc<Mutex<HashMap<String, Remembered>>>,
}

#[derive(Clone, Default)]
struct Remembered {
    holdings: Vec<Held>,
    // What Postgres holds (which coins, how many) and when it was written.
    stored: String,
    stored_ms: u64,
}

// Postgres is written when the coins or amounts change, and at least this often otherwise.
const STORE_EVERY_MS: u64 = 10 * 60 * 1000;

fn fingerprint(holdings: &[Held]) -> String {
    holdings
        .iter()
        .map(|h| format!("{}|{}|{}|{}", h.location, h.chain, h.asset_id, h.units))
        .collect::<Vec<_>>()
        .join(";")
}

impl BalanceMemory {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut memory = Self::default();
        if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("balance memory database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_balance_memory (
                    user_id TEXT PRIMARY KEY,
                    payload TEXT NOT NULL,
                    updated_ms BIGINT NOT NULL
                )",
                )
                .await?;
            memory.postgres = Some(Arc::new(client));
        }
        Ok(memory)
    }

    async fn recall(&self, user_id: &str) -> Vec<Held> {
        if let Some(holdings) = self
            .users
            .lock()
            .ok()
            .and_then(|users| users.get(user_id).map(|r| r.holdings.clone()))
        {
            return holdings;
        }
        let Some(pg) = &self.postgres else {
            return Vec::new();
        };
        let holdings: Vec<Held> = match pg
            .query_opt(
                "SELECT payload FROM atlas_balance_memory WHERE user_id=$1",
                &[&user_id],
            )
            .await
        {
            Ok(Some(row)) => serde_json::from_str(row.get::<_, &str>(0)).unwrap_or_default(),
            Ok(None) => Vec::new(),
            Err(error) => {
                eprintln!("balance memory unavailable: {error}");
                return Vec::new();
            }
        };
        if let Ok(mut users) = self.users.lock() {
            users
                .entry(user_id.to_owned())
                .or_insert_with(|| Remembered {
                    stored: fingerprint(&holdings),
                    stored_ms: now_ms(),
                    holdings: holdings.clone(),
                });
        }
        holdings
    }

    fn remember(&self, user_id: &str, holdings: &[Held]) {
        let print = fingerprint(holdings);
        let now = now_ms();
        let store = {
            let Ok(mut users) = self.users.lock() else {
                return;
            };
            let entry = users.entry(user_id.to_owned()).or_default();
            entry.holdings = holdings.to_vec();
            let due = entry.stored != print || now.saturating_sub(entry.stored_ms) > STORE_EVERY_MS;
            if due {
                entry.stored = print;
                entry.stored_ms = now;
            }
            due
        };
        let (true, Some(pg), Ok(payload)) = (
            store,
            self.postgres.clone(),
            serde_json::to_string(holdings),
        ) else {
            return;
        };
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            let at = i64::try_from(now).unwrap_or(i64::MAX);
            if let Err(error) = pg
                .execute(
                    "INSERT INTO atlas_balance_memory (user_id,payload,updated_ms) VALUES ($1,$2,$3)
                     ON CONFLICT (user_id) DO UPDATE SET payload=EXCLUDED.payload, updated_ms=EXCLUDED.updated_ms",
                    &[&user_id, &payload, &at],
                )
                .await
            {
                eprintln!("balance memory not saved: {error}");
            }
        });
    }
}

// Solana's tank (`sol` lamports) and Base's (the wallet's ETH), valued at live prices. A tank that
// can't be priced right now is still listed, at zero.
async fn gas_tanks(
    state: &AppState,
    sol: u128,
    evm: &str,
    currency: &str,
    rate: u128,
) -> Result<Vec<GasTank>, ApiError> {
    let eth = markets::base_eth(state, evm).await.unwrap_or(0);
    let mints = [markets::SOL_MINT.to_string(), PORTAL_ETH_MINT.to_string()];
    let prices = markets::usd_prices(&state.markets, &mints)
        .await
        .unwrap_or_default();
    let worth = |units: u128, decimals: i32, mint: &str| {
        prices
            .get(mint)
            .map(|(usd, _)| (units as f64 / 10f64.powi(decimals) * usd * 1_000_000.0) as u128)
            .unwrap_or(0)
    };
    Ok(vec![
        GasTank {
            chain: "solana",
            symbol: "SOL",
            amount: markets::format_units(sol, 9),
            value: money(worth(sol, 9, markets::SOL_MINT), currency, rate)?,
        },
        GasTank {
            chain: "base",
            symbol: "ETH",
            amount: markets::format_units(eth, 18),
            value: money(worth(eth, 18, PORTAL_ETH_MINT), currency, rate)?,
        },
    ])
}

fn indicative_usdc_value(units: u128, one_dollar_units: u128) -> Result<u128, ApiError> {
    if one_dollar_units == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            "venue returned zero-priced asset".into(),
        ));
    }
    units
        .checked_mul(1_000_000)
        .map(|n| n / one_dollar_units)
        .ok_or((
            StatusCode::BAD_GATEWAY,
            "portfolio valuation overflow".into(),
        ))
}
pub(super) async fn verified_wallets(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<VerifiedWallets, ApiError> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Ok(VerifiedWallets {
            user_id: state.user_id.clone(),
            evm_wallet: Some(state.base_wallet.clone()),
            solana_wallet: Some(state.solana_owner.clone()),
            email: None,
            name: None,
        });
    };
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    // A bridge that's restarting gets one more try before sign-in checks fail.
    let verify = || {
        http.post(format!("{bridge_url}/verify"))
            .json(&serde_json::json!({"accessToken":token}))
            .send()
    };
    let response = match verify().await {
        Ok(response) => Ok(response),
        Err(_) => {
            tokio::time::sleep(Duration::from_millis(700)).await;
            verify().await
        }
    }
    .map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy verification unavailable".into(),
        )
    })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid or expired Privy access token".into(),
        ));
    }
    let user: VerifiedWallets = response
        .error_for_status()
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Privy verification unavailable".into(),
            )
        })?
        .json()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Privy verification unavailable".into(),
            )
        })?;
    if !user.user_id.starts_with("did:privy:") {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy identity unavailable".into(),
        ));
    }
    Ok(user)
}

// The Trade list and search only need to know the caller is signed in. A token the bridge verified
// in the last minute isn't sent to Privy again on every keystroke; anything that moves money still
// calls `verified_wallets` each time.
const SIGNED_IN_TTL: Duration = Duration::from_secs(60);
static SIGNED_IN: std::sync::LazyLock<Mutex<HashMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);
pub(super) async fn signed_in(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !matches!(state.auth, AuthMode::Privy { .. }) {
        return Ok(());
    }
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty());
    if let Some(at) = token.and_then(|t| SIGNED_IN.lock().ok()?.get(t).copied()) {
        if at.elapsed() < SIGNED_IN_TTL {
            return Ok(());
        }
    }
    verified_wallets(state, headers).await?;
    if let (Some(token), Ok(mut seen)) = (token, SIGNED_IN.lock()) {
        seen.retain(|_, at| at.elapsed() < SIGNED_IN_TTL);
        seen.insert(token.to_owned(), Instant::now());
    }
    Ok(())
}

// Each asset's last good dollar price per whole token. When a live price is missing, a holding is
// valued at its price from the past 30 minutes instead of dropping out of the balance (and the
// total) for one failed lookup.
static LAST_PRICES: std::sync::LazyLock<Mutex<HashMap<String, (Instant, f64)>>> =
    std::sync::LazyLock::new(Default::default);
fn remember_price(asset_id: &str, usd: f64) {
    if usd.is_finite() && usd > 0.0 {
        if let Ok(mut last) = LAST_PRICES.lock() {
            last.insert(asset_id.to_owned(), (Instant::now(), usd));
        }
    }
}
fn recent_price(asset_id: &str) -> Option<f64> {
    LAST_PRICES
        .lock()
        .ok()?
        .get(asset_id)
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(30 * 60))
        .map(|(_, usd)| *usd)
}
// A price as 1Click lists it (number or text), remembered; or the recent one when it's missing.
fn live_or_recent(asset_id: &str, live: Option<&Value>) -> Option<f64> {
    match live
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|p: &f64| p.is_finite() && *p > 0.0)
    {
        Some(usd) => {
            remember_price(asset_id, usd);
            Some(usd)
        }
        None => recent_price(asset_id),
    }
}
// USDC micros for `units` of a token at `usd` per whole token.
fn worth(units: u128, decimals: u32, usd: f64) -> u128 {
    (units as f64 / 10f64.powi(decimals as i32) * usd * 1_000_000.0) as u128
}

// The Base tokens outside the fixed list someone has traded through Atlas, once each.
fn base_tokens_bought(trades: &[positions::Trade]) -> Vec<&str> {
    let mut ids: Vec<&str> = trades
        .iter()
        .map(|t| t.asset_id.as_str())
        .filter(|id| id.starts_with("base:"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn coin_held(asset: &markets::Asset, units: u128, value_usdc: u128) -> Held {
    Held {
        asset_id: asset.id.clone(),
        symbol: asset.symbol.clone(),
        name: asset.name.clone(),
        kind: asset.kind.clone(),
        chain: asset.chain.clone(),
        location: "wallet".into(),
        icon_url: asset.icon_url.clone(),
        amount: markets::format_units(units, asset.decimals),
        units,
        decimals: asset.decimals,
        value_usdc,
        seen_ms: 0,
    }
}

// USDC cash: nothing when there's none.
fn cash_held(chain: &str, location: &str, units: u128) -> Option<Held> {
    (units > 0).then(|| Held {
        asset_id: "usdc".into(),
        symbol: "USDC".into(),
        name: "USD Coin".into(),
        kind: "cash".into(),
        chain: chain.into(),
        location: location.into(),
        icon_url: None,
        amount: usd(units),
        units,
        decimals: 6,
        value_usdc: units,
        seen_ms: 0,
    })
}

// One rate per currency is shared for a minute: every list, search and quote used to fetch it
// again, on a new connection, before doing anything else.
const FX_TTL: Duration = Duration::from_secs(60);
static FX_RATES: std::sync::LazyLock<Mutex<HashMap<String, (Instant, u128)>>> =
    std::sync::LazyLock::new(Default::default);
static FX_HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_default()
});

// Rate is represented as destination-currency micros per USD.
pub(super) async fn fx_rate(currency: &str) -> Result<u128, ApiError> {
    if currency == "USD" {
        return Ok(1_000_000);
    }
    if let Some((at, rate)) = FX_RATES.lock().map_err(internal)?.get(currency) {
        if at.elapsed() < FX_TTL {
            return Ok(*rate);
        }
    }
    let rate = fetch_fx_rate(currency).await?;
    FX_RATES
        .lock()
        .map_err(internal)?
        .insert(currency.to_owned(), (Instant::now(), rate));
    Ok(rate)
}
async fn fetch_fx_rate(currency: &str) -> Result<u128, ApiError> {
    let client = &*FX_HTTP;
    let frankfurter = client
        .get(format!(
            "https://api.frankfurter.dev/v2/rate/USD/{currency}"
        ))
        .send()
        .await;
    if let Ok(response) = frankfurter {
        if let Ok(response) = response.error_for_status() {
            if let Ok(data) = response.json::<Value>().await {
                if data["base"] == "USD" && data["quote"] == currency {
                    if let Some(rate) = data["rate"]
                        .as_number()
                        .and_then(|v| decimal_micros(&v.to_string()))
                    {
                        return Ok(rate);
                    }
                }
            }
        }
    }
    let response: Value = client
        .get("https://api.coinbase.com/v2/exchange-rates?currency=USD")
        .send()
        .await
        .map_err(internal)?
        .error_for_status()
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;
    let rate = response["data"]["rates"][currency]
        .as_str()
        .ok_or((StatusCode::BAD_GATEWAY, "FX rate unavailable".into()))?;
    decimal_micros(rate).ok_or((StatusCode::BAD_GATEWAY, "FX rate invalid".into()))
}
fn decimal_micros(value: &str) -> Option<u128> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|v| v.is_ascii_digit())
        || !frac.bytes().all(|v| v.is_ascii_digit())
    {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let fraction: u128 = format!("{:0<6}", &frac[..frac.len().min(6)]).parse().ok()?;
    let result = whole.checked_mul(1_000_000)?.checked_add(fraction)?;
    (result > 0).then_some(result)
}

fn money(base_units: u128, currency: &str, rate_micros: u128) -> Result<Money, ApiError> {
    let cents = base_units
        .checked_mul(rate_micros)
        .and_then(|v| v.checked_add(5_000_000_000))
        .map(|v| v / 10_000_000_000)
        .ok_or((StatusCode::BAD_GATEWAY, "display amount overflow".into()))?;
    Ok(Money {
        amount: format!("{}.{:02}", cents / 100, cents % 100),
        currency: currency.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portfolio_value_uses_raw_token_units() {
        // A $1 quote buys 0.0005 WETH. Half a WETH is therefore $1,000.
        assert_eq!(
            indicative_usdc_value(500_000_000_000_000_000, 500_000_000_000_000).unwrap(),
            1_000_000_000
        );
        assert!(indicative_usdc_value(1, 0).is_err());
        let trade = |asset_id: &str| positions::Trade {
            intent_id: "i".into(),
            user_id: "u".into(),
            asset_id: asset_id.into(),
            side: "buy".into(),
            token_units: 1,
            usdc_units: 1,
            tx_id: None,
            filled_at_ms: 0,
        };
        let trades = [
            trade("base:0x4ed4e862860bed51a9570b96d89af5e1b0efefed"),
            trade("brett-base"),
            trade("So11111111111111111111111111111111111111112"),
            trade("base:0x4ed4e862860bed51a9570b96d89af5e1b0efefed"),
        ];
        assert_eq!(
            base_tokens_bought(&trades),
            vec!["base:0x4ed4e862860bed51a9570b96d89af5e1b0efefed"]
        );
    }
    fn held(location: &str, chain: &str, asset_id: &str, kind: &str, seen_ms: u64) -> Held {
        Held {
            asset_id: asset_id.into(),
            symbol: asset_id.into(),
            name: asset_id.into(),
            kind: kind.into(),
            chain: chain.into(),
            location: location.into(),
            icon_url: None,
            amount: "1".into(),
            units: 1_000_000,
            decimals: 6,
            value_usdc: 2_000_000,
            seen_ms,
        }
    }

    #[test]
    fn a_coin_nobody_could_read_stays_and_a_coin_read_as_gone_leaves() {
        let now = 100 * 60 * 60 * 1000;
        let deep = "near:sui:0xdeeb::deep::DEEP";
        let remembered = [
            held("wallet", "sui", deep, "crypto", now - 60_000),
            held(
                "wallet",
                "sui",
                "near:nep141:sui.omft.near",
                "crypto",
                now - 60_000,
            ),
            held("wallet", "solana", "bonk", "crypto", now - 60_000),
            held("wallet", "arc", "usdc", "cash", now - 60_000),
            held("perps", "hyperliquid", "usdc", "cash", now - KEEP_UNREAD_MS),
        ];
        // DEEP's price lookup failed and Hyperliquid didn't answer; SUI was read and is still there;
        // BONK and Arc cash were read as gone.
        let read = vec![held(
            "wallet",
            "sui",
            "near:nep141:sui.omft.near",
            "crypto",
            0,
        )];
        let unsure = [
            Unsure::One("wallet", "sui", deep.into()),
            Unsure::One("perps", "hyperliquid", "usdc".into()),
        ];
        let settled = settle(read, &unsure, &remembered, now);
        let ids: Vec<_> = settled.iter().map(|h| h.asset_id.as_str()).collect();
        // Hyperliquid's last read is a day old: too old to keep showing.
        assert_eq!(ids, ["near:nep141:sui.omft.near", deep]);
        assert!(settled
            .iter()
            .all(|h| h.asset_id != deep || h.seen_ms == now - 60_000));
        assert_eq!(settled[0].seen_ms, now);
    }

    #[test]
    fn a_failed_chain_read_keeps_its_coins_but_never_its_cash() {
        let now = 10_000_000;
        let remembered = [
            held("wallet", "solana", "bonk", "crypto", now - 1),
            held("wallet", "solana", "usdc", "cash", now - 1),
            held("earn", "solana", earn::JITO_MINT, "crypto", now - 1),
        ];
        let settled = settle(Vec::new(), &[Unsure::Coins("solana")], &remembered, now);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].asset_id, "bonk");
        // Base addresses match in either case.
        let base = [held("wallet", "base", "base:0xABC", "crypto", now - 1)];
        let unsure = [Unsure::One("wallet", "base", "base:0xabc".into())];
        assert_eq!(settle(Vec::new(), &unsure, &base, now).len(), 1);
    }

    #[test]
    fn remembered_holdings_survive_a_round_trip_through_text() {
        let mut h = held("wallet", "near", "near:nep141:wrap.near", "crypto", 5);
        h.units = 452_112_000_000_000_000_000_000;
        h.decimals = 24;
        let text = serde_json::to_string(&[h.clone()]).unwrap();
        assert!(text.contains("\"units\":\"452112000000000000000000\""));
        let back: Vec<Held> = serde_json::from_str(&text).unwrap();
        assert_eq!(back[0].units, h.units);
        assert_eq!(fingerprint(&back), fingerprint(&[h]));
    }

    #[test]
    fn rates_and_money_use_integer_arithmetic() {
        assert_eq!(decimal_micros("1328.44"), Some(1_328_440_000));
        assert_eq!(
            money(1_500_000, "NGN", 1_328_440_000).unwrap().amount,
            "1992.66"
        );
        assert_eq!(money(1_000_000, "USD", 1_000_000).unwrap().amount, "1.00");
    }
}
