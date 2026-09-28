//! Poll the Atlas Solana relayer SOL balance so ATA creation cannot run dry.
//! ATLAS_SOLANA_RELAYER_KEYPAIR_PATH must point to an Atlas-owned keypair.

use std::{error::Error, time::Duration};

use engine_execution::solana::{SolanaAtaPreflight, SolanaNetwork};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let key_path = std::env::var("ATLAS_SOLANA_RELAYER_KEYPAIR_PATH")?;
    let rpc_url = std::env::var("ATLAS_SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.devnet.solana.com".to_owned());
    let once = std::env::args().any(|arg| arg == "--once");
    let preflight = SolanaAtaPreflight::new(SolanaNetwork::Devnet, rpc_url, key_path)?;
    loop {
        match preflight.relayer_balance().await {
            Ok(balance) => {
                println!(
                    "relayer={} lamports={} low={}",
                    balance.address, balance.lamports, balance.low
                );
                if once && balance.low {
                    return Err("relayer SOL is below the 0.05 SOL warning threshold".into());
                }
            }
            Err(error) => {
                eprintln!("relayer balance check failed: {error}");
                if once {
                    return Err(error.into());
                }
            }
        }
        if once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
