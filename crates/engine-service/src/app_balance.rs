//! App-facing read API. Wallet addresses come from the verified Privy user.

use super::*;
use axum::{extract::Query, http::header::AUTHORIZATION};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
pub(super) struct BalanceQuery {
    currency: Option<String>,
}

#[derive(Deserialize)]
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
    as_of_unix_ms: u128,
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
    let evm = user.evm_wallet.filter(|v| !v.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let solana_owner = user.solana_wallet.filter(|v| !v.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Solana wallet is not ready".into(),
    ))?;
    let (base_usdc, solana_usdc) = match state.network {
        AtlasNetwork::Mainnet => {
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
            (base.map_err(internal)?, solana.map_err(internal)?)
        }
        AtlasNetwork::Testnet => {
            state.solana.assert_network().await.map_err(internal)?;
            let target = DepositTarget {
                user_id: user.user_id.clone(),
                wallet_address: evm.clone(),
                token_contract: "0x036CbD53842c5426634e7929541eC2318f3dCF7e".into(),
                chain_id: 84532,
            };
            let (base, solana) = tokio::join!(
                state.scanner.wallet_token_balance(&target),
                state.solana.owner_mint_balance(
                    &solana_owner,
                    engine_execution::solana::DEVNET_USDC_MINT,
                    6,
                ),
            );
            (base.map_err(internal)?, solana.map_err(internal)?)
        }
    };
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
    let mut total = base_usdc
        .checked_add(solana_usdc)
        .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;

    if state.network == AtlasNetwork::Testnet {
        let as_of_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(internal)?
            .as_millis();
        return Ok(Json(AppBalanceResponse {
            total: money(total, &currency, rate)?,
            total_usd: usd(total),
            pending: None,
            holdings,
            as_of_unix_ms,
        }));
    }

    // Savings on Aave are cash that's earning: they count, labelled as being in Earn.
    let savings = earn::savings_units(&state, &evm).await?;
    if savings > 0 {
        add_holding(&mut holdings, "base", "earn", savings, &currency, rate)?;
        total = total
            .checked_add(savings)
            .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    }
    // Perps margin is still the user's money: it counts, labelled as being in perps.
    if let Some(units) = perps::paradex_account_value(&state, &headers, &user.user_id, &evm).await {
        add_holding(&mut holdings, "paradex", "perps", units, &currency, rate)?;
        total = total
            .checked_add(units)
            .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    }
    let catalog = markets::catalog(&state.markets).await?;
    // Base assets: read each, valued through the same live venue route as the trade preview.
    // Valuation is indicative; a missing route fails explicitly instead of an incomplete total.
    for asset in catalog.iter().filter(|a| a.chain == "base") {
        let units = state
            .markets
            .base
            .balance_of(&asset.token, &evm)
            .await
            .map_err(internal)?;
        if units == 0 {
            continue;
        }
        let (_, one_dollar_units, _) =
            markets::venue_quote(&state.markets, asset, "buy", 1_000_000).await?;
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
    for (mint, units, decimals) in &held {
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
    let as_of_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    Ok(Json(AppBalanceResponse {
        total: money(total, &currency, rate)?,
        total_usd: usd(total),
        pending: None,
        holdings,
        as_of_unix_ms,
    }))
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
    let response = http
        .post(format!("{bridge_url}/verify"))
        .json(&serde_json::json!({"accessToken":token}))
        .send()
        .await
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
