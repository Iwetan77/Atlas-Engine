//! CoW Protocol on Base fills an empty gas tank without gas. The wallet signs two messages: a USDC
//! permit (EIP-2612) letting CoW's vault relayer take exactly the top-up, and a sell order USDC →
//! ETH to itself. A solver runs the permit as a pre-hook, settles the order and pays the gas, taking
//! its cost (a fraction of a cent) from the USDC sold. No API key.
use reqwest::{Client, Url};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::layerswap::SwapState;
use crate::swaps::uniswap::BASE_USDC;

const API: &str = "https://api.cow.fi/base/api/v1/";
// CoW's contracts on Base (the Privy bridge pins them too).
pub const VAULT_RELAYER: &str = "0xc92e8bdf79f0507f65a392b0ab4667716bfe0110";
pub const SETTLEMENT: &str = "0x9008d19f58aabd9ed0d60971565aa8510560ab41";
// How CoW names native ETH as the token bought.
pub const NATIVE_ETH: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
// USDC permit(address,address,uint256,uint256,uint8,bytes32,bytes32).
const PERMIT_SELECTOR: &str = "d505accf";
// Enough gas for USDC's permit inside the settlement.
const PERMIT_GAS: &str = "80000";
// An order stays open this long.
const ORDER_SECS: u64 = 20 * 60;

#[derive(Debug, thiserror::Error)]
pub enum CowError {
    #[error("CoW request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("CoW returned HTTP {status}: {reason}")]
    Rejected {
        status: reqwest::StatusCode,
        reason: String,
    },
    #[error("CoW returned an unexpected answer: {0}")]
    InvalidResponse(&'static str),
}

#[derive(Clone)]
pub struct CowClient {
    http: Client,
    base: Url,
}

/// A USDC → ETH order for `owner`'s own wallet, ready to sign (`typed_data`) and place.
#[derive(Clone, Debug, PartialEq)]
pub struct GasOrder {
    pub owner: String,
    pub sell_units: u128,
    pub buy_wei: u128,
    pub valid_to: u64,
    pub app_data: String,
    pub app_data_hash: String,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn word(hex_or_number: &str) -> String {
    format!("{:0>64}", hex_or_number.trim_start_matches("0x"))
}

/// The EIP-712 permit the wallet signs: CoW's vault relayer may take exactly `value` USDC.
pub fn permit_typed_data(owner: &str, value: u128, nonce: u128, deadline: u64) -> Value {
    json!({
        "types":{"Permit":[{"name":"owner","type":"address"},{"name":"spender","type":"address"},
            {"name":"value","type":"uint256"},{"name":"nonce","type":"uint256"},
            {"name":"deadline","type":"uint256"}]},
        "primaryType":"Permit",
        "domain":{"name":"USD Coin","version":"2","chainId":8453,"verifyingContract":BASE_USDC.to_ascii_lowercase()},
        "message":{"owner":owner.to_ascii_lowercase(),"spender":VAULT_RELAYER,"value":value.to_string(),
            "nonce":nonce.to_string(),"deadline":deadline.to_string()}
    })
}

/// The order's app data: the signed permit as a pre-hook, so a wallet that never approved anything
/// can still sell. `signature` is the permit's (0x r s v).
pub fn permit_app_data(
    owner: &str,
    value: u128,
    deadline: u64,
    signature: &str,
) -> Result<String, CowError> {
    let sig = signature
        .strip_prefix("0x")
        .filter(|s| s.len() == 130 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or(CowError::InvalidResponse("permit signature"))?;
    let v = u8::from_str_radix(&sig[128..], 16)
        .map_err(|_| CowError::InvalidResponse("permit signature"))?;
    let v = if v < 27 { v + 27 } else { v };
    let call_data = format!(
        "0x{PERMIT_SELECTOR}{}{}{}{}{}{}{}",
        word(&owner.to_ascii_lowercase()),
        word(VAULT_RELAYER),
        word(&format!("{value:x}")),
        word(&format!("{deadline:x}")),
        word(&format!("{v:x}")),
        &sig[..64],
        &sig[64..128],
    );
    // Keys in a fixed order: the hash covers these exact bytes.
    Ok(serde_json::to_string(&json!({
        "appCode":"Atlas",
        "metadata":{"hooks":{"pre":[{"callData":call_data,"gasLimit":PERMIT_GAS,"target":BASE_USDC.to_ascii_lowercase()}],"version":"0.1.0"}},
        "version":"1.3.0"
    }))
    .expect("static JSON"))
}

pub fn app_data_hash(app_data: &str) -> String {
    format!("0x{}", hex(&Keccak256::digest(app_data.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl GasOrder {
    /// The EIP-712 order the wallet signs.
    pub fn typed_data(&self) -> Value {
        json!({
            "types":{"Order":[{"name":"sellToken","type":"address"},{"name":"buyToken","type":"address"},
                {"name":"receiver","type":"address"},{"name":"sellAmount","type":"uint256"},
                {"name":"buyAmount","type":"uint256"},{"name":"validTo","type":"uint32"},
                {"name":"appData","type":"bytes32"},{"name":"feeAmount","type":"uint256"},
                {"name":"kind","type":"string"},{"name":"partiallyFillable","type":"bool"},
                {"name":"sellTokenBalance","type":"string"},{"name":"buyTokenBalance","type":"string"}]},
            "primaryType":"Order",
            "domain":{"name":"Gnosis Protocol","version":"v2","chainId":8453,"verifyingContract":SETTLEMENT},
            "message":{"sellToken":BASE_USDC.to_ascii_lowercase(),"buyToken":NATIVE_ETH,
                "receiver":self.owner,"sellAmount":self.sell_units.to_string(),
                "buyAmount":self.buy_wei.to_string(),"validTo":self.valid_to,"appData":self.app_data_hash,
                "feeAmount":"0","kind":"sell","partiallyFillable":false,"sellTokenBalance":"erc20",
                "buyTokenBalance":"erc20"}
        })
    }
}

impl CowClient {
    pub fn new() -> Result<Self, CowError> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(20)).build()?,
            base: Url::parse(API).map_err(|_| CowError::InvalidResponse("base URL"))?,
        })
    }

    /// Prices selling `sell_units` USDC for ETH to `owner` (the permit hook's gas included) and
    /// returns the order to sign, accepting 2% less ETH than quoted.
    pub async fn gas_order(
        &self,
        owner: &str,
        sell_units: u128,
        app_data: &str,
    ) -> Result<GasOrder, CowError> {
        let owner = owner.to_ascii_lowercase();
        let hash = app_data_hash(app_data);
        let url = self
            .base
            .join("quote")
            .map_err(|_| CowError::InvalidResponse("URL"))?;
        let response = self
            .http
            .post(url)
            .json(&json!({
                "sellToken":BASE_USDC.to_ascii_lowercase(),"buyToken":NATIVE_ETH,"from":owner,
                "receiver":owner,"sellAmountBeforeFee":sell_units.to_string(),"kind":"sell",
                "signingScheme":"eip712","onchainOrder":false,"priceQuality":"optimal",
                "appData":app_data,"appDataHash":hash
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_gas_quote(&body, &owner, sell_units, app_data, &hash, unix_now())
    }

    /// Places the signed order; returns its id.
    pub async fn place(&self, order: &GasOrder, signature: &str) -> Result<String, CowError> {
        let url = self
            .base
            .join("orders")
            .map_err(|_| CowError::InvalidResponse("URL"))?;
        let response = self
            .http
            .post(url)
            .json(&json!({
                "sellToken":BASE_USDC.to_ascii_lowercase(),"buyToken":NATIVE_ETH,"receiver":order.owner,
                "sellAmount":order.sell_units.to_string(),"buyAmount":order.buy_wei.to_string(),
                "validTo":order.valid_to,"appData":order.app_data,"appDataHash":order.app_data_hash,
                "feeAmount":"0","kind":"sell","partiallyFillable":false,"sellTokenBalance":"erc20",
                "buyTokenBalance":"erc20","signingScheme":"eip712","signature":signature,"from":order.owner
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        body.as_str()
            .filter(|uid| uid.starts_with("0x") && uid.len() == 2 + 112)
            .map(str::to_string)
            .ok_or(CowError::InvalidResponse("order id"))
    }

    pub async fn order_state(&self, uid: &str) -> Result<SwapState, CowError> {
        let url = self
            .base
            .join(&format!("orders/{uid}"))
            .map_err(|_| CowError::InvalidResponse("URL"))?;
        let body = checked(self.http.get(url).send().await?).await?;
        Ok(match body["status"].as_str().unwrap_or_default() {
            "fulfilled" => SwapState::Completed,
            status @ ("cancelled" | "expired") => SwapState::Failed(status.into()),
            _ => SwapState::Waiting,
        })
    }
}

async fn checked(response: reqwest::Response) -> Result<Value, CowError> {
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = [&body["errorType"], &body["description"]]
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(": ")
            .chars()
            .take(200)
            .collect();
        return Err(CowError::Rejected { status, reason });
    }
    Ok(body)
}

fn parse_gas_quote(
    body: &Value,
    owner: &str,
    sell_units: u128,
    app_data: &str,
    app_data_hash: &str,
    now: u64,
) -> Result<GasOrder, CowError> {
    let invalid = CowError::InvalidResponse;
    let quote = &body["quote"];
    let text = |v: &Value| v.as_str().unwrap_or_default().to_ascii_lowercase();
    let units = |v: &Value| v.as_str().and_then(|s| s.parse::<u128>().ok());
    if text(&quote["sellToken"]) != BASE_USDC.to_ascii_lowercase()
        || text(&quote["buyToken"]) != NATIVE_ETH
        || text(&quote["receiver"]) != owner
        || quote["kind"].as_str() != Some("sell")
    {
        return Err(invalid("not USDC to ETH for this wallet"));
    }
    let sell = units(&quote["sellAmount"]).ok_or(invalid("sell amount"))?;
    let fee = units(&quote["feeAmount"]).ok_or(invalid("fee"))?;
    // The fee comes out of what's sold: the two add up to the top-up, and the fee is small.
    if sell + fee != sell_units || fee > sell_units / 10 {
        return Err(invalid("amounts differ from the top-up"));
    }
    let buy = units(&quote["buyAmount"])
        .filter(|b| *b > 0)
        .ok_or(invalid("buy amount"))?;
    Ok(GasOrder {
        owner: owner.into(),
        sell_units,
        buy_wei: buy * 98 / 100,
        valid_to: now + ORDER_SECS,
        app_data: app_data.into(),
        app_data_hash: app_data_hash.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "0x1b3f69a97d8918532f5f338a72fa7d5fd06d9106";

    #[test]
    fn the_permit_hook_carries_the_signature_as_permit_arguments() {
        let sig = format!("0x{}{}1c", "11".repeat(32), "22".repeat(32));
        let app_data = permit_app_data(OWNER, 500_000, 1_790_000_000, &sig).unwrap();
        let parsed: Value = serde_json::from_str(&app_data).unwrap();
        let hook = &parsed["metadata"]["hooks"]["pre"][0];
        assert_eq!(hook["target"], BASE_USDC.to_ascii_lowercase());
        let call = hook["callData"].as_str().unwrap();
        assert!(call.starts_with("0xd505accf"));
        // owner, spender, value, deadline, v, r, s: seven words.
        assert_eq!(call.len(), 2 + 8 + 7 * 64);
        assert!(call.contains(&word(VAULT_RELAYER)));
        assert!(call.contains(&word("7a120"))); // 500000
        assert!(call.contains(&word("1c")));
        assert!(call.ends_with(&"22".repeat(32)));
        // A 0/1 recovery bit becomes 27/28.
        let low = permit_app_data(OWNER, 1, 1, &format!("0x{}01", "33".repeat(64))).unwrap();
        assert!(low.contains(&word("1c")));
        assert!(permit_app_data(OWNER, 1, 1, "0x12").is_err());
        assert_eq!(app_data_hash(&app_data).len(), 66);
    }

    #[test]
    fn a_quote_becomes_an_order_for_exactly_the_top_up() {
        // Captured from CoW's live API on 2026-09-30 ($0.50 USDC → ETH with a permit hook).
        let body = json!({"quote":{"sellToken":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            "buyToken":"0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","receiver":OWNER,"sellAmount":"496426",
            "buyAmount":"184811366693731","feeAmount":"3574","kind":"sell"}});
        let order = parse_gas_quote(&body, OWNER, 500_000, "{}", "0xab", 1_000).unwrap();
        assert_eq!(order.sell_units, 500_000);
        assert_eq!(order.buy_wei, 184_811_366_693_731 * 98 / 100);
        assert_eq!(order.valid_to, 1_000 + ORDER_SECS);
        let typed = order.typed_data();
        assert_eq!(typed["message"]["feeAmount"], "0");
        assert_eq!(typed["message"]["receiver"], OWNER);
        assert_eq!(typed["domain"]["verifyingContract"], SETTLEMENT);
        let mut elsewhere = body.clone();
        elsewhere["quote"]["receiver"] = json!("0x0000000000000000000000000000000000000001");
        assert!(parse_gas_quote(&elsewhere, OWNER, 500_000, "{}", "0xab", 1).is_err());
        let mut other = body.clone();
        other["quote"]["buyToken"] = json!("0x4200000000000000000000000000000000000006");
        assert!(parse_gas_quote(&other, OWNER, 500_000, "{}", "0xab", 1).is_err());
        assert!(parse_gas_quote(&body, OWNER, 600_000, "{}", "0xab", 1).is_err());
    }

    // Network: a real quote with a (dummy-signed) permit hook. Nothing is placed.
    // cargo test -p engine-execution live_cow -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_cow_gas_quote() {
        let client = CowClient::new().unwrap();
        let sig = format!("0x{}{}1b", "11".repeat(32), "22".repeat(32));
        let app_data = permit_app_data(OWNER, 500_000, unix_now() + 3600, &sig).unwrap();
        let order = client.gas_order(OWNER, 500_000, &app_data).await.unwrap();
        println!("{order:?}");
        assert!(order.buy_wei > 0);
    }
}
