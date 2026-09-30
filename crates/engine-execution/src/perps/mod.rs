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
    v2_base: Url,
}

#[derive(Debug, Error)]
pub enum ParadexError {
    #[error("invalid Paradex environment")]
    InvalidEnvironment,
    #[error("Paradex request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Paradex returned HTTP {0}")]
    Rejected(reqwest::StatusCode),
    #[error("Paradex {operation} returned HTTP {status}: {reason}")]
    RejectedOperation {
        operation: &'static str,
        status: reqwest::StatusCode,
        reason: String,
    },
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

async fn rejected_operation(response: reqwest::Response, operation: &'static str) -> ParadexError {
    let status = response.status();
    let body = response.json::<Value>().await.ok();
    let code = body.as_ref().and_then(|body| {
        body["error"]
            .as_str()
            .or_else(|| body["error"]["code"].as_str())
            .or_else(|| body["code"].as_str())
    });
    let message = body.as_ref().and_then(|body| {
        body["message"]
            .as_str()
            .or_else(|| body["error"]["message"].as_str())
    });
    let reason = [code, message]
        .into_iter()
        .flatten()
        .map(|part| {
            part.chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .take(120)
                .collect::<String>()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    ParadexError::RejectedOperation {
        operation,
        status,
        reason: if reason.is_empty() {
            "request rejected".into()
        } else {
            reason
        },
    }
}

impl ParadexClient {
    pub fn new(environment: &str) -> Result<Self, ParadexError> {
        let url = match environment {
            "prod" => "https://api.prod.paradex.trade/v1/",
            "testnet" => "https://api.testnet.paradex.trade/v1/",
            _ => return Err(ParadexError::InvalidEnvironment),
        };
        let v2_url = url.replace("/v1/", "/v2/");
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(15)).build()?,
            base: Url::parse(url).map_err(|_| ParadexError::InvalidEnvironment)?,
            v2_base: Url::parse(&v2_url).map_err(|_| ParadexError::InvalidEnvironment)?,
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
            .v2_base
            .join("onboarding")
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
            return Err(rejected_operation(response, "account onboarding").await);
        }
        Ok(())
    }
    pub async fn authenticate_evm(
        &self,
        account_address: &str,
        signature: &str,
        siwe_message_base64: &str,
    ) -> Result<String, ParadexError> {
        if !account_address.starts_with("0x")
            || !signature.starts_with("0x")
            || siwe_message_base64.is_empty()
        {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .v2_base
            .join("auth")
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .header("accept", "application/json")
            .header("PARADEX-STARKNET-ACCOUNT", account_address)
            .header("PARADEX-EVM-SIGNATURE", signature)
            .header("PARADEX-SIWE-MESSAGE", siwe_message_base64)
            .json(&serde_json::json!({}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ParadexError::Rejected(response.status()));
        }
        let body: Value = response.json().await?;
        body["jwt_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or(ParadexError::InvalidResponse)
    }
    /// Every perpetual Paradex lists. The venue returns options and spot in the same ~8 MB
    /// response and can't filter it, so callers should cache this.
    pub async fn perp_markets(&self) -> Result<Vec<Value>, ParadexError> {
        let data = self.get("markets", None).await?;
        Ok(data["results"]
            .as_array()
            .ok_or(ParadexError::InvalidResponse)?
            .iter()
            .filter(|market| market["asset_kind"].as_str() == Some("PERP"))
            .cloned()
            .collect())
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

    pub async fn bbo(&self, market: &str) -> Result<Value, ParadexError> {
        self.get(&format!("bbo/{market}"), None).await
    }

    pub async fn account(&self, jwt: &str) -> Result<Value, ParadexError> {
        self.get("account", Some(jwt)).await
    }

    pub async fn fills(
        &self,
        jwt: &str,
        market: &str,
        start_at: u64,
    ) -> Result<Vec<Value>, ParadexError> {
        if !market
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ParadexError::InvalidResponse);
        }
        let data = self
            .get(
                &format!("fills?market={market}&start_at={start_at}&page_size=100"),
                Some(jwt),
            )
            .await?;
        data["results"]
            .as_array()
            .cloned()
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn order_history(
        &self,
        jwt: &str,
        client_id: &str,
    ) -> Result<Vec<Value>, ParadexError> {
        if client_id.is_empty()
            || client_id.len() > 64
            || !client_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(ParadexError::InvalidResponse);
        }
        let data = self
            .get(
                &format!("orders-history?client_id={client_id}&page_size=10"),
                Some(jwt),
            )
            .await?;
        data["results"]
            .as_array()
            .cloned()
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn set_cross_margin(
        &self,
        jwt: &str,
        market: &str,
        leverage: u64,
    ) -> Result<Value, ParadexError> {
        if leverage == 0
            || leverage > 100
            || !market
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .base
            .join(&format!("account/margin/{market}"))
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(jwt)
            .json(&serde_json::json!({"leverage":leverage,"margin_type":"CROSS"}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(rejected_operation(response, "margin setup").await);
        }
        response.json().await.map_err(ParadexError::Transport)
    }

    pub async fn system_config(&self) -> Result<Value, ParadexError> {
        self.get("system/config", None).await
    }

    pub async fn account_balance(&self, jwt: &str) -> Result<Value, ParadexError> {
        self.get("balance", Some(jwt)).await
    }

    pub async fn subkey_exists(&self, jwt: &str, public_key: &str) -> Result<bool, ParadexError> {
        if !public_key.starts_with("0x") || public_key.len() > 66 {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .base
            .join(&format!("account/keys/subkeys/{public_key}"))
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self.http.get(url).bearer_auth(jwt).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !response.status().is_success() {
            return Err(rejected_operation(response, "subkey lookup").await);
        }
        Ok(true)
    }

    pub async fn register_subkey(
        &self,
        jwt: &str,
        public_key: &str,
        evm_signature: &str,
        siwe_message: &str,
    ) -> Result<(), ParadexError> {
        if !public_key.starts_with("0x")
            || !evm_signature.starts_with("0x")
            || siwe_message.is_empty()
        {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .base
            .join("account/keys/subkeys")
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(jwt)
            .json(&serde_json::json!({
                "name":"Atlas trade-only",
                "public_key":public_key,
                "evm_signature":evm_signature,
                "siwe_message":siwe_message
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(rejected_operation(response, "subkey registration").await);
        }
        Ok(())
    }

    pub async fn authenticate_subkey(
        &self,
        account_address: &str,
        public_key: &str,
        signature: &str,
        timestamp: u64,
        expiration: u64,
    ) -> Result<String, ParadexError> {
        if !account_address.starts_with("0x")
            || !public_key.starts_with("0x")
            || !signature.starts_with('[')
            || expiration <= timestamp
        {
            return Err(ParadexError::InvalidResponse);
        }
        let url = self
            .base
            .join(&format!("auth/{public_key}"))
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .header("PARADEX-STARKNET-ACCOUNT", account_address)
            .header("PARADEX-STARKNET-SIGNATURE", signature)
            .header("PARADEX-TIMESTAMP", timestamp.to_string())
            .header("PARADEX-SIGNATURE-EXPIRATION", expiration.to_string())
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(rejected_operation(response, "subkey authentication").await);
        }
        let body: Value = response.json().await?;
        body["jwt_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or(ParadexError::InvalidResponse)
    }

    pub async fn submit_order(&self, jwt: &str, order: &Value) -> Result<Value, ParadexError> {
        let url = self
            .base
            .join("orders")
            .map_err(|_| ParadexError::InvalidResponse)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(jwt)
            .json(order)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(rejected_operation(response, "order submission").await);
        }
        response.json().await.map_err(ParadexError::Transport)
    }

    pub async fn order(&self, jwt: &str, order_id: &str) -> Result<Value, ParadexError> {
        if order_id.is_empty()
            || order_id.len() > 128
            || !order_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ParadexError::InvalidResponse);
        }
        self.get(&format!("orders/{order_id}"), Some(jwt)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evm_endpoints_use_v2_root() {
        for environment in ["testnet", "prod"] {
            let client = ParadexClient::new(environment).unwrap();
            assert_eq!(client.base.path(), "/v1/");
            assert_eq!(client.v2_base.path(), "/v2/");
            assert_eq!(
                client.v2_base.join("onboarding").unwrap().path(),
                "/v2/onboarding"
            );
            assert_eq!(client.v2_base.join("auth").unwrap().path(), "/v2/auth");
        }
    }
}
