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
    location: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_url: Option<String>,
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
async fn sol_bought(state: &AppState, user_id: &str, catalog: &[markets::Asset]) -> u128 {
    let Some(sol) = catalog
        .iter()
        .find(|a| a.chain == "solana" && a.token == markets::SOL_MINT)
    else {
        return 0;
    };
    let Ok(trades) = state.trades.for_user(user_id).await else {
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
    let (base_usdc, solana_usdc) = (base.map_err(internal)?, solana.map_err(internal)?);
    let rate = fx_rate(&currency).await?;
    let mut holdings = Vec::new();
    add_holding(&mut holdings, "base", "wallet", base_usdc, &currency, rate)?;
    add_holding(
        &mut holdings,
        "solana",
        "wallet",
        solana_usdc,
        &currency,
        rate,
    )?;
    // Arc cash counts; if Arc can't be read right now, the rest of the balance still shows.
    let arc_usdc = arc_balance(&evm).await.unwrap_or_else(|(_, error)| {
        eprintln!("Arc balance unavailable: {error}");
        0
    });
    add_holding(&mut holdings, "arc", "wallet", arc_usdc, &currency, rate)?;
    let mut total = base_usdc
        .checked_add(solana_usdc)
        .and_then(|cash| cash.checked_add(arc_usdc))
        .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;

    // Savings on Aave and Morpho are cash that's earning: they count, labelled as being in Earn.
    let savings = earn::base_savings_units(&state, &evm).await?;
    if savings > 0 {
        add_holding(&mut holdings, "base", "earn", savings, &currency, rate)?;
        total = total
            .checked_add(savings)
            .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    }
    // Margin on Hyperliquid is still the user's money: it counts, as perps.
    if let Some(units) = hl::account_value(&state, &evm).await {
        add_holding(
            &mut holdings,
            "hyperliquid",
            "perps",
            units,
            &currency,
            rate,
        )?;
        total = total
            .checked_add(units)
            .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    }
    let catalog = markets::catalog(&state.markets).await?;
    // Base assets: the fixed list, plus every other Base token they bought through Atlas (found by
    // its address). Each is read and valued through the same live route as the trade preview.
    // Valuation is indicative; a missing route on the fixed list fails explicitly instead of an
    // incomplete total, while a pasted token nobody can price any more is left out.
    let mut base_assets: Vec<(markets::Asset, bool)> = catalog
        .iter()
        .filter(|a| a.chain == "base")
        .map(|a| (a.clone(), true))
        .collect();
    if let Ok(trades) = state.trades.for_user(&user.user_id).await {
        for id in base_tokens_bought(&trades) {
            if let Ok(asset) = markets::find_asset(&state.markets, id).await {
                if !base_assets
                    .iter()
                    .any(|(a, _)| a.token.eq_ignore_ascii_case(&asset.token))
                {
                    base_assets.push((asset, false));
                }
            }
        }
    }
    for (asset, fixed) in &base_assets {
        let units = state
            .markets
            .base
            .balance_of(&asset.token, &evm)
            .await
            .map_err(internal)?;
        if units == 0 {
            continue;
        }
        let Some(one_dollar_units) = markets::base_rate(&state.markets, asset).await else {
            if *fixed {
                return Err(markets::unavailable(format!(
                    "Couldn't price {} right now",
                    asset.symbol
                )));
            }
            continue;
        };
        let value_usdc = indicative_usdc_value(units, one_dollar_units)?;
        total = total
            .checked_add(value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
        holdings.push(catalog_holding(asset, units, value_usdc, &currency, rate)?);
    }
    // Solana: everything the wallet holds that Atlas lists (unlisted tokens, e.g. airdropped spam,
    // are left out), valued at Jupiter's live USD price. SOL counts native plus wrapped.
    let (held, native_sol) = tokio::join!(
        state.solana_mainnet.owner_token_balances(&solana_owner),
        state.solana_mainnet.owner_sol_balance(&solana_owner),
    );
    let mut held = held.map_err(internal)?;
    let native_sol = native_sol.map_err(internal)?;
    // Native SOL beyond what they bought is gas: off the asset list and out of the total.
    let gas_sol = native_sol.saturating_sub(sol_bought(&state, &user.user_id, &catalog).await);
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
    let mut listed: Vec<(markets::Asset, u128)> = Vec::new();
    let mut unlisted = Vec::new();
    // Savings on Jupiter Lend are cash that's earning, like Aave on Base.
    let units = earn::lend_savings_units(&state, &held).await?;
    if units > 0 {
        add_holding(&mut holdings, "solana", "earn", units, &currency, rate)?;
        total = total
            .checked_add(units)
            .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    }
    let jito_units = held
        .iter()
        .filter(|(mint, _, _)| mint == earn::JITO_MINT)
        .map(|(_, units, _)| *units)
        .sum::<u128>();
    if jito_units > 0 {
        let value_usdc = earn::jito_staked_value(&state, &held).await?;
        total = total
            .checked_add(value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
        holdings.push(Holding {
            asset_id: earn::JITO_MINT.into(),
            symbol: "JitoSOL".into(),
            name: "SOL staking".into(),
            kind: "crypto".into(),
            chain: "solana".into(),
            amount: markets::format_units(jito_units, 9),
            value: money(value_usdc, &currency, rate)?,
            value_usd: usd(value_usdc),
            location: "earn",
            icon_url: Some(earn::JITO_ICON.into()),
        });
    }
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
    // Tokens bought by pasting their address aren't in the catalog. Look up a few held ones; the
    // same liquidity bar as pasting keeps worthless airdrops out.
    for (mint, units, decimals) in unlisted.into_iter().take(15) {
        if let Ok(Some(asset)) = markets::pasted_token(&state.markets, &mint).await {
            if asset.decimals == decimals {
                listed.push((asset, units));
            }
        }
    }
    let mints: Vec<String> = listed.iter().map(|(a, _)| a.token.clone()).collect();
    // Unlisted tokens Jupiter can't price are skipped rather than failing the balance.
    let unpriced_ok = |asset: &markets::Asset| !asset.verified;
    let prices = markets::usd_prices(&state.markets, &mints).await?;
    for (asset, units) in &listed {
        let (asset, units) = (asset, *units);
        let Some((usd, _)) = prices.get(&asset.token) else {
            if unpriced_ok(asset) {
                continue;
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("no live price for {}", asset.symbol),
            ));
        };
        let value_usdc =
            (units as f64 / 10f64.powi(asset.decimals as i32) * usd * 1_000_000.0) as u128;
        total = total
            .checked_add(value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
        holdings.push(catalog_holding(asset, units, value_usdc, &currency, rate)?);
    }
    // Sui coins bought through Atlas, read from the user's own Sui wallet.
    if let Ok(held) = near_intents::sui_holdings(&state, &headers, &user).await {
        for (asset_id, symbol, name, decimals, units, value_usdc, icon_url) in held {
            total = total
                .checked_add(value_usdc)
                .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
            holdings.push(Holding {
                asset_id,
                symbol,
                name,
                kind: "crypto".into(),
                chain: "sui".into(),
                amount: markets::format_units(units, decimals),
                value: money(value_usdc, &currency, rate)?,
                value_usd: usd(value_usdc),
                location: "wallet",
                icon_url,
            });
        }
    }
    // A NEAR buy is held in the user's own Privy wallet; the current on-chain amount is authoritative.
    for (asset, units) in near_intents::near_holdings(&state, &headers, &user).await? {
        let price = asset
            .price
            .as_ref()
            .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
            .filter(|price: &f64| price.is_finite() && *price > 0.0)
            .ok_or((
                StatusCode::BAD_GATEWAY,
                "An asset price is temporarily unavailable".into(),
            ))?;
        let value = units as f64 / 10f64.powi(asset.decimals as i32) * price * 1_000_000.0;
        if !value.is_finite() || value < 0.0 || value > u128::MAX as f64 {
            return Err((StatusCode::BAD_GATEWAY, "Portfolio value overflow".into()));
        }
        let value_usdc = value as u128;
        total = total
            .checked_add(value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "Portfolio value overflow".into()))?;
        holdings.push(Holding {
            asset_id: format!("near:{}", asset.asset_id),
            symbol: asset.symbol.clone(),
            name: asset.symbol.clone(),
            kind: "crypto".into(),
            chain: "near".into(),
            amount: markets::format_units(units, asset.decimals),
            value: money(value_usdc, &currency, rate)?,
            value_usd: usd(value_usdc),
            location: "wallet",
            icon_url: state.near.icon_for(&asset),
        });
    }
    // Once a 1Click buy settles, read the destination wallet on Monad itself.
    // The spent Base USDC and received asset must both be reflected in Home.
    for (asset, units) in state.near.monad_holdings(&user.user_id, &evm).await? {
        let price = asset
            .price
            .as_ref()
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
            })
            .filter(|p| p.is_finite() && *p > 0.0)
            .ok_or((
                StatusCode::BAD_GATEWAY,
                format!("no live price for {}", asset.symbol),
            ))?;
        let value_usdc_f = units as f64 / 10f64.powi(asset.decimals as i32) * price * 1_000_000.0;
        if !value_usdc_f.is_finite() || value_usdc_f < 0.0 || value_usdc_f > u128::MAX as f64 {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Monad portfolio value overflow".into(),
            ));
        }
        let value_usdc = value_usdc_f as u128;
        total = total
            .checked_add(value_usdc)
            .ok_or((StatusCode::BAD_GATEWAY, "portfolio value overflow".into()))?;
        holdings.push(Holding {
            asset_id: format!("near:{}", asset.asset_id),
            symbol: asset.symbol.clone(),
            name: format!("{} on Monad", asset.symbol),
            kind: "crypto".into(),
            chain: "monad".into(),
            amount: markets::format_units(units, asset.decimals),
            value: money(value_usdc, &currency, rate)?,
            value_usd: usd(value_usdc),
            location: "wallet",
            icon_url: state.near.icon_for(&asset),
        });
    }
    let gas = gas_tanks(&state, gas_sol, &evm, &currency, rate).await?;
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

fn catalog_holding(
    asset: &markets::Asset,
    units: u128,
    value_usdc: u128,
    currency: &str,
    rate: u128,
) -> Result<Holding, ApiError> {
    Ok(Holding {
        asset_id: asset.id.clone(),
        symbol: asset.symbol.clone(),
        name: asset.name.clone(),
        kind: asset.kind.clone(),
        chain: asset.chain.clone(),
        amount: markets::format_units(units, asset.decimals),
        value: money(value_usdc, currency, rate)?,
        value_usd: usd(value_usdc),
        location: "wallet",
        icon_url: asset.icon_url.clone(),
    })
}

fn add_holding(
    holdings: &mut Vec<Holding>,
    chain: &'static str,
    location: &'static str,
    units: u128,
    currency: &str,
    rate: u128,
) -> Result<(), ApiError> {
    if units > 0 {
        holdings.push(Holding {
            asset_id: "usdc".into(),
            symbol: "USDC".into(),
            name: "USD Coin".into(),
            kind: "cash".into(),
            chain: chain.into(),
            amount: usd(units),
            value: money(units, currency, rate)?,
            value_usd: usd(units),
            location,
            icon_url: None,
        });
    }
    Ok(())
}

// Rate is represented as destination-currency micros per USD.
pub(super) async fn fx_rate(currency: &str) -> Result<u128, ApiError> {
    if currency == "USD" {
        return Ok(1_000_000);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(internal)?;
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
