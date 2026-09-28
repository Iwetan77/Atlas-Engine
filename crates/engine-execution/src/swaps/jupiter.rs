//! Jupiter Swap API V2 client for one Solana token path.
//! Spot, memes and tokenized equities differ only by their mint addresses.

use std::str::FromStr;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

const ORDER_URL: &str = "https://api.jup.ag/swap/v2/order";
const EXECUTE_URL: &str = "https://api.jup.ag/swap/v2/execute";

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
    pub last_valid_block_height: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterExecution {
    pub status: String,
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
    #[error("Jupiter returned a quote that cannot be executed")]
    NotExecutable,
    #[error("Jupiter request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Jupiter returned HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("Jupiter reported a failed on-chain swap: code {0}")]
    ExecutionFailed(i64),
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
        let mut url = Url::parse(ORDER_URL)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("inputMint", &request.input_mint);
            query.append_pair("outputMint", &request.output_mint);
            query.append_pair("amount", &request.amount_base_units.to_string());
            if let Some(taker) = &request.taker {
                query.append_pair("taker", taker);
            }
        }
        let mut call = self.http.get(url);
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(JupiterError::Rejected(response.status()));
        }
        let order: JupiterOrder = response.json().await?;
        if request.taker.is_some() && order.transaction.as_deref().unwrap_or("").is_empty() {
            return Err(JupiterError::NotExecutable);
        }
        Ok(order)
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
            return Err(JupiterError::Rejected(response.status()));
        }
        let result: JupiterExecution = response.json().await?;
        if result.status != "Success" || result.code != 0 {
            return Err(JupiterError::ExecutionFailed(result.code));
        }
        Ok(result)
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
    fn quote_only_order_has_no_signable_transaction() {
        let sample = r#"{"inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","outputMint":"So11111111111111111111111111111111111111112","inAmount":"1000000","outAmount":"8415352","requestId":"real-id","router":"jupiterz","transaction":null,"feeMint":"So11111111111111111111111111111111111111112","feeBps":2}"#;
        let order: JupiterOrder = serde_json::from_str(sample).unwrap();
        assert_eq!(order.out_amount, "8415352");
        assert!(order.transaction.is_none());
    }
}
