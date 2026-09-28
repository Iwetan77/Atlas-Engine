use engine_execution::swaps::jupiter::{JupiterClient, JupiterOrderRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: quote_jupiter <input-mint> <output-mint> <base-units>".into());
    }
    let client = JupiterClient::new(std::env::var("JUPITER_API_KEY").ok());
    let order = client
        .order(&JupiterOrderRequest {
            input_mint: args[1].clone(),
            output_mint: args[2].clone(),
            amount_base_units: args[3].parse()?,
            taker: None,
        })
        .await?;
    println!("{}", serde_json::to_string(&order)?);
    Ok(())
}
