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
    asset_id: &'static str,
    symbol: &'static str,
    name: &'static str,
    kind: &'static str,
    chain: &'static str,
    amount: String,
    value: Money,
    value_usd: String,
    location: &'static str,
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
    if !matches!(currency.as_str(), "USD" | "NGN" | "KES" | "GHS" | "ZAR") {
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
    let target = DepositTarget {
        user_id: user.user_id.clone(),
        wallet_address: evm.clone(),
        token_contract: "0x036CbD53842c5426634e7929541eC2318f3dCF7e".into(),
        chain_id: 84532,
    };
    let sources = [GatewaySource {
        depositor: evm.clone(),
        domain: Some(6),
    }];
    let (wallet, gateway, deposits, solana) = tokio::join!(
        state.scanner.wallet_token_balance(&target),
        state.gateway.balances(&sources),
        state.gateway.deposits(&sources),
        state.solana.owner_usdc_balance(&solana_owner),
    );
    let wallet = wallet.map_err(internal)?;
    let gateway = gateway.map_err(internal)?;
    let deposits = deposits.map_err(internal)?;
    let solana = solana.map_err(internal)?;
    let buckets = usdc_buckets(wallet, &evm, &gateway, &deposits).map_err(internal)?;

    // Only the original single-user test runner writes this movement ledger.
    let movement = if user.user_id == state.user_id {
        state.movement.lock().map_err(internal)?.clone()
    } else {
        None
    };
    let outgoing = movement.as_ref().map(|m| OutgoingGatewayMovement {
        amount_base_units: m.amount_base_units,
        gateway_before_base_units: m.gateway_before_base_units,
        solana_before_base_units: m.solana_before_base_units,
    });
    let unified =
        include_solana_and_outgoing(&buckets, solana, outgoing.as_ref()).map_err(internal)?;
    let total = unified.total_accounted_base_units.ok_or((
        StatusCode::BAD_GATEWAY,
        "balance sources are temporarily inconsistent".into(),
    ))?;
    let pending = buckets
        .gateway_pending_base_units
        .checked_add(unified.outgoing_in_flight_base_units)
        .ok_or((StatusCode::BAD_GATEWAY, "balance overflow".into()))?;
    let rate = fx_rate(&currency).await?;
    let mut holdings = Vec::new();
    add_holding(&mut holdings, "base", "wallet", wallet, &currency, rate)?;
    add_holding(
        &mut holdings,
        "base",
        "gateway",
        buckets.gateway_confirmed_base_units,
        &currency,
        rate,
    )?;
    add_holding(
        &mut holdings,
        "base",
        "gateway_pending",
        buckets.gateway_pending_base_units,
        &currency,
        rate,
    )?;
    add_holding(&mut holdings, "solana", "wallet", solana, &currency, rate)?;
    add_holding(
        &mut holdings,
        "solana",
        "gateway_pending",
        unified.outgoing_in_flight_base_units,
        &currency,
        rate,
    )?;
    let as_of_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    Ok(Json(AppBalanceResponse {
        total: money(total, &currency, rate)?,
        total_usd: usd(total),
        pending: if pending == 0 {
            None
        } else {
            Some(money(pending, &currency, rate)?)
        },
        holdings,
        as_of_unix_ms,
    }))
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
            asset_id: "usdc",
            symbol: "USDC",
            name: "USD Coin",
            kind: "cash",
            chain,
            amount: usd(units),
            value: money(units, currency, rate)?,
            value_usd: usd(units),
            location,
        });
    }
    Ok(())
}

// Rate is represented as destination-currency micros per USD.
pub(super) async fn fx_rate(currency: &str) -> Result<u128, ApiError> {
    if currency == "USD" {
        return Ok(1_000_000);
    }
    let response: Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(internal)?
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
    fn rates_and_money_use_integer_arithmetic() {
        assert_eq!(decimal_micros("1328.44"), Some(1_328_440_000));
        assert_eq!(
            money(1_500_000, "NGN", 1_328_440_000).unwrap().amount,
            "1992.66"
        );
        assert_eq!(money(1_000_000, "USD", 1_000_000).unwrap().amount, "1.00");
    }
}
