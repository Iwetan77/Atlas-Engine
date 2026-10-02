//! Jupiter Swap API V2 client for one Solana token path.
//! Spot, memes and tokenized equities differ only by their mint addresses.

use std::{str::FromStr, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

const ORDER_URL: &str = "https://api.jup.ag/swap/v2/order";
const EXECUTE_URL: &str = "https://api.jup.ag/swap/v2/execute";
// Jupiter covers the fee itself for a low-SOL taker, but only above a minimum ("Minimum $5 for
// gasless"); below it the order comes back with this code and no transaction.
const BELOW_GASLESS_MINIMUM: i64 = 3;
// The taker can't pay what the swap costs (its tokens, or the SOL for fees and a new token account).
const INSUFFICIENT_FUNDS: i64 = 1;

#[derive(Clone)]
pub struct JupiterClient {
    http: Client,
    api_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct JupiterOrderRequest {
    pub input_mint: String,
    pub output_mint: String,
    pub amount_base_units: u64,
    pub taker: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterOrder {
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub request_id: String,
    pub router: String,
    pub transaction: Option<String>,
    #[serde(default)]
    pub fee_bps: Option<u32>,
    #[serde(default)]
    pub fee_mint: Option<String>,
    #[serde(default)]
    pub error_code: Option<i64>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "optional_height")]
    pub last_valid_block_height: Option<u64>,
    /// True when Jupiter pays the network fee and account rent (low-SOL takers on eligible routes).
    #[serde(default)]
    pub gasless: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterExecution {
    pub status: String,
    #[serde(default)]
    pub signature: String,
    pub code: i64,
    #[serde(default)]
    pub total_input_amount: Option<String>,
    #[serde(default)]
    pub total_output_amount: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Error)]
pub enum JupiterError {
    #[error("invalid Jupiter swap mints, taker, or amount")]
    InvalidRequest,
    #[error("signed transaction is not base64")]
    InvalidTransaction,
    #[error("Jupiter returned a quote that cannot be executed: {0}")]
    NotExecutable(String),
    #[error("Jupiter request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Jupiter {operation} returned HTTP {status}: {reason}")]
    Rejected {
        operation: &'static str,
        status: reqwest::StatusCode,
        reason: String,
    },
    #[error("Jupiter could not complete this swap (code {code}): {reason}")]
    ExecutionFailed { code: i64, reason: String },
    #[error("Jupiter URL could not be parsed")]
    Url(#[from] url::ParseError),
}

impl JupiterClient {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: Client::new(),
            api_key,
        }
    }

    /// Without a taker this returns a real quote but no signable transaction.
    /// A fresh order with the Privy wallet address must be fetched at confirm.
    pub async fn order(&self, request: &JupiterOrderRequest) -> Result<JupiterOrder, JupiterError> {
        validate_order(request)?;
        let order = self.fetch_order(request, None).await?;
        if request.taker.is_some() && order.transaction.as_deref().unwrap_or("").is_empty() {
            return Err(JupiterError::NotExecutable(not_executable_reason(&order)));
        }
        Ok(order)
    }

    // `payer` only sponsors gas when it differs from the taker; naming the taker has no effect.
    async fn fetch_order(
        &self,
        request: &JupiterOrderRequest,
        payer: Option<&str>,
    ) -> Result<JupiterOrder, JupiterError> {
        let mut url = Url::parse(ORDER_URL)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("inputMint", &request.input_mint);
            query.append_pair("outputMint", &request.output_mint);
            query.append_pair("amount", &request.amount_base_units.to_string());
            if let Some(taker) = &request.taker {
                query.append_pair("taker", taker);
            }
            if let Some(payer) = payer {
                query.append_pair("payer", payer);
            }
        }
        let mut call = self.http.get(url).timeout(Duration::from_secs(20));
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(rejected("quote", response).await);
        }
        Ok(response.json().await?)
    }

    /// The app signs the order with Privy; Jupiter lands the signed transaction.
    pub async fn execute(
        &self,
        request_id: &str,
        signed_transaction: &str,
    ) -> Result<JupiterExecution, JupiterError> {
        if request_id.is_empty() {
            return Err(JupiterError::InvalidRequest);
        }
        if STANDARD.decode(signed_transaction).is_err() {
            return Err(JupiterError::InvalidTransaction);
        }
        let mut call = self.http.post(EXECUTE_URL).json(&serde_json::json!({
            "requestId": request_id,
            "signedTransaction": signed_transaction,
        }));
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(rejected("submission", response).await);
        }
        let result: JupiterExecution = response.json().await?;
        if result.status != "Success" || result.code != 0 {
            return Err(JupiterError::ExecutionFailed {
                code: result.code,
                reason: safe_reason(
                    result
                        .error
                        .as_deref()
                        .unwrap_or("Check your transaction status before trying again."),
                ),
            });
        }
        Ok(result)
    }
}

// Jupiter offered to pay the fee but the swap is below its minimum for that.
fn own_gas_needed(order: &JupiterOrder) -> bool {
    order.router != "jupiterz"
        && order.error_code == Some(BELOW_GASLESS_MINIMUM)
        && order.transaction.as_deref().unwrap_or("").is_empty()
}

// What the user reads when Jupiter can't make the swap.
fn not_executable_reason(order: &JupiterOrder) -> String {
    if own_gas_needed(order) {
        return order.error_message.clone().unwrap_or_else(|| {
            "This amount is below the route's minimum for a fee-paid swap.".into()
        });
    }
    match order.error_code {
        Some(INSUFFICIENT_FUNDS) => {
            "There isn't enough in this wallet for the swap and its network fee (paid in SOL)"
                .into()
        }
        _ => order
            .error_message
            .clone()
            .unwrap_or_else(|| format!("code {:?}", order.error_code)),
    }
}

// Some routers send the block height as a JSON string, others as a number or null.
fn optional_height<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    v.map(|v| {
        v.as_u64()
            .or_else(|| v.as_str()?.parse().ok())
            .ok_or_else(|| serde::de::Error::custom("invalid block height"))
    })
    .transpose()
}
fn safe_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .map(|w| if w.len() > 100 { "[omitted]" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}
async fn rejected(operation: &'static str, response: reqwest::Response) -> JupiterError {
    let status = response.status();
    let mut response = response;
    // Read only a small error body; never expose the request, signed bytes or partner key.
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > 4096 {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default();
    let reason = body["errorMessage"]
        .as_str()
        .or_else(|| body["error"].as_str())
        .or_else(|| body["message"].as_str())
        .filter(|s| {
            !s.to_ascii_lowercase().contains("signedtransaction")
                && !s.to_ascii_lowercase().contains("api-key")
        })
        .map(safe_reason)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            "The venue rejected this request. Check the transaction status before retrying.".into()
        });
    let reason = if let Some(code) = body["code"].as_i64() {
        format!("{reason} (code {code})")
    } else {
        reason
    };
    JupiterError::Rejected {
        operation,
        status,
        reason,
    }
}

fn validate_order(request: &JupiterOrderRequest) -> Result<(), JupiterError> {
    if request.amount_base_units == 0
        || request.input_mint == request.output_mint
        || Pubkey::from_str(&request.input_mint).is_err()
        || Pubkey::from_str(&request.output_mint).is_err()
        || request
            .taker
            .as_deref()
            .is_some_and(|address| Pubkey::from_str(address).is_err())
    {
        return Err(JupiterError::InvalidRequest);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_whether_jupiter_pays_the_gas() {
        // Captured 2026-09-30: $0.50 USDC → SOL for a low-SOL taker, filled by JupiterZ.
        let order: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","outputMint":"So11111111111111111111111111111111111111112",
            "inAmount":"500000","outAmount":"4200538","requestId":"r","router":"jupiterz","transaction":"tx","gasless":true
        })).unwrap();
        assert!(order.gasless);
        let older: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r","router":"metis","transaction":null
        })).unwrap();
        assert!(!older.gasless);
    }

    #[test]
    fn reads_string_block_heights_and_does_not_misclassify_jupiterz() {
        let order: JupiterOrder=serde_json::from_value(serde_json::json!({"inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r","router":"jupiterz","transaction":null,"errorCode":3,"lastValidBlockHeight":"350001234"})).unwrap();
        assert_eq!(order.last_valid_block_height, Some(350001234));
        assert!(!own_gas_needed(&order));
        assert_eq!(safe_reason(&"a".repeat(200)), "[omitted]");
    }
    use super::*;

    #[test]
    fn meme_and_stock_mints_use_the_same_order_shape() {
        let request = JupiterOrderRequest {
            input_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            amount_base_units: 1_000_000,
            taker: None,
        };
        assert!(validate_order(&request).is_ok());
        let mut invalid = request.clone();
        invalid.amount_base_units = 0;
        assert!(matches!(
            validate_order(&invalid),
            Err(JupiterError::InvalidRequest)
        ));
    }

    #[test]
    fn gasless_minimum_is_reported_only_for_aggregator_routes() {
        // Shape from Jupiter's /order for a ₦500 (about $0.34) buy by a low-SOL taker, 2026-10-02.
        let refused: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"340000","outAmount":"1","requestId":"r",
            "router":"metis","transaction":null,"gasless":true,"errorCode":3,
            "errorMessage":"Minimum $5 for gasless"
        }))
        .unwrap();
        assert!(own_gas_needed(&refused));
        // A normally funded aggregator route comes back with a signable transaction.
        let own: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"340000","outAmount":"1","requestId":"r",
            "router":"okx","transaction":"tx","gasless":false
        }))
        .unwrap();
        assert!(!own_gas_needed(&own));
        let broke: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r",
            "router":"okx","transaction":null,"errorCode":1,"errorMessage":"Insufficient funds"
        }))
        .unwrap();
        assert!(!own_gas_needed(&broke));
        assert!(not_executable_reason(&broke).contains("network fee"));
    }

    #[test]
    fn quote_only_order_has_no_signable_transaction() {
        let sample = r#"{"inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","outputMint":"So11111111111111111111111111111111111111112","inAmount":"1000000","outAmount":"8415352","requestId":"real-id","router":"jupiterz","transaction":null,"feeMint":"So11111111111111111111111111111111111111112","feeBps":2}"#;
        let order: JupiterOrder = serde_json::from_str(sample).unwrap();
        assert_eq!(order.out_amount, "8415352");
        assert!(order.transaction.is_none());
    }
}
