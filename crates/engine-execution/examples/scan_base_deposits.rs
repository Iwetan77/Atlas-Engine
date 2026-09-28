//! Read-only live Base Sepolia USDC deposit check for the Phase 1 gate.
//! Usage: cargo run -p engine-execution --example scan_base_deposits -- <wallet>

use engine_execution::funding::deposit::{DepositTarget, EvmDepositScanner};
use reqwest::Url;

const USDC_BASE_SEPOLIA: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wallet = std::env::args().nth(1).ok_or("missing wallet address")?;
    let rpc =
        std::env::var("BASE_SEPOLIA_RPC_URL").unwrap_or_else(|_| "https://sepolia.base.org".into());
    let scanner = EvmDepositScanner::new(Url::parse(&rpc)?, 2)?;
    let target = DepositTarget {
        user_id: "atlas-phase1".into(),
        wallet_address: wallet,
        token_contract: USDC_BASE_SEPOLIA.into(),
        chain_id: 84532,
    };
    let deposits = scanner.scan_recent(&target).await?;
    for deposit in &deposits {
        println!(
            "user={} chain={} tx={} log={} block={} usdc_base_units={}",
            deposit.user_id,
            deposit.chain_id,
            deposit.transaction_hash,
            deposit.log_index,
            deposit.block_number,
            deposit.amount_base_units
        );
    }
    println!("confirmed_deposits={}", deposits.len());
    Ok(())
}
