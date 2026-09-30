//! Layerswap moves USDC between the user's Base wallet and their Paradex account, so perps margin
//! comes from the Atlas balance. No API key: the public v2 API creates swaps and reports status.
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

use crate::swaps::uniswap::BASE_USDC;

const API: &str = "https://api.layerswap.io/api/v2/";
const BASE_CHAIN_ID: &str = "8453";
// ERC-20 transfer(address,uint256).
const TRANSFER_SELECTOR: &str = "a9059cbb";

#[derive(Debug, Error)]
pub enum LayerswapError {
    #[error("Layerswap request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Layerswap returned HTTP {status}: {reason}")]
    Rejected {
        status: reqwest::StatusCode,
        reason: String,
    },
    #[error("Layerswap returned an unexpected deposit: {0}")]
    InvalidResponse(&'static str),
}

#[derive(Clone)]
pub struct LayerswapClient {
    http: Client,
    base: Url,
}

/// The one Base transaction that funds a swap: a USDC transfer whose calldata carries Layerswap's
/// swap reference after the ERC-20 arguments. It must be sent byte-for-byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseDeposit {
    pub swap_id: String,
    pub to: String,
    pub data: String,
    pub amount_units: u128,
}

/// Where a swap stands. `Completed` means the destination received the funds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwapState {
    Waiting,
    Completed,
    Failed(String),
}

impl LayerswapClient {
    pub fn new() -> Result<Self, LayerswapError> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(20)).build()?,
            base: Url::parse(API).map_err(|_| LayerswapError::InvalidResponse("base URL"))?,
        })
    }

    /// Creates a Base → Paradex USDC swap and returns the deposit transaction to sign.
    /// `amount_units` is USDC with 6 decimals; the reference ties the swap to an Atlas intent.
    pub async fn base_to_paradex(
        &self,
        source_address: &str,
        paradex_account: &str,
        amount_units: u128,
        reference: &str,
    ) -> Result<BaseDeposit, LayerswapError> {
        let amount: serde_json::Number = usdc_decimal(amount_units)
            .parse()
            .map_err(|_| LayerswapError::InvalidResponse("amount"))?;
        let url = self
            .base
            .join("swaps")
            .map_err(|_| LayerswapError::InvalidResponse("URL"))?;
        let response = self
            .http
            .post(url)
            .json(&json!({
                "source_network":"BASE_MAINNET","source_token":"USDC",
                "destination_network":"PARADEX_MAINNET","destination_token":"USDC",
                "amount":amount,"source_address":source_address,
                "destination_address":paradex_account,
                "use_deposit_address":false,"reference_id":reference
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_base_deposit(&body, amount_units)
    }

    pub async fn swap_state(&self, swap_id: &str) -> Result<SwapState, LayerswapError> {
        let url = self
            .base
            .join(&format!("swaps/{swap_id}"))
            .map_err(|_| LayerswapError::InvalidResponse("URL"))?;
        let body = checked(self.http.get(url).send().await?).await?;
        Ok(parse_swap_state(&body))
    }
}

async fn checked(response: reqwest::Response) -> Result<Value, LayerswapError> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() || !body["error"].is_null() {
        let reason = body["error"]["message"]
            .as_str()
            .unwrap_or("no reason given")
            .chars()
            .take(200)
            .collect();
        return Err(LayerswapError::Rejected { status, reason });
    }
    Ok(body)
}

fn usdc_decimal(units: u128) -> String {
    format!("{}.{:06}", units / 1_000_000, units % 1_000_000)
}

fn parse_base_deposit(body: &Value, amount_units: u128) -> Result<BaseDeposit, LayerswapError> {
    let data = &body["data"];
    let swap_id = data["swap"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or(LayerswapError::InvalidResponse("swap id"))?;
    let actions = data["deposit_actions"]
        .as_array()
        .ok_or(LayerswapError::InvalidResponse("deposit actions"))?;
    let [action] = actions.as_slice() else {
        return Err(LayerswapError::InvalidResponse(
            "expected one deposit action",
        ));
    };
    let chain_id = match &action["network"]["chain_id"] {
        Value::String(id) => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => String::new(),
    };
    if action["type"].as_str() != Some("transfer")
        || action["network"]["name"].as_str() != Some("BASE_MAINNET")
        || chain_id != BASE_CHAIN_ID
    {
        return Err(LayerswapError::InvalidResponse("not a Base transfer"));
    }
    let to = action["to_address"]
        .as_str()
        .ok_or(LayerswapError::InvalidResponse("to address"))?
        .to_ascii_lowercase();
    if to != BASE_USDC.to_ascii_lowercase() {
        return Err(LayerswapError::InvalidResponse("not a USDC transfer"));
    }
    // The ERC-20 call moves the tokens; the native value must be zero.
    if action["amount_in_base_units"].as_str().unwrap_or("0") != "0" {
        return Err(LayerswapError::InvalidResponse("asks for native value"));
    }
    let data_hex = action["call_data"]
        .as_str()
        .ok_or(LayerswapError::InvalidResponse("call data"))?
        .to_ascii_lowercase();
    let body_hex = data_hex
        .strip_prefix("0x")
        .filter(|hex| hex.len() >= 8 + 128 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(LayerswapError::InvalidResponse("call data"))?;
    if !body_hex.starts_with(TRANSFER_SELECTOR) || !body_hex[8..32].bytes().all(|b| b == b'0') {
        return Err(LayerswapError::InvalidResponse("not an ERC-20 transfer"));
    }
    let amount = u128::from_str_radix(&body_hex[72..136], 16)
        .map_err(|_| LayerswapError::InvalidResponse("transfer amount"))?;
    if amount != amount_units {
        return Err(LayerswapError::InvalidResponse("transfer amount differs"));
    }
    Ok(BaseDeposit {
        swap_id: swap_id.into(),
        to,
        data: data_hex,
        amount_units,
    })
}

fn parse_swap_state(body: &Value) -> SwapState {
    let swap = &body["data"]["swap"];
    match swap["status"].as_str().unwrap_or_default() {
        "completed" => SwapState::Completed,
        "failed" | "expired" | "pending_refund" | "refunded" => SwapState::Failed(
            swap["fail_reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .unwrap_or(swap["status"].as_str().unwrap_or("failed"))
                .into(),
        ),
        _ => SwapState::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from Layerswap's live API on 2026-09-30 (a 20 USDC Base → Paradex swap).
    fn live_response() -> Value {
        json!({"data":{"swap":{"id":"52507d08-1caf-43c6-923d-bde9ba5efd80","status":"user_transfer_pending"},
            "deposit_actions":[{"type":"transfer","to_address":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            "amount":0,"amount_in_base_units":"0","order":0,
            "call_data":"0xa9059cbb0000000000000000000000002fc617e933a52713247ce25730f6695920b3befe0000000000000000000000000000000000000000000000000000000001312d00c8d430a1a75548f49ec3d99fa6ab58872a50760c10edb427000000000114eb3c",
            "network":{"name":"BASE_MAINNET","chain_id":"8453"},"token":{"symbol":"USDC"}}]},"error":null})
    }

    #[test]
    fn accepts_the_live_deposit_shape_exactly() {
        let deposit = parse_base_deposit(&live_response(), 20_000_000).unwrap();
        assert_eq!(deposit.swap_id, "52507d08-1caf-43c6-923d-bde9ba5efd80");
        assert_eq!(deposit.to, BASE_USDC.to_ascii_lowercase());
        assert!(deposit
            .data
            .ends_with("c8d430a1a75548f49ec3d99fa6ab58872a50760c10edb427000000000114eb3c"));
        assert_eq!(usdc_decimal(20_000_000), "20.000000");
        assert_eq!(usdc_decimal(2_500_001), "2.500001");
    }

    #[test]
    fn refuses_deposits_that_differ_from_the_request() {
        assert!(parse_base_deposit(&live_response(), 19_000_000).is_err());
        let mut wrong_chain = live_response();
        wrong_chain["data"]["deposit_actions"][0]["network"]["chain_id"] = json!("1");
        assert!(parse_base_deposit(&wrong_chain, 20_000_000).is_err());
        let mut wrong_token = live_response();
        wrong_token["data"]["deposit_actions"][0]["to_address"] =
            json!("0x0000000000000000000000000000000000000001");
        assert!(parse_base_deposit(&wrong_token, 20_000_000).is_err());
        let mut native = live_response();
        native["data"]["deposit_actions"][0]["amount_in_base_units"] = json!("1000");
        assert!(parse_base_deposit(&native, 20_000_000).is_err());
        let mut two = live_response();
        let action = two["data"]["deposit_actions"][0].clone();
        two["data"]["deposit_actions"] = json!([action.clone(), action]);
        assert!(parse_base_deposit(&two, 20_000_000).is_err());
    }

    #[test]
    fn swap_states() {
        let state = |status: &str| parse_swap_state(&json!({"data":{"swap":{"status":status}}}));
        assert_eq!(state("completed"), SwapState::Completed);
        assert_eq!(state("user_transfer_pending"), SwapState::Waiting);
        assert_eq!(state("ls_transfer_pending"), SwapState::Waiting);
        assert_eq!(state("refunded"), SwapState::Failed("refunded".into()));
    }

    // Network: creates (and abandons) a real swap record. Nothing moves without a deposit.
    // cargo test -p engine-execution live_layerswap -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_layerswap_base_to_paradex_deposit() {
        let client = LayerswapClient::new().unwrap();
        let deposit = client
            .base_to_paradex(
                "0x845c22a46398E0a702733e556bEB6aFcB2E92132",
                "0x287dd502cd9e5e6267f1aeeaf577db69e7cf71b7fd8f29118de2e37104e17eb",
                12_345_678,
                "atlas-live-check",
            )
            .await
            .unwrap();
        println!("{deposit:?}");
        assert_eq!(
            client.swap_state(&deposit.swap_id).await.unwrap(),
            SwapState::Waiting
        );
    }
}
