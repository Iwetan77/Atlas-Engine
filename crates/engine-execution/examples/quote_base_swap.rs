//! Read a real Base mainnet Uniswap V3 quote without sending a transaction.
//! Usage: cargo run -p engine-execution --example quote_base_swap -- <destination-token> <USDC-base-units>

use engine_execution::swaps::{
    oneinch::BaseSwapRequest,
    uniswap::{UniswapV3Client, BASE_USDC},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let destination = arguments
        .next()
        .ok_or("destination token address required")?;
    let amount: u128 = arguments
        .next()
        .ok_or("USDC base-unit amount required")?
        .parse()?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let rpc_url = std::env::var("ATLAS_BASE_MAINNET_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.base.org".into());
    let client = UniswapV3Client::new(rpc_url.parse()?)?;
    let quote = client
        .quote_direct(&BaseSwapRequest {
            source_token: BASE_USDC.into(),
            destination_token: destination,
            amount_base_units: amount,
        })
        .await?;
    println!(
        "Base Uniswap V3 direct quote: {} USDC base units -> {} output base units, fee tier {}, quoter gas {}",
        quote.amount_in, quote.amount_out, quote.fee, quote.quoter_gas_estimate
    );
    Ok(())
}
