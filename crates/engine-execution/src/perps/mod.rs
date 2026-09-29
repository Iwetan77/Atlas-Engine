//! Paradex venue reads. Orders require an account-scoped trade-only signer.
use reqwest::{Client, Url};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use thiserror::Error;

#[derive(Clone)]
pub struct ParadexClient {
    http: Client,
    base: Url,
}

#[derive(Debug, Error)]
pub enum ParadexError {
    #[error("invalid Paradex environment")]
    InvalidEnvironment,
    #[error("Paradex request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Paradex returned HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("Paradex returned malformed venue data")]
    InvalidResponse,
    #[error("invalid Ethereum wallet address")]
    InvalidWalletAddress,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStatus {
    pub wallet_address: String,
    pub account_address: String,
    pub exists: bool,
}

impl ParadexClient {
    pub fn new(environment: &str) -> Result<Self, ParadexError> {
        let url = match environment {
            "prod" => "https://api.prod.paradex.trade/v1/",
            "testnet" => "https://api.testnet.paradex.trade/v1/",
            _ => return Err(ParadexError::InvalidEnvironment),
        };
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(15)).build()?,
            base: Url::parse(url).map_err(|_| ParadexError::InvalidEnvironment)?,
        })
    }

    async fn get(&self, path: &str, jwt: Option<&str>) -> Result<Value, ParadexError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| ParadexError::InvalidResponse)?;
        let mut call = self.http.get(url).header("accept", "application/json");
        if let Some(jwt) = jwt {
            call = call.bearer_auth(jwt);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(ParadexError::Rejected(response.status()));
        }
        response.json().await.map_err(ParadexError::Transport)
    }

    pub async fn onboarding_status(
        &self,
        wallet_address: &str,
    ) -> Result<OnboardingStatus, ParadexError> {
        if wallet_address.len() != 42
            || !wallet_address.starts_with("0x")
            || !wallet_address[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ParadexError::InvalidWalletAddress);
        }
        let mut url = self
            .base
            .join("onboarding")
            .map_err(|_| ParadexError::InvalidResponse)?;
        url.query_pairs_mut()
            .append_pair("account_signer_type", "eip191")
            .append_pair("eth_address", wallet_address);
        let response = self
            .http
            .get(url)
            .header("accept", "application/json")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ParadexError::Rejected(response.status()));
        }
        let body: Value = response.json().await?;
        let account_address = body["address"]
            .as_str()
            .filter(|address| address.starts_with("0x") && address.len() > 2)
            .ok_or(ParadexError::InvalidResponse)?;
        let exists = body["exists"]
            .as_bool()
            .ok_or(ParadexError::InvalidResponse)?;
        Ok(OnboardingStatus {
            wallet_address: wallet_address.to_owned(),
            account_address: account_address.to_owned(),
            exists,
        })
    }
    pub async fn onboard_evm(
        &self,
        account_address: &str,
        signature: &str,
        siwe_message_base64: &str,
        public_key: &str,
    ) -> Result<(), ParadexError> {
        if !account_address.starts_with("0x")
            || !signature.starts_with("0x")
            || !public_key.starts_with("0x04")
            || public_key.len() != 132
            || siwe_message_base64.is_empty()
        {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .base
            .join("v2/onboarding")
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .header("accept", "application/json")
            .header("PARADEX-STARKNET-ACCOUNT", account_address)
            .header("PARADEX-EVM-SIGNATURE", signature)
            .header("PARADEX-SIWE-MESSAGE", siwe_message_base64)
            .json(&serde_json::json!({"public_key": public_key}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ParadexError::Rejected(response.status()));
        }
        Ok(())
    }
    pub async fn market(&self, market: &str) -> Result<Value, ParadexError> {
        let data = self.get(&format!("markets?market={market}"), None).await?;
        data["results"]
            .as_array()
            .and_then(|v| v.first())
            .cloned()
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn summary(&self, market: &str) -> Result<Value, ParadexError> {
        let data = self
            .get(&format!("markets/summary?market={market}"), None)
            .await?;
        data["results"]
            .as_array()
            .and_then(|v| v.first())
            .cloned()
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn funding(&self, market: &str) -> Result<Option<Value>, ParadexError> {
        let data = self
            .get(&format!("funding/data?market={market}&page_size=1"), None)
            .await?;
        data["results"]
            .as_array()
            .map(|v| v.first().cloned())
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn positions(&self, jwt: &str) -> Result<Vec<Value>, ParadexError> {
        let data = self.get("positions", Some(jwt)).await?;
        data["results"]
            .as_array()
            .cloned()
            .ok_or(ParadexError::InvalidResponse)
    }
}
