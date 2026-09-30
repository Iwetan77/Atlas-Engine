//! Layerswap moves USDC out of the user's Base wallet: into their Paradex account (perps margin) or
//! to their Solana wallet (a Solana buy paid with Base cash), gasless when it can be. No API key:
//! the public v2 API quotes, creates swaps and reports status.
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

use crate::swaps::uniswap::BASE_USDC;

const API: &str = "https://api.layerswap.io/api/v2/";
const BASE_CHAIN_ID: &str = "8453";
// ERC-20 transfer(address,uint256).
const TRANSFER_SELECTOR: &str = "a9059cbb";
// Layerswap's gasless deposit receiver on Base: the only account that can redeem the user's
// authorization (the Privy bridge pins it too).
const GASLESS_RECEIVER: &str = "0x6351c235e6f7e08f80974009d01829e5a8250d62";

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

/// A Base → Paradex swap funded without Base gas: the user's wallet signs one EIP-3009
/// ReceiveWithAuthorization (see `gasless_authorization`) and Layerswap's relayer sends it, paying
/// the gas; the gasless quote includes that cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GaslessDeposit {
    pub swap_id: String,
    pub amount_units: u128,
}

/// The one Solana transaction that funds a Solana → Base swap, unsigned, base64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolanaDeposit {
    pub swap_id: String,
    pub transaction: String,
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
        self.base_to(
            "PARADEX_MAINNET",
            source_address,
            paradex_account,
            amount_units,
            reference,
        )
        .await
    }

    /// Creates a Base → Solana USDC swap to the user's own Solana wallet.
    pub async fn base_to_solana(
        &self,
        source_address: &str,
        solana_owner: &str,
        amount_units: u128,
        reference: &str,
    ) -> Result<BaseDeposit, LayerswapError> {
        self.base_to(
            "SOLANA_MAINNET",
            source_address,
            solana_owner,
            amount_units,
            reference,
        )
        .await
    }

    /// Base → Paradex without Base gas.
    pub async fn base_to_paradex_gasless(
        &self,
        source_address: &str,
        paradex_account: &str,
        amount_units: u128,
        reference: &str,
    ) -> Result<GaslessDeposit, LayerswapError> {
        self.base_to_gasless(
            "PARADEX_MAINNET",
            source_address,
            paradex_account,
            amount_units,
            reference,
        )
        .await
    }

    /// The typed data the user's wallet signs to fund a gasless swap, checked: Base USDC, from
    /// `source_address`, to Layerswap's receiver, for exactly `amount_units`.
    pub async fn gasless_authorization(
        &self,
        swap_id: &str,
        source_address: &str,
        amount_units: u128,
    ) -> Result<Value, LayerswapError> {
        let mut url = self
            .base
            .join(&format!("swaps/{swap_id}/deposit_actions"))
            .map_err(|_| LayerswapError::InvalidResponse("URL"))?;
        url.query_pairs_mut()
            .append_pair("source_address", source_address);
        let body = checked(self.http.get(url).send().await?).await?;
        let actions = body["data"]
            .as_array()
            .ok_or(LayerswapError::InvalidResponse("deposit actions"))?;
        gasless_typed_data(actions, source_address, amount_units)
    }

    /// Hands Layerswap the user's signed authorization; its relayer then makes the deposit.
    pub async fn authorize(
        &self,
        swap_id: &str,
        signer: &str,
        signature: &str,
    ) -> Result<(), LayerswapError> {
        let url = self
            .base
            .join(&format!("swaps/{swap_id}/authorize"))
            .map_err(|_| LayerswapError::InvalidResponse("URL"))?;
        let response = self
            .http
            .post(url)
            .json(&json!({"signature":signature,"signer_address":signer}))
            .send()
            .await?;
        checked(response).await.map(|_| ())
    }

    /// Layerswap's total fee (USDC units, rounded up) to move `amount_units` of USDC Base → Solana,
    /// gasless (a fraction of a cent more than a plain transfer).
    pub async fn base_to_solana_fee(&self, amount_units: u128) -> Result<u128, LayerswapError> {
        self.fee("BASE_MAINNET", "SOLANA_MAINNET", amount_units, true)
            .await
    }

    /// The same, Solana → Base.
    pub async fn solana_to_base_fee(&self, amount_units: u128) -> Result<u128, LayerswapError> {
        self.fee("SOLANA_MAINNET", "BASE_MAINNET", amount_units, false)
            .await
    }

    /// Creates a Solana → Base USDC swap from the user's Solana wallet to their Base wallet. The
    /// deposit is one Solana transaction (base64) for the user to sign; its fee payer is the user.
    /// With `refuel`, part of it (about $0.50) arrives as native ETH on Base: gas for the wallet,
    /// paid from the user's own cash.
    pub async fn solana_to_base(
        &self,
        solana_owner: &str,
        base_address: &str,
        amount_units: u128,
        reference: &str,
        refuel: bool,
    ) -> Result<SolanaDeposit, LayerswapError> {
        self.solana_to(
            "BASE_MAINNET",
            solana_owner,
            base_address,
            amount_units,
            reference,
            refuel,
        )
        .await
    }

    /// Solana USDC straight into the user's Paradex account (perps margin paid with Solana cash).
    pub async fn solana_to_paradex(
        &self,
        solana_owner: &str,
        paradex_account: &str,
        amount_units: u128,
        reference: &str,
    ) -> Result<SolanaDeposit, LayerswapError> {
        self.solana_to(
            "PARADEX_MAINNET",
            solana_owner,
            paradex_account,
            amount_units,
            reference,
            false,
        )
        .await
    }

    async fn solana_to(
        &self,
        destination_network: &str,
        solana_owner: &str,
        destination_address: &str,
        amount_units: u128,
        reference: &str,
        refuel: bool,
    ) -> Result<SolanaDeposit, LayerswapError> {
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
                "source_network":"SOLANA_MAINNET","source_token":"USDC",
                "destination_network":destination_network,"destination_token":"USDC",
                "amount":amount,"source_address":solana_owner,
                "destination_address":destination_address,
                "use_deposit_address":false,"reference_id":reference,"refuel":refuel
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_solana_deposit(&body, amount_units)
    }

    async fn fee(
        &self,
        source: &str,
        destination: &str,
        amount_units: u128,
        gasless: bool,
    ) -> Result<u128, LayerswapError> {
        let mut url = self
            .base
            .join("quote")
            .map_err(|_| LayerswapError::InvalidResponse("URL"))?;
        url.query_pairs_mut()
            .append_pair("source_network", source)
            .append_pair("source_token", "USDC")
            .append_pair("destination_network", destination)
            .append_pair("destination_token", "USDC")
            .append_pair("amount", &usdc_decimal(amount_units))
            .append_pair("use_deposit_address", "false")
            .append_pair("use_gasless", if gasless { "true" } else { "false" });
        let body = checked(self.http.get(url).send().await?).await?;
        quote_fee_units(&body)
    }

    async fn base_to(
        &self,
        destination_network: &str,
        source_address: &str,
        destination_address: &str,
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
                "destination_network":destination_network,"destination_token":"USDC",
                "amount":amount,"source_address":source_address,
                "destination_address":destination_address,
                "use_deposit_address":false,"reference_id":reference
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_base_deposit(&body, amount_units)
    }

    async fn base_to_gasless(
        &self,
        destination_network: &str,
        source_address: &str,
        destination_address: &str,
        amount_units: u128,
        reference: &str,
    ) -> Result<GaslessDeposit, LayerswapError> {
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
                "destination_network":destination_network,"destination_token":"USDC",
                "amount":amount,"source_address":source_address,"refund_address":source_address,
                "destination_address":destination_address,
                "use_deposit_address":false,"use_gasless":true,"reference_id":reference
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        let swap_id = body["data"]["swap"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(LayerswapError::InvalidResponse("swap id"))?;
        // Only plan around it once the authorization to sign is on offer and checks out.
        let actions = body["data"]["deposit_actions"]
            .as_array()
            .ok_or(LayerswapError::InvalidResponse("deposit actions"))?;
        gasless_typed_data(actions, source_address, amount_units)?;
        Ok(GaslessDeposit {
            swap_id: swap_id.into(),
            amount_units,
        })
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

fn gasless_typed_data(
    actions: &[Value],
    source_address: &str,
    amount_units: u128,
) -> Result<Value, LayerswapError> {
    let action = actions
        .iter()
        .find(|a| a["type"].as_str() == Some("sign"))
        .ok_or(LayerswapError::InvalidResponse("no gasless authorization"))?;
    let typed = &action["typed_data"];
    let domain = &typed["domain"];
    let message = &typed["message"];
    let text = |v: &Value| match v {
        Value::String(s) => s.to_ascii_lowercase(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    if action["signing_standard"].as_str() != Some("eip3009")
        || typed["primaryType"].as_str() != Some("ReceiveWithAuthorization")
        || text(&domain["chainId"]) != BASE_CHAIN_ID
        || text(&domain["verifyingContract"]) != BASE_USDC.to_ascii_lowercase()
    {
        return Err(LayerswapError::InvalidResponse(
            "not a Base USDC authorization",
        ));
    }
    if text(&message["from"]) != source_address.to_ascii_lowercase()
        || text(&message["to"]) != GASLESS_RECEIVER
        || text(&message["value"]) != amount_units.to_string()
    {
        return Err(LayerswapError::InvalidResponse(
            "authorization differs from the swap",
        ));
    }
    Ok(typed.clone())
}

fn parse_solana_deposit(body: &Value, amount_units: u128) -> Result<SolanaDeposit, LayerswapError> {
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
    if action["type"].as_str() != Some("transfer")
        || action["network"]["name"].as_str() != Some("SOLANA_MAINNET")
    {
        return Err(LayerswapError::InvalidResponse("not a Solana transfer"));
    }
    if action["amount_in_base_units"].as_str() != Some(amount_units.to_string().as_str()) {
        return Err(LayerswapError::InvalidResponse("amount changed"));
    }
    let transaction = action["call_data"]
        .as_str()
        .filter(|tx| !tx.is_empty())
        .ok_or(LayerswapError::InvalidResponse("transaction"))?;
    Ok(SolanaDeposit {
        swap_id: swap_id.into(),
        transaction: transaction.into(),
        amount_units,
    })
}

fn quote_fee_units(body: &Value) -> Result<u128, LayerswapError> {
    let fee = body["data"]["quote"]["total_fee"]
        .as_f64()
        .filter(|f| f.is_finite() && *f >= 0.0)
        .ok_or(LayerswapError::InvalidResponse("quote fee"))?;
    Ok((fee * 1_000_000.0).ceil() as u128)
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

    #[test]
    fn a_solana_deposit_is_one_transfer_of_the_asked_amount() {
        // Shape captured from Layerswap's live API on 2026-09-30 (Solana → Base, 5 USDC).
        let body = |amount: &str, network: &str| {
            json!({"data":{"swap":{"id":"s1","status":"user_transfer_pending"},"deposit_actions":[{"step":"deposit",
                "type":"transfer","to_address":"2ZUoHEPcN7bsSXw6YTj85CMrU8xNtcYNGiSMXPLomaa2","amount":5,
                "amount_in_base_units":amount,"call_data":"AAEAAgV+jAiH","network":{"name":network}}]},"error":null})
        };
        let deposit = parse_solana_deposit(&body("5000000", "SOLANA_MAINNET"), 5_000_000).unwrap();
        assert_eq!(deposit.swap_id, "s1");
        assert_eq!(deposit.transaction, "AAEAAgV+jAiH");
        assert!(parse_solana_deposit(&body("6000000", "SOLANA_MAINNET"), 5_000_000).is_err());
        assert!(parse_solana_deposit(&body("5000000", "BASE_MAINNET"), 5_000_000).is_err());
    }

    #[test]
    fn reads_the_quote_fee_rounded_up() {
        let body = json!({"data":{"quote":{"total_fee":0.310603,"receive_amount":4.689397}}});
        assert_eq!(quote_fee_units(&body).unwrap(), 310_603);
        assert_eq!(
            quote_fee_units(&json!({"data":{"quote":{"total_fee":0.1000001}}})).unwrap(),
            100_001
        );
        assert!(quote_fee_units(&json!({"data":{}})).is_err());
    }

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

    // Captured from Layerswap's live API on 2026-09-30 (a gasless 5 USDC Base → Solana swap).
    fn gasless_actions() -> Vec<Value> {
        vec![
            json!({"step":"sign","status":"action_required","signing_standard":"eip3009","type":"sign",
            "to_address":"0x6351c235e6f7e08f80974009d01829e5a8250d62","amount_in_base_units":"0",
            "typed_data":{"primaryType":"ReceiveWithAuthorization",
                "domain":{"name":"USD Coin","version":"2","chainId":"8453",
                    "verifyingContract":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},
                "message":{"from":"0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
                    "to":"0x6351c235e6f7e08f80974009d01829e5a8250d62","value":"5000000","validAfter":"0",
                    "validBefore":"1790807789",
                    "nonce":"0xc5d45ec13b4d970bdd7cc5de84e783c807df2d1c645831e6d8b12717e7bdc87b"}}}),
        ]
    }

    #[test]
    fn a_gasless_deposit_is_one_authorization_for_the_asked_amount() {
        let from = "0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97";
        let typed = gasless_typed_data(&gasless_actions(), from, 5_000_000).unwrap();
        assert_eq!(typed["message"]["value"], "5000000");
        assert!(gasless_typed_data(&gasless_actions(), from, 4_000_000).is_err());
        assert!(gasless_typed_data(
            &gasless_actions(),
            "0x0000000000000000000000000000000000000001",
            5_000_000
        )
        .is_err());
        let changed = |path: &[&str], value: Value| {
            let mut actions = gasless_actions();
            let mut at = &mut actions[0];
            for key in path {
                at = &mut at[*key];
            }
            *at = value;
            gasless_typed_data(&actions, from, 5_000_000).is_err()
        };
        assert!(changed(
            &["typed_data", "message", "to"],
            json!("0x0000000000000000000000000000000000000002")
        ));
        assert!(changed(
            &["typed_data", "domain", "verifyingContract"],
            json!("0x0000000000000000000000000000000000000003")
        ));
        assert!(changed(&["typed_data", "domain", "chainId"], json!(1)));
        assert!(changed(
            &["typed_data", "primaryType"],
            json!("TransferWithAuthorization")
        ));
        assert!(changed(&["signing_standard"], json!("permit2")));
        assert!(changed(&["type"], json!("transfer")));
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

    // A Base → Solana swap to a Solana wallet: one Base USDC transfer, and a fee we can read first.
    #[tokio::test]
    #[ignore]
    async fn live_layerswap_base_to_solana_deposit() {
        let client = LayerswapClient::new().unwrap();
        let fee = client.base_to_solana_fee(5_000_000).await.unwrap();
        println!("fee to move 5 USDC: {}", usdc_decimal(fee));
        assert!(fee > 0 && fee < 2_000_000);
        let deposit = client
            .base_to_solana(
                "0x845c22a46398E0a702733e556bEB6aFcB2E92132",
                "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
                5_000_000,
                "atlas-live-check",
            )
            .await
            .unwrap();
        println!("{deposit:?}");
        assert_eq!(deposit.amount_units, 5_000_000);
        let fee = client.solana_to_base_fee(5_000_000).await.unwrap();
        println!("fee Solana → Base: {}", usdc_decimal(fee));
        let back = client
            .solana_to_base(
                "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
                "0x845c22a46398E0a702733e556bEB6aFcB2E92132",
                5_000_000,
                "atlas-live-check",
                true,
            )
            .await
            .unwrap();
        println!("solana deposit tx: {} chars", back.transaction.len());
    }

    // A gasless Base → Paradex swap: one authorization to sign, nothing sent until it's authorized.
    #[tokio::test]
    #[ignore]
    async fn live_layerswap_gasless_deposit() {
        let client = LayerswapClient::new().unwrap();
        let from = "0x845c22a46398E0a702733e556bEB6aFcB2E92132";
        let deposit = client
            .base_to_paradex_gasless(
                from,
                "0x287dd502cd9e5e6267f1aeeaf577db69e7cf71b7fd8f29118de2e37104e17eb",
                12_345_678,
                "atlas-live-check",
            )
            .await
            .unwrap();
        let typed = client
            .gasless_authorization(&deposit.swap_id, from, 12_345_678)
            .await
            .unwrap();
        println!("{deposit:?}\n{typed}");
    }
}
