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
    // The domain's own type, spelled out as EIP-712 asks (its fields in the standard order). A signer
    // that isn't told it can hash the domain as an empty one, and that signature then belongs to
    // nobody: the approval is refused and the money never moves.
    if data["types"].get("EIP712Domain").is_none() {
        let fields: Vec<Value> = [
            ("name", "string"),
            ("version", "string"),
            ("chainId", "uint256"),
            ("verifyingContract", "address"),
            ("salt", "bytes32"),
        ]
        .into_iter()
        .filter(|(name, _)| data["domain"].get(name).is_some())
        .map(|(name, kind)| json!({"name":name,"type":kind}))
        .collect();
        if let Some(types) = data["types"].as_object_mut() {
            types.insert("EIP712Domain".into(), json!(fields));
        }
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
        for field in ["domain", "message"] {
            assert_eq!(step["typedData"][field], typed[field]);
        }
        assert_eq!(
            step["typedData"]["types"]["TransferWithAuthorization"],
            typed["types"]["TransferWithAuthorization"]
        );
    }

    #[test]
    fn the_phone_is_told_the_domain_type_it_signs() {
        let typed = json!({"primaryType":"ReceiveWithAuthorization",
            "domain":{"name":"USD Coin","version":"2","chainId":8453,
                "verifyingContract":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},
            "types":{"ReceiveWithAuthorization":[{"name":"value","type":"uint256"}]},
            "message":{"value":"1"}});
        let domain = &step(&typed, "base")["typedData"]["types"]["EIP712Domain"];
        assert_eq!(
            domain,
            &json!([{"name":"name","type":"string"},{"name":"version","type":"string"},
                {"name":"chainId","type":"uint256"},{"name":"verifyingContract","type":"address"}])
        );
        // A domain with fewer fields (Hyperliquid-style) gets just those; one already given is kept.
        let short = json!({"primaryType":"X","domain":{"name":"N","chainId":1},"types":{"X":[]},"message":{}});
        assert_eq!(
            step(&short, "base")["typedData"]["types"]["EIP712Domain"],
            json!([{"name":"name","type":"string"},{"name":"chainId","type":"uint256"}])
        );
        let given = json!({"primaryType":"X","domain":{"name":"N"},
            "types":{"X":[],"EIP712Domain":[{"name":"name","type":"string"}]},"message":{}});
        assert_eq!(step(&given, "base")["typedData"]["types"], given["types"]);
    }
}
