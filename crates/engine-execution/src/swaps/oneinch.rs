//! 1inch Classic Swap v6.1 client for Base mainnet tokens.
//! Memes and tokenized stocks share this same ERC-20 route.

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const BASE_CHAIN_ID: u64 = 8453;
const API_BASE: &str = "https://api.1inch.com/swap/v6.1/8453";

#[derive(Clone)]
pub struct OneInchClient {
    http: Client,
    api_key: String,
}

#[derive(Clone, Debug)]
pub struct BaseSwapRequest {
    pub source_token: String,
    pub destination_token: String,
    pub amount_base_units: u128,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OneInchQuote {
    pub dst_amount: String,
    #[serde(default)]
    pub gas: Option<u64>,
    #[serde(default)]
    pub src_token: Option<OneInchToken>,
    #[serde(default)]
    pub dst_token: Option<OneInchToken>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OneInchToken {
    pub address: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub decimals: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OneInchSwap {
    pub dst_amount: String,
    pub tx: OneInchTransaction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OneInchTransaction {
    pub from: String,
    pub to: String,
    pub data: String,
    pub value: String,
    pub gas: u64,
    pub gas_price: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OneInchAllowance {
    pub allowance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OneInchApprovalTransaction {
    pub to: String,
    pub data: String,
    pub value: String,
    pub gas_price: String,
}

#[derive(Debug, Error)]
pub enum OneInchError {
    #[error("1inch API key is required")]
    MissingApiKey,
    #[error("invalid Base swap request")]
    InvalidRequest,
    #[error("1inch request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("1inch returned HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("1inch returned a transaction for the wrong sender")]
    WrongSender,
    #[error("1inch URL could not be parsed")]
    Url(#[from] url::ParseError),
}

impl OneInchClient {
    pub fn new(api_key: String) -> Result<Self, OneInchError> {
        if api_key.is_empty() {
            return Err(OneInchError::MissingApiKey);
        }
        Ok(Self {
            http: Client::new(),
            api_key,
        })
    }

    pub fn chain_id(&self) -> u64 {
        BASE_CHAIN_ID
    }

    pub async fn quote(&self, request: &BaseSwapRequest) -> Result<OneInchQuote, OneInchError> {
        validate(request)?;
        let url = route_url("quote", request)?;
        let response = self.http.get(url).bearer_auth(&self.api_key).send().await?;
        if !response.status().is_success() {
            return Err(OneInchError::Rejected(response.status()));
        }
        Ok(response.json().await?)
    }

    pub async fn allowance(
        &self,
        token_address: &str,
        wallet_address: &str,
    ) -> Result<u128, OneInchError> {
        if !valid_evm_address(token_address) || !valid_evm_address(wallet_address) {
            return Err(OneInchError::InvalidRequest);
        }
        let mut url = Url::parse(&format!("{API_BASE}/approve/allowance"))?;
        url.query_pairs_mut()
            .append_pair("tokenAddress", token_address)
            .append_pair("walletAddress", wallet_address);
        let response = self.http.get(url).bearer_auth(&self.api_key).send().await?;
        if !response.status().is_success() {
            return Err(OneInchError::Rejected(response.status()));
        }
        let body: OneInchAllowance = response.json().await?;
        body.allowance
            .parse()
            .map_err(|_| OneInchError::InvalidRequest)
    }

    /// First-time ERC-20 approval is a separate wallet transaction in Classic
    /// mode. The caller must finish this before requesting swap calldata.
    pub async fn approval_transaction(
        &self,
        token_address: &str,
        amount_base_units: u128,
    ) -> Result<OneInchApprovalTransaction, OneInchError> {
        if !valid_evm_address(token_address) || amount_base_units == 0 {
            return Err(OneInchError::InvalidRequest);
        }
        let mut url = Url::parse(&format!("{API_BASE}/approve/transaction"))?;
        url.query_pairs_mut()
            .append_pair("tokenAddress", token_address)
            .append_pair("amount", &amount_base_units.to_string());
        let response = self.http.get(url).bearer_auth(&self.api_key).send().await?;
        if !response.status().is_success() {
            return Err(OneInchError::Rejected(response.status()));
        }
        Ok(response.json().await?)
    }

    /// Build a fresh transaction after ERC-20 allowance is ready. The app signs
    /// and broadcasts the returned calldata with its Privy Base wallet.
    pub async fn build_swap(
        &self,
        request: &BaseSwapRequest,
        sender: &str,
        slippage_percent: &str,
    ) -> Result<OneInchSwap, OneInchError> {
        validate(request)?;
        if !valid_evm_address(sender) || slippage_percent.parse::<f64>().is_err() {
            return Err(OneInchError::InvalidRequest);
        }
        let slippage = slippage_percent
            .parse::<f64>()
            .map_err(|_| OneInchError::InvalidRequest)?;
        if !(0.0..=50.0).contains(&slippage) {
            return Err(OneInchError::InvalidRequest);
        }
        let mut url = route_url("swap", request)?;
        url.query_pairs_mut()
            .append_pair("from", sender)
            .append_pair("slippage", slippage_percent);
        let response = self.http.get(url).bearer_auth(&self.api_key).send().await?;
        if !response.status().is_success() {
            return Err(OneInchError::Rejected(response.status()));
        }
        let swap: OneInchSwap = response.json().await?;
        if !swap.tx.from.eq_ignore_ascii_case(sender) {
            return Err(OneInchError::WrongSender);
        }
        Ok(swap)
    }
}

fn route_url(path: &str, request: &BaseSwapRequest) -> Result<Url, OneInchError> {
    let mut url = Url::parse(&format!("{API_BASE}/{path}"))?;
    url.query_pairs_mut()
        .append_pair("src", &request.source_token)
        .append_pair("dst", &request.destination_token)
        .append_pair("amount", &request.amount_base_units.to_string());
    Ok(url)
}

fn validate(request: &BaseSwapRequest) -> Result<(), OneInchError> {
    if request.amount_base_units == 0
        || !valid_evm_address(&request.source_token)
        || !valid_evm_address(&request.destination_token)
        || request
            .source_token
            .eq_ignore_ascii_case(&request.destination_token)
    {
        return Err(OneInchError::InvalidRequest);
    }
    Ok(())
}

fn valid_evm_address(address: &str) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_the_only_classic_swap_chain() {
        let client = OneInchClient::new("test-key".into()).unwrap();
        assert_eq!(client.chain_id(), 8453);
        let request = BaseSwapRequest {
            source_token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
            destination_token: "0x4200000000000000000000000000000000000006".into(),
            amount_base_units: 1_000_000,
        };
        let url = route_url("quote", &request).unwrap();
        assert_eq!(url.path(), "/swap/v6.1/8453/quote");
        assert!(validate(&request).is_ok());
    }

    #[test]
    fn malformed_token_and_zero_amount_are_rejected() {
        let mut request = BaseSwapRequest {
            source_token: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
            destination_token: "0x4200000000000000000000000000000000000006".into(),
            amount_base_units: 0,
        };
        assert!(validate(&request).is_err());
        request.amount_base_units = 1;
        request.destination_token = "not an address".into();
        assert!(validate(&request).is_err());
    }
}
