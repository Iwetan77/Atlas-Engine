//! Read-only Circle Gateway API client.
//!
//! Wallet-facing code uses Circle Unified Balance Kit for deposit() and
//! spend(). This client reads Circle's own balances, pending deposits, and
//! transfer status; it never constructs or submits a burn intent.

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayEnvironment {
    Testnet,
    Mainnet,
}

impl GatewayEnvironment {
    fn base_url(self) -> &'static str {
        match self {
            Self::Testnet => "https://gateway-api-testnet.circle.com",
            Self::Mainnet => "https://gateway-api.circle.com",
        }
    }
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("Gateway request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Gateway URL could not be parsed")]
    Url(#[from] url::ParseError),
    #[error("Gateway rejected the request with HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("Gateway returned an unsuccessful response")]
    Unsuccessful,
    #[error("Gateway transfer ID is invalid")]
    InvalidTransferId,
}

#[derive(Clone)]
pub struct GatewayClient {
    http: Client,
    base: Url,
}

#[derive(Clone, Debug, Serialize)]
pub struct GatewaySource {
    pub depositor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<u32>,
}

#[derive(Serialize)]
struct SourcesRequest<'a> {
    token: &'static str,
    sources: &'a [GatewaySource],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayBalanceEntry {
    pub domain: u32,
    pub depositor: String,
    pub balance: String,
    #[serde(default)]
    pub pending_batch: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayBalances {
    pub token: String,
    pub balances: Vec<GatewayBalanceEntry>,
    #[serde(default)]
    pub success: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayDeposit {
    pub domain: u32,
    pub depositor: String,
    pub transaction_hash: String,
    pub amount: String,
    pub status: String,
    #[serde(default)]
    pub block_height: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GatewayDeposits {
    pub token: String,
    pub deposits: Vec<GatewayDeposit>,
    #[serde(default)]
    pub success: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayTransfer {
    pub status: String,
    #[serde(default)]
    pub destination_domain: Option<u32>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub message: Option<String>,
}

impl GatewayClient {
    pub fn new(environment: GatewayEnvironment) -> Result<Self, GatewayError> {
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            base: Url::parse(environment.base_url())?,
        })
    }

    pub async fn balances(
        &self,
        sources: &[GatewaySource],
    ) -> Result<GatewayBalances, GatewayError> {
        let response = self
            .http
            .post(self.base.join("/v1/balances")?)
            .json(&SourcesRequest {
                token: "USDC",
                sources,
            })
            .send()
            .await?;
        let response = accepted(response)?;
        let body: GatewayBalances = response.json().await?;
        if body.success == Some(false) {
            return Err(GatewayError::Unsuccessful);
        }
        Ok(body)
    }

    pub async fn deposits(
        &self,
        sources: &[GatewaySource],
    ) -> Result<GatewayDeposits, GatewayError> {
        let response = self
            .http
            .post(self.base.join("/v1/deposits")?)
            .json(&SourcesRequest {
                token: "USDC",
                sources,
            })
            .send()
            .await?;
        let response = accepted(response)?;
        let body: GatewayDeposits = response.json().await?;
        if body.success == Some(false) {
            return Err(GatewayError::Unsuccessful);
        }
        Ok(body)
    }

    pub async fn transfer(&self, transfer_id: &str) -> Result<GatewayTransfer, GatewayError> {
        if transfer_id.is_empty()
            || !transfer_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(GatewayError::InvalidTransferId);
        }
        let response = self
            .http
            .get(self.base.join(&format!("/v1/transfer/{transfer_id}"))?)
            .send()
            .await?;
        let body: GatewayTransfer = accepted(response)?.json().await?;
        if body.success == Some(false) {
            return Err(GatewayError::Unsuccessful);
        }
        Ok(body)
    }
}

fn accepted(response: reqwest::Response) -> Result<reqwest::Response, GatewayError> {
    if !response.status().is_success() {
        return Err(GatewayError::Rejected(response.status()));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_pending_deposit_shape_keeps_block_height_as_string() {
        let body = r#"{"token":"USDC","deposits":[{"depositor":"0x123","domain":6,"transactionHash":"0xabc","amount":"5000000","status":"pending","blockHeight":"47415771"}]}"#;
        let response: GatewayDeposits = serde_json::from_str(body).unwrap();
        assert_eq!(response.deposits[0].amount, "5000000");
        assert_eq!(
            response.deposits[0].block_height.as_deref(),
            Some("47415771")
        );
    }
    #[test]
    fn balance_query_uses_circle_gateway_sources_shape() {
        let source = GatewaySource {
            depositor: "0x123".into(),
            domain: Some(6),
        };
        let request = SourcesRequest {
            token: "USDC",
            sources: &[source],
        };
        let body = serde_json::to_value(request).unwrap();
        assert_eq!(body["token"], "USDC");
        assert_eq!(body["sources"][0]["domain"], 6);
        assert_eq!(body["sources"][0]["depositor"], "0x123");
    }
}
