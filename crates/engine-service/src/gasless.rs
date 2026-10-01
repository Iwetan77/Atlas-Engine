//! Base cash that moves without Base gas. The user confirmed the plan in the app; their own Privy
//! session then signs the one EIP-3009 authorization the venue needs (the bridge only signs a Base
//! USDC deposit from their wallet to a pinned receiver, and checks the signature), and the venue's
//! relayer pays the gas. Atlas never pays and holds no key for it.
use super::*;
use serde_json::{json, Value};

// What the user reads when the move couldn't start: nothing left their balance.
pub(super) const NOT_MOVED: &str =
    "Moving your cash didn't go through, so nothing left your balance. Try again.";

/// The user's signature over `typed_data`, made with their own access token.
pub(super) async fn sign(
    state: &AppState,
    headers: &HeaderMap,
    wallet: &str,
    typed_data: &Value,
) -> Result<String, String> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err("Privy signing unavailable".into());
    };
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or("Privy access token required")?;
    let response = http
        .post(format!("{bridge_url}/evm/sign-authorization"))
        .json(
            &json!({"accessToken":token,"identityToken":app_balance::identity_token(headers),
            "walletAddress":wallet,"typedData":typed_data}),
        )
        .send()
        .await
        .map_err(|_| "Privy signing unavailable".to_string())?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(body["error"]
            .as_str()
            .unwrap_or("Privy could not sign")
            .into());
    }
    let signature = body["signature"].as_str().unwrap_or_default();
    if !body["walletAddress"]
        .as_str()
        .is_some_and(|w| w.eq_ignore_ascii_case(wallet))
        || signature.len() != 132
        || !signature.starts_with("0x")
    {
        return Err("Privy signing answer did not match".into());
    }
    Ok(signature.into())
}
