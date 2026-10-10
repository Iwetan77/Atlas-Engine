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
pub const ARC_CHAIN_ID: u64 = 5042;
pub const ARC_USDC: &str = "0x3600000000000000000000000000000000000000";
// Relay's depository: the same address on Arc, Base and Monad.
const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const SOLANA_CHAIN_ID: u64 = 792703809;
pub const MONAD_CHAIN_ID: u64 = 143;
// `depositNative(address depositor, bytes32 id)` on the depository.
const DEPOSIT_NATIVE: &str = "0x49290c1c";
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

fn arc_dest(wallet: &str) -> Dest<'_> {
    Dest {
        chain: ARC_CHAIN_ID,
        currency: ARC_USDC,
        recipient: wallet,
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

fn monad_dest(wallet: &str) -> Dest<'_> {
    Dest {
        chain: MONAD_CHAIN_ID,
        currency: NATIVE,
        recipient: wallet,
        scale: 1,
    }
}

/// Where a MON swap starts: the user's wallet, its chain and what leaves it.
#[derive(Clone, Copy)]
struct Origin<'a> {
    chain: u64,
    currency: &'a str,
    wallet: &'a str,
}

fn monad_origin(wallet: &str) -> Origin<'_> {
    Origin {
        chain: MONAD_CHAIN_ID,
        currency: NATIVE,
        wallet,
    }
}

// Relay's names for chains inside an order.
fn chain_name(chain: u64) -> &'static str {
    match chain {
        SOLANA_CHAIN_ID => "solana",
        BASE_CHAIN_ID => "base",
        MONAD_CHAIN_ID => "monad",
        _ => "",
    }
}

// The same address: exactly on Solana (base58), any case on EVM chains.
fn same(chain: u64, a: &Value, b: &str) -> bool {
    let a = a.as_str().unwrap_or_default();
    !a.is_empty()
        && if chain == SOLANA_CHAIN_ID {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
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

/// Arc pays its network fee in native USDC; these are exact approval/deposit transactions.
#[derive(Clone, Debug, PartialEq)]
pub struct ArcMove {
    pub request_id: String,
    pub amount_in_units: u128,
    pub amount_out_units: u128,
    pub transactions: Vec<Value>,
}

/// MON (Monad's own coin) bought or sold through Relay, exact in: `amount_in_units` leaves, about
/// `expected_out_units` lands and never less than `minimum_out_units` (or Relay refunds it).
#[derive(Clone, Debug, PartialEq)]
pub struct MonadSwap {
    pub request_id: String,
    pub amount_in_units: u128,
    pub expected_out_units: u128,
    pub minimum_out_units: u128,
    pub pay: MonadPay,
}

/// What the user's wallet signs to start a MON swap.
#[derive(Clone, Debug, PartialEq)]
pub enum MonadPay {
    /// Solana USDC: Relay's deposit instructions; the engine builds the transaction and lands it.
    Solana {
        instructions: Vec<Value>,
        lookup_tables: Vec<String>,
    },
    /// Base USDC: one gasless EIP-3009 authorization to Relay's receiver.
    Base { typed_data: Value, api: String },
    /// MON: the deposit the phone sends on Monad, to Relay's depository with `value` wei.
    Monad {
        to: String,
        data: String,
        value: u128,
    },
}

impl RelayClient {
    /// Exactly `usdc_in` of the Solana wallet's USDC into MON in the EVM wallet `evm`.
    pub async fn solana_to_mon(
        &self,
        solana_owner: &str,
        evm: &str,
        usdc_in: u128,
    ) -> Result<MonadSwap, RelayError> {
        let from = Origin {
            chain: SOLANA_CHAIN_ID,
            currency: MAINNET_USDC_MINT,
            wallet: solana_owner,
        };
        self.monad_swap(from, monad_dest(evm), usdc_in).await
    }

    /// Exactly `usdc_in` of the wallet's Base USDC into its MON, gasless.
    pub async fn base_to_mon(&self, evm: &str, usdc_in: u128) -> Result<MonadSwap, RelayError> {
        let from = Origin {
            chain: BASE_CHAIN_ID,
            currency: BASE_USDC,
            wallet: evm,
        };
        self.monad_swap(from, monad_dest(evm), usdc_in).await
    }

    /// Exactly `mon_in` wei of the wallet's MON into USDC in the Solana wallet `solana_owner`.
    pub async fn mon_to_solana(
        &self,
        evm: &str,
        solana_owner: &str,
        mon_in: u128,
    ) -> Result<MonadSwap, RelayError> {
        self.monad_swap(monad_origin(evm), solana_dest(solana_owner), mon_in)
            .await
    }

    /// Exactly `mon_in` wei of the wallet's MON into its Base USDC.
    pub async fn mon_to_base(&self, evm: &str, mon_in: u128) -> Result<MonadSwap, RelayError> {
        self.monad_swap(monad_origin(evm), base_dest(evm), mon_in)
            .await
    }

    async fn monad_swap(
        &self,
        from: Origin<'_>,
        dest: Dest<'_>,
        amount_in: u128,
    ) -> Result<MonadSwap, RelayError> {
        let url = self
            .base
            .join("quote/v2")
            .map_err(|_| RelayError::InvalidResponse("URL"))?;
        let mut request = json!({
            "user":from.wallet,"recipient":dest.recipient,"refundTo":from.wallet,
            "originChainId":from.chain,"destinationChainId":dest.chain,
            "originCurrency":from.currency.to_string(),"destinationCurrency":dest.currency,
            "amount":amount_in.to_string(),"tradeType":"EXACT_INPUT",
            // MON's price moves more than a dollar's; 1% is still far tighter than Relay's default.
            "slippageTolerance":"100"
        });
        if from.chain == BASE_CHAIN_ID {
            request["originCurrency"] = json!(BASE_USDC.to_ascii_lowercase());
            request["usePermit"] = json!(true);
        }
        let response = self
            .with_key(self.http.post(url))
            .json(&request)
            .send()
            .await?;
        let body = checked(response).await?;
        parse_monad_swap(&body, from, dest, amount_in)
    }

    pub async fn base_to_arc(&self, evm: &str, out: u128) -> Result<GaslessMove, RelayError> {
        self.gasless_move(evm, arc_dest(evm), Exact::Out(out)).await
    }

    pub async fn arc_to_base(&self, evm: &str, out: u128) -> Result<ArcMove, RelayError> {
        self.arc_move(evm, base_dest(evm), out).await
    }

    pub async fn arc_to_solana(
        &self,
        evm: &str,
        sol: &str,
        out: u128,
    ) -> Result<ArcMove, RelayError> {
        self.arc_move(evm, solana_dest(sol), out).await
    }

    async fn arc_move(&self, evm: &str, dest: Dest<'_>, out: u128) -> Result<ArcMove, RelayError> {
        let response = self.with_key(self.http.post(self.base.join("quote").map_err(|_| RelayError::InvalidResponse("URL"))?))
            .json(&json!({"user":evm,"recipient":dest.recipient,"originChainId":ARC_CHAIN_ID,
                "destinationChainId":dest.chain,"originCurrency":ARC_USDC,"destinationCurrency":dest.currency,
                "amount":out.to_string(),"tradeType":"EXACT_OUTPUT","usePermit":false}))
            .send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(RelayError::Rejected {
                status,
                reason: "cash move unavailable".into(),
            });
        }
        parse_arc_move(&response.json().await?, evm, dest, out)
    }

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

    /// Keep a cash-link payout on Base, avoiding a fresh recipient's Solana token-account
    /// setup. The solver pays Base gas out of the quoted input, using the same pinned
    /// EIP-3009 receiver as cross-chain payouts.
    pub async fn base_to_base_all(
        &self,
        escrow: &str,
        recipient: &str,
        amount_in_units: u128,
    ) -> Result<GaslessMove, RelayError> {
        self.gasless_move(escrow, base_dest(recipient), Exact::In(amount_in_units))
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
                // Same-chain sends otherwise return a normal transfer that needs escrow ETH.
                // Force the solver's supported gasless authorization instead.
                "forceSolverExecution":dest.chain == BASE_CHAIN_ID,
                // Same-token Base delivery needs no price room; keep the full quoted
                // payout guaranteed instead of reserving 0.5% on large gifts.
                "slippageTolerance":if dest.chain == BASE_CHAIN_ID { "0" } else { "50" }
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
    let (request_id, typed_data, api) = base_authorization(body, evm, amount_in_units)?;
    if dest.chain == BASE_CHAIN_ID {
        validate_base_payout_order(body, evm, dest.recipient, amount_in_units, least_out)?;
    }
    Ok(GaslessMove {
        request_id,
        amount_in_units,
        amount_out_units,
        typed_data,
        api,
    })
}

// A same-chain solver payout is still an order, not an arbitrary escrow call. Bind the
// quoted minimum to one Base USDC payment, with no extra calls, fees, or refund destinations.
fn validate_base_payout_order(
    body: &Value,
    escrow: &str,
    recipient: &str,
    amount_in: u128,
    minimum_out: u128,
) -> Result<(), RelayError> {
    let invalid = RelayError::InvalidResponse;
    let protocol = &body["protocol"]["v2"];
    let order = &protocol["orderData"];
    let payment = &protocol["paymentDetails"];
    let [input] = order["inputs"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one Base payout input"));
    };
    let [output] = order["output"]["payments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    else {
        return Err(invalid("expected one Base payout recipient"));
    };
    if order["output"]["chainId"] != "base"
        || input["payment"]["chainId"] != "base"
        || !same(BASE_CHAIN_ID, &input["payment"]["currency"], BASE_USDC)
        || units(&input["payment"]["amount"]) != Some(amount_in)
        || payment["chainId"] != "base"
        || !same(BASE_CHAIN_ID, &payment["currency"], BASE_USDC)
        || !same(BASE_CHAIN_ID, &payment["depository"], DEPOSITORY)
        || units(&payment["amount"]) != Some(amount_in)
        || !same(BASE_CHAIN_ID, &output["recipient"], recipient)
        || !same(BASE_CHAIN_ID, &output["currency"], BASE_USDC)
        || units(&output["minimumAmount"]) != Some(minimum_out)
        || units(&output["expectedAmount"]) != units(&body["details"]["currencyOut"]["amount"])
        || !order["output"]["calls"]
            .as_array()
            .is_some_and(Vec::is_empty)
        || !order["fees"].as_array().is_some_and(Vec::is_empty)
    {
        return Err(invalid("Base payout order differs from the quote"));
    }
    let refunds = input["refunds"]
        .as_array()
        .ok_or(invalid("Base payout refunds"))?;
    if refunds.is_empty()
        || refunds.iter().any(|refund| {
            refund["chainId"] != "base"
                || !same(BASE_CHAIN_ID, &refund["currency"], BASE_USDC)
                || !(same(BASE_CHAIN_ID, &refund["recipient"], escrow)
                    || same(BASE_CHAIN_ID, &refund["recipient"], recipient))
        })
    {
        return Err(invalid("Base payout refund destination changed"));
    }
    Ok(())
}

// A gasless Base quote's one step: an EIP-3009 authorization from `evm` to Relay's receiver for
// exactly `amount` of Base USDC, posted back to Relay. Returns the request id, the typed data to
// sign and Relay's name for the flow.
fn base_authorization(
    body: &Value,
    evm: &str,
    amount: u128,
) -> Result<(String, Value, String), RelayError> {
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
    let message = &sign["value"];
    if text(&message["from"]) != evm.to_ascii_lowercase()
        || text(&message["to"]) != RECEIVER
        || text(&message["value"]) != amount.to_string()
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
    Ok((request_id.into(), typed_data, api.into()))
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
    let (instructions, lookup_tables) = solana_deposit(item, solana_owner)?;
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
        instructions,
        lookup_tables,
        amount_in_units,
        amount_out_units,
    })
}

fn parse_arc_move(
    body: &Value,
    evm: &str,
    dest: Dest<'_>,
    out: u128,
) -> Result<ArcMove, RelayError> {
    let invalid = RelayError::InvalidResponse;
    let details = &body["details"];
    let incoming = &details["currencyIn"];
    let amount = units(&incoming["amount"]).ok_or(invalid("amount in"))?;
    if out == 0
        || amount < out
        || amount > out.saturating_add(out / 50).saturating_add(500_000)
        || text(&incoming["currency"]["chainId"]) != ARC_CHAIN_ID.to_string()
        || text(&incoming["currency"]["address"]) != ARC_USDC
        || incoming["currency"]["decimals"].as_u64() != Some(6)
        || text(&details["sender"]) != evm.to_ascii_lowercase()
        || !lands_at(details, dest)
        || units(&details["currencyOut"]["minimumAmount"]).unwrap_or(0) < out
    {
        return Err(invalid("cash route changed"));
    }
    let request_id = body["requestId"]
        .as_str()
        .filter(|s| is_hash(s))
        .ok_or(invalid("request id"))?;
    let protocol = &body["protocol"]["v2"];
    let order_id = protocol["orderId"]
        .as_str()
        .filter(|s| is_hash(s))
        .ok_or(invalid("order id"))?;
    let payment = &protocol["paymentDetails"];
    if text(&payment["depository"]) != DEPOSITORY
        || text(&payment["currency"]) != ARC_USDC
        || units(&payment["amount"]) != Some(amount)
        || payment["chainId"] != "arc"
    {
        return Err(invalid("deposit changed"));
    }
    let order = &protocol["orderData"];
    let inputs = order["inputs"].as_array().ok_or(invalid("inputs"))?;
    let outputs = order["output"]["payments"]
        .as_array()
        .ok_or(invalid("outputs"))?;
    let chain = if dest.chain == BASE_CHAIN_ID {
        "base"
    } else {
        "solana"
    };
    if inputs.len() != 1
        || outputs.len() != 1
        || order["output"]["chainId"] != chain
        || !order["output"]["calls"]
            .as_array()
            .is_some_and(Vec::is_empty)
        || !order["fees"].as_array().is_some_and(Vec::is_empty)
        || inputs[0]["payment"]["chainId"] != "arc"
        || text(&inputs[0]["payment"]["currency"]) != ARC_USDC
        || units(&inputs[0]["payment"]["amount"]) != Some(amount)
        || outputs[0]["recipient"].as_str() != Some(dest.recipient)
        || text(&outputs[0]["currency"]) != dest.currency.to_ascii_lowercase()
        || units(&outputs[0]["minimumAmount"]).unwrap_or(0) < out
    {
        return Err(invalid("order changed"));
    }
    let refunds = inputs[0]["refunds"].as_array().ok_or(invalid("refunds"))?;
    if refunds.is_empty()
        || refunds.iter().any(|r| match r["chainId"].as_str() {
            Some("arc") => {
                text(&r["recipient"]) != evm.to_ascii_lowercase()
                    || text(&r["currency"]) != ARC_USDC
            }
            Some(c) if c == chain => {
                r["recipient"].as_str() != Some(dest.recipient)
                    || r["currency"].as_str().map(str::to_ascii_lowercase)
                        != Some(dest.currency.to_ascii_lowercase())
            }
            _ => true,
        })
    {
        return Err(invalid("refund wallet changed"));
    }
    let word = |address: &str| {
        format!(
            "{:0>64}",
            address.trim_start_matches("0x").to_ascii_lowercase()
        )
    };
    let approval = format!("0x095ea7b3{}{:064x}", word(DEPOSITORY), amount);
    let deposit = format!(
        "0xe8017952{}{}{:064x}{}",
        word(evm),
        word(ARC_USDC),
        amount,
        &order_id[2..]
    );
    let steps = body["steps"].as_array().ok_or(invalid("steps"))?;
    if steps.is_empty() || steps.len() > 2 {
        return Err(invalid("steps"));
    }
    let mut transactions = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let last = index + 1 == steps.len();
        let expected_id = if last { "deposit" } else { "approve" };
        let target = if last { DEPOSITORY } else { ARC_USDC };
        let calldata = if last { &deposit } else { &approval };
        let items = step["items"].as_array().ok_or(invalid("items"))?;
        if step["id"] != expected_id
            || step["kind"] != "transaction"
            || step["requestId"] != request_id
            || items.len() != 1
        {
            return Err(invalid("unexpected step"));
        }
        let tx = &items[0]["data"];
        if text(&tx["from"]) != evm.to_ascii_lowercase()
            || text(&tx["to"]) != target
            || text(&tx["data"]) != *calldata
            || text(&tx["value"]) != "0"
            || tx["chainId"].as_u64() != Some(ARC_CHAIN_ID)
        {
            return Err(invalid("transaction changed"));
        }
        transactions.push(
            json!({"chain":"arc","chainId":ARC_CHAIN_ID,"to":target,"data":calldata,"value":"0"}),
        );
    }
    Ok(ArcMove {
        request_id: request_id.into(),
        amount_in_units: amount,
        amount_out_units: out,
        transactions,
    })
}

// A Solana deposit's instructions (only Relay's deposit program and compute budget, and only the
// user signs) and its lookup tables.
fn solana_deposit(
    item: &Value,
    solana_owner: &str,
) -> Result<(Vec<Value>, Vec<String>), RelayError> {
    let invalid = RelayError::InvalidResponse;
    let instructions = item["data"]["instructions"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or(invalid("instructions"))?;
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
    Ok((instructions.clone(), lookup_tables))
}

fn parse_monad_swap(
    body: &Value,
    from: Origin<'_>,
    dest: Dest<'_>,
    amount_in: u128,
) -> Result<MonadSwap, RelayError> {
    let invalid = RelayError::InvalidResponse;
    // MON on one side, the user's USDC on the other.
    if (from.chain == MONAD_CHAIN_ID) == (dest.chain == MONAD_CHAIN_ID) || amount_in == 0 {
        return Err(invalid("not a MON swap"));
    }
    let details = &body["details"];
    let (money_in, money_out) = (&details["currencyIn"], &details["currencyOut"]);
    if text(&money_in["currency"]["chainId"]) != from.chain.to_string()
        || !same(from.chain, &money_in["currency"]["address"], from.currency)
        || units(&money_in["amount"]) != Some(amount_in)
        || !same(from.chain, &details["sender"], from.wallet)
        || !lands_at(details, dest)
    {
        return Err(invalid("not this swap from this wallet"));
    }
    let expected = units(&money_out["amount"]).ok_or(invalid("amount out"))?;
    let minimum = units(&money_out["minimumAmount"])
        .filter(|least| *least > 0 && *least <= expected)
        .ok_or(invalid("minimum out"))?;
    let order_id = monad_order(body, from, dest, amount_in, minimum)?;
    let request_id = body["requestId"]
        .as_str()
        .filter(|id| is_hash(id))
        .ok_or(invalid("request id"))?;
    let [step] = body["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one step"));
    };
    let [item] = step["items"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("expected one item"));
    };
    if step["requestId"].as_str() != Some(request_id) {
        return Err(invalid("request id"));
    }
    let pay = match from.chain {
        BASE_CHAIN_ID => {
            let (_, typed_data, api) = base_authorization(body, from.wallet, amount_in)?;
            MonadPay::Base { typed_data, api }
        }
        SOLANA_CHAIN_ID => {
            if step["id"] != "deposit" || step["kind"] != "transaction" {
                return Err(invalid("not a deposit"));
            }
            let (instructions, lookup_tables) = solana_deposit(item, from.wallet)?;
            MonadPay::Solana {
                instructions,
                lookup_tables,
            }
        }
        _ => {
            // MON sent with `depositNative(user, order)`: the order binds what Relay pays out.
            let data = format!(
                "{DEPOSIT_NATIVE}{:0>64}{}",
                from.wallet.trim_start_matches("0x").to_ascii_lowercase(),
                &order_id[2..]
            );
            let tx = &item["data"];
            if step["id"] != "deposit"
                || step["kind"] != "transaction"
                || !same(MONAD_CHAIN_ID, &tx["from"], from.wallet)
                || !same(MONAD_CHAIN_ID, &tx["to"], DEPOSITORY)
                || text(&tx["data"]) != data
                || units(&tx["value"]) != Some(amount_in)
                || tx["chainId"].as_u64() != Some(MONAD_CHAIN_ID)
            {
                return Err(invalid("deposit transaction changed"));
            }
            MonadPay::Monad {
                to: DEPOSITORY.into(),
                data,
                value: amount_in,
            }
        }
    };
    Ok(MonadSwap {
        request_id: request_id.into(),
        amount_in_units: amount_in,
        expected_out_units: expected,
        minimum_out_units: minimum,
        pay,
    })
}

// Relay's order for a MON swap: paid in from the user's wallet on the origin chain into Relay's
// depository, paid out only to the recipient (at least `minimum`), refunded only to the user's own
// wallets, with no extra calls or fees. Returns its id.
fn monad_order<'a>(
    body: &'a Value,
    from: Origin<'_>,
    dest: Dest<'_>,
    amount_in: u128,
    minimum: u128,
) -> Result<&'a str, RelayError> {
    let invalid = RelayError::InvalidResponse;
    let protocol = &body["protocol"]["v2"];
    let order_id = protocol["orderId"]
        .as_str()
        .filter(|s| is_hash(s))
        .ok_or(invalid("order id"))?;
    let (origin, out) = (chain_name(from.chain), chain_name(dest.chain));
    let depository = if from.chain == SOLANA_CHAIN_ID {
        SOLANA_DEPOSIT_PROGRAMS[0]
    } else {
        DEPOSITORY
    };
    let payment = &protocol["paymentDetails"];
    if payment["chainId"] != origin
        || !same(from.chain, &payment["depository"], depository)
        || !same(from.chain, &payment["currency"], from.currency)
        || units(&payment["amount"]) != Some(amount_in)
    {
        return Err(invalid("deposit changed"));
    }
    let order = &protocol["orderData"];
    let [input] = order["inputs"].as_array().map(Vec::as_slice).unwrap_or(&[]) else {
        return Err(invalid("inputs"));
    };
    let [paid] = order["output"]["payments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    else {
        return Err(invalid("outputs"));
    };
    if input["payment"]["chainId"] != origin
        || !same(from.chain, &input["payment"]["currency"], from.currency)
        || units(&input["payment"]["amount"]) != Some(amount_in)
        || order["output"]["chainId"] != out
        || !same(dest.chain, &paid["recipient"], dest.recipient)
        || !same(dest.chain, &paid["currency"], dest.currency)
        || units(&paid["minimumAmount"]).unwrap_or(0) < minimum
        || !order["output"]["calls"]
            .as_array()
            .is_some_and(Vec::is_empty)
        || !order["fees"].as_array().is_some_and(Vec::is_empty)
    {
        return Err(invalid("order changed"));
    }
    let refunds = input["refunds"].as_array().ok_or(invalid("refunds"))?;
    if refunds.is_empty()
        || refunds.iter().any(|r| {
            if r["chainId"] == origin {
                !same(from.chain, &r["recipient"], from.wallet)
                    || !same(from.chain, &r["currency"], from.currency)
            } else if r["chainId"] == out {
                !same(dest.chain, &r["recipient"], dest.recipient)
                    || !same(dest.chain, &r["currency"], dest.currency)
            } else {
                true
            }
        })
    {
        return Err(invalid("refund wallet changed"));
    }
    Ok(order_id)
}

fn is_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|b| b.is_ascii_hexdigit())
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

    #[test]
    fn arc_cash_pins_the_approval_deposit_and_recipient() {
        let body: Value =
            serde_json::from_str(include_str!("fixtures/relay-arc-base-tx.json")).unwrap();
        let evm = body["details"]["sender"].as_str().unwrap();
        let quote = parse_arc_move(&body, evm, base_dest(evm), 1_000_000).unwrap();
        assert_eq!(quote.transactions.len(), 2);
        assert_eq!(quote.amount_in_units, 1_024_294);
        for path in [
            vec!["details", "recipient"],
            vec!["protocol", "v2", "paymentDetails", "depository"],
        ] {
            let mut bad = body.clone();
            let mut at = &mut bad;
            for key in path {
                at = &mut at[key];
            }
            *at = json!("0x0000000000000000000000000000000000000001");
            assert!(parse_arc_move(&bad, evm, base_dest(evm), 1_000_000).is_err());
        }
        for field in ["to", "data", "value", "chainId", "from"] {
            let mut bad = body.clone();
            bad["steps"][0]["items"][0]["data"][field] = json!("changed");
            assert!(parse_arc_move(&bad, evm, base_dest(evm), 1_000_000).is_err());
        }
        let mut bad = body.clone();
        bad["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["recipient"] =
            json!("another wallet");
        assert!(parse_arc_move(&bad, evm, base_dest(evm), 1_000_000).is_err());
    }

    #[test]
    fn base_cash_lands_on_arc_from_the_captured_quote() {
        let body: Value =
            serde_json::from_str(include_str!("fixtures/relay-base-arc.json")).unwrap();
        let evm = body["details"]["recipient"].as_str().unwrap();
        assert!(parse_gasless_move(&body, evm, arc_dest(evm), Exact::Out(1_000_000)).is_ok());
    }

    #[tokio::test]
    #[ignore = "live Relay read-only quotes; no funds moved"]
    async fn live_arc_cash_quotes() {
        let client = RelayClient::new(None).unwrap();
        let evm = "0xEe8646AF9e1DDA672716389aB64a7bD0Fd202ba7";
        let inbound = client.base_to_arc(evm, 1_000_000).await.unwrap();
        println!("LIVE DRY Base -> Arc: {} units in", inbound.amount_in_units);
        let outbound = client.arc_to_base(evm, 1_000_000).await.unwrap();
        println!(
            "LIVE DRY Arc -> Base: {} units in, {} transactions",
            outbound.amount_in_units,
            outbound.transactions.len()
        );
        let sol = client.arc_to_solana(evm, SOLANA, 1_000_000).await.unwrap();
        println!("LIVE DRY Arc -> Solana: {} units in", sol.amount_in_units);
    }

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

    // Shape captured from an unsigned same-chain forceSolverExecution quote, with
    // synthetic wallets, request, nonce and amounts. Nothing is signed or submitted.
    fn base_payout_quote() -> Value {
        let recipient = "0x1111111111111111111111111111111111111111";
        let mut quote = live_quote();
        quote["details"]["recipient"] = json!(recipient);
        quote["details"]["currencyIn"]["amount"] = json!("1060000");
        quote["details"]["currencyOut"] = json!({
            "currency":{"chainId":BASE_CHAIN_ID,"address":BASE_USDC},
            "amount":"1033000","minimumAmount":"1028000"
        });
        quote["steps"][0]["items"][0]["data"]["sign"]["value"]["value"] = json!("1060000");
        quote["protocol"] = json!({"v2":{
            "paymentDetails":{"chainId":"base","currency":BASE_USDC,
                "depository":DEPOSITORY,"amount":"1060000"},
            "orderData":{"inputs":[{
                "payment":{"chainId":"base","currency":BASE_USDC,"amount":"1060000"},
                "refunds":[
                    {"chainId":"base","currency":BASE_USDC,"recipient":EVM},
                    {"chainId":"base","currency":BASE_USDC,"recipient":recipient}
                ]
            }],"output":{"chainId":"base","payments":[{
                "recipient":recipient,"currency":BASE_USDC,"minimumAmount":"1028000","expectedAmount":"1033000"
            }],"calls":[]},"fees":[]}
        }});
        quote
    }

    #[test]
    fn same_chain_gasless_payout_keeps_the_checked_authorization_and_recipient() {
        let body = base_payout_quote();
        let recipient = body["details"]["recipient"].as_str().unwrap();
        let parsed =
            parse_gasless_move(&body, EVM, base_dest(recipient), Exact::In(1_060_000)).unwrap();
        assert_eq!(parsed.amount_in_units, 1_060_000);
        assert_eq!(parsed.amount_out_units, 1_028_000);
        assert_eq!(parsed.typed_data["message"]["to"], RECEIVER);
        assert_eq!(parsed.api, "swap");
        let bad_paths = [
            vec![
                "protocol",
                "v2",
                "orderData",
                "output",
                "payments",
                "0",
                "recipient",
            ],
            vec![
                "protocol",
                "v2",
                "orderData",
                "output",
                "payments",
                "0",
                "currency",
            ],
            vec!["protocol", "v2", "paymentDetails", "depository"],
            vec![
                "protocol",
                "v2",
                "orderData",
                "inputs",
                "0",
                "refunds",
                "0",
                "recipient",
            ],
        ];
        for path in bad_paths {
            let bad = with(
                body.clone(),
                &path,
                json!("0x2222222222222222222222222222222222222222"),
            );
            assert!(
                parse_gasless_move(&bad, EVM, base_dest(recipient), Exact::In(1_060_000)).is_err(),
                "{path:?}"
            );
        }
        for (path, value) in [
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "output",
                    "payments",
                    "0",
                    "minimumAmount",
                ],
                json!("1"),
            ),
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "output",
                    "payments",
                    "0",
                    "minimumAmount",
                ],
                json!("1028001"),
            ),
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "output",
                    "payments",
                    "0",
                    "expectedAmount",
                ],
                json!("1033001"),
            ),
            (
                vec!["protocol", "v2", "orderData", "output", "calls"],
                json!([{"to":recipient}]),
            ),
            (
                vec!["protocol", "v2", "orderData", "fees"],
                json!([{"amount":"1"}]),
            ),
            (
                vec!["protocol", "v2", "paymentDetails", "amount"],
                json!("1060001"),
            ),
        ] {
            let bad = with(body.clone(), &path, value);
            assert!(
                parse_gasless_move(&bad, EVM, base_dest(recipient), Exact::In(1_060_000)).is_err()
            );
        }
    }

    #[tokio::test]
    #[ignore = "live Relay read-only quote; no funds moved"]
    async fn live_same_chain_cashlink_quote() {
        let client = RelayClient::new(std::env::var("RELAY_API_KEY").ok()).unwrap();
        let quote = client
            .base_to_base_all(EVM, "0x1111111111111111111111111111111111111111", 1_060_000)
            .await
            .unwrap();
        assert_eq!(quote.amount_in_units, 1_060_000);
        assert!(quote.amount_out_units >= 1_000_000);
        assert_eq!(quote.typed_data["message"]["to"], RECEIVER);
        println!(
            "Base cash-link payout: {} units in, at least {} out; unsigned",
            quote.amount_in_units, quote.amount_out_units
        );
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

    // Captured from Relay's live API on 2026-10-02 (trimmed): 1.5 USDC from Solana or Base into
    // MON, and 40 MON into Solana USDC.
    fn mon_quote(name: &str) -> Value {
        serde_json::from_str(match name {
            "solana" => include_str!("fixtures/relay-solana-mon.json"),
            "base" => include_str!("fixtures/relay-base-mon.json"),
            _ => include_str!("fixtures/relay-mon-solana.json"),
        })
        .unwrap()
    }

    fn with(mut body: Value, path: &[&str], value: Value) -> Value {
        let mut at = &mut body;
        for key in path {
            at = match key.parse::<usize>() {
                Ok(i) => &mut at[i],
                Err(_) => &mut at[*key],
            };
        }
        *at = value;
        body
    }

    const OTHER: &str = "0x0000000000000000000000000000000000000001";
    const MON_IN: u128 = 40_000_000_000_000_000_000;

    fn from_solana() -> Origin<'static> {
        Origin {
            chain: SOLANA_CHAIN_ID,
            currency: MAINNET_USDC_MINT,
            wallet: SOLANA,
        }
    }

    fn from_base() -> Origin<'static> {
        Origin {
            chain: BASE_CHAIN_ID,
            currency: BASE_USDC,
            wallet: EVM,
        }
    }

    #[test]
    fn mon_bought_with_solana_cash_is_one_deposit_of_exactly_that_cash() {
        let body = mon_quote("solana");
        let swap = parse_monad_swap(&body, from_solana(), monad_dest(EVM), 1_500_000).unwrap();
        assert_eq!(swap.expected_out_units, 43_388_356_662_737_351_665);
        assert_eq!(swap.minimum_out_units, 41_782_987_466_216_069_654);
        let MonadPay::Solana { lookup_tables, .. } = &swap.pay else {
            panic!("a Solana deposit")
        };
        assert_eq!(lookup_tables.len(), 1);
        // Another amount or recipient: refused.
        assert!(parse_monad_swap(&body, from_solana(), monad_dest(EVM), 1_400_000).is_err());
        assert!(parse_monad_swap(&body, from_solana(), monad_dest(OTHER), 1_500_000).is_err());
        let ix = ["steps", "0", "items", "0", "data", "instructions", "0"];
        let order = ["protocol", "v2", "orderData"];
        for (path, value) in [
            (
                [&ix[..], &["programId"]].concat(),
                json!("11111111111111111111111111111111"),
            ),
            ([&ix[..], &["keys", "0", "isSigner"]].concat(), json!(true)),
            (
                [&order[..], &["output", "payments", "0", "recipient"]].concat(),
                json!(OTHER),
            ),
            (
                [&order[..], &["output", "calls"]].concat(),
                json!([{"to": OTHER}]),
            ),
            (
                [&order[..], &["inputs", "0", "refunds", "0", "recipient"]].concat(),
                json!("Other1111111111111111111111111111"),
            ),
            (
                vec!["protocol", "v2", "paymentDetails", "depository"],
                json!("Other1111111111111111111111111111"),
            ),
            (
                vec!["details", "currencyOut", "minimumAmount"],
                json!("50000000000000000000"),
            ),
            (
                vec!["details", "currencyOut", "currency", "address"],
                json!(OTHER),
            ),
        ] {
            let bad = with(body.clone(), &path, value);
            assert!(
                parse_monad_swap(&bad, from_solana(), monad_dest(EVM), 1_500_000).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn mon_bought_with_base_cash_is_one_authorization_to_relays_receiver() {
        let body = mon_quote("base");
        let swap = parse_monad_swap(&body, from_base(), monad_dest(EVM), 1_500_000).unwrap();
        assert_eq!(swap.minimum_out_units, 41_516_979_653_237_946_927);
        let MonadPay::Base { typed_data, api } = &swap.pay else {
            panic!("a Base authorization")
        };
        assert_eq!(api, "swap");
        assert_eq!(typed_data["message"]["value"], "1500000");
        assert_eq!(typed_data["message"]["to"], RECEIVER);
        let sign = ["steps", "0", "items", "0", "data", "sign"];
        for (path, value) in [
            ([&sign[..], &["value", "to"]].concat(), json!(OTHER)),
            ([&sign[..], &["value", "value"]].concat(), json!("9000000")),
            ([&sign[..], &["value", "from"]].concat(), json!(OTHER)),
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "output",
                    "payments",
                    "0",
                    "recipient",
                ],
                json!(OTHER),
            ),
            (
                vec!["protocol", "v2", "orderData", "fees"],
                json!([{"amount": "1"}]),
            ),
            (vec!["details", "recipient"], json!(OTHER)),
        ] {
            let bad = with(body.clone(), &path, value);
            assert!(
                parse_monad_swap(&bad, from_base(), monad_dest(EVM), 1_500_000).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn mon_sold_is_exactly_the_deposit_bound_to_its_order() {
        let body = mon_quote("monad");
        let swap = parse_monad_swap(&body, monad_origin(EVM), solana_dest(SOLANA), MON_IN).unwrap();
        assert_eq!(swap.expected_out_units, 1_298_017);
        assert_eq!(swap.minimum_out_units, 1_248_432);
        assert_eq!(
            swap.pay,
            MonadPay::Monad {
                to: DEPOSITORY.into(),
                data: "0x49290c1c0000000000000000000000004838b106fce9647bdf1e7877bf73ce8b0bad5f97b06a98c49e67643f9fc55d308e1988e2d85d9db099a3736af137ad27afcc7be9".into(),
                value: MON_IN,
            }
        );
        assert!(
            parse_monad_swap(&body, monad_origin(EVM), solana_dest(SOLANA), MON_IN / 2).is_err()
        );
        assert!(parse_monad_swap(&body, monad_origin(OTHER), solana_dest(SOLANA), MON_IN).is_err());
        assert!(parse_monad_swap(&body, monad_origin(EVM), base_dest(EVM), MON_IN).is_err());
        let tx = ["steps", "0", "items", "0", "data"];
        for (path, value) in [
            ([&tx[..], &["to"]].concat(), json!(OTHER)),
            (
                [&tx[..], &["value"]].concat(),
                json!("41000000000000000000"),
            ),
            ([&tx[..], &["chainId"]].concat(), json!(8453)),
            ([&tx[..], &["data"]].concat(), json!("0x49290c1c")),
            (
                vec!["protocol", "v2", "orderId"],
                json!(format!("0x{}", "ab".repeat(32))),
            ),
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "output",
                    "payments",
                    "0",
                    "recipient",
                ],
                json!("Other1111111111111111111111111111"),
            ),
            (
                vec![
                    "protocol",
                    "v2",
                    "orderData",
                    "inputs",
                    "0",
                    "refunds",
                    "0",
                    "recipient",
                ],
                json!(OTHER),
            ),
            (
                vec!["protocol", "v2", "paymentDetails", "depository"],
                json!(OTHER),
            ),
            (
                vec!["steps", "0", "requestId"],
                json!(format!("0x{}", "cd".repeat(32))),
            ),
        ] {
            let bad = with(body.clone(), &path, value);
            assert!(
                parse_monad_swap(&bad, monad_origin(EVM), solana_dest(SOLANA), MON_IN).is_err(),
                "{path:?}"
            );
        }
        // USDC to USDC isn't a MON swap.
        assert!(parse_monad_swap(&body, from_base(), solana_dest(SOLANA), MON_IN).is_err());
    }

    // cargo test -p engine-execution live_mon -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live Relay read-only quotes; no funds moved"]
    async fn live_mon_quotes() {
        let client = RelayClient::new(std::env::var("RELAY_API_KEY").ok()).unwrap();
        let mon = |wei: u128| wei as f64 / 1e18;
        let buy = client.solana_to_mon(SOLANA, EVM, 1_500_000).await.unwrap();
        println!(
            "Solana 1.5 USDC -> {:.4} MON (at least {:.4})",
            mon(buy.expected_out_units),
            mon(buy.minimum_out_units)
        );
        let buy = client.base_to_mon(EVM, 1_500_000).await.unwrap();
        println!(
            "Base 1.5 USDC -> {:.4} MON (at least {:.4})",
            mon(buy.expected_out_units),
            mon(buy.minimum_out_units)
        );
        let sell = client.mon_to_solana(EVM, SOLANA, MON_IN).await.unwrap();
        println!(
            "40 MON -> {} Solana USDC units (at least {}); {:?}",
            sell.expected_out_units, sell.minimum_out_units, sell.pay
        );
        let sell = client.mon_to_base(EVM, MON_IN).await.unwrap();
        println!(
            "40 MON -> {} Base USDC units (at least {})",
            sell.expected_out_units, sell.minimum_out_units
        );
        // The Solana deposit builds into a transaction, and a fresh request waits for its payment.
        let buy = client.solana_to_mon(SOLANA, EVM, 1_500_000).await.unwrap();
        let MonadPay::Solana {
            instructions,
            lookup_tables,
        } = &buy.pay
        else {
            panic!("a Solana deposit")
        };
        let rpc = crate::solana::SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let tx = rpc
            .v0_transaction(SOLANA, instructions, lookup_tables)
            .await
            .unwrap();
        println!("Solana deposit transaction: {} base64 chars", tx.len());
        assert_eq!(
            client.state(&buy.request_id).await.unwrap(),
            SwapState::Waiting
        );
        let buy = client.base_to_mon(EVM, 1_500_000).await.unwrap();
        if let MonadPay::Base { typed_data, .. } = &buy.pay {
            println!("BASE_TYPED {typed_data}");
        }
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

    // A bank payout from Solana cash: Relay delivers to the payout partner's fresh Base address. The
    // Solana deposit is built and simulated exactly as the engine does; nothing is signed or sent.
    // cargo test -p engine-execution live_relay_bank_payout -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_relay_bank_payout_to_a_fresh_base_address() {
        let client = RelayClient::new(std::env::var("RELAY_API_KEY").ok()).unwrap();
        let owner = "6metVveeGpQN6YoXYmevmvtQp7k5CvCgaKBRuQUgevKR";
        let fresh = format!(
            "0x{:040x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let moved = client
            .solana_to_base(owner, &fresh, 1_100_000, 0)
            .await
            .unwrap();
        assert_eq!(moved.amount_out_units, 1_100_000);
        let solana = crate::solana::SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            std::env::var("ATLAS_SOLANA_MAINNET_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
            "",
        )
        .unwrap();
        let tx = solana
            .v0_transaction(owner, &moved.instructions, &moved.lookup_tables)
            .await
            .unwrap();
        let units = solana.preflight_swap(&tx).await.unwrap();
        println!(
            "Solana cash → fresh Base payout address: {} in for 1.10 USDC out (fee {}), simulated OK ({units} CU); nothing signed or sent",
            moved.amount_in_units,
            moved.amount_in_units - 1_100_000
        );
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
