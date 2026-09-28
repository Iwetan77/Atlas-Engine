use engine_execution::funding::deposit::{DepositTarget, EvmDepositScanner};
use engine_execution::gateway::{GatewayClient, GatewayEnvironment, GatewaySource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .ok_or("usage: read_gateway_balance <wallet-address>")?;
    let scanner = EvmDepositScanner::new("https://sepolia.base.org".parse()?, 2)?;
    let target = DepositTarget {
        user_id: "atlas-phase1".into(),
        wallet_address: address.clone(),
        token_contract: "0x036CbD53842c5426634e7929541eC2318f3dCF7e".into(),
        chain_id: 84532,
    };
    let wallet_units = scanner.wallet_token_balance(&target).await?;
    println!("Base Sepolia wallet USDC base units: {wallet_units}");
    let gateway = GatewayClient::new(GatewayEnvironment::Testnet)?;
    let sources = [GatewaySource {
        depositor: address,
        domain: None,
    }];
    let balances = gateway.balances(&sources).await?;
    let deposits = gateway.deposits(&sources).await?;
    println!("{}", serde_json::to_string_pretty(&balances)?);
    println!("{}", serde_json::to_string_pretty(&deposits)?);
    Ok(())
}
