//! Base cash that moves without Base gas. The user confirmed the plan in the app; their own Privy
//! phone signs the one EIP-3009 authorization the venue needs (the bridge only verifies a Base
//! USDC deposit from their wallet to a pinned receiver, and checks the signature), and the venue's
//! relayer pays the gas. Atlas never pays and holds no key for it.
use super::*;
use serde_json::{json, Value};

// What the user reads when the move couldn't start: nothing left their balance.
pub(super) const NOT_MOVED: &str =
    "Moving your cash didn't go through, so nothing left your balance. Try again.";

/// Verify the user's phone signature over the exact prepared typed data.
pub(super) async fn verify(
    state: &AppState,
    headers: &HeaderMap,
    wallet: &str,
    typed_data: &Value,
    supplied_signature: &str,
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
        .json(&json!({"accessToken":token,
            "walletAddress":wallet,"typedData":typed_data,"signature":supplied_signature}))
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

// The phone uses the standard EIP-712 field names; Privy's server format uses primary_type.
pub(super) fn step(typed: &Value, chain: &str) -> Value {
    let mut data = typed.clone();
    if let Some(primary) = data.get("primary_type").cloned() {
        data.as_object_mut()
            .expect("typed object")
            .remove("primary_type");
        data["primaryType"] = primary;
    }
    json!({"chain":chain,"typedData":data})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phone_steps_keep_every_bound_field_and_use_standard_eip712_names() {
        let typed = json!({"primary_type":"TransferWithAuthorization","domain":{"chainId":8453},
            "types":{"TransferWithAuthorization":[{"name":"value","type":"uint256"}]},"message":{"value":"123","to":"receiver","nonce":"bound"}});
        let step = step(&typed, "base");
        assert_eq!(step["typedData"]["primaryType"], typed["primary_type"]);
        assert!(step["typedData"].get("primary_type").is_none());
        for field in ["domain", "types", "message"] {
            assert_eq!(step["typedData"][field], typed[field]);
        }
    }
}
