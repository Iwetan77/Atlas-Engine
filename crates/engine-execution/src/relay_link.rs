//! Relay (relay.link) moves USDC from the user's Base wallet to their Solana wallet in seconds with
//! no Base gas: the wallet signs one EIP-3009 ReceiveWithAuthorization for exactly the quoted amount,
//! redeemable only by Relay's receiver, and Relay's solver pays the gas and fills from its own
//! capital. The fee (a few cents) is in the quote. The public API needs no key; a free key from
//! Relay's dashboard raises its rate limits.
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::time::Duration;

use crate::layerswap::SwapState;
use crate::solana::MAINNET_USDC_MINT;
use crate::swaps::uniswap::BASE_USDC;

const API: &str = "https://api.relay.link/";
const BASE_CHAIN_ID: u64 = 8453;
const SOLANA_CHAIN_ID: u64 = 792703809;
// Relay's receiver on Base: the only account that can redeem the user's authorization (the Privy
// bridge pins it too).
const RECEIVER: &str = "0xccc88a9d1b4ed6b0eaba998850414b24f1c315be";

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("Relay request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Relay returned HTTP {status}: {reason}")]
    Rejected {
        status: reqwest::StatusCode,
        reason: String,
    },
    #[error("Relay returned an unexpected quote: {0}")]
    InvalidResponse(&'static str),
}

#[derive(Clone)]
pub struct RelayClient {
    http: Client,
    base: Url,
    api_key: Option<String>,
}

/// A Base → Solana move waiting for the user's signature. `typed_data` (checked, standard EIP-712
/// shape) is what their wallet signs; `amount_in_units` leaves Base so that exactly
/// `amount_out_units` lands on Solana.
#[derive(Clone, Debug, PartialEq)]
pub struct GaslessMove {
    pub request_id: String,
    pub amount_in_units: u128,
    pub amount_out_units: u128,
    pub typed_data: Value,
    // Relay's name for the flow, handed back with the signature.
    pub api: String,
}

impl RelayClient {
    pub fn new(api_key: Option<String>) -> Result<Self, RelayError> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(20)).build()?,
            base: Url::parse(API).map_err(|_| RelayError::InvalidResponse("base URL"))?,
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    fn with_key(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.header("x-api-key", key),
            None => request,
        }
    }

    /// Quotes moving Base USDC from `evm` so that exactly `amount_out_units` of USDC lands in
    /// `solana_owner`'s wallet.
    pub async fn base_to_solana(
        &self,
        evm: &str,
        solana_owner: &str,
        amount_out_units: u128,
    ) -> Result<GaslessMove, RelayError> {
        let url = self
            .base
            .join("quote/v2")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        let response = self
            .with_key(self.http.post(url))
            .json(&json!({
                "user":evm,"recipient":solana_owner,"refundTo":evm,
                "originChainId":BASE_CHAIN_ID,"destinationChainId":SOLANA_CHAIN_ID,
                "originCurrency":BASE_USDC.to_ascii_lowercase(),"destinationCurrency":MAINNET_USDC_MINT,
                "amount":amount_out_units.to_string(),"tradeType":"EXACT_OUTPUT","usePermit":true
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_gasless_move(&body, evm, solana_owner, amount_out_units)
    }

    /// Hands Relay the user's signed authorization; its solver then makes the move.
    pub async fn submit(
        &self,
        request_id: &str,
        api: &str,
        signature: &str,
    ) -> Result<(), RelayError> {
        let mut url = self
            .base
            .join("execute/permits")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        url.query_pairs_mut().append_pair("signature", signature);
        let response = self
            .with_key(self.http.post(url))
            .json(&json!({"kind":"eip3009","requestId":request_id,"api":api}))
            .send()
            .await?;
        checked(response).await.map(|_| ())
    }

    pub async fn state(&self, request_id: &str) -> Result<SwapState, RelayError> {
        let mut url = self
            .base
            .join("intents/status/v3")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        url.query_pairs_mut().append_pair("requestId", request_id);
        let body = checked(self.with_key(self.http.get(url)).send().await?).await?;
        Ok(parse_state(&body))
    }
}

async fn checked(response: reqwest::Response) -> Result<Value, RelayError> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body["message"]
            .as_str()
            .unwrap_or("no reason given")
            .chars()
            .take(200)
            .collect();
        return Err(RelayError::Rejected { status, reason });
    }
    Ok(body)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_ascii_lowercase(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn units(value: &Value) -> Option<u128> {
    value.as_str()?.parse().ok()
}

fn parse_gasless_move(
    body: &Value,
    evm: &str,
    solana_owner: &str,
    amount_out_units: u128,
) -> Result<GaslessMove, RelayError> {
    let invalid = RelayError::InvalidResponse;
    let [step] = body["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one step"));
    };
    let [item] = step["items"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one signature"));
    };
    let request_id = step["requestId"]
        .as_str()
        .filter(|id| id.starts_with("0x") && id.len() == 66)
        .ok_or(invalid("request id"))?;
    let sign = &item["data"]["sign"];
    let post = &item["data"]["post"];
    if step["kind"].as_str() != Some("signature")
        || sign["signatureKind"].as_str() != Some("eip712")
        || post["endpoint"].as_str() != Some("/execute/permits")
        || post["body"]["kind"].as_str() != Some("eip3009")
        || post["body"]["requestId"].as_str() != Some(request_id)
    {
        return Err(invalid("not a gasless authorization"));
    }
    let api = post["body"]["api"]
        .as_str()
        .filter(|api| ["bridge", "swap", "user-swap"].contains(api))
        .ok_or(invalid("flow"))?;
    let domain = &sign["domain"];
    if sign["primaryType"].as_str() != Some("ReceiveWithAuthorization")
        || text(&domain["chainId"]) != BASE_CHAIN_ID.to_string()
        || text(&domain["verifyingContract"]) != BASE_USDC.to_ascii_lowercase()
    {
        return Err(invalid("not a Base USDC authorization"));
    }
    let details = &body["details"];
    let (money_in, money_out) = (&details["currencyIn"], &details["currencyOut"]);
    if text(&money_in["currency"]["chainId"]) != BASE_CHAIN_ID.to_string()
        || text(&money_in["currency"]["address"]) != BASE_USDC.to_ascii_lowercase()
        || text(&money_out["currency"]["chainId"]) != SOLANA_CHAIN_ID.to_string()
        || money_out["currency"]["address"].as_str() != Some(MAINNET_USDC_MINT)
        || details["recipient"].as_str() != Some(solana_owner)
    {
        return Err(invalid("not USDC from Base to this Solana wallet"));
    }
    let amount_in_units = units(&money_in["amount"]).ok_or(invalid("amount in"))?;
    if units(&money_out["minimumAmount"]).is_none_or(|least| least < amount_out_units) {
        return Err(invalid("less would land than asked"));
    }
    // A move costs cents; anything past 2% + $0.50 is a quote to refuse, not to sign.
    if amount_in_units > amount_out_units + amount_out_units / 50 + 500_000 {
        return Err(invalid("fee too high"));
    }
    let message = &sign["value"];
    if text(&message["from"]) != evm.to_ascii_lowercase()
        || text(&message["to"]) != RECEIVER
        || text(&message["value"]) != amount_in_units.to_string()
    {
        return Err(invalid("authorization differs from the quote"));
    }
    let typed_data = json!({
        "types":{"ReceiveWithAuthorization":sign["types"]["ReceiveWithAuthorization"]},
        "primaryType":"ReceiveWithAuthorization",
        "domain":domain,
        "message":{
            "from":message["from"],"to":message["to"],"value":text(&message["value"]),
            "validAfter":text(&message["validAfter"]),"validBefore":text(&message["validBefore"]),
            "nonce":message["nonce"]
        }
    });
    Ok(GaslessMove {
        request_id: request_id.into(),
        amount_in_units,
        amount_out_units,
        typed_data,
        api: api.into(),
    })
}

fn parse_state(body: &Value) -> SwapState {
    match body["status"].as_str().unwrap_or_default() {
        "success" => SwapState::Completed,
        status @ ("refund" | "failure") => SwapState::Failed(status.into()),
        _ => SwapState::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVM: &str = "0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97";
    const SOLANA: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";

    // Captured from Relay's live API on 2026-09-30 (exactly 5 USDC to land on Solana).
    fn live_quote() -> Value {
        json!({"steps":[{"id":"authorize1","kind":"signature",
            "requestId":"0x17908063968ff6a9d48857c2594dd4744ab94c198381da1f6ed0154b50512b7b",
            "items":[{"status":"incomplete","data":{
                "sign":{"signatureKind":"eip712","primaryType":"ReceiveWithAuthorization",
                    "types":{"ReceiveWithAuthorization":[{"name":"from","type":"address"},{"name":"to","type":"address"},
                        {"name":"value","type":"uint256"},{"name":"validAfter","type":"uint256"},
                        {"name":"validBefore","type":"uint256"},{"name":"nonce","type":"bytes32"}]},
                    "domain":{"name":"USD Coin","version":"2","chainId":8453,
                        "verifyingContract":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},
                    "value":{"from":EVM,"to":"0xccc88a9d1b4ed6b0eaba998850414b24f1c315be","value":"5035780",
                        "validAfter":0,"validBefore":1790807024,
                        "nonce":"0xb4a7fe8d9607b823e2d26f4f444398df8db37f9d8d12502dcc285017a20109d9"}},
                "post":{"endpoint":"/execute/permits","method":"POST","body":{"kind":"eip3009",
                    "requestId":"0x17908063968ff6a9d48857c2594dd4744ab94c198381da1f6ed0154b50512b7b","api":"swap"}}}}]}],
            "details":{"recipient":SOLANA,
                "currencyIn":{"currency":{"chainId":8453,"address":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},
                    "amount":"5035780"},
                "currencyOut":{"currency":{"chainId":792703809,"address":MAINNET_USDC_MINT},
                    "amount":"5000000","minimumAmount":"5000000"}}})
    }

    #[test]
    fn a_move_is_one_authorization_for_exactly_what_lands() {
        let quote = parse_gasless_move(&live_quote(), EVM, SOLANA, 5_000_000).unwrap();
        assert_eq!(quote.amount_in_units, 5_035_780);
        assert_eq!(quote.api, "swap");
        let message = &quote.typed_data["message"];
        assert_eq!(message["value"], "5035780");
        assert_eq!(message["validAfter"], "0");
        assert_eq!(message["validBefore"], "1790807024");
        assert_eq!(quote.typed_data["domain"]["chainId"], 8453);
        // Asked for more than the quote lands, or for another wallet: refused.
        assert!(parse_gasless_move(&live_quote(), EVM, SOLANA, 6_000_000).is_err());
        assert!(parse_gasless_move(
            &live_quote(),
            EVM,
            "Other1111111111111111111111111111",
            5_000_000
        )
        .is_err());
        assert!(parse_gasless_move(
            &live_quote(),
            "0x0000000000000000000000000000000000000001",
            SOLANA,
            5_000_000
        )
        .is_err());
    }

    #[test]
    fn refuses_quotes_that_differ_from_a_plain_usdc_move() {
        let changed = |path: &[&str], value: Value| {
            let mut quote = live_quote();
            let mut at = &mut quote;
            for key in path {
                at = match key.parse::<usize>() {
                    Ok(i) => &mut at[i],
                    Err(_) => &mut at[*key],
                };
            }
            *at = value;
            parse_gasless_move(&quote, EVM, SOLANA, 5_000_000).is_err()
        };
        let sign = ["steps", "0", "items", "0", "data", "sign"];
        let with = |tail: &[&'static str]| [&sign[..], tail].concat();
        assert!(changed(
            &with(&["value", "to"]),
            json!("0x0000000000000000000000000000000000000002")
        ));
        assert!(changed(&with(&["value", "value"]), json!("9000000")));
        assert!(changed(
            &with(&["domain", "verifyingContract"]),
            json!("0x0000000000000000000000000000000000000003")
        ));
        assert!(changed(&with(&["primaryType"]), json!("Permit")));
        assert!(changed(
            &["details", "currencyOut", "currency", "address"],
            json!("So11111111111111111111111111111111111111112")
        ));
        assert!(changed(
            &["details", "currencyOut", "minimumAmount"],
            json!("4900000")
        ));
        // A fee far beyond a few cents.
        assert!(changed(
            &["details", "currencyIn", "amount"],
            json!("6000000")
        ));
        assert!(changed(
            &["steps", "0", "items", "0", "data", "post", "body", "kind"],
            json!("permit2")
        ));
        let mut two = live_quote();
        let step = two["steps"][0].clone();
        two["steps"] = json!([step.clone(), step]);
        assert!(parse_gasless_move(&two, EVM, SOLANA, 5_000_000).is_err());
    }

    #[test]
    fn states() {
        let state = |s: &str| parse_state(&json!({"status":s}));
        assert_eq!(state("success"), SwapState::Completed);
        assert_eq!(state("waiting"), SwapState::Waiting);
        assert_eq!(state("delayed"), SwapState::Waiting);
        assert_eq!(state("refund"), SwapState::Failed("refund".into()));
        assert_eq!(state("failure"), SwapState::Failed("failure".into()));
    }

    // Network: a real quote (nothing moves without the signature).
    // cargo test -p engine-execution live_relay -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_relay_base_to_solana() {
        let client = RelayClient::new(std::env::var("RELAY_API_KEY").ok()).unwrap();
        let quote = client.base_to_solana(EVM, SOLANA, 1_000_000).await.unwrap();
        println!(
            "{} in for 1 USDC out, fee {}",
            quote.amount_in_units,
            quote.amount_in_units - 1_000_000
        );
        assert!(quote.amount_in_units < 1_100_000);
        assert_eq!(
            client.state(&quote.request_id).await.unwrap(),
            SwapState::Waiting
        );
    }
}
