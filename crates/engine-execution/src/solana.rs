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
    #[error("This swap could not be prepared: {0}")]
    SwapSimulation(String),
    #[error("Not enough gas to cover this swap and its account rent")]
    InsufficientGas,
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

    #[cfg(test)]
    pub(crate) fn with_test_http(mut self, http: Client) -> Self {
        self.http = http;
        self
    }

    pub async fn owner_usdc_balance(
        &self,
        owner_address: &str,
    ) -> Result<u128, SolanaPreflightError> {
        let owner =
            Pubkey::from_str(owner_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let mint = Pubkey::from_str(self.network.mint())
            .map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let ata = get_associated_token_address_with_program_id(&owner, &mint, &spl_token::id());
        let Some(account) = self.account(&ata).await? else {
            return Ok(0);
        };
        verify_token_account(&account, &owner, &mint, &spl_token::id())?;
        let response = self
            .rpc(
                "getTokenAccountBalance",
                json!([
                    ata.to_string(), {"commitment":"confirmed"}
                ]),
            )
            .await?;
        if response["value"]["decimals"].as_u64() != Some(6) {
            return Err(SolanaPreflightError::InvalidResponse);
        }
        response["value"]["amount"]
            .as_str()
            .ok_or(SolanaPreflightError::InvalidResponse)?
            .parse()
            .map_err(|_| SolanaPreflightError::InvalidResponse)
    }

    /// Refuse an RPC configured for a different cluster before reading balances.
    pub async fn assert_network(&self) -> Result<(), SolanaPreflightError> {
        let expected = match self.network {
            SolanaNetwork::Mainnet => "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d",
            SolanaNetwork::Devnet => "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
        };
        let actual = self.rpc("getGenesisHash", json!([])).await?;
        if actual.as_str() == Some(expected) {
            Ok(())
        } else {
            Err(SolanaPreflightError::Rpc(
                "Solana RPC is connected to the wrong network".into(),
            ))
        }
    }
    /// Sum all standard token accounts for a mint owned by this wallet.
    pub async fn owner_mint_balance(
        &self,
        owner_address: &str,
        mint_address: &str,
        decimals: u64,
    ) -> Result<u128, SolanaPreflightError> {
        Pubkey::from_str(owner_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        Pubkey::from_str(mint_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let response = self
            .rpc(
                "getTokenAccountsByOwner",
                json!([owner_address, {"mint": mint_address}, {"encoding": "jsonParsed", "commitment": "confirmed"}]),
            )
            .await?;
        let accounts = response["value"]
            .as_array()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        let mut total = 0u128;
        for account in accounts {
            let info = &account["account"]["data"]["parsed"]["info"];
            if info["owner"].as_str() != Some(owner_address)
                || info["mint"].as_str() != Some(mint_address)
                || info["tokenAmount"]["decimals"].as_u64() != Some(decimals)
            {
                return Err(SolanaPreflightError::InvalidResponse);
            }
            let units: u128 = info["tokenAmount"]["amount"]
                .as_str()
                .ok_or(SolanaPreflightError::InvalidResponse)?
                .parse()
                .map_err(|_| SolanaPreflightError::InvalidResponse)?;
            total = total
                .checked_add(units)
                .ok_or(SolanaPreflightError::InvalidResponse)?;
        }
        Ok(total)
    }

    /// Every token this wallet holds, across the SPL Token and Token-2022 programs (tokenized stocks
    /// use the latter), summed per mint: (mint, raw units, decimals).
    pub async fn owner_token_balances(
        &self,
        owner_address: &str,
    ) -> Result<Vec<(String, u128, u32)>, SolanaPreflightError> {
        Pubkey::from_str(owner_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let mut totals: std::collections::BTreeMap<String, (u128, u32)> = Default::default();
        for program in [
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        ] {
            let response = self
                .rpc(
                    "getTokenAccountsByOwner",
                    json!([owner_address, {"programId": program}, {"encoding": "jsonParsed", "commitment": "confirmed"}]),
                )
                .await?;
            let accounts = response["value"]
                .as_array()
                .ok_or(SolanaPreflightError::InvalidResponse)?;
            for account in accounts {
                let info = &account["account"]["data"]["parsed"]["info"];
                if info["owner"].as_str() != Some(owner_address) {
                    return Err(SolanaPreflightError::InvalidResponse);
                }
                let (Some(mint), Some(amount), Some(decimals)) = (
                    info["mint"].as_str(),
                    info["tokenAmount"]["amount"].as_str(),
                    info["tokenAmount"]["decimals"].as_u64(),
                ) else {
                    return Err(SolanaPreflightError::InvalidResponse);
                };
                let units: u128 = amount
                    .parse()
                    .map_err(|_| SolanaPreflightError::InvalidResponse)?;
                let entry = totals
                    .entry(mint.to_owned())
                    .or_insert((0, decimals as u32));
                entry.0 = entry
                    .0
                    .checked_add(units)
                    .ok_or(SolanaPreflightError::InvalidResponse)?;
            }
        }
        Ok(totals
            .into_iter()
            .filter(|(_, (units, _))| *units > 0)
            .map(|(mint, (units, decimals))| (mint, units, decimals))
            .collect())
    }

    /// Read native SOL, including Jupiter outputs that unwrap wrapped SOL.
    pub async fn owner_sol_balance(
        &self,
        owner_address: &str,
    ) -> Result<u128, SolanaPreflightError> {
        Pubkey::from_str(owner_address).map_err(|_| SolanaPreflightError::InvalidAddress)?;
        let response = self
            .rpc(
                "getBalance",
                json!([owner_address, {"commitment": "confirmed"}]),
            )
            .await?;
        Ok(response["value"]
            .as_u64()
            .ok_or(SolanaPreflightError::InvalidResponse)? as u128)
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

    /// An unsigned USDC transfer from one wallet to another (base64, legacy message, the sender pays
    /// the fee), creating the recipient's USDC account if it doesn't exist yet. Also says whether
    /// it creates that account (the sender then pays its rent, about 0.002 SOL).
    pub async fn usdc_transfer_transaction(
        &self,
        from_owner: &str,
        to_owner: &str,
        amount: u64,
    ) -> Result<(String, bool), SolanaPreflightError> {
        let from =
            Pubkey::from_str(from_owner).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let to = Pubkey::from_str(to_owner).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let mint = Pubkey::from_str(match self.network {
            SolanaNetwork::Mainnet => MAINNET_USDC_MINT,
            _ => DEVNET_USDC_MINT,
        })
        .map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let source = get_associated_token_address_with_program_id(&from, &mint, &spl_token::id());
        let destination =
            get_associated_token_address_with_program_id(&to, &mint, &spl_token::id());
        let creates = self.account(&destination).await?.is_none();
        let blockhash = self
            .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let blockhash = blockhash["value"]["blockhash"]
            .as_str()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        let hash = Hash::from_str(blockhash).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let instructions = vec![
            create_associated_token_account_idempotent(&from, &to, &mint, &spl_token::id()),
            spl_token::instruction::transfer_checked(
                &spl_token::id(),
                &source,
                &mint,
                &destination,
                &from,
                &[],
                amount,
                6,
            )
            .map_err(|_| SolanaPreflightError::InvalidResponse)?,
        ];
        let mut transaction = Transaction::new_with_payer(&instructions, Some(&from));
        transaction.message.recent_blockhash = hash;
        let bytes =
            bincode::serialize(&transaction).map_err(|_| SolanaPreflightError::InvalidResponse)?;
        Ok((STANDARD.encode(bytes), creates))
    }

    /// Fee-only probe before cash lands: the same two signatures and ATA rent, with no transfers.
    /// Extra read-only signer does not spend; this unsigned probe never reaches a signing endpoint.
    pub(crate) async fn usdc_transfer_fee_probe(
        &self,
        owner: &str,
        recipient: &str,
        payer: &str,
        payment: &str,
    ) -> Result<String, SolanaPreflightError> {
        self.assert_network().await?;
        let key = |s: &str| Pubkey::from_str(s).map_err(|_| SolanaPreflightError::InvalidResponse);
        let (owner, recipient, payer, payment) =
            (key(owner)?, key(recipient)?, key(payer)?, key(payment)?);
        let instructions = fee_probe_instructions(&owner, &recipient, &payer, &payment)?;
        let block = self
            .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let hash = Hash::from_str(
            block["value"]["blockhash"]
                .as_str()
                .ok_or(SolanaPreflightError::InvalidResponse)?,
        )
        .map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let mut message = solana_sdk::message::Message::new(&instructions, Some(&payer));
        message.recent_blockhash = hash;
        if message.header.num_required_signatures != 2 {
            return Err(SolanaPreflightError::InvalidResponse);
        }
        let tx = solana_sdk::transaction::VersionedTransaction {
            message: solana_sdk::message::VersionedMessage::Legacy(message),
            signatures: vec![solana_sdk::signature::Signature::default(); 2],
        };
        Ok(STANDARD
            .encode(bincode::serialize(&tx).map_err(|_| SolanaPreflightError::InvalidResponse)?))
    }

    /// Cash plus the provider fee are authorized together. The provider pays account rent and SOL;
    /// it never controls the user's wallet or gets an approval for a separate payment.
    pub async fn usdc_transfer_with_fee(
        &self,
        owner: &str,
        recipient: &str,
        amount: u64,
        payer: &str,
        payment: &str,
        fee: u64,
    ) -> Result<(String, u64), SolanaPreflightError> {
        use solana_sdk::{
            message::VersionedMessage, signature::Signature, transaction::VersionedTransaction,
        };
        if self.network != SolanaNetwork::Mainnet || amount == 0 || fee == 0 || owner == payer {
            return Err(SolanaPreflightError::InvalidResponse);
        }
        self.assert_network().await?;
        let key = |s: &str| Pubkey::from_str(s).map_err(|_| SolanaPreflightError::InvalidResponse);
        let (owner, recipient, payer, payment) =
            (key(owner)?, key(recipient)?, key(payer)?, key(payment)?);
        let instructions =
            fee_transfer_instructions(&owner, &recipient, &payer, &payment, amount, fee)?;
        let block = self
            .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let hash = Hash::from_str(
            block["value"]["blockhash"]
                .as_str()
                .ok_or(SolanaPreflightError::InvalidResponse)?,
        )
        .map_err(|_| SolanaPreflightError::InvalidResponse)?;
        let height = block["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or(SolanaPreflightError::InvalidResponse)?;
        let mut message = solana_sdk::message::Message::new(&instructions, Some(&payer));
        message.recent_blockhash = hash;
        if message.header.num_required_signatures != 2
            || message.account_keys[..2] != [payer, owner]
        {
            return Err(SolanaPreflightError::InvalidResponse);
        }
        let tx = VersionedTransaction {
            signatures: vec![Signature::default(); 2],
            message: VersionedMessage::Legacy(message),
        };
        Ok((
            STANDARD.encode(
                bincode::serialize(&tx).map_err(|_| SolanaPreflightError::InvalidResponse)?,
            ),
            height,
        ))
    }

    /// An unsigned v0 transaction paid by `payer`, base64: what a venue hands over as instructions
    /// (`{programId, keys: [{pubkey, isSigner, isWritable}], data: hex}`) and lookup tables to use,
    /// for the user to sign (Relay's Solana deposits).
    pub async fn v0_transaction(
        &self,
        payer: &str,
        instructions: &[Value],
        lookup_tables: &[String],
    ) -> Result<String, SolanaPreflightError> {
        use solana_sdk::{
            instruction::{AccountMeta, Instruction},
            message::{v0, AddressLookupTableAccount, VersionedMessage},
            signature::Signature,
            transaction::VersionedTransaction,
        };
        let invalid = || SolanaPreflightError::InvalidResponse;
        let key = |v: &Value| {
            v.as_str()
                .and_then(|s| Pubkey::from_str(s).ok())
                .ok_or_else(invalid)
        };
        let payer = Pubkey::from_str(payer).map_err(|_| invalid())?;
        let mut built = Vec::with_capacity(instructions.len());
        for ix in instructions {
            let accounts = ix["keys"]
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(|k| {
                    Ok(AccountMeta {
                        pubkey: key(&k["pubkey"])?,
                        is_signer: k["isSigner"].as_bool().ok_or_else(invalid)?,
                        is_writable: k["isWritable"].as_bool().ok_or_else(invalid)?,
                    })
                })
                .collect::<Result<Vec<_>, SolanaPreflightError>>()?;
            let data = ix["data"].as_str().ok_or_else(invalid)?;
            let data = (0..data.len())
                .step_by(2)
                .map(|i| {
                    data.get(i..i + 2)
                        .and_then(|b| u8::from_str_radix(b, 16).ok())
                })
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(invalid)?;
            built.push(Instruction {
                program_id: key(&ix["programId"])?,
                accounts,
                data,
            });
        }
        let mut tables = Vec::with_capacity(lookup_tables.len());
        for table in lookup_tables {
            let address = Pubkey::from_str(table).map_err(|_| invalid())?;
            let info = self
                .rpc("getAccountInfo", json!([table, {"encoding":"base64"}]))
                .await?;
            let raw = info["value"]["data"][0].as_str().ok_or_else(invalid)?;
            let bytes = STANDARD.decode(raw).map_err(|_| invalid())?;
            // A lookup table's addresses follow its 56-byte header, 32 bytes each.
            let addresses = bytes
                .get(56..)
                .ok_or_else(invalid)?
                .chunks_exact(32)
                .map(|c| Pubkey::try_from(c).map_err(|_| invalid()))
                .collect::<Result<Vec<_>, _>>()?;
            tables.push(AddressLookupTableAccount {
                key: address,
                addresses,
            });
        }
        let blockhash = self
            .rpc("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let hash = Hash::from_str(
            blockhash["value"]["blockhash"]
                .as_str()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        let message =
            v0::Message::try_compile(&payer, &built, &tables, hash).map_err(|_| invalid())?;
        let signers = usize::from(message.header.num_required_signatures);
        let transaction = VersionedTransaction {
            signatures: vec![Signature::default(); signers],
            message: VersionedMessage::V0(message),
        };
        let bytes = bincode::serialize(&transaction).map_err(|_| invalid())?;
        Ok(STANDARD.encode(bytes))
    }

    /// Simulate the exact user-paid transaction, including fees, wrapping SOL and account rent.
    /// No signature or submission happens here. Token/liquidity errors stay distinct from gas.
    pub async fn preflight_swap(&self, transaction: &str) -> Result<u32, SolanaPreflightError> {
        let simulated = self
            .rpc(
                "simulateTransaction",
                json!([transaction, {
                    "encoding":"base64", "sigVerify":false, "replaceRecentBlockhash":true,
                    "commitment":"confirmed"
                }]),
            )
            .await?;
        let value = &simulated["value"];
        if !value["err"].is_null() {
            if simulation_short_of_gas(value) {
                return Err(SolanaPreflightError::InsufficientGas);
            }
            return Err(SolanaPreflightError::SwapSimulation(
                value["err"].to_string(),
            ));
        }
        value["unitsConsumed"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(SolanaPreflightError::InvalidResponse)
    }

    pub async fn block_height(&self) -> Result<u64, SolanaPreflightError> {
        self.rpc("getBlockHeight", json!([{"commitment":"finalized"}]))
            .await?
            .as_u64()
            .ok_or(SolanaPreflightError::InvalidResponse)
    }

    /// Actual wallet changes from the confirmed swap; rent and fees are excluded from SOL output.
    pub async fn swap_amounts(
        &self,
        signature: &str,
        wallet: &str,
        input: &str,
        output: &str,
    ) -> Result<Option<(u128, u128)>, SolanaPreflightError> {
        let tx = self
            .rpc(
                "getTransaction",
                json!([signature, {"encoding":"jsonParsed",
            "commitment":"confirmed", "maxSupportedTransactionVersion":0}]),
            )
            .await?;
        if tx.is_null() {
            return Ok(None);
        }
        swap_amounts(&tx, wallet, input, output).map(Some)
    }

    /// None while a signature hasn't landed; Some(Ok) once confirmed, Some(Err) if it failed.
    pub async fn signature_status(
        &self,
        signature: &str,
    ) -> Result<Option<Result<(), String>>, SolanaPreflightError> {
        let statuses = self
            .rpc(
                "getSignatureStatuses",
                json!([[signature], {"searchTransactionHistory": true}]),
            )
            .await?;
        let status = &statuses["value"][0];
        if status.is_null() {
            return Ok(None);
        }
        if !status["err"].is_null() {
            return Ok(Some(Err(status["err"].to_string())));
        }
        Ok(matches!(
            status["confirmationStatus"].as_str(),
            Some("confirmed" | "finalized")
        )
        .then_some(Ok(())))
    }

    /// Sends a transaction the user already signed (base64) and returns its signature.
    pub async fn send_signed(
        &self,
        transaction_base64: &str,
    ) -> Result<String, SolanaPreflightError> {
        self.rpc(
            "sendTransaction",
            json!([transaction_base64, {"encoding":"base64","preflightCommitment":"confirmed"}]),
        )
        .await?
        .as_str()
        .map(str::to_owned)
        .ok_or(SolanaPreflightError::InvalidResponse)
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, SolanaPreflightError> {
        // A busy RPC (rate limited, a dropped connection) gets one more try before the call fails; a
        // timeout doesn't, so a dead RPC isn't waited on twice. Resending is safe: a signed
        // transaction lands once however often it's sent.
        match self.rpc_once(method, &params).await {
            Err(SolanaPreflightError::Transport(error)) if !error.is_timeout() => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                self.rpc_once(method, &params).await
            }
            other => other,
        }
    }

    async fn rpc_once(&self, method: &str, params: &Value) -> Result<Value, SolanaPreflightError> {
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

fn fee_probe_instructions(
    owner: &Pubkey,
    recipient: &Pubkey,
    payer: &Pubkey,
    payment: &Pubkey,
) -> Result<Vec<solana_sdk::instruction::Instruction>, SolanaPreflightError> {
    if owner == payer || owner == recipient || owner == payment || recipient == payment {
        return Err(SolanaPreflightError::InvalidResponse);
    }
    let mint =
        Pubkey::from_str(MAINNET_USDC_MINT).map_err(|_| SolanaPreflightError::InvalidResponse)?;
    let mut instructions = vec![
        create_associated_token_account_idempotent(payer, recipient, &mint, &spl_token::id()),
        create_associated_token_account_idempotent(payer, payment, &mint, &spl_token::id()),
    ];
    instructions[0]
        .accounts
        .push(solana_sdk::instruction::AccountMeta::new_readonly(
            *owner, true,
        ));
    Ok(instructions)
}

fn fee_transfer_instructions(
    owner: &Pubkey,
    recipient: &Pubkey,
    payer: &Pubkey,
    payment: &Pubkey,
    amount: u64,
    fee: u64,
) -> Result<Vec<solana_sdk::instruction::Instruction>, SolanaPreflightError> {
    if amount == 0
        || fee == 0
        || owner == payer
        || owner == recipient
        || owner == payment
        || recipient == payment
    {
        return Err(SolanaPreflightError::InvalidResponse);
    }
    let mint =
        Pubkey::from_str(MAINNET_USDC_MINT).map_err(|_| SolanaPreflightError::InvalidResponse)?;
    let ata = |wallet: &Pubkey| {
        get_associated_token_address_with_program_id(wallet, &mint, &spl_token::id())
    };
    let source = ata(owner);
    let mut instructions = vec![
        create_associated_token_account_idempotent(payer, recipient, &mint, &spl_token::id()),
        create_associated_token_account_idempotent(payer, payment, &mint, &spl_token::id()),
    ];
    for (to, value) in [(recipient, amount), (payment, fee)] {
        instructions.push(
            spl_token::instruction::transfer_checked(
                &spl_token::id(),
                &source,
                &mint,
                &ata(to),
                owner,
                &[],
                value,
                6,
            )
            .map_err(|_| SolanaPreflightError::InvalidResponse)?,
        );
    }
    Ok(instructions)
}

fn simulation_short_of_gas(value: &Value) -> bool {
    if matches!(value["err"].as_str(), Some("InsufficientFundsForFee"))
        || value["err"].get("InsufficientFundsForRent").is_some()
    {
        return true;
    }
    // A token-program "insufficient funds" is the asset balance, not gas. Only the system
    // program's explicit lamport shortage qualifies for the gasless fallback.
    value["logs"].as_array().is_some_and(|logs| {
        logs.iter()
            .filter_map(Value::as_str)
            .any(|line| line.contains("insufficient lamports"))
    })
}

/// Reject a changed message, another fee payer or a bad signature before broadcasting anything.
pub fn checked_swap_signature(
    unsigned: &str,
    signed: &str,
    wallet: &str,
) -> Result<String, SolanaPreflightError> {
    use solana_sdk::transaction::VersionedTransaction;
    let invalid = || SolanaPreflightError::InvalidResponse;
    let decode = |s: &str| -> Result<VersionedTransaction, SolanaPreflightError> {
        bincode::deserialize(&STANDARD.decode(s).map_err(|_| invalid())?).map_err(|_| invalid())
    };
    let planned = decode(unsigned)?;
    let tx = decode(signed)?;
    let owner = Pubkey::from_str(wallet).map_err(|_| invalid())?;
    if planned.message != tx.message
        || tx.signatures.len() != 1
        || tx.message.header().num_required_signatures != 1
        || tx.message.static_account_keys().first() != Some(&owner)
        || !tx.signatures[0].verify(owner.as_ref(), &tx.message.serialize())
    {
        return Err(invalid());
    }
    Ok(tx.signatures[0].to_string())
}

fn swap_amounts(
    tx: &Value,
    wallet: &str,
    input: &str,
    output: &str,
) -> Result<(u128, u128), SolanaPreflightError> {
    use std::collections::{HashMap, HashSet};
    let invalid = || SolanaPreflightError::InvalidResponse;
    let meta = &tx["meta"];
    if !meta["err"].is_null() {
        return Err(invalid());
    }
    let mut delta: HashMap<String, i128> = HashMap::new();
    let mut owned_accounts = HashSet::new();
    for (field, sign) in [("preTokenBalances", -1_i128), ("postTokenBalances", 1)] {
        for balance in meta[field].as_array().ok_or_else(invalid)? {
            if balance["owner"].as_str() != Some(wallet) {
                continue;
            }
            let mint = balance["mint"].as_str().ok_or_else(invalid)?;
            let amount: i128 = balance["uiTokenAmount"]["amount"]
                .as_str()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            *delta.entry(mint.into()).or_default() += sign * amount;
            owned_accounts.insert(balance["accountIndex"].as_u64().ok_or_else(invalid)? as usize);
        }
    }
    let keys = tx["transaction"]["message"]["accountKeys"]
        .as_array()
        .ok_or_else(invalid)?;
    let owner = keys
        .iter()
        .position(|k| k.as_str().or_else(|| k["pubkey"].as_str()) == Some(wallet))
        .ok_or_else(invalid)?;
    let lamports = |field: &str, i: usize| -> Result<i128, SolanaPreflightError> {
        meta[field][i].as_u64().map(i128::from).ok_or_else(invalid)
    };
    let mut native = lamports("postBalances", owner)? - lamports("preBalances", owner)?;
    // The builder pins the wallet as fee payer. Adding its fee and changes in ATA rent
    // gives the principal SOL change, including wrap/unwrap instructions.
    if owner != 0 {
        return Err(invalid());
    }
    native += meta["fee"].as_u64().map(i128::from).ok_or_else(invalid)?;
    for i in owned_accounts {
        native += lamports("postBalances", i)? - lamports("preBalances", i)?;
    }
    delta.insert(spl_token::native_mint::id().to_string(), native);
    let paid = delta
        .get(input)
        .copied()
        .ok_or_else(invalid)?
        .checked_neg()
        .and_then(|n| u128::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    let got = delta
        .get(output)
        .copied()
        .and_then(|n| u128::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    Ok((paid, got))
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
    #[test]
    fn fee_probe_matches_signature_count_and_rent_without_moving_any_token() {
        let (owner, deposit, payer, payment) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let instructions = fee_probe_instructions(&owner, &deposit, &payer, &payment).unwrap();
        assert_eq!(instructions.len(), 2);
        assert!(instructions
            .iter()
            .all(|ix| ix.program_id == spl_associated_token_account::id()));
        let message = solana_sdk::message::Message::new(&instructions, Some(&payer));
        assert_eq!(message.header.num_required_signatures, 2);
        assert_eq!(message.account_keys[..2], [payer, owner]);
    }
    #[test]
    fn cash_and_provider_fee_transfer_only_exact_mainnet_usdc() {
        let (owner, deposit, payer, payment) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mint = Pubkey::from_str(MAINNET_USDC_MINT).unwrap();
        let ix = fee_transfer_instructions(&owner, &deposit, &payer, &payment, 500_000, 302_323)
            .unwrap();
        assert_eq!(ix.len(), 4);
        for create in &ix[..2] {
            assert_eq!(create.program_id, spl_associated_token_account::id());
            assert_eq!(create.accounts[0].pubkey, payer);
        }
        for (transfer, (recipient, amount)) in
            ix[2..].iter().zip([(deposit, 500_000), (payment, 302_323)])
        {
            assert_eq!(transfer.program_id, spl_token::id());
            assert_eq!(transfer.accounts[1].pubkey, mint);
            assert_eq!(
                transfer.accounts[2].pubkey,
                get_associated_token_address_with_program_id(&recipient, &mint, &spl_token::id())
            );
            assert_eq!(transfer.accounts[3].pubkey, owner);
            assert!(transfer.accounts[3].is_signer);
            assert_eq!(
                spl_token::instruction::TokenInstruction::unpack(&transfer.data).unwrap(),
                spl_token::instruction::TokenInstruction::TransferChecked {
                    amount,
                    decimals: 6
                }
            );
        }
        assert!(
            fee_transfer_instructions(&owner, &deposit, &owner, &payment, 500_000, 302_323)
                .is_err()
        );
        assert!(
            fee_transfer_instructions(&owner, &deposit, &payer, &owner, 500_000, 302_323).is_err()
        );
    }

    #[test]
    fn gas_shortage_is_distinct_from_not_enough_tokens() {
        assert!(simulation_short_of_gas(
            &json!({"err":"InsufficientFundsForFee"})
        ));
        assert!(simulation_short_of_gas(
            &json!({"err":{"InsufficientFundsForRent":{"account_index":2}}})
        ));
        assert!(simulation_short_of_gas(
            &json!({"err":{"InstructionError":[0,{"Custom":1}]},
            "logs":["Transfer: insufficient lamports 3000, need 2039280"]})
        ));
        assert!(!simulation_short_of_gas(
            &json!({"err":{"InstructionError":[0,{"Custom":1}]},
            "logs":["Program log: Error: insufficient funds"]})
        ));
    }

    #[test]
    fn signed_swap_cannot_change_the_plan_or_payer() {
        use solana_sdk::{
            message::{Message, VersionedMessage},
            signature::Keypair,
            transaction::VersionedTransaction,
        };
        let wallet = Keypair::new();
        let other = Keypair::new();
        let message = VersionedMessage::Legacy(Message::new(&[], Some(&wallet.pubkey())));
        let planned = VersionedTransaction {
            signatures: vec![Default::default()],
            message: message.clone(),
        };
        let signed = VersionedTransaction::try_new(message, &[&wallet]).unwrap();
        let encoded = |tx: &VersionedTransaction| STANDARD.encode(bincode::serialize(tx).unwrap());
        assert!(checked_swap_signature(
            &encoded(&planned),
            &encoded(&signed),
            &wallet.pubkey().to_string()
        )
        .is_ok());
        assert!(checked_swap_signature(
            &encoded(&planned),
            &encoded(&planned),
            &wallet.pubkey().to_string()
        )
        .is_err());
        assert!(checked_swap_signature(
            &encoded(&planned),
            &encoded(&signed),
            &other.pubkey().to_string()
        )
        .is_err());
        let mut changed = signed;
        changed.message.set_recent_blockhash(Hash::new_unique());
        assert!(checked_swap_signature(
            &encoded(&planned),
            &encoded(&changed),
            &wallet.pubkey().to_string()
        )
        .is_err());
    }

    #[test]
    fn confirmed_swap_uses_actual_token_amounts_and_excludes_sol_rent_and_fees() {
        let wallet = "owner";
        let native = spl_token::native_mint::id().to_string();
        let mut tx = json!({"transaction":{"message":{"accountKeys":[{"pubkey":wallet},{"pubkey":"cash"},{"pubkey":"token"}]}},
            "meta":{"err":null,"fee":5000,"preBalances":[10_000_000,2_039_280,0], "postBalances":[11_955_720,2_039_280,2_039_280],
            "preTokenBalances":[{"owner":wallet,"mint":"USDC","accountIndex":1,"uiTokenAmount":{"amount":"1000000"}}],
            "postTokenBalances":[{"owner":wallet,"mint":"USDC","accountIndex":1,"uiTokenAmount":{"amount":"500000"}},
                {"owner":wallet,"mint":"OTHER","accountIndex":2,"uiTokenAmount":{"amount":"1"}}]}});
        assert_eq!(
            swap_amounts(&tx, wallet, "USDC", &native).unwrap(),
            (500_000, 4_000_000)
        );
        tx["meta"]["postBalances"][0] = json!(7_951_720);
        tx["meta"]["postTokenBalances"][0]["uiTokenAmount"]["amount"] = json!("1400000");
        assert_eq!(
            swap_amounts(&tx, wallet, &native, "USDC").unwrap(),
            (4_000, 400_000)
        );
        tx["meta"]["err"] = json!("failed");
        assert!(swap_amounts(&tx, wallet, &native, "USDC").is_err());
    }
    use super::*;
    // A friend send on Solana: a mainnet USDC transfer builds, and says whether it opens an account.
    #[tokio::test]
    #[ignore]
    async fn live_usdc_transfer_builds() {
        let client = SolanaAtaPreflight::new(
            SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let (tx, creates) = client
            .usdc_transfer_transaction(
                "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
                &Pubkey::new_unique().to_string(),
                1_000_000,
            )
            .await
            .unwrap();
        let bytes = STANDARD.decode(tx).unwrap();
        let parsed: Transaction = bincode::deserialize(&bytes).unwrap();
        assert!(creates, "a brand-new recipient has no USDC account yet");
        assert_eq!(parsed.message.instructions.len(), 2);
        assert_eq!(parsed.signatures.len(), 1);
    }
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
