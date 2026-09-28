//! Solana USDC destination preflight for Gateway spends.
//! The relayer pays account rent; the user's wallet never needs SOL.

use std::{path::PathBuf, str::FromStr, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::{
    hash::Hash, program_pack::Pack, pubkey::Pubkey, signature::read_keypair_file, signer::Signer,
    transaction::Transaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use thiserror::Error;

pub const DEVNET_USDC_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
pub const MAINNET_USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const LOW_RELAYER_LAMPORTS: u64 = 50_000_000;
const FEE_RESERVE_LAMPORTS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SolanaNetwork {
    Devnet,
    Mainnet,
}

impl SolanaNetwork {
    fn mint(self) -> &'static str {
        match self {
            Self::Devnet => DEVNET_USDC_MINT,
            Self::Mainnet => MAINNET_USDC_MINT,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SolanaAtaPreflight {
    http: Client,
    rpc_url: String,
    network: SolanaNetwork,
    relayer_keypair_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayerBalance {
    pub address: String,
    pub lamports: u64,
    pub low: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SolanaDestination {
    pub ubk_recipient_address: String,
    pub gateway_destination_recipient: String,
    pub ata_creation_signature: Option<String>,
    pub relayer: RelayerBalance,
}

#[derive(Debug, Error)]
pub enum SolanaPreflightError {
    #[error("invalid Solana wallet address or configured USDC mint")]
    InvalidAddress,
    #[error("Solana RPC request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Solana RPC returned an invalid response")]
    InvalidResponse,
    #[error("Solana RPC rejected the request: {0}")]
    Rpc(String),
    #[error("relayer keypair could not be loaded")]
    RelayerKey,
    #[error("relayer has insufficient SOL for ATA rent and transaction fee")]
    RelayerUnderfunded,
    #[error("destination ATA exists but is not the expected USDC token account")]
    WrongTokenAccount,
    #[error("Solana ATA creation transaction failed: {0}")]
    TransactionFailed(String),
    #[error("Solana ATA creation was not confirmed before timeout")]
    ConfirmationTimeout,
}

impl SolanaAtaPreflight {
    pub fn new(
        network: SolanaNetwork,
        rpc_url: impl Into<String>,
        relayer_keypair_path: impl Into<PathBuf>,
    ) -> Result<Self, SolanaPreflightError> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(15)).build()?,
            rpc_url: rpc_url.into(),
            network,
            relayer_keypair_path: relayer_keypair_path.into(),
        })
    }

    pub async fn relayer_balance(&self) -> Result<RelayerBalance, SolanaPreflightError> {
        let relayer = read_keypair_file(&self.relayer_keypair_path)
            .map_err(|_| SolanaPreflightError::RelayerKey)?;
        let address = relayer.pubkey().to_string();
        let response = self.rpc("getBalance", json!([address])).await?;
        let lamports = response["value"]
            .as_u64()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        Ok(RelayerBalance {
            address,
            lamports,
            low: lamports < LOW_RELAYER_LAMPORTS,
        })
    }

    /// Run before *every* UBK spend whose destination is Solana. Pass the
    /// on-curve owner as UBK's recipientAddress. UBK derives this ATA for
    /// Gateway's destinationRecipient; verify that mapping at the spend boundary.
    pub async fn ensure_usdc_ata(
        &self,
        owner_address: &str,
    ) -> Result<SolanaDestination, SolanaPreflightError> {
        let owner =
            Pubkey::from_str(owner_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let mint = Pubkey::from_str(self.network.mint())
            .map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let token_program = spl_token::id();
        let ata = get_associated_token_address_with_program_id(&owner, &mint, &token_program);
        let relayer = read_keypair_file(&self.relayer_keypair_path)
            .map_err(|_| SolanaPreflightError::RelayerKey)?;
        let before = self.relayer_balance().await?;

        if let Some(account) = self.account(&ata).await? {
            verify_token_account(&account, &owner, &mint, &token_program)?;
            return Ok(SolanaDestination {
                ubk_recipient_address: owner.to_string(),
                gateway_destination_recipient: ata.to_string(),
                ata_creation_signature: None,
                relayer: before,
            });
        }

        let rent = self
            .rpc(
                "getMinimumBalanceForRentExemption",
                json!([spl_token::state::Account::LEN]),
            )
            .await?
            .as_u64()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        if before.lamports < rent.saturating_add(FEE_RESERVE_LAMPORTS) {
            return Err(SolanaPreflightError::RelayerUnderfunded);
        }

        let blockhash = self
            .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let blockhash = blockhash["value"]["blockhash"]
            .as_str()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        let hash = Hash::from_str(blockhash).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let instruction = create_associated_token_account_idempotent(
            &relayer.pubkey(),
            &owner,
            &mint,
            &token_program,
        );
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&relayer.pubkey()),
            &[&relayer],
            hash,
        );
        let bytes =
            bincode::serialize(&transaction).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let signature = self
            .rpc(
                "sendTransaction",
                json!([
                    STANDARD.encode(bytes), {"encoding":"base64","preflightCommitment":"confirmed"}
                ]),
            )
            .await?
            .as_str()
            .ok_or(SolanaPreflightError::InvalidResponse)?
            .to_owned();

        for _ in 0..30 {
            let statuses = self
                .rpc("getSignatureStatuses", json!([[signature]]))
                .await?;
            let status = &statuses["value"][0];
            if !status.is_null() {
                if !status["err"].is_null() {
                    return Err(SolanaPreflightError::TransactionFailed(
                        status["err"].to_string(),
                    ));
                }
                if matches!(
                    status["confirmationStatus"].as_str(),
                    Some("confirmed" | "finalized")
                ) {
                    let account = self
                        .account(&ata)
                        .await?
                        .ok_or(SolanaPreflightError::WrongTokenAccount)?;
                    verify_token_account(&account, &owner, &mint, &token_program)?;
                    return Ok(SolanaDestination {
                        ubk_recipient_address: owner.to_string(),
                        gateway_destination_recipient: ata.to_string(),
                        ata_creation_signature: Some(signature),
                        relayer: self.relayer_balance().await?,
                    });
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(SolanaPreflightError::ConfirmationTimeout)
    }

    async fn account(&self, address: &Pubkey) -> Result<Option<Value>, SolanaPreflightError> {
        let result = self
            .rpc(
                "getAccountInfo",
                json!([
                    address.to_string(), {"encoding":"base64","commitment":"confirmed"}
                ]),
            )
            .await?;
        Ok(result.get("value").filter(|v| !v.is_null()).cloned())
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, SolanaPreflightError> {
        let response: Value = self
            .http
            .post(&self.rpc_url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if !response["error"].is_null() {
            return Err(SolanaPreflightError::Rpc(response["error"].to_string()));
        }
        response
            .get("result")
            .cloned()
            .ok_or(SolanaPreflightError::InvalidResponse)
    }
}

fn verify_token_account(
    account: &Value,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<(), SolanaPreflightError> {
    if account["owner"].as_str() != Some(&token_program.to_string()) {
        return Err(SolanaPreflightError::WrongTokenAccount);
    }
    let encoded = account["data"][0]
        .as_str()
        .ok_or(SolanaPreflightError::InvalidResponse)?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| SolanaPreflightError::InvalidResponse)?;
    let token = spl_token::state::Account::unpack(&bytes)
        .map_err(|_| SolanaPreflightError::WrongTokenAccount)?;
    if token.owner != *owner || token.mint != *mint {
        return Err(SolanaPreflightError::WrongTokenAccount);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derives_the_native_usdc_ata_for_a_first_time_recipient() {
        let owner = Pubkey::from_str("CVgQZTfRhyweQZdYT2kzmdKgT6mKpHNryiFobSWU7ri5").unwrap();
        let mint = Pubkey::from_str(DEVNET_USDC_MINT).unwrap();
        let ata = get_associated_token_address_with_program_id(&owner, &mint, &spl_token::id());
        assert_eq!(
            ata.to_string(),
            "CKXcU4oQbVoQgNXsUQQnpiBSKrTd32GMABPz8f4oi6z4"
        );
        let ix = create_associated_token_account_idempotent(
            &Pubkey::new_unique(),
            &owner,
            &mint,
            &spl_token::id(),
        );
        assert_eq!(ix.data, vec![1]);
    }
}
