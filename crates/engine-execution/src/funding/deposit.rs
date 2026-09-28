//! Read-only ERC-20 deposit detection over an EVM JSON-RPC endpoint.
//!
//! The service must create and persist each DepositTarget from a wallet it
//! associates with the signed-in user. A caller-supplied target is not proof
//! of wallet ownership. This scanner reads finalized Transfer logs; it never
//! signs or transfers funds.

use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const MAX_BLOCK_SPAN: u64 = 100;

#[derive(Clone, Debug)]
pub struct DepositTarget {
    pub user_id: String,
    pub wallet_address: String,
    pub token_contract: String,
    pub chain_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDeposit {
    pub user_id: String,
    pub chain_id: u64,
    pub token_contract: String,
    pub wallet_address: String,
    pub transaction_hash: String,
    pub log_index: u64,
    pub block_number: u64,
    pub amount_base_units: u128,
}

#[derive(Debug, Error)]
pub enum DepositError {
    #[error("RPC URL must use HTTPS")]
    InsecureRpc,
    #[error("invalid EVM address: {0}")]
    InvalidAddress(&'static str),
    #[error("block scan range is invalid or exceeds 100 blocks")]
    InvalidRange,
    #[error("RPC is connected to chain {actual}, expected {expected}")]
    WrongChain { expected: u64, actual: u64 },
    #[error("RPC request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("RPC returned an error")]
    Rpc,
    #[error("RPC response is missing required data")]
    InvalidResponse,
}

#[derive(Clone)]
pub struct EvmDepositScanner {
    http: Client,
    rpc_url: Url,
    confirmations: u64,
}

#[derive(Deserialize)]
struct RpcEnvelope {
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferLog {
    address: String,
    topics: Vec<String>,
    data: String,
    transaction_hash: String,
    log_index: String,
    block_number: String,
    #[serde(default)]
    removed: bool,
}

impl EvmDepositScanner {
    pub fn new(rpc_url: Url, confirmations: u64) -> Result<Self, DepositError> {
        if rpc_url.scheme() != "https" {
            return Err(DepositError::InsecureRpc);
        }
        Ok(Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            rpc_url,
            confirmations: confirmations.max(1),
        })
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, DepositError> {
        let response = self
            .http
            .post(self.rpc_url.clone())
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await?
            .error_for_status()?;
        let envelope: RpcEnvelope = response.json().await?;
        if envelope.error.is_some() {
            return Err(DepositError::Rpc);
        }
        envelope.result.ok_or(DepositError::InvalidResponse)
    }

    /// Read current ERC-20 units in a stored user's wallet. This is separate
    /// from Circle Gateway's deposited balance until a real deposit confirms.
    pub async fn wallet_token_balance(&self, target: &DepositTarget) -> Result<u128, DepositError> {
        let wallet = normalize_address(&target.wallet_address)
            .ok_or(DepositError::InvalidAddress("wallet_address"))?;
        let token = normalize_address(&target.token_contract)
            .ok_or(DepositError::InvalidAddress("token_contract"))?;
        let actual_chain = parse_hex_u64(
            self.rpc("eth_chainId", json!([]))
                .await?
                .as_str()
                .ok_or(DepositError::InvalidResponse)?,
        )?;
        if actual_chain != target.chain_id {
            return Err(DepositError::WrongChain {
                expected: target.chain_id,
                actual: actual_chain,
            });
        }
        let data = format!("0x70a08231{:0>64}", &wallet[2..]);
        let result = self
            .rpc("eth_call", json!([{"to": token, "data": data}, "latest"]))
            .await?;
        u128::from_str_radix(
            result
                .as_str()
                .and_then(|s| s.strip_prefix("0x"))
                .ok_or(DepositError::InvalidResponse)?,
            16,
        )
        .map_err(|_| DepositError::InvalidResponse)
    }
    /// Scan the most recent bounded window. This is useful for a live deposit
    /// gate; a hosted service should persist a cursor and use scan instead.
    pub async fn scan_recent(
        &self,
        target: &DepositTarget,
    ) -> Result<Vec<VerifiedDeposit>, DepositError> {
        let latest = parse_hex_u64(
            self.rpc("eth_blockNumber", json!([]))
                .await?
                .as_str()
                .ok_or(DepositError::InvalidResponse)?,
        )?;
        self.scan(target, latest.saturating_sub(MAX_BLOCK_SPAN - 1), latest)
            .await
    }
    /// Scan a bounded block range for confirmed deposits to a stored user wallet.
    /// The caller persists (chain ID, transaction hash, log index) as an
    /// idempotency key so rescans cannot credit the same transfer twice.
    pub async fn scan(
        &self,
        target: &DepositTarget,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<VerifiedDeposit>, DepositError> {
        let wallet = normalize_address(&target.wallet_address)
            .ok_or(DepositError::InvalidAddress("wallet_address"))?;
        let token = normalize_address(&target.token_contract)
            .ok_or(DepositError::InvalidAddress("token_contract"))?;
        if to_block < from_block || to_block - from_block >= MAX_BLOCK_SPAN {
            return Err(DepositError::InvalidRange);
        }

        let actual_chain = parse_hex_u64(
            self.rpc("eth_chainId", json!([]))
                .await?
                .as_str()
                .ok_or(DepositError::InvalidResponse)?,
        )?;
        if actual_chain != target.chain_id {
            return Err(DepositError::WrongChain {
                expected: target.chain_id,
                actual: actual_chain,
            });
        }
        let latest = parse_hex_u64(
            self.rpc("eth_blockNumber", json!([]))
                .await?
                .as_str()
                .ok_or(DepositError::InvalidResponse)?,
        )?;
        let confirmed_head = latest.saturating_sub(self.confirmations - 1);
        if from_block > confirmed_head {
            return Ok(Vec::new());
        }
        let to_block = to_block.min(confirmed_head);
        let to_topic = format!("0x{:0>64}", &wallet[2..]);
        let logs = self
            .rpc(
                "eth_getLogs",
                json!([{
                    "address": token,
                    "fromBlock": format!("0x{from_block:x}"),
                    "toBlock": format!("0x{to_block:x}"),
                    "topics": [TRANSFER_TOPIC, null, to_topic]
                }]),
            )
            .await?;
        let logs: Vec<TransferLog> =
            serde_json::from_value(logs).map_err(|_| DepositError::InvalidResponse)?;
        logs.into_iter()
            .filter(|log| !log.removed)
            .map(|log| parse_transfer(log, target, &wallet, &token))
            .collect()
    }
}

fn normalize_address(value: &str) -> Option<String> {
    let hex = value.strip_prefix("0x")?;
    if hex.len() != 40 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", hex.to_ascii_lowercase()))
}

fn parse_hex_u64(value: &str) -> Result<u64, DepositError> {
    u64::from_str_radix(
        value
            .strip_prefix("0x")
            .ok_or(DepositError::InvalidResponse)?,
        16,
    )
    .map_err(|_| DepositError::InvalidResponse)
}

fn parse_transfer(
    log: TransferLog,
    target: &DepositTarget,
    wallet: &str,
    token: &str,
) -> Result<VerifiedDeposit, DepositError> {
    if normalize_address(&log.address).as_deref() != Some(token)
        || log
            .topics
            .first()
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
            != Some(TRANSFER_TOPIC)
        || log.topics.len() != 3
        || log.topics[2].len() != 66
        || !log.topics[2].ends_with(&wallet[2..])
    {
        return Err(DepositError::InvalidResponse);
    }
    let amount = u128::from_str_radix(
        log.data
            .strip_prefix("0x")
            .ok_or(DepositError::InvalidResponse)?,
        16,
    )
    .map_err(|_| DepositError::InvalidResponse)?;
    Ok(VerifiedDeposit {
        user_id: target.user_id.clone(),
        chain_id: target.chain_id,
        token_contract: token.to_owned(),
        wallet_address: wallet.to_owned(),
        transaction_hash: log.transaction_hash,
        log_index: parse_hex_u64(&log.log_index)?,
        block_number: parse_hex_u64(&log.block_number)?,
        amount_base_units: amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_log_is_attributed_only_to_the_registered_wallet_and_token() {
        let target = DepositTarget {
            user_id: "user-1".into(),
            wallet_address: "0x1234567890123456789012345678901234567890".into(),
            token_contract: "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".into(),
            chain_id: 84532,
        };
        let log = TransferLog {
            address: target.token_contract.clone(),
            topics: vec![
                TRANSFER_TOPIC.into(),
                format!("0x{:0>64}", "1111111111111111111111111111111111111111"),
                format!("0x{:0>64}", &target.wallet_address[2..]),
            ],
            data: "0x0f4240".into(),
            transaction_hash: "0xdeadbeef".into(),
            log_index: "0x0".into(),
            block_number: "0x10".into(),
            removed: false,
        };
        let deposit = parse_transfer(
            log,
            &target,
            &normalize_address(&target.wallet_address).unwrap(),
            &normalize_address(&target.token_contract).unwrap(),
        )
        .unwrap();
        assert_eq!(deposit.user_id, "user-1");
        assert_eq!(deposit.amount_base_units, 1_000_000);
        assert_eq!(deposit.chain_id, 84532);
    }
}
