//! Jupiter Swap API V2 client for one Solana token path.
//! Spot, memes and tokenized equities differ only by their mint addresses.

use std::{collections::HashMap, str::FromStr, time::Duration};

use crate::solana::{SolanaAtaPreflight, SolanaPreflightError};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, AddressLookupTableAccount, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use thiserror::Error;

const ORDER_URL: &str = "https://api.jup.ag/swap/v2/order";
// The price margins a user-paid swap is built with, narrowest first (0.5%, 1.5%, 3%).
const SLIPPAGE_STEPS_BPS: [u32; 3] = [50, 150, 300];

// Jupiter's swap program refuses with custom error 6001 when the price moved past the margin.
fn slippage_exceeded(error: &JupiterError) -> bool {
    matches!(error, JupiterError::Preflight(SolanaPreflightError::SwapSimulation(reason))
        if reason.contains("\"Custom\":6001"))
}
const BUILD_URL: &str = "https://api.jup.ag/swap/v2/build";
const EXECUTE_URL: &str = "https://api.jup.ag/swap/v2/execute";
// Jupiter covers the fee itself for a low-SOL taker, but only above a minimum ("Minimum $5 for
// gasless"); below it the order comes back with this code and no transaction.
const BELOW_GASLESS_MINIMUM: i64 = 3;
// The taker can't pay what the swap costs (its tokens, or the SOL for fees and a new token account).
const INSUFFICIENT_FUNDS: i64 = 1;

#[derive(Clone)]
pub struct JupiterClient {
    http: Client,
    api_key: Option<String>,
    #[cfg(test)]
    endpoints: Option<String>,
}

#[derive(Clone, Debug)]
pub struct JupiterOrderRequest {
    pub input_mint: String,
    pub output_mint: String,
    pub amount_base_units: u64,
    pub taker: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterOrder {
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub request_id: String,
    pub router: String,
    pub transaction: Option<String>,
    #[serde(default)]
    pub fee_bps: Option<u32>,
    #[serde(default)]
    pub fee_mint: Option<String>,
    #[serde(default)]
    pub error_code: Option<i64>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "optional_height")]
    pub last_valid_block_height: Option<u64>,
    /// True when Jupiter pays the network fee and account rent (low-SOL takers on eligible routes).
    #[serde(default)]
    pub gasless: bool,
}

/// The user pays this swap's network fee. Kept with the intent to bind the signed bytes and
/// follow its signature through confirmation, including an interrupted submission.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UserPaidSwap {
    pub transaction: String,
    pub input_mint: String,
    pub output_mint: String,
    pub last_valid_block_height: u64,
    #[serde(default)]
    pub minimum_out: u128,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildQuote {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    other_amount_threshold: String,
    swap_mode: String,
    slippage_bps: u32,
    #[serde(default)]
    compute_budget_instructions: Vec<BuildInstruction>,
    setup_instructions: Vec<BuildInstruction>,
    swap_instruction: BuildInstruction,
    cleanup_instruction: Option<BuildInstruction>,
    #[serde(default)]
    other_instructions: Vec<BuildInstruction>,
    tip_instruction: Option<BuildInstruction>,
    #[serde(default, deserialize_with = "lookup_tables")]
    addresses_by_lookup_table_address: HashMap<String, Vec<String>>,
    blockhash_with_metadata: BuildBlockhash,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildBlockhash {
    blockhash: Vec<u8>,
    last_valid_block_height: u64,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildInstruction {
    program_id: String,
    accounts: Vec<BuildAccount>,
    data: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildAccount {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

impl BuildQuote {
    fn transaction(
        &self,
        request: &JupiterOrderRequest,
        units: u32,
        priority_fee: bool,
        max_slippage_bps: u32,
    ) -> Result<String, JupiterError> {
        let invalid = || JupiterError::InvalidRequest;
        let key = |s: &str| Pubkey::from_str(s).map_err(|_| invalid());
        let payer = key(request.taker.as_deref().ok_or_else(invalid)?)?;
        let input: u64 = self.in_amount.parse().map_err(|_| invalid())?;
        let output: u128 = self.out_amount.parse().map_err(|_| invalid())?;
        let minimum: u128 = self.other_amount_threshold.parse().map_err(|_| invalid())?;
        if self.input_mint != request.input_mint
            || self.output_mint != request.output_mint
            || input != request.amount_base_units
            || output == 0
            || self.swap_mode != "ExactIn"
            || self.slippage_bps > max_slippage_bps
            || minimum < output * u128::from(10_000 - max_slippage_bps.min(10_000)) / 10_000
            || minimum > output
        {
            return Err(invalid());
        }
        let mut limit = vec![2];
        limit.extend_from_slice(&units.to_le_bytes());
        let mut instructions = vec![Instruction {
            program_id: key("ComputeBudget111111111111111111111111111111")?,
            accounts: Vec::new(),
            data: limit,
        }];
        for ix in self
            .compute_budget_instructions
            .iter()
            .chain(&self.setup_instructions)
            .chain(&self.other_instructions)
            .chain(std::iter::once(&self.swap_instruction))
            .chain(self.cleanup_instruction.iter())
            .chain(self.tip_instruction.iter())
        {
            let accounts = ix
                .accounts
                .iter()
                .map(|a| {
                    let pubkey = key(&a.pubkey)?;
                    if a.is_signer && pubkey != payer {
                        return Err(invalid());
                    }
                    Ok(AccountMeta {
                        pubkey,
                        is_signer: a.is_signer,
                        is_writable: a.is_writable,
                    })
                })
                .collect::<Result<Vec<_>, JupiterError>>()?;
            let data = STANDARD.decode(&ix.data).map_err(|_| invalid())?;
            if ix.program_id == "ComputeBudget111111111111111111111111111111"
                && (data.len() != 9 || data[0] != 3)
            {
                return Err(invalid());
            }
            if !priority_fee && ix.program_id == "ComputeBudget111111111111111111111111111111" {
                continue;
            }
            instructions.push(Instruction {
                program_id: key(&ix.program_id)?,
                accounts,
                data,
            });
        }
        let tables = self
            .addresses_by_lookup_table_address
            .iter()
            .map(|(address, keys)| {
                Ok(AddressLookupTableAccount {
                    key: key(address)?,
                    addresses: keys.iter().map(|k| key(k)).collect::<Result<Vec<_>, _>>()?,
                })
            })
            .collect::<Result<Vec<_>, JupiterError>>()?;
        let hash = Hash::new_from_array(
            self.blockhash_with_metadata
                .blockhash
                .as_slice()
                .try_into()
                .map_err(|_| invalid())?,
        );
        let message = v0::Message::try_compile(&payer, &instructions, &tables, hash)
            .map_err(|_| invalid())?;
        if message.header.num_required_signatures != 1 {
            return Err(invalid());
        }
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(message),
        };
        let bytes = bincode::serialize(&tx).map_err(|_| invalid())?;
        if bytes.len() > 1232 {
            return Err(invalid());
        }
        Ok(STANDARD.encode(bytes))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JupiterExecution {
    pub status: String,
    #[serde(default)]
    pub signature: String,
    pub code: i64,
    #[serde(default)]
    pub total_input_amount: Option<String>,
    #[serde(default)]
    pub total_output_amount: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Error)]
pub enum JupiterError {
    #[error("invalid Jupiter swap mints, taker, or amount")]
    InvalidRequest,
    #[error("signed transaction is not base64")]
    InvalidTransaction,
    #[error("Jupiter returned a quote that cannot be executed: {0}")]
    NotExecutable(String),
    #[error("{0}")]
    Preflight(#[from] SolanaPreflightError),
    #[error("Jupiter request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Jupiter {operation} returned HTTP {status}: {reason}")]
    Rejected {
        operation: &'static str,
        status: reqwest::StatusCode,
        reason: String,
    },
    #[error("Jupiter could not complete this swap (code {code}): {reason}")]
    ExecutionFailed { code: i64, reason: String },
    #[error("This coin's price is moving too fast to buy right now. Try again in a moment.")]
    PriceMoving,
    #[error("Jupiter URL could not be parsed")]
    Url(#[from] url::ParseError),
}

impl JupiterClient {
    fn endpoint(&self, _path: &str, default: &str) -> String {
        #[cfg(test)]
        if let Some(base) = &self.endpoints {
            return format!("{base}/{_path}");
        }
        default.into()
    }

    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: Client::new(),
            api_key,
            #[cfg(test)]
            endpoints: None,
        }
    }

    /// Without a taker this returns a real quote but no signable transaction.
    /// A fresh order with the Privy wallet address must be fetched at confirm.
    pub async fn order(&self, request: &JupiterOrderRequest) -> Result<JupiterOrder, JupiterError> {
        validate_order(request)?;
        let order = self.fetch_order(request, None).await?;
        if request.taker.is_some() && order.transaction.as_deref().unwrap_or("").is_empty() {
            return Err(JupiterError::NotExecutable(not_executable_reason(&order)));
        }
        Ok(order)
    }

    /// Read SOL first. Build a normal swap when the user can cover its fee and rent; only
    /// ask for an automatic gasless order after the on-chain preflight proves gas is short.
    pub async fn order_for_wallet(
        &self,
        request: &JupiterOrderRequest,
        solana: &SolanaAtaPreflight,
    ) -> Result<(JupiterOrder, Option<UserPaidSwap>), JupiterError> {
        validate_order(request)?;
        let wallet = request
            .taker
            .as_deref()
            .ok_or(JupiterError::InvalidRequest)?;
        let balance = solana.owner_sol_balance(wallet).await?;
        if balance >= 5_000 {
            match self.user_paid_order(request, solana).await {
                Ok(prepared) => return Ok(prepared),
                Err(JupiterError::Preflight(SolanaPreflightError::InsufficientGas)) => {}
                Err(error) => return Err(error),
            }
        }
        self.gasless_order(request).await.map(|order| (order, None))
    }

    /// An executable route only. An indicative quote never authorizes a fee-paid transfer.
    pub async fn gasless_order(
        &self,
        request: &JupiterOrderRequest,
    ) -> Result<JupiterOrder, JupiterError> {
        validate_order(request)?;
        let order = match self.fetch_order(request, None).await {
            Err(JupiterError::Rejected { status, reason, .. })
                if status == reqwest::StatusCode::BAD_REQUEST
                    && reason.to_ascii_lowercase().contains("gasless")
                    && reason.to_ascii_lowercase().contains("minimum") =>
            {
                return Err(SolanaPreflightError::InsufficientGas.into())
            }
            result => result?,
        };
        if order.transaction.as_deref().unwrap_or("").is_empty() {
            if own_gas_needed(&order) || (order.router != "jupiterz" && order.error_code == Some(2))
            {
                return Err(SolanaPreflightError::InsufficientGas.into());
            }
            return Err(JupiterError::NotExecutable(not_executable_reason(&order)));
        }
        if !order.gasless {
            return Err(SolanaPreflightError::InsufficientGas.into());
        }
        Ok(order)
    }

    /// Build and simulate only the user-paid route; never fall back to a gasless minimum. A
    /// fast-moving coin can move past the margin between quoting and the check before signing
    /// (Jupiter's 6001, slippage exceeded): the swap is built again with a wider margin, each one
    /// simulated before it's offered.
    pub async fn user_paid_order(
        &self,
        request: &JupiterOrderRequest,
        solana: &SolanaAtaPreflight,
    ) -> Result<(JupiterOrder, Option<UserPaidSwap>), JupiterError> {
        for slippage_bps in SLIPPAGE_STEPS_BPS {
            match self.user_paid_order_at(request, solana, slippage_bps).await {
                Err(error) if slippage_exceeded(&error) => continue,
                other => return other,
            }
        }
        Err(JupiterError::PriceMoving)
    }

    async fn user_paid_order_at(
        &self,
        request: &JupiterOrderRequest,
        solana: &SolanaAtaPreflight,
        slippage_bps: u32,
    ) -> Result<(JupiterOrder, Option<UserPaidSwap>), JupiterError> {
        let mut url = Url::parse(&self.endpoint("build", BUILD_URL))?;
        url.query_pairs_mut()
            .append_pair("inputMint", &request.input_mint)
            .append_pair("outputMint", &request.output_mint)
            .append_pair("amount", &request.amount_base_units.to_string())
            .append_pair(
                "taker",
                request
                    .taker
                    .as_deref()
                    .ok_or(JupiterError::InvalidRequest)?,
            )
            .append_pair("slippageBps", &slippage_bps.to_string())
            .append_pair("computeUnitPricePercentile", "medium");
        let mut call = self.http.get(url).timeout(Duration::from_secs(20));
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(rejected("user-paid quote", response).await);
        }
        let quote: BuildQuote = response.json().await?;
        let initial = quote.transaction(request, 1_400_000, false, slippage_bps)?;
        let consumed = solana.preflight_swap(&initial).await?;
        let units = consumed
            .saturating_mul(12)
            .div_ceil(10)
            .saturating_add(10_000)
            .min(1_400_000);
        let transaction = quote.transaction(request, units, true, slippage_bps)?;
        solana.preflight_swap(&transaction).await?;
        let prepared = UserPaidSwap {
            transaction: transaction.clone(),
            input_mint: quote.input_mint.clone(),
            output_mint: quote.output_mint.clone(),
            last_valid_block_height: quote.blockhash_with_metadata.last_valid_block_height,
            minimum_out: quote
                .other_amount_threshold
                .parse()
                .map_err(|_| JupiterError::InvalidRequest)?,
        };
        Ok((
            JupiterOrder {
                input_mint: quote.input_mint,
                output_mint: quote.output_mint,
                in_amount: quote.in_amount,
                out_amount: quote.out_amount,
                request_id: String::new(),
                router: "metis".into(),
                transaction: Some(transaction),
                fee_bps: Some(0),
                fee_mint: None,
                error_code: None,
                error_message: None,
                last_valid_block_height: Some(prepared.last_valid_block_height),
                gasless: false,
            },
            Some(prepared),
        ))
    }

    // `payer` only sponsors gas when it differs from the taker; naming the taker has no effect.
    async fn fetch_order(
        &self,
        request: &JupiterOrderRequest,
        payer: Option<&str>,
    ) -> Result<JupiterOrder, JupiterError> {
        let mut url = Url::parse(&self.endpoint("order", ORDER_URL))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("inputMint", &request.input_mint);
            query.append_pair("outputMint", &request.output_mint);
            query.append_pair("amount", &request.amount_base_units.to_string());
            if let Some(taker) = &request.taker {
                query.append_pair("taker", taker);
            }
            if let Some(payer) = payer {
                query.append_pair("payer", payer);
            }
        }
        let mut call = self.http.get(url).timeout(Duration::from_secs(20));
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(rejected("quote", response).await);
        }
        Ok(response.json().await?)
    }

    /// The app signs the order with Privy; Jupiter lands the signed transaction.
    pub async fn execute(
        &self,
        request_id: &str,
        signed_transaction: &str,
    ) -> Result<JupiterExecution, JupiterError> {
        if request_id.is_empty() {
            return Err(JupiterError::InvalidRequest);
        }
        if STANDARD.decode(signed_transaction).is_err() {
            return Err(JupiterError::InvalidTransaction);
        }
        let mut call = self.http.post(EXECUTE_URL).json(&serde_json::json!({
            "requestId": request_id,
            "signedTransaction": signed_transaction,
        }));
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(rejected("submission", response).await);
        }
        let result: JupiterExecution = response.json().await?;
        if result.status != "Success" || result.code != 0 {
            return Err(JupiterError::ExecutionFailed {
                code: result.code,
                reason: safe_reason(
                    result
                        .error
                        .as_deref()
                        .unwrap_or("Check your transaction status before trying again."),
                ),
            });
        }
        Ok(result)
    }
}

// Jupiter offered to pay the fee but the swap is below its minimum for that.
fn own_gas_needed(order: &JupiterOrder) -> bool {
    order.router != "jupiterz"
        && order.error_code == Some(BELOW_GASLESS_MINIMUM)
        && order.transaction.as_deref().unwrap_or("").is_empty()
}

// What the user reads when Jupiter can't make the swap.
fn not_executable_reason(order: &JupiterOrder) -> String {
    if own_gas_needed(order) {
        return order.error_message.clone().unwrap_or_else(|| {
            "This amount is below the route's minimum for a fee-paid swap.".into()
        });
    }
    match order.error_code {
        Some(INSUFFICIENT_FUNDS) => {
            "There isn't enough in this wallet for the swap and its network fee (paid in SOL)"
                .into()
        }
        _ => order
            .error_message
            .clone()
            .unwrap_or_else(|| format!("code {:?}", order.error_code)),
    }
}

fn lookup_tables<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<HashMap<String, Vec<String>>, D::Error> {
    Option::<HashMap<String, Vec<String>>>::deserialize(d).map(Option::unwrap_or_default)
}

// Some routers send the block height as a JSON string, others as a number or null.
fn optional_height<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    v.map(|v| {
        v.as_u64()
            .or_else(|| v.as_str()?.parse().ok())
            .ok_or_else(|| serde::de::Error::custom("invalid block height"))
    })
    .transpose()
}
fn safe_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .map(|w| if w.len() > 100 { "[omitted]" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}
async fn rejected(operation: &'static str, response: reqwest::Response) -> JupiterError {
    let status = response.status();
    let mut response = response;
    // Read only a small error body; never expose the request, signed bytes or partner key.
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > 4096 {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default();
    let reason = body["errorMessage"]
        .as_str()
        .or_else(|| body["error"].as_str())
        .or_else(|| body["message"].as_str())
        .filter(|s| {
            !s.to_ascii_lowercase().contains("signedtransaction")
                && !s.to_ascii_lowercase().contains("api-key")
        })
        .map(safe_reason)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            "The venue rejected this request. Check the transaction status before retrying.".into()
        });
    let reason = if let Some(code) = body["code"].as_i64() {
        format!("{reason} (code {code})")
    } else {
        reason
    };
    JupiterError::Rejected {
        operation,
        status,
        reason,
    }
}

fn validate_order(request: &JupiterOrderRequest) -> Result<(), JupiterError> {
    if request.amount_base_units == 0
        || request.input_mint == request.output_mint
        || Pubkey::from_str(&request.input_mint).is_err()
        || Pubkey::from_str(&request.output_mint).is_err()
        || request
            .taker
            .as_deref()
            .is_some_and(|address| Pubkey::from_str(address).is_err())
    {
        return Err(JupiterError::InvalidRequest);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn captured_build() -> serde_json::Value {
        serde_json::from_str(include_str!("fixtures/jupiter_user_paid.json")).unwrap()
    }
    fn small_request() -> JupiterOrderRequest {
        JupiterOrderRequest {
            input_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            amount_base_units: 500_000,
            taker: Some("6metVveeGpQN6YoXYmevmvtQp7k5CvCgaKBRuQUgevKR".into()),
        }
    }

    #[test]
    fn only_a_slippage_refusal_is_retried_with_a_wider_margin() {
        let refusal =
            |err: &str| JupiterError::Preflight(SolanaPreflightError::SwapSimulation(err.into()));
        assert!(slippage_exceeded(&refusal(
            r#"{"InstructionError":[2,{"Custom":6001}]}"#
        )));
        assert!(!slippage_exceeded(&refusal(
            r#"{"InstructionError":[2,{"Custom":6000}]}"#
        )));
        assert!(!slippage_exceeded(&JupiterError::InvalidRequest));
        assert_eq!(SLIPPAGE_STEPS_BPS[0], 50);
        assert!(SLIPPAGE_STEPS_BPS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn user_paid_build_pins_amount_slippage_and_sole_fee_payer() {
        let v = captured_build();
        let quote: BuildQuote = serde_json::from_value(v.clone()).unwrap();
        let request = small_request();
        let bytes = STANDARD
            .decode(quote.transaction(&request, 240_000, true, 100).unwrap())
            .unwrap();
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        assert_eq!(tx.message.header().num_required_signatures, 1);
        assert_eq!(
            tx.message.static_account_keys()[0].to_string(),
            request.taker.clone().unwrap()
        );
        for (field, changed) in [
            ("inAmount", serde_json::json!("500001")),
            ("outputMint", serde_json::json!(request.input_mint)),
            ("otherAmountThreshold", serde_json::json!("1")),
            ("slippageBps", serde_json::json!(101)),
        ] {
            let mut bad = v.clone();
            bad[field] = changed;
            let bad: BuildQuote = serde_json::from_value(bad).unwrap();
            assert!(
                bad.transaction(&request, 240_000, true, 100).is_err(),
                "{field}"
            );
        }
        let mut changed = v;
        changed["swapInstruction"]["accounts"][0]["isSigner"] = serde_json::json!(true);
        changed["swapInstruction"]["accounts"][0]["pubkey"] =
            serde_json::json!(spl_token::id().to_string());
        let changed: BuildQuote = serde_json::from_value(changed).unwrap();
        assert!(changed.transaction(&request, 240_000, true, 100).is_err());
    }

    // The real HTTP routing path, with read-only RPC responses and the live /build capture.
    fn mock_route(
        replies: Vec<serde_json::Value>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let task = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for reply in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(_) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("missing request: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut raw = Vec::new();
                loop {
                    let mut bytes = [0; 8192];
                    let n = stream.read(&mut bytes).unwrap();
                    assert!(n > 0);
                    raw.extend_from_slice(&bytes[..n]);
                    if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..split]);
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|n| n.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if raw.len() >= split + 4 + len {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(raw).unwrap();
                if request.starts_with("GET ") {
                    paths.push(
                        request
                            .lines()
                            .next()
                            .unwrap()
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .split('?')
                            .next()
                            .unwrap()
                            .into(),
                    );
                } else {
                    let body: serde_json::Value =
                        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                    paths.push(body["method"].as_str().unwrap().into());
                }
                let body = reply.to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",body.len(),body).unwrap();
            }
            paths
        });
        (url, task)
    }

    #[tokio::test]
    async fn funded_wallet_below_jupiters_gasless_threshold_uses_own_gas_first() {
        let simulation =
            serde_json::json!({"result":{"value":{"err":null,"unitsConsumed":180000}}});
        let (url, task) = mock_route(vec![
            serde_json::json!({"result":{"value":4_000_000}}),
            captured_build(),
            simulation.clone(),
            simulation,
        ]);
        let mut client = JupiterClient::new(None);
        client.endpoints = Some(url.clone());
        let solana =
            SolanaAtaPreflight::new(crate::solana::SolanaNetwork::Mainnet, url, "unused").unwrap();
        let (order, swap) = client
            .order_for_wallet(&small_request(), &solana)
            .await
            .unwrap();
        assert!(!order.gasless);
        assert!(swap.is_some());
        assert_eq!(
            task.join().unwrap(),
            [
                "getBalance",
                "/build",
                "simulateTransaction",
                "simulateTransaction"
            ]
        );
    }

    #[tokio::test]
    async fn empty_gas_tank_is_checked_before_an_automatic_order() {
        let (url, task) = mock_route(vec![
            serde_json::json!({"result":{"value":0}}),
            serde_json::json!({"inputMint":"a","outputMint":"b","inAmount":"500000","outAmount":"1",
                "requestId":"r","router":"jupiterz","transaction":"tx","gasless":true}),
        ]);
        let mut client = JupiterClient::new(None);
        client.endpoints = Some(url.clone());
        let solana =
            SolanaAtaPreflight::new(crate::solana::SolanaNetwork::Mainnet, url, "unused").unwrap();
        let (order, swap) = client
            .order_for_wallet(&small_request(), &solana)
            .await
            .unwrap();
        assert!(order.gasless);
        assert!(swap.is_none());
        assert_eq!(task.join().unwrap(), ["getBalance", "/order"]);
    }

    #[test]
    fn quote_without_lookup_tables_is_valid_to_parse() {
        let mut value = captured_build();
        value["addressesByLookupTableAddress"] = serde_json::Value::Null;
        let quote: BuildQuote = serde_json::from_value(value).unwrap();
        assert!(quote.addresses_by_lookup_table_address.is_empty());
    }

    #[tokio::test]
    async fn missing_account_rent_checks_gasless_only_after_preflight_and_refuses_minimum() {
        let (url, task) = mock_route(vec![
            serde_json::json!({"result":{"value":1_000_000}}),
            captured_build(),
            serde_json::json!({"result":{"value":{"err":{"InsufficientFundsForRent":{"account_index":1}}}}}),
            serde_json::json!({"inputMint":"a","outputMint":"b","inAmount":"500000","outAmount":"1", "requestId":"r", "router":"metis","transaction":null,"gasless":true,"errorCode":3,"errorMessage":"Minimum $5 for gasless"}),
        ]);
        let mut client = JupiterClient::new(None);
        client.endpoints = Some(url.clone());
        let solana =
            SolanaAtaPreflight::new(crate::solana::SolanaNetwork::Mainnet, url, "unused").unwrap();
        assert!(matches!(
            client.order_for_wallet(&small_request(), &solana).await,
            Err(JupiterError::Preflight(
                SolanaPreflightError::InsufficientGas
            ))
        ));
        assert_eq!(
            task.join().unwrap(),
            ["getBalance", "/build", "simulateTransaction", "/order"]
        );
    }

    #[tokio::test]
    #[ignore = "read-only mainnet quote, balance and simulation; never signs or submits"]
    async fn live_small_user_paid_swap() {
        let rpc = std::env::var("ATLAS_SOLANA_MAINNET_RPC_URL")
            .unwrap_or("https://api.mainnet-beta.solana.com".into());
        let mut builder = Client::builder().timeout(Duration::from_secs(25));
        if let Ok(ip) = std::env::var("JUP_DNS_IP") {
            builder = builder.resolve("api.jup.ag", format!("{ip}:443").parse().unwrap());
        }
        if let Ok(ip) = std::env::var("SOL_DNS_IP") {
            builder = builder.resolve(
                Url::parse(&rpc).unwrap().host_str().unwrap(),
                format!("{ip}:443").parse().unwrap(),
            );
        }
        let http = builder.build().unwrap();
        let mut client = JupiterClient::new(std::env::var("JUPITER_API_KEY").ok());
        client.http = http.clone();
        let solana = SolanaAtaPreflight::new(crate::solana::SolanaNetwork::Mainnet, rpc, "unused")
            .unwrap()
            .with_test_http(http);
        let mut request = small_request();
        if let Ok(wallet) = std::env::var("JUP_DRY_WALLET") {
            request.taker = Some(wallet);
        }
        let balance = solana
            .owner_sol_balance(request.taker.as_deref().unwrap())
            .await
            .unwrap();
        println!("mainnet SOL balance: {balance} lamports");
        let (order, swap) = client.order_for_wallet(&request, &solana).await.unwrap();
        let swap = match swap {
            Some(swap) => swap,
            None => {
                println!("actual fee/rent preflight found insufficient SOL; executable gasless fallback={} input={}; nothing signed or sent", order.gasless, order.in_amount);
                assert!(order.gasless);
                // Exercise the fully funded path on the public fee payer from the live order.
                // Its account is only read and simulated. No key, approval or submission is used.
                let quoted: VersionedTransaction =
                    bincode::deserialize(&STANDARD.decode(order.transaction.unwrap()).unwrap())
                        .unwrap();
                let payer = quoted.message.static_account_keys()[0].to_string();
                request.taker = Some(payer);
                request.input_mint = "So11111111111111111111111111111111111111112".into();
                request.output_mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into();
                request.amount_base_units = 500_000;
                let funded = solana
                    .owner_sol_balance(request.taker.as_deref().unwrap())
                    .await
                    .unwrap();
                println!(
                    "public fee-payer SOL balance: {funded} lamports; testing unsigned SOL to USDC"
                );
                let (paid_order, paid_swap) =
                    client.order_for_wallet(&request, &solana).await.unwrap();
                assert!(!paid_order.gasless);
                println!(
                    "funded user-paid mainnet build: input={} output={} simulation=passed",
                    paid_order.in_amount, paid_order.out_amount
                );
                paid_swap.expect("funded wallet must use its own gas")
            }
        };
        let tx: VersionedTransaction =
            bincode::deserialize(&STANDARD.decode(&swap.transaction).unwrap()).unwrap();
        assert_eq!(
            tx.message.static_account_keys()[0].to_string(),
            request.taker.unwrap()
        );
        println!("user-paid build: input={} output={} gasless={} payer={} bytes={} simulation=passed; nothing signed or sent",
            request.amount_base_units, "verified by simulation", false, tx.message.static_account_keys()[0], STANDARD.decode(&swap.transaction).unwrap().len());
    }
    #[test]
    fn reads_whether_jupiter_pays_the_gas() {
        // Captured 2026-09-30: $0.50 USDC → SOL for a low-SOL taker, filled by JupiterZ.
        let order: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","outputMint":"So11111111111111111111111111111111111111112",
            "inAmount":"500000","outAmount":"4200538","requestId":"r","router":"jupiterz","transaction":"tx","gasless":true
        })).unwrap();
        assert!(order.gasless);
        let older: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r","router":"metis","transaction":null
        })).unwrap();
        assert!(!older.gasless);
    }

    #[test]
    fn reads_string_block_heights_and_does_not_misclassify_jupiterz() {
        let order: JupiterOrder=serde_json::from_value(serde_json::json!({"inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r","router":"jupiterz","transaction":null,"errorCode":3,"lastValidBlockHeight":"350001234"})).unwrap();
        assert_eq!(order.last_valid_block_height, Some(350001234));
        assert!(!own_gas_needed(&order));
        assert_eq!(safe_reason(&"a".repeat(200)), "[omitted]");
    }
    use super::*;

    #[test]
    fn meme_and_stock_mints_use_the_same_order_shape() {
        let request = JupiterOrderRequest {
            input_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            output_mint: "So11111111111111111111111111111111111111112".into(),
            amount_base_units: 1_000_000,
            taker: None,
        };
        assert!(validate_order(&request).is_ok());
        let mut invalid = request.clone();
        invalid.amount_base_units = 0;
        assert!(matches!(
            validate_order(&invalid),
            Err(JupiterError::InvalidRequest)
        ));
    }

    #[test]
    fn gasless_minimum_is_reported_only_for_aggregator_routes() {
        // Shape from Jupiter's /order for a ₦500 (about $0.34) buy by a low-SOL taker, 2026-10-02.
        let refused: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"340000","outAmount":"1","requestId":"r",
            "router":"metis","transaction":null,"gasless":true,"errorCode":3,
            "errorMessage":"Minimum $5 for gasless"
        }))
        .unwrap();
        assert!(own_gas_needed(&refused));
        // A normally funded aggregator route comes back with a signable transaction.
        let own: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"340000","outAmount":"1","requestId":"r",
            "router":"okx","transaction":"tx","gasless":false
        }))
        .unwrap();
        assert!(!own_gas_needed(&own));
        let broke: JupiterOrder = serde_json::from_value(serde_json::json!({
            "inputMint":"a","outputMint":"b","inAmount":"1","outAmount":"1","requestId":"r",
            "router":"okx","transaction":null,"errorCode":1,"errorMessage":"Insufficient funds"
        }))
        .unwrap();
        assert!(!own_gas_needed(&broke));
        assert!(not_executable_reason(&broke).contains("network fee"));
    }

    #[test]
    fn quote_only_order_has_no_signable_transaction() {
        let sample = r#"{"inputMint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","outputMint":"So11111111111111111111111111111111111111112","inAmount":"1000000","outAmount":"8415352","requestId":"real-id","router":"jupiterz","transaction":null,"feeMint":"So11111111111111111111111111111111111111112","feeBps":2}"#;
        let order: JupiterOrder = serde_json::from_str(sample).unwrap();
        assert_eq!(order.out_amount, "8415352");
        assert!(order.transaction.is_none());
    }
}
