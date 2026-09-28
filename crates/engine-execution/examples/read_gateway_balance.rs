use engine_execution::gateway::{GatewayClient, GatewayEnvironment, GatewaySource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .ok_or("usage: read_gateway_balance <wallet-address>")?;
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
