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
// Hyperliquid's chain id on Relay, and its USDC (8 decimals there; 6 on Base and Solana).
const HYPERLIQUID_CHAIN_ID: u64 = 1337;
const HYPERLIQUID_USDC: &str = "0x00000000000000000000000000000000";
// ETH (a gas top-up) on Relay's EVM chains.
const NATIVE: &str = "0x0000000000000000000000000000000000000000";
// Relay's Solana deposit program, and the compute-budget program a deposit may also use.
const SOLANA_DEPOSIT_PROGRAMS: [&str; 2] = [
    "99vQwtBwYtrqqD9YSXbdum3KBdxPAVxYTaQ3cfnJSrN2",
    "ComputeBudget111111111111111111111111111111",
];
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

/// Where a move lands: the chain, its USDC, the recipient, and how many of its units make one of
/// Base's (Hyperliquid's USDC has 8 decimals, Base's and Solana's 6).
#[derive(Clone, Copy)]
struct Dest<'a> {
    chain: u64,
    currency: &'a str,
    recipient: &'a str,
    scale: u128,
}

fn solana_dest(owner: &str) -> Dest<'_> {
    Dest {
        chain: SOLANA_CHAIN_ID,
        currency: MAINNET_USDC_MINT,
        recipient: owner,
        scale: 1,
    }
}

fn base_dest(wallet: &str) -> Dest<'_> {
    Dest {
        chain: BASE_CHAIN_ID,
        currency: BASE_USDC,
        recipient: wallet,
        scale: 1,
    }
}

fn hyperliquid_dest(account: &str) -> Dest<'_> {
    Dest {
        chain: HYPERLIQUID_CHAIN_ID,
        currency: HYPERLIQUID_USDC,
        recipient: account,
        scale: 100,
    }
}

/// A move from Solana (to Hyperliquid or Base): Relay's deposit instructions for the user's Solana wallet to sign
/// (the engine builds the transaction), moving `amount_in_units` so `amount_out_units` lands.
#[derive(Clone, Debug, PartialEq)]
pub struct SolanaMove {
    pub request_id: String,
    pub instructions: Vec<Value>,
    pub lookup_tables: Vec<String>,
    pub amount_in_units: u128,
    pub amount_out_units: u128,
}

/// How a move is sized: exactly this much lands, or exactly this much leaves (all of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exact {
    Out(u128),
    In(u128),
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
        self.gasless_move(evm, solana_dest(solana_owner), Exact::Out(amount_out_units))
            .await
    }

    /// Base USDC from `evm` into its own Hyperliquid account, gasless, so exactly
    /// `amount_out_units` (6 decimals) lands there.
    pub async fn base_to_hyperliquid(
        &self,
        evm: &str,
        amount_out_units: u128,
    ) -> Result<GaslessMove, RelayError> {
        self.gasless_move(evm, hyperliquid_dest(evm), Exact::Out(amount_out_units))
            .await
    }

    /// Solana USDC from `solana_owner` into the Hyperliquid account `evm`, so exactly
    /// `amount_out_units` (6 decimals) lands there.
    pub async fn solana_to_hyperliquid(
        &self,
        solana_owner: &str,
        evm: &str,
        amount_out_units: u128,
    ) -> Result<SolanaMove, RelayError> {
        self.solana_move(solana_owner, hyperliquid_dest(evm), amount_out_units, 0)
            .await
    }

    /// Solana USDC from `solana_owner` into the Base wallet `evm`, so exactly `amount_out_units`
    /// of USDC lands there, plus `gas_units` (dollars, 6 decimals; 0 for none) of ETH for an empty
    /// gas tank.
    pub async fn solana_to_base(
        &self,
        solana_owner: &str,
        evm: &str,
        amount_out_units: u128,
        gas_units: u128,
    ) -> Result<SolanaMove, RelayError> {
        self.solana_move(solana_owner, base_dest(evm), amount_out_units, gas_units)
            .await
    }

    async fn solana_move(
        &self,
        solana_owner: &str,
        dest: Dest<'_>,
        amount_out_units: u128,
        gas_units: u128,
    ) -> Result<SolanaMove, RelayError> {
        let url = self
            .base
            .join("quote/v2")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        let mut request = json!({
            "user":solana_owner,"recipient":dest.recipient,"refundTo":solana_owner,
            "originChainId":SOLANA_CHAIN_ID,"destinationChainId":dest.chain,
            "originCurrency":MAINNET_USDC_MINT,"destinationCurrency":dest.currency,
            "amount":(amount_out_units * dest.scale).to_string(),"tradeType":"EXACT_OUTPUT",
            "slippageTolerance":"50"
        });
        if gas_units > 0 {
            request["topupGas"] = json!(true);
            request["topupGasAmount"] = json!(gas_units.to_string());
        }
        let response = self
            .with_key(self.http.post(url))
            .json(&request)
            .send()
            .await?;
        let body = checked(response).await?;
        parse_solana_move(&body, solana_owner, dest, amount_out_units, gas_units)
    }

    /// The same, sending exactly `amount_in_units` (everything an address holds); what lands is
    /// that less Relay's fee.
    pub async fn base_to_solana_all(
        &self,
        evm: &str,
        solana_owner: &str,
        amount_in_units: u128,
    ) -> Result<GaslessMove, RelayError> {
        self.gasless_move(evm, solana_dest(solana_owner), Exact::In(amount_in_units))
            .await
    }

    async fn gasless_move(
        &self,
        evm: &str,
        dest: Dest<'_>,
        exact: Exact,
    ) -> Result<GaslessMove, RelayError> {
        let (amount, trade_type) = match exact {
            Exact::Out(units) => (units * dest.scale, "EXACT_OUTPUT"),
            Exact::In(units) => (units, "EXACT_INPUT"),
        };
        let url = self
            .base
            .join("quote/v2")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        let response = self
            .with_key(self.http.post(url))
            .json(&json!({
                "user":evm,"recipient":dest.recipient,"refundTo":evm,
                "originChainId":BASE_CHAIN_ID,"destinationChainId":dest.chain,
                "originCurrency":BASE_USDC.to_ascii_lowercase(),"destinationCurrency":dest.currency,
                "amount":amount.to_string(),"tradeType":trade_type,"usePermit":true,
                // Dollars to dollars: 0.5% is room enough (Relay's default is wider).
                "slippageTolerance":"50"
            }))
            .send()
            .await?;
        let body = checked(response).await?;
        parse_gasless_move(&body, evm, dest, exact)
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

// Whether Relay's quote lands USDC at `dest`.
fn lands_at(details: &Value, dest: Dest<'_>) -> bool {
    let out = &details["currencyOut"]["currency"];
    let recipient = details["recipient"].as_str().unwrap_or_default();
    text(&out["chainId"]) == dest.chain.to_string()
        && text(&out["address"]) == dest.currency.to_ascii_lowercase()
        && if dest.chain == SOLANA_CHAIN_ID {
            recipient == dest.recipient
        } else {
            recipient.eq_ignore_ascii_case(dest.recipient)
        }
}

fn parse_gasless_move(
    body: &Value,
    evm: &str,
    dest: Dest<'_>,
    exact: Exact,
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
        || !lands_at(details, dest)
    {
        return Err(invalid("not USDC from Base to this recipient"));
    }
    let amount_in_units = units(&money_in["amount"]).ok_or(invalid("amount in"))?;
    // In Base's units (6 decimals) whatever the destination's.
    let least_out = units(&money_out["minimumAmount"]).ok_or(invalid("amount out"))? / dest.scale;
    // What's promised to land: the asked amount, or (sending all of it) the quote's minimum.
    let amount_out_units = match exact {
        Exact::Out(asked) if least_out < asked => {
            return Err(invalid("less would land than asked"));
        }
        Exact::Out(asked) => asked,
        Exact::In(sent) if sent != amount_in_units => {
            return Err(invalid("amount sent differs from the quote"));
        }
        Exact::In(_) => least_out,
    };
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

fn parse_solana_move(
    body: &Value,
    solana_owner: &str,
    dest: Dest<'_>,
    amount_out_units: u128,
    gas_units: u128,
) -> Result<SolanaMove, RelayError> {
    let invalid = RelayError::InvalidResponse;
    let [step] = body["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one step"));
    };
    let [item] = step["items"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one transaction"));
    };
    let request_id = step["requestId"]
        .as_str()
        .filter(|id| id.starts_with("0x") && id.len() == 66)
        .ok_or(invalid("request id"))?;
    let instructions = item["data"]["instructions"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or(invalid("instructions"))?;
    // Only Relay's deposit program (and compute budget), and only the user signs.
    for ix in instructions {
        if !SOLANA_DEPOSIT_PROGRAMS.contains(&ix["programId"].as_str().unwrap_or_default()) {
            return Err(invalid("unknown program"));
        }
        let keys = ix["keys"].as_array().ok_or(invalid("instruction keys"))?;
        if keys.iter().any(|k| {
            k["isSigner"].as_bool() == Some(true) && k["pubkey"].as_str() != Some(solana_owner)
        }) {
            return Err(invalid("another signer"));
        }
    }
    let lookup_tables = item["data"]["addressLookupTableAddresses"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let details = &body["details"];
    let money_in = &details["currencyIn"];
    if step["kind"].as_str() != Some("transaction")
        || text(&money_in["currency"]["chainId"]) != SOLANA_CHAIN_ID.to_string()
        || money_in["currency"]["address"].as_str() != Some(MAINNET_USDC_MINT)
        || !lands_at(details, dest)
    {
        return Err(invalid("not USDC from Solana to this recipient"));
    }
    // Gas asked for lands as ETH with the same recipient; none asked for, none paid for.
    let gas = &details["currencyGasTopup"];
    if gas_units > 0
        && (text(&gas["currency"]["chainId"]) != dest.chain.to_string()
            || text(&gas["currency"]["address"]) != NATIVE)
    {
        return Err(invalid("gas top-up missing"));
    }
    if gas_units == 0 && units(&gas["amount"]).is_some_and(|wei| wei > 0) {
        return Err(invalid("unasked gas top-up"));
    }
    let amount_in_units = units(&money_in["amount"]).ok_or(invalid("amount in"))?;
    let least_out = units(&details["currencyOut"]["minimumAmount"]).ok_or(invalid("amount out"))?;
    if least_out < amount_out_units * dest.scale {
        return Err(invalid("less would land than asked"));
    }
    if amount_in_units > amount_out_units + gas_units + amount_out_units / 50 + 500_000 {
        return Err(invalid("fee too high"));
    }
    Ok(SolanaMove {
        request_id: request_id.into(),
        instructions: instructions.clone(),
        lookup_tables,
        amount_in_units,
        amount_out_units,
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
        let quote = parse_gasless_move(
            &live_quote(),
            EVM,
            solana_dest(SOLANA),
            Exact::Out(5_000_000),
        )
        .unwrap();
        assert_eq!(quote.amount_in_units, 5_035_780);
        assert_eq!(quote.api, "swap");
        let message = &quote.typed_data["message"];
        assert_eq!(message["value"], "5035780");
        assert_eq!(message["validAfter"], "0");
        assert_eq!(message["validBefore"], "1790807024");
        assert_eq!(quote.typed_data["domain"]["chainId"], 8453);
        // Asked for more than the quote lands, or for another wallet: refused.
        assert!(parse_gasless_move(
            &live_quote(),
            EVM,
            solana_dest(SOLANA),
            Exact::Out(6_000_000)
        )
        .is_err());
        assert!(parse_gasless_move(
            &live_quote(),
            EVM,
            solana_dest("Other1111111111111111111111111111"),
            Exact::Out(5_000_000)
        )
        .is_err());
        assert!(parse_gasless_move(
            &live_quote(),
            "0x0000000000000000000000000000000000000001",
            solana_dest(SOLANA),
            Exact::Out(5_000_000)
        )
        .is_err());
    }

    #[test]
    fn sending_everything_promises_the_quotes_minimum() {
        let quote = parse_gasless_move(
            &live_quote(),
            EVM,
            solana_dest(SOLANA),
            Exact::In(5_035_780),
        )
        .unwrap();
        assert_eq!(quote.amount_in_units, 5_035_780);
        assert_eq!(quote.amount_out_units, 5_000_000);
        assert!(parse_gasless_move(
            &live_quote(),
            EVM,
            solana_dest(SOLANA),
            Exact::In(9_000_000)
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
            parse_gasless_move(&quote, EVM, solana_dest(SOLANA), Exact::Out(5_000_000)).is_err()
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
        assert!(parse_gasless_move(&two, EVM, solana_dest(SOLANA), Exact::Out(5_000_000)).is_err());
    }

    // Captured from Relay's live API on 2026-10-01: exactly 1.40 USDC to land on Base from Solana,
    // with $0.20 of ETH for an empty gas tank (trimmed to what's read).
    fn solana_to_base_quote() -> Value {
        json!({"steps":[{"id":"deposit","kind":"transaction",
            "requestId":"0x1790830053b377ac7a605b68bb557143e0673d6725eda466aa59ae8847bf7cbb",
            "items":[{"status":"incomplete","data":{"instructions":[{
                "keys":[{"pubkey":"Dodg2HifwU8rmaVVyMyUZDGTRbqAJTyVYxXPwcbNpBKc","isSigner":false,"isWritable":false},
                    {"pubkey":SOLANA,"isSigner":true,"isWritable":true},
                    {"pubkey":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","isSigner":false,"isWritable":false}],
                "programId":"99vQwtBwYtrqqD9YSXbdum3KBdxPAVxYTaQ3cfnJSrN2",
                "data":"0b9c60da27a3b413a9091900000000008eebc381c5819b08bdef4f1527dcc2cb7538f8aeb4ab811b094b2cc57d93ecbf"}],
                "addressLookupTableAddresses":["Hm9fUgcn7qwDaiNTFiGh6pNtVATgnaRcmK6Bbx6EMZfP"]}}]}],
            "details":{"recipient":"0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
                "currencyIn":{"currency":{"chainId":792703809,"address":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"},
                    "amount":"1640873","minimumAmount":"1640873"},
                "currencyOut":{"currency":{"chainId":8453,"address":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"},
                    "amount":"1400000","minimumAmount":"1400000"},
                "currencyGasTopup":{"currency":{"chainId":8453,"address":"0x0000000000000000000000000000000000000000"},
                    "amount":"73940251639142","minimumAmount":"73940251639142"}}})
    }

    #[test]
    fn reads_a_move_from_solana_to_base_with_gas() {
        let body = solana_to_base_quote();
        let moved = parse_solana_move(&body, SOLANA, base_dest(EVM), 1_400_000, 200_000).unwrap();
        assert_eq!(moved.amount_in_units, 1_640_873);
        assert_eq!(moved.lookup_tables.len(), 1);
        // Gas nobody asked for, more than asked to land, another recipient or another signer: refused.
        assert!(parse_solana_move(&body, SOLANA, base_dest(EVM), 1_400_000, 0).is_err());
        assert!(parse_solana_move(&body, SOLANA, base_dest(EVM), 1_500_000, 200_000).is_err());
        let other = "0x0000000000000000000000000000000000000001";
        assert!(parse_solana_move(&body, SOLANA, base_dest(other), 1_400_000, 200_000).is_err());
        let mut signer = body.clone();
        signer["steps"][0]["items"][0]["data"]["instructions"][0]["keys"][0]["isSigner"] =
            json!(true);
        assert!(parse_solana_move(&signer, SOLANA, base_dest(EVM), 1_400_000, 200_000).is_err());
        // Without gas asked for, no gas top-up in the quote either.
        let mut plain = body;
        plain["details"]
            .as_object_mut()
            .unwrap()
            .remove("currencyGasTopup");
        plain["details"]["currencyIn"]["amount"] = json!("1424393");
        assert!(parse_solana_move(&plain, SOLANA, base_dest(EVM), 1_400_000, 0).is_ok());
        assert!(parse_solana_move(&plain, SOLANA, base_dest(EVM), 1_400_000, 200_000).is_err());
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
        // A link's payout: everything the escrow holds.
        let all = client
            .base_to_solana_all(EVM, SOLANA, 5_050_000)
            .await
            .unwrap();
        println!("5.05 USDC sent, {} lands", all.amount_out_units);
        // Into a Hyperliquid account: gasless from Base, or a built Solana transaction.
        let hl = client.base_to_hyperliquid(EVM, 10_000_000).await.unwrap();
        println!("Base → Hyperliquid: {} in for 10 USDC", hl.amount_in_units);
        let from_solana = client
            .solana_to_hyperliquid(SOLANA, EVM, 10_000_000)
            .await
            .unwrap();
        println!(
            "Solana → Hyperliquid: {} in for 10 USDC",
            from_solana.amount_in_units
        );
        let to_base = client
            .solana_to_base(SOLANA, EVM, 1_400_000, 200_000)
            .await
            .unwrap();
        println!(
            "Solana → Base: {} in for 1.40 USDC and $0.20 of gas",
            to_base.amount_in_units
        );
        let rpc = crate::solana::SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let tx = rpc
            .v0_transaction(
                SOLANA,
                &from_solana.instructions,
                &from_solana.lookup_tables,
            )
            .await
            .unwrap();
        println!("deposit transaction: {} base64 chars", tx.len());
        assert_eq!(all.amount_in_units, 5_050_000);
        assert!(all.amount_out_units > 4_950_000);
    }
}
