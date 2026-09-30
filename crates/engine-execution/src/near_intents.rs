use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
const API: &str = "https://1click.chaindefuser.com/v0";
#[derive(Debug, Error)]
pub enum Error {
    #[error("1Click request: {0}")]
    Http(#[from] reqwest::Error),
    #[error("1Click HTTP {0}: {1}")]
    Venue(reqwest::StatusCode, String),
    #[error("1Click quote lacks a safe deposit route")]
    InvalidQuote,
}
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    key: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub asset_id: String,
    pub blockchain: String,
    pub symbol: String,
    pub decimals: u32,
    pub contract_address: Option<String>,
    pub price: Option<serde_json::Value>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest<'a> {
    pub dry: bool,
    pub swap_type: &'static str,
    pub slippage_tolerance: u32,
    pub origin_asset: &'a str,
    pub deposit_type: &'static str,
    pub destination_asset: &'a str,
    pub amount: &'a str,
    pub recipient: &'a str,
    pub recipient_type: &'static str,
    pub refund_to: &'a str,
    pub refund_type: &'static str,
    pub deadline: &'a str,
}
impl<'a> QuoteRequest<'a> {
    pub fn exact_input(
        origin_asset: &'a str,
        destination_asset: &'a str,
        amount: &'a str,
        recipient: &'a str,
        refund_to: &'a str,
        deadline: &'a str,
        dry: bool,
    ) -> Self {
        Self {
            dry,
            swap_type: "EXACT_INPUT",
            slippage_tolerance: 100,
            origin_asset,
            deposit_type: "ORIGIN_CHAIN",
            destination_asset,
            amount,
            recipient,
            recipient_type: "DESTINATION_CHAIN",
            refund_to,
            refund_type: "ORIGIN_CHAIN",
            deadline,
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub amount_in: String,
    pub amount_out: String,
    pub min_amount_out: Option<String>,
    pub deposit_address: Option<String>,
    pub deposit_memo: Option<String>,
    pub time_estimate: Option<u64>,
    pub deadline: Option<String>,
}
#[derive(Deserialize)]
pub struct QuoteResponse {
    pub quote: Quote,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub status: String,
    pub swap_details: Option<SwapDetails>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapDetails {
    #[serde(default)]
    pub destination_chain_tx_hashes: Vec<ChainTx>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainTx {
    pub hash: String,
    pub explorer_url: Option<String>,
}
impl Client {
    pub fn new(key: Option<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("atlas-engine")
                .build()?,
            key,
        })
    }
    pub async fn tokens(&self) -> Result<Vec<Token>, Error> {
        self.decode(
            self.auth(self.http.get(format!("{API}/tokens")))
                .send()
                .await?,
        )
        .await
    }
    pub async fn quote(&self, req: &QuoteRequest<'_>) -> Result<Quote, Error> {
        let response: QuoteResponse = self
            .decode(
                self.auth(self.http.post(format!("{API}/quote")))
                    .json(req)
                    .send()
                    .await?,
            )
            .await?;
        let q = response.quote;
        if q.amount_in
            .parse::<u128>()
            .ok()
            .filter(|n| *n > 0)
            .is_none()
            || q.amount_out
                .parse::<u128>()
                .ok()
                .filter(|n| *n > 0)
                .is_none()
            || (!req.dry && q.deposit_address.as_deref().unwrap_or("").is_empty())
        {
            return Err(Error::InvalidQuote);
        }
        Ok(q)
    }
    pub async fn status(
        &self,
        deposit_address: &str,
        deposit_memo: Option<&str>,
    ) -> Result<Status, Error> {
        let mut params = vec![("depositAddress", deposit_address)];
        if let Some(memo) = deposit_memo {
            params.push(("depositMemo", memo));
        }
        self.decode(
            self.auth(self.http.get(format!("{API}/status")))
                .query(&params)
                .send()
                .await?,
        )
        .await
    }
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.key {
            req.header("X-API-Key", key)
        } else {
            req
        }
    }
    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, Error> {
        if !response.status().is_success() {
            let code = response.status();
            return Err(Error::Venue(
                code,
                response.text().await?.chars().take(500).collect(),
            ));
        }
        Ok(response.json().await?)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_status_has_destination_hash_objects() {
        let result: Status = serde_json::from_value(serde_json::json!({
            "status":"SUCCESS","swapDetails":{"destinationChainTxHashes":[
                {"hash":"0xabc","explorerUrl":"https://example.com/tx/0xabc"}]}}))
        .unwrap();
        assert_eq!(
            result.swap_details.unwrap().destination_chain_tx_hashes[0].hash,
            "0xabc"
        );
    }
    #[test]
    fn quote_request_is_exact_input_and_origin_chain() {
        let r = QuoteRequest::exact_input(
            "usdc",
            "mon",
            "1000000",
            "recipient",
            "refund",
            "2026-10-01T00:00:00Z",
            false,
        );
        let v = serde_json::to_value(r).unwrap();
        assert_eq!(v["swapType"], "EXACT_INPUT");
        assert_eq!(v["depositType"], "ORIGIN_CHAIN");
        assert_eq!(v["slippageTolerance"], 100);
    }
}
