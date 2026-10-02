//! User-paid USDC transfers through an external mainnet fee payer. No Atlas payer key exists.
use crate::solana::{SolanaAtaPreflight, SolanaPreflightError, MAINNET_USDC_MINT};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::{pubkey::Pubkey, signature::Signature, transaction::VersionedTransaction};
use std::{str::FromStr, time::Duration};

pub const PUBLIC_MAINNET: &str = "https://mainnet.kora-nodes.com";
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    key: Option<String>,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The network fee service did not answer")]
    Http(#[from] reqwest::Error),
    #[error("The network fee service could not prepare this transfer")]
    Invalid,
    #[error("The network fee service rejected this transfer")]
    Rejected,
    #[error("The network fee service rejected {operation} (code {code})")]
    Rpc { operation: &'static str, code: i64 },
    #[error("The network fee changed; refresh the quote")]
    FeeChanged,
    #[error("The cash needed for network fees is not available")]
    Cash,
    #[error("Could not verify the fee-paid transaction")]
    Solana(#[from] SolanaPreflightError),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transfer {
    pub transaction: String,
    pub payer: String,
    pub payment: String,
    pub fee_units: u64,
    pub last_valid_block_height: u64,
}
#[derive(Clone, Debug)]
pub struct Estimate {
    pub payer: String,
    pub payment: String,
    pub fee_units: u64,
}
impl Client {
    pub fn new(endpoint: &str, key: Option<String>) -> Result<Self, Error> {
        let endpoint: reqwest::Url = endpoint.parse().map_err(|_| Error::Invalid)?;
        if endpoint.scheme() != "https"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            endpoint,
            key,
        })
    }
    async fn rpc(&self, method: &'static str, params: Value) -> Result<Value, Error> {
        let mut call = self
            .http
            .post(self.endpoint.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
        if let Some(key) = &self.key {
            call = call.header("x-api-key", key);
        }
        let response = call.send().await?;
        if !response.status().is_success() {
            return Err(Error::Rpc {
                operation: method,
                code: i64::from(response.status().as_u16()),
            });
        }
        let response: Value = response.json().await?;
        if let Some(error) = response.get("error") {
            return Err(Error::Rpc {
                operation: method,
                code: error["code"].as_i64().unwrap_or(-1),
            });
        }
        response.get("result").cloned().ok_or(Error::Invalid)
    }
    async fn payer(&self) -> Result<(String, String), Error> {
        let config = self.rpc("getConfig", json!([])).await?;
        let rules = &config["validation_config"];
        for program in [
            spl_token::id().to_string(),
            spl_associated_token_account::id().to_string(),
        ] {
            if !rules["allowed_programs"]
                .as_array()
                .is_some_and(|p| p.iter().any(|v| v.as_str() == Some(&program)))
            {
                return Err(Error::Rejected);
            }
        }
        if !rules["allowed_spl_paid_tokens"]
            .as_array()
            .is_some_and(|p| p.iter().any(|v| v.as_str() == Some(MAINNET_USDC_MINT)))
        {
            return Err(Error::Rejected);
        }
        let info = self.rpc("getPayerSigner", json!([])).await?;
        let payer = info["signer_address"].as_str().ok_or(Error::Invalid)?;
        let payment = info["payment_address"].as_str().ok_or(Error::Invalid)?;
        if !config["fee_payers"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(payer)))
        {
            return Err(Error::Invalid);
        }
        Pubkey::from_str(payer).map_err(|_| Error::Invalid)?;
        Pubkey::from_str(payment).map_err(|_| Error::Invalid)?;
        Ok((payer.into(), payment.into()))
    }
    pub async fn estimate_new_recipient(
        &self,
        solana: &SolanaAtaPreflight,
        owner: &str,
        amount: u64,
    ) -> Result<Estimate, Error> {
        let destination = Pubkey::new_unique().to_string();
        if solana
            .owner_mint_balance(owner, MAINNET_USDC_MINT, 6)
            .await?
            >= u128::from(amount) + 1_000_000
        {
            return self.estimate(solana, owner, &destination, amount).await;
        }
        // No priority fee instructions: native cost is two signatures and ATA rent. The final
        // actual transfer is independently re-estimated and simulated before it can be signed.
        let (payer, payment) = self.payer().await?;
        let transaction = solana
            .usdc_transfer_fee_probe(owner, &destination, &payer, &payment)
            .await?;
        let value = self.rpc("estimateTransactionFee",json!({"transaction":transaction,"fee_token":MAINNET_USDC_MINT,"signer_key":payer,"sig_verify":false})).await?;
        fee_estimate(&value, &payer, &payment)
    }
    pub async fn estimate(
        &self,
        solana: &SolanaAtaPreflight,
        owner: &str,
        recipient: &str,
        amount: u64,
    ) -> Result<Estimate, Error> {
        let (payer, payment) = self.payer().await?;
        let (transaction, _) = solana
            .usdc_transfer_with_fee(owner, recipient, amount, &payer, &payment, 1)
            .await?;
        let value = self.rpc("estimateTransactionFee", json!({"transaction":transaction,"fee_token":MAINNET_USDC_MINT,"signer_key":payer,"sig_verify":false})).await?;
        fee_estimate(&value, &payer, &payment)
    }
    /// Provider signs only its fee-payer slot. Without the phone's matching signature this cannot spend.
    pub async fn prepare(
        &self,
        solana: &SolanaAtaPreflight,
        owner: &str,
        recipient: &str,
        amount: u64,
        max_fee: u64,
    ) -> Result<Transfer, Error> {
        let estimate = self.estimate(solana, owner, recipient, amount).await?;
        if max_fee == 0 || max_fee > 1_000_000 || estimate.fee_units > max_fee {
            return Err(Error::FeeChanged);
        }
        let held = solana
            .owner_mint_balance(owner, MAINNET_USDC_MINT, 6)
            .await?;
        if held < u128::from(amount) + u128::from(max_fee) {
            return Err(Error::Cash);
        }
        let (transaction, height) = solana
            .usdc_transfer_with_fee(
                owner,
                recipient,
                amount,
                &estimate.payer,
                &estimate.payment,
                max_fee,
            )
            .await?;
        // Adding the actual reimbursement must still fit the fee the user confirmed.
        let value = self.rpc("estimateTransactionFee", json!({"transaction":transaction,"fee_token":MAINNET_USDC_MINT,"signer_key":estimate.payer,"sig_verify":false})).await?;
        let final_fee = fee_estimate(&value, &estimate.payer, &estimate.payment)?;
        if final_fee.fee_units > max_fee {
            return Err(Error::FeeChanged);
        }
        solana.preflight_swap(&transaction).await?;
        let response = self
            .rpc(
                "signTransaction",
                json!({"transaction":transaction,"signer_key":estimate.payer,"sig_verify":false}),
            )
            .await?;
        if response["signer_pubkey"].as_str() != Some(&estimate.payer) {
            return Err(Error::Invalid);
        }
        let signed = response["signed_transaction"]
            .as_str()
            .ok_or(Error::Invalid)?;
        checked(&transaction, signed, owner, &estimate.payer, false)?;
        Ok(Transfer {
            transaction: signed.into(),
            payer: estimate.payer,
            payment: estimate.payment,
            fee_units: max_fee,
            last_valid_block_height: height,
        })
    }
}
fn fee_estimate(value: &Value, payer: &str, payment: &str) -> Result<Estimate, Error> {
    if value["signer_pubkey"].as_str() != Some(payer)
        || value["payment_address"].as_str() != Some(payment)
    {
        return Err(Error::Invalid);
    }
    let fee = value["fee_in_token"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 1_000_000)
        .ok_or(Error::Invalid)?;
    Ok(Estimate {
        payer: payer.into(),
        payment: payment.into(),
        fee_units: fee,
    })
}
pub fn checked(
    planned: &str,
    returned: &str,
    owner: &str,
    payer: &str,
    fully_signed: bool,
) -> Result<String, Error> {
    let decode = |s: &str| -> Result<VersionedTransaction, Error> {
        let bytes = STANDARD.decode(s).map_err(|_| Error::Invalid)?;
        if bytes.len() > 1232 {
            return Err(Error::Invalid);
        }
        bincode::deserialize(&bytes).map_err(|_| Error::Invalid)
    };
    let (before, after) = (decode(planned)?, decode(returned)?);
    let owner = Pubkey::from_str(owner).map_err(|_| Error::Invalid)?;
    let payer = Pubkey::from_str(payer).map_err(|_| Error::Invalid)?;
    if owner == payer
        || before.message != after.message
        || after.message.header().num_required_signatures != 2
        || after.signatures.len() != 2
        || after.message.static_account_keys().get(..2) != Some(&[payer, owner])
    {
        return Err(Error::Invalid);
    }
    let message = after.message.serialize();
    if !after.signatures[0].verify(payer.as_ref(), &message)
        || (fully_signed && !after.signatures[1].verify(owner.as_ref(), &message))
        || (!fully_signed && after.signatures[1] != Signature::default())
    {
        return Err(Error::Invalid);
    }
    Ok(after.signatures[0].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        instruction::Instruction,
        message::{Message, VersionedMessage},
        signature::{Keypair, Signer},
    };
    #[test]
    fn fees_are_positive_capped_and_bound_to_the_provider() {
        let mut value =
            json!({"signer_pubkey":"payer","payment_address":"payment","fee_in_token":1000});
        assert_eq!(
            fee_estimate(&value, "payer", "payment").unwrap().fee_units,
            1000
        );
        assert!(fee_estimate(&value, "other", "payment").is_err());
        for amount in [0, 1_000_001] {
            value["fee_in_token"] = json!(amount);
            assert!(fee_estimate(&value, "payer", "payment").is_err());
        }
    }
    #[test]
    fn both_signatures_bind_the_exact_cash_and_fee_transaction() {
        let (payer, owner) = (Keypair::new(), Keypair::new());
        let ix = Instruction::new_with_bytes(
            spl_token::id(),
            &[12, 1],
            vec![solana_sdk::instruction::AccountMeta::new_readonly(
                owner.pubkey(),
                true,
            )],
        );
        let mut message = Message::new(&[ix], Some(&payer.pubkey()));
        message.recent_blockhash = Hash::new_unique();
        let mut tx = VersionedTransaction {
            message: VersionedMessage::Legacy(message),
            signatures: vec![Signature::default(); 2],
        };
        let encode = |t: &VersionedTransaction| STANDARD.encode(bincode::serialize(t).unwrap());
        let unsigned = encode(&tx);
        tx.signatures[0] = payer.sign_message(&tx.message.serialize());
        let planned = encode(&tx);
        assert!(checked(
            &unsigned,
            &planned,
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            false
        )
        .is_ok());
        tx.signatures[1] = owner.sign_message(&tx.message.serialize());
        let signed = encode(&tx);
        assert!(checked(
            &planned,
            &signed,
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_ok());
        assert!(checked(
            &planned,
            &planned,
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_err());
        let other = Keypair::new();
        assert!(checked(
            &planned,
            &signed,
            &other.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_err());
        tx.signatures[0] = Signature::default();
        assert!(checked(
            &planned,
            &encode(&tx),
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_err());
        tx.signatures[0] = payer.sign_message(&tx.message.serialize());
        tx.signatures[1] = other.sign_message(&tx.message.serialize());
        assert!(checked(
            &planned,
            &encode(&tx),
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_err());
        tx.message = VersionedMessage::Legacy(Message::new(&[], Some(&payer.pubkey())));
        assert!(checked(
            &planned,
            &encode(&tx),
            &owner.pubkey().to_string(),
            &payer.pubkey().to_string(),
            true
        )
        .is_err());
    }
    #[tokio::test]
    #[ignore]
    async fn live_kora_empty_cash_wallet_estimate() {
        let solana = SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            std::env::var("ATLAS_SOLANA_MAINNET_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
            "",
        )
        .unwrap();
        let client = Client::new(PUBLIC_MAINNET, None).unwrap();
        let owner = Pubkey::new_unique().to_string();
        let estimate = client
            .estimate_new_recipient(&solana, &owner, 500_000)
            .await
            .unwrap_or_else(|error| {
                let reason = match error {
                    Error::Rpc { operation, code } => format!("{operation} code {code}"),
                    Error::Solana(crate::solana::SolanaPreflightError::SwapSimulation(_)) => {
                        "simulation".into()
                    }
                    Error::Solana(_) => "RPC or build".into(),
                    _ => "provider unavailable".into(),
                };
                panic!("Mainnet fee estimate failed: {reason}; endpoint details withheld")
            });
        println!("Wallet awaiting cash from Base: USDC transfer fee estimate={} units; user signatures=0; broadcasts=0",estimate.fee_units);
    }
    #[tokio::test]
    #[ignore]
    async fn live_kora_provider_partial_signature() {
        let solana = SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let client = Client::new(PUBLIC_MAINNET, None).unwrap();
        let owner = "6metVveeGpQN6YoXYmevmvtQp7k5CvCgaKBRuQUgevKR";
        let destination = Pubkey::new_unique().to_string();
        let estimate = client
            .estimate(&solana, owner, &destination, 500_000)
            .await
            .unwrap();
        let cap = estimate.fee_units.saturating_mul(11).div_ceil(10) + 1000;
        let transfer = client
            .prepare(&solana, owner, &destination, 500_000, cap)
            .await
            .unwrap();
        checked(
            &transfer.transaction,
            &transfer.transaction,
            owner,
            &transfer.payer,
            false,
        )
        .unwrap();
        println!("Mainnet USDC reserve transfer built and simulated; provider partial signature verified; fee={} units; user signatures=0; broadcasts=0",transfer.fee_units);
    }
    #[tokio::test]
    #[ignore]
    async fn live_kora_transfer_estimate() {
        let solana = SolanaAtaPreflight::new(
            crate::solana::SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let client = Client::new(PUBLIC_MAINNET, None).unwrap();
        let owner = "6metVveeGpQN6YoXYmevmvtQp7k5CvCgaKBRuQUgevKR";
        let destination = Pubkey::new_unique().to_string();
        let estimate = client
            .estimate(&solana, owner, &destination, 500_000)
            .await
            .unwrap();
        println!("USDC transfer to a new recipient: fee={} USDC units, external payer={}, user signatures=0, submitted=0",estimate.fee_units,estimate.payer);
    }
}
