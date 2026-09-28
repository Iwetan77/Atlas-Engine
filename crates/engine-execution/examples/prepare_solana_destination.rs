//! Prepare a first-time Solana USDC recipient for a real Gateway spend.
//! Example: ATLAS_SOLANA_RELAYER_KEYPAIR_PATH=.env.test-solana-relayer.json cargo run -p engine-execution --example prepare_solana_destination -- <owner>

use std::error::Error;

use engine_execution::solana::{SolanaAtaPreflight, SolanaNetwork};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let owner = std::env::args()
        .nth(1)
        .ok_or("expected Solana owner wallet address")?;
    let key_path = std::env::var("ATLAS_SOLANA_RELAYER_KEYPAIR_PATH")?;
    let rpc_url = std::env::var("ATLAS_SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.devnet.solana.com".to_owned());
    let preflight = SolanaAtaPreflight::new(SolanaNetwork::Devnet, rpc_url, key_path)?;
    let result = preflight.ensure_usdc_ata(&owner).await?;
    println!("ubkRecipientAddress={}", result.ubk_recipient_address);
    println!(
        "gatewayDestinationRecipient={}",
        result.gateway_destination_recipient
    );
    if let Some(signature) = result.ata_creation_signature {
        println!("ataCreationSignature={signature}");
    }
    println!("relayerAddress={}", result.relayer.address);
    println!("relayerLamports={}", result.relayer.lamports);
    println!("relayerLow={}", result.relayer.low);
    Ok(())
}
