//! Paradex venue reads. Orders require an account-scoped trade-only signer.
use reqwest::{Client, Url};
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
