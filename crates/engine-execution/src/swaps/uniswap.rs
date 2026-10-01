//! Keyless Base mainnet swaps through deployed Uniswap V3 contracts.
//! A quote is an on-chain read; signing and broadcasting remain separate.

use reqwest::{Client, Url};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use thiserror::Error;

use super::oneinch::BaseSwapRequest;

pub const BASE_CHAIN_ID: u64 = 8453;
pub const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
pub const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
pub const QUOTER_V2: &str = "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a";
pub const SWAP_ROUTER_02: &str = "0x2626664c2603336E57B271c5C0b26F421741e481";
const FEE_TIERS: [u32; 4] = [100, 500, 3000, 10000];

#[derive(Clone)]
pub struct UniswapV3Client {
    http: Client,
    rpc_url: Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseV3Quote {
    pub source_token: String,
    pub destination_token: String,
    pub amount_in: u128,
    pub amount_out: u128,
    pub fee: u32,
    pub quoter_gas_estimate: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmUnsignedTransaction {
    pub from: String,
    pub to: String,
    pub data: String,
    pub value: String,
}

#[derive(Debug, Error)]
pub enum UniswapV3Error {
    #[error("Base RPC URL must use HTTPS")]
    InsecureRpc,
    #[error("invalid token, wallet, amount, or slippage")]
    InvalidRequest,
    #[error("RPC is connected to chain {0}, expected Base mainnet 8453")]
    WrongChain(u64),
    #[error("Uniswap V3 has no quoted direct pool for this pair")]
    NoQuotedPool,
    #[error("RPC transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("RPC rejected the call: {0}")]
    Rpc(String),
    #[error("RPC returned invalid contract data")]
    InvalidRpcResponse,
    #[error("contract amount exceeds supported range")]
    AmountOverflow,
}

impl UniswapV3Client {
    pub fn new(rpc_url: Url) -> Result<Self, UniswapV3Error> {
        if rpc_url.scheme() != "https" {
            return Err(UniswapV3Error::InsecureRpc);
        }
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()?,
            rpc_url,
        })
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, UniswapV3Error> {
        let response = self
            .http
            .post(self.rpc_url.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        if let Some(error) = body.get("error") {
            return Err(UniswapV3Error::Rpc(error.to_string()));
        }
        body.get("result")
            .cloned()
            .ok_or(UniswapV3Error::InvalidRpcResponse)
    }

    async fn check_chain(&self) -> Result<(), UniswapV3Error> {
        let chain = self.rpc("eth_chainId", json!([])).await?;
        let id = u64::from_str_radix(
            chain
                .as_str()
                .and_then(|value| value.strip_prefix("0x"))
                .ok_or(UniswapV3Error::InvalidRpcResponse)?,
            16,
        )
        .map_err(|_| UniswapV3Error::InvalidRpcResponse)?;
        if id != BASE_CHAIN_ID {
            return Err(UniswapV3Error::WrongChain(id));
        }
        Ok(())
    }

    async fn eth_call(&self, to: &str, data: &str) -> Result<String, UniswapV3Error> {
        let result = self
            .rpc("eth_call", json!([{"to": to, "data": data}, "latest"]))
            .await?;
        result
            .as_str()
            .map(str::to_owned)
            .ok_or(UniswapV3Error::InvalidRpcResponse)
    }

    /// Read all standard V3 fee tiers and choose the largest real output.
    /// This covers a direct pool only; absent direct liquidity is explicit.
    pub async fn quote_direct(
        &self,
        request: &BaseSwapRequest,
    ) -> Result<BaseV3Quote, UniswapV3Error> {
        validate_request(request)?;
        self.check_chain().await?;
        let source = address_bytes(&request.source_token)?;
        let destination = address_bytes(&request.destination_token)?;
        let mut best: Option<BaseV3Quote> = None;
        for fee in FEE_TIERS {
            let data = quote_call(source, destination, request.amount_base_units, fee);
            let response = match self.eth_call(QUOTER_V2, &data).await {
                Ok(response) => response,
                Err(UniswapV3Error::Rpc(reason)) if looks_like_revert(&reason) => continue,
                Err(error) => return Err(error),
            };
            let (amount_out, gas) = decode_quote(&response)?;
            if amount_out == 0 {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|quote| amount_out > quote.amount_out)
            {
                best = Some(BaseV3Quote {
                    source_token: request.source_token.clone(),
                    destination_token: request.destination_token.clone(),
                    amount_in: request.amount_base_units,
                    amount_out,
                    fee,
                    quoter_gas_estimate: gas,
                });
            }
        }
        best.ok_or(UniswapV3Error::NoQuotedPool)
    }

    /// Read a mainnet ERC-20 balance before returning a user-signable plan.
    pub async fn balance_of(&self, token: &str, owner: &str) -> Result<u128, UniswapV3Error> {
        let token = address_bytes(token)?;
        let owner = address_bytes(owner)?;
        self.check_chain().await?;
        let mut data = selector("balanceOf(address)").to_vec();
        data.extend_from_slice(&address_word(owner));
        let result = self
            .eth_call(&hex_prefixed(&token), &hex_prefixed(&data))
            .await?;
        decode_first_u128(&result)
    }
    /// Read the sender's ERC-20 allowance to Uniswap's Base SwapRouter02.
    pub async fn allowance(
        &self,
        source_token: &str,
        sender: &str,
    ) -> Result<u128, UniswapV3Error> {
        self.allowance_to(source_token, sender, SWAP_ROUTER_02)
            .await
    }

    /// Read the sender's ERC-20 allowance to `spender` (a swap router).
    pub async fn allowance_to(
        &self,
        source_token: &str,
        sender: &str,
        spender: &str,
    ) -> Result<u128, UniswapV3Error> {
        let token = address_bytes(source_token)?;
        let owner = address_bytes(sender)?;
        self.check_chain().await?;
        let mut data = selector("allowance(address,address)").to_vec();
        data.extend_from_slice(&address_word(owner));
        data.extend_from_slice(&address_word(address_bytes(spender)?));
        let result = self
            .eth_call(&hex_prefixed(&token), &hex_prefixed(&data))
            .await?;
        decode_first_u128(&result)
    }

    /// An ERC-20's name, symbol and decimals, read from the contract. Strings may come back as
    /// ABI strings or as bytes32 (older tokens).
    pub async fn token_info(&self, token: &str) -> Result<(String, String, u32), UniswapV3Error> {
        let token = hex_prefixed(&address_bytes(token)?);
        self.check_chain().await?;
        let [name, symbol, decimals] = ["name()", "symbol()", "decimals()"]
            .map(|signature| hex_prefixed(&selector(signature)));
        let (name, symbol, decimals) = tokio::join!(
            self.eth_call(&token, &name),
            self.eth_call(&token, &symbol),
            self.eth_call(&token, &decimals),
        );
        let decimals = u32::try_from(decode_first_u128(&decimals?)?)
            .ok()
            .filter(|d| *d <= 36)
            .ok_or(UniswapV3Error::InvalidRpcResponse)?;
        let symbol = decode_text(&symbol?).ok_or(UniswapV3Error::InvalidRpcResponse)?;
        let name = name
            .ok()
            .and_then(|n| decode_text(&n))
            .unwrap_or_else(|| symbol.clone());
        Ok((name, symbol, decimals))
    }

    /// Build a direct ERC-20 transfer; the user wallet signs and sends it.
    pub fn transfer_transaction(
        &self,
        token: &str,
        sender: &str,
        recipient: &str,
        amount: u128,
    ) -> Result<EvmUnsignedTransaction, UniswapV3Error> {
        if amount == 0 {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let token = address_bytes(token)?;
        let from = address_bytes(sender)?;
        let to = address_bytes(recipient)?;
        if from == to {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let mut data = selector("transfer(address,uint256)").to_vec();
        data.extend_from_slice(&address_word(to));
        data.extend_from_slice(&uint_word(amount));
        Ok(EvmUnsignedTransaction {
            from: hex_prefixed(&from),
            to: hex_prefixed(&token),
            data: hex_prefixed(&data),
            value: "0x0".into(),
        })
    }
    /// Approval is a separate on-chain transaction if the existing allowance
    /// is below the swap amount. It must be completed before submitting swap.
    pub fn approval_transaction(
        &self,
        source_token: &str,
        sender: &str,
        amount: u128,
    ) -> Result<EvmUnsignedTransaction, UniswapV3Error> {
        self.approval_to(source_token, sender, SWAP_ROUTER_02, amount)
    }

    /// An exact-amount approval for `spender` (a swap router), never an unlimited one.
    pub fn approval_to(
        &self,
        source_token: &str,
        sender: &str,
        spender: &str,
        amount: u128,
    ) -> Result<EvmUnsignedTransaction, UniswapV3Error> {
        if amount == 0 {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let token = address_bytes(source_token)?;
        let owner = address_bytes(sender)?;
        let mut data = selector("approve(address,uint256)").to_vec();
        data.extend_from_slice(&address_word(address_bytes(spender)?));
        data.extend_from_slice(&uint_word(amount));
        Ok(EvmUnsignedTransaction {
            from: hex_prefixed(&owner),
            to: hex_prefixed(&token),
            data: hex_prefixed(&data),
            value: "0x0".into(),
        })
    }

    /// Build one exact-input swap from a freshly fetched quote. The minimum
    /// output is enforced by SwapRouter02, not merely displayed in the app.
    pub fn swap_transaction(
        &self,
        quote: &BaseV3Quote,
        sender: &str,
        slippage_bps: u16,
    ) -> Result<EvmUnsignedTransaction, UniswapV3Error> {
        if quote.amount_in == 0
            || quote.amount_out == 0
            || !FEE_TIERS.contains(&quote.fee)
            || !(1..=5000).contains(&slippage_bps)
        {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let source = address_bytes(&quote.source_token)?;
        let destination = address_bytes(&quote.destination_token)?;
        let recipient = address_bytes(sender)?;
        if source == destination {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let minimum = quote
            .amount_out
            .checked_mul(10_000 - u128::from(slippage_bps))
            .ok_or(UniswapV3Error::AmountOverflow)?
            / 10_000;
        if minimum == 0 {
            return Err(UniswapV3Error::InvalidRequest);
        }
        let mut data =
            selector("exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))")
                .to_vec();
        data.extend_from_slice(&address_word(source));
        data.extend_from_slice(&address_word(destination));
        data.extend_from_slice(&uint_word(u128::from(quote.fee)));
        data.extend_from_slice(&address_word(recipient));
        data.extend_from_slice(&uint_word(quote.amount_in));
        data.extend_from_slice(&uint_word(minimum));
        data.extend_from_slice(&uint_word(0));
        Ok(EvmUnsignedTransaction {
            from: hex_prefixed(&recipient),
            to: SWAP_ROUTER_02.into(),
            data: hex_prefixed(&data),
            value: "0x0".into(),
        })
    }
}

fn validate_request(request: &BaseSwapRequest) -> Result<(), UniswapV3Error> {
    if request.amount_base_units == 0
        || address_bytes(&request.source_token)? == address_bytes(&request.destination_token)?
    {
        return Err(UniswapV3Error::InvalidRequest);
    }
    Ok(())
}

fn looks_like_revert(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    lower.contains("revert") || lower.contains("pool does not exist")
}

fn quote_call(source: [u8; 20], destination: [u8; 20], amount: u128, fee: u32) -> String {
    let mut data =
        selector("quoteExactInputSingle((address,address,uint256,uint24,uint160))").to_vec();
    data.extend_from_slice(&address_word(source));
    data.extend_from_slice(&address_word(destination));
    data.extend_from_slice(&uint_word(amount));
    data.extend_from_slice(&uint_word(u128::from(fee)));
    data.extend_from_slice(&uint_word(0));
    hex_prefixed(&data)
}

fn decode_quote(result: &str) -> Result<(u128, u128), UniswapV3Error> {
    let bytes = from_hex(result)?;
    if bytes.len() != 128 {
        return Err(UniswapV3Error::InvalidRpcResponse);
    }
    Ok((decode_word(&bytes[0..32])?, decode_word(&bytes[96..128])?))
}

// An ABI string (offset, length, bytes) or a bytes32, as printable text without padding.
fn decode_text(result: &str) -> Option<String> {
    let bytes = from_hex(result).ok()?;
    let raw: Vec<u8> = if bytes.len() == 32 {
        bytes.into_iter().take_while(|b| *b != 0).collect()
    } else {
        let word = |at: usize| -> Option<usize> {
            let w = bytes.get(at..at + 32)?;
            w[..24]
                .iter()
                .all(|b| *b == 0)
                .then(|| usize::try_from(u64::from_be_bytes(w[24..].try_into().ok()?)).ok())?
        };
        let start = word(0)?;
        let length = word(start)?;
        bytes.get(start + 32..start + 32 + length)?.to_vec()
    };
    let text = String::from_utf8(raw).ok()?;
    let text = text.trim();
    (!text.is_empty() && text.len() <= 64 && !text.chars().any(char::is_control))
        .then(|| text.to_string())
}
fn decode_first_u128(result: &str) -> Result<u128, UniswapV3Error> {
    let bytes = from_hex(result)?;
    if bytes.len() != 32 {
        return Err(UniswapV3Error::InvalidRpcResponse);
    }
    decode_word(&bytes)
}

fn decode_word(word: &[u8]) -> Result<u128, UniswapV3Error> {
    if word.len() != 32 {
        return Err(UniswapV3Error::InvalidRpcResponse);
    }
    if word[..16].iter().any(|byte| *byte != 0) {
        return Err(UniswapV3Error::AmountOverflow);
    }
    Ok(u128::from_be_bytes(
        word[16..32]
            .try_into()
            .map_err(|_| UniswapV3Error::InvalidRpcResponse)?,
    ))
}

fn selector(signature: &str) -> [u8; 4] {
    let digest = Keccak256::digest(signature.as_bytes());
    digest[..4].try_into().expect("four selector bytes")
}

fn uint_word(value: u128) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn address_word(address: [u8; 20]) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(&address);
    word
}

fn address_bytes(value: &str) -> Result<[u8; 20], UniswapV3Error> {
    let bytes = from_hex(value)?;
    bytes.try_into().map_err(|_| UniswapV3Error::InvalidRequest)
}

fn from_hex(value: &str) -> Result<Vec<u8>, UniswapV3Error> {
    let input = value
        .strip_prefix("0x")
        .ok_or(UniswapV3Error::InvalidRequest)?;
    if input.len() % 2 != 0 || !input.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UniswapV3Error::InvalidRequest);
    }
    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            u8::from_str_radix(
                std::str::from_utf8(pair).map_err(|_| UniswapV3Error::InvalidRequest)?,
                16,
            )
            .map_err(|_| UniswapV3Error::InvalidRequest)
        })
        .collect()
}

fn hex_prefixed(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(2 + 2 * bytes.len());
    output.push_str("0x");
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_names_decode_from_abi_strings_and_bytes32() {
        // symbol() of BRETT on Base: an ABI string "BRETT".
        let abi = "0x0000000000000000000000000000000000000000000000000000000000000020\
                   0000000000000000000000000000000000000000000000000000000000000005\
                   4252455454000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_text(abi).as_deref(), Some("BRETT"));
        // An older token's bytes32 symbol ("MKR").
        let short = "0x4d4b520000000000000000000000000000000000000000000000000000000000";
        assert_eq!(decode_text(short).as_deref(), Some("MKR"));
        assert_eq!(decode_text("0x"), None);
        assert_eq!(
            decode_text("0x0000000000000000000000000000000000000000000000000000000000000000"),
            None
        );
    }

    #[test]
    fn abi_selectors_match_known_contract_methods() {
        assert_eq!(
            hex_prefixed(&selector("transfer(address,uint256)")),
            "0xa9059cbb"
        );
        assert_eq!(
            hex_prefixed(&selector("approve(address,uint256)")),
            "0x095ea7b3"
        );
        assert_eq!(
            hex_prefixed(&selector(
                "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))"
            )),
            "0x04e45aaf"
        );
        assert_eq!(
            hex_prefixed(&selector(
                "quoteExactInputSingle((address,address,uint256,uint24,uint160))"
            )),
            "0xc6a5026a"
        );
    }

    #[test]
    fn swap_enforces_minimum_output_and_sender() {
        let client = UniswapV3Client::new(Url::parse("https://mainnet.base.org").unwrap()).unwrap();
        let quote = BaseV3Quote {
            source_token: BASE_USDC.into(),
            destination_token: BASE_WETH.into(),
            amount_in: 1_000_000,
            amount_out: 400_000_000_000_000,
            fee: 500,
            quoter_gas_estimate: 120_000,
        };
        let sender = "0x1111111111111111111111111111111111111111";
        let tx = client.swap_transaction(&quote, sender, 100).unwrap();
        assert_eq!(tx.from, sender);
        assert_eq!(tx.to, SWAP_ROUTER_02);
        let bytes = from_hex(&tx.data).unwrap();
        assert_eq!(bytes.len(), 4 + 7 * 32);
        assert_eq!(
            decode_word(&bytes[4 + 5 * 32..4 + 6 * 32]).unwrap(),
            396_000_000_000_000
        );
        assert!(client.swap_transaction(&quote, sender, 0).is_err());
    }

    #[test]
    fn transfer_binds_recipient_and_exact_amount() {
        let client = UniswapV3Client::new(Url::parse("https://mainnet.base.org").unwrap()).unwrap();
        let sender = "0x1111111111111111111111111111111111111111";
        let recipient = "0x2222222222222222222222222222222222222222";
        let tx = client
            .transfer_transaction(BASE_USDC, sender, recipient, 1_250_000)
            .unwrap();
        assert_eq!(tx.from, sender);
        assert_eq!(tx.to.to_ascii_lowercase(), BASE_USDC.to_ascii_lowercase());
        assert_eq!(&tx.data[..10], "0xa9059cbb");
        let bytes = from_hex(&tx.data).unwrap();
        assert_eq!(&bytes[4 + 12..4 + 32], &address_bytes(recipient).unwrap());
        assert_eq!(decode_word(&bytes[4 + 32..4 + 64]).unwrap(), 1_250_000);
        assert!(client
            .transfer_transaction(BASE_USDC, sender, sender, 1)
            .is_err());
    }
    #[tokio::test]
    async fn invalid_inputs_fail_before_rpc() {
        let client = UniswapV3Client::new(Url::parse("https://mainnet.base.org").unwrap()).unwrap();
        assert!(client.approval_transaction(BASE_USDC, "bad", 1).is_err());
        assert!(client
            .approval_transaction(BASE_USDC, BASE_WETH, 0)
            .is_err());
        assert!(client
            .quote_direct(&BaseSwapRequest {
                source_token: BASE_USDC.into(),
                destination_token: BASE_USDC.into(),
                amount_base_units: 1,
            })
            .await
            .is_err());
    }
}
