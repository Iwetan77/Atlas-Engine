//! Sends a Base transaction the user confirmed in the app, paid from the wallet's own ETH (plans
//! make sure there is some: a CoW top-up or a refuel hop first). Privy sponsorship is never used.
//! Only a transaction Atlas planned for that user's intent, byte for byte, is ever relayed.
use super::*;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RelayRequest {
    intent_id: String,
    chain_id: u64,
    to: String,
    data: Option<String>,
    value: Option<String>,
}

pub(super) async fn evm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RelayRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let wallet = user.evm_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    // Every plan Atlas builds is a zero-value Base mainnet call.
    if req.chain_id != 8453 {
        return Err((
            StatusCode::BAD_REQUEST,
            "only Base transactions are relayed".into(),
        ));
    }
    if !matches!(req.value.as_deref(), None | Some("0") | Some("0x0")) {
        return Err((
            StatusCode::BAD_REQUEST,
            "only zero-value transactions are relayed".into(),
        ));
    }
    let to = req.to.to_ascii_lowercase();
    let data = req.data.unwrap_or_else(|| "0x".into()).to_ascii_lowercase();
    let planned: Vec<(String, String)> = if req.intent_id.starts_with("near-intent-") {
        state
            .near
            .planned_base_txs(&req.intent_id, &user.user_id)
            .await?
    } else if req.intent_id.starts_with("perp-") {
        perps::trade::planned_funding(&state, &req.intent_id, &user.user_id)
            .await?
            .into_iter()
            .collect()
    } else {
        state
            .markets
            .planned_base_txs(&req.intent_id, &user.user_id)
            .await?
    };
    if !planned.iter().any(|(t, d)| *t == to && *d == data) {
        return Err((
            StatusCode::FORBIDDEN,
            "not a transaction Atlas planned for you".into(),
        ));
    }
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy relay unavailable".into(),
        ));
    };
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    // The same planned transaction is only ever sent once, however often the app retries.
    let idempotency_key = format!(
        "atlas:{}:{}",
        req.intent_id,
        planned_index(&planned, &to, &data)
    );
    let response = http
        .post(format!("{bridge_url}/relay/evm-transaction"))
        .json(
            &json!({"accessToken":token,"walletAddress":wallet,"chainId":req.chain_id,
            "to":to,"data":data,"idempotencyKey":idempotency_key}),
        )
        .send()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Privy relay unavailable".into(),
            )
        })?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body["error"]
            .as_str()
            .unwrap_or("Privy could not send the transaction");
        // No ETH left for gas (plans top up first, so this is rare): say what to do.
        if reason.to_ascii_lowercase().contains("insufficient funds") {
            return Err((
                StatusCode::CONFLICT,
                "Add money to cover network fees, then try again.".into(),
            ));
        }
        return Err((StatusCode::BAD_GATEWAY, reason.into()));
    }
    let hash = body["hash"].as_str().unwrap_or_default();
    if body["userId"].as_str() != Some(&user.user_id)
        || !body["walletAddress"]
            .as_str()
            .is_some_and(|w| w.eq_ignore_ascii_case(&wallet))
        || !perps::trade::is_tx_hash(hash)
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Privy relay answer did not match".into(),
        ));
    }
    Ok(Json(json!({"chain":"base","id":hash})))
}

fn planned_index(planned: &[(String, String)], to: &str, data: &str) -> usize {
    planned
        .iter()
        .position(|(t, d)| t == to && d == data)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idempotency_follows_the_plan_position() {
        let plan = vec![
            ("0xa".to_string(), "0x01".to_string()),
            ("0xb".to_string(), "0x02".to_string()),
        ];
        assert_eq!(planned_index(&plan, "0xb", "0x02"), 1);
        assert_eq!(planned_index(&plan, "0xa", "0x01"), 0);
    }
}
