//! Bootstrap a user's fee reserve from their cash. Kora fronts SOL for the USDC transfer;
//! 1Click returns native SOL to the same wallet. Atlas has no fee-payer wallet.
use super::*;
use engine_execution::{
    kora,
    near_intents::{Client, Quote, QuoteRequest},
};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

const CASH: &str = "nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near";
const SOL: &str = "nep141:sol.omft.near";
pub(super) const FLOOR: u128 = 5_000_000;
const AMOUNTS: [u64; 5] = [500_000, 750_000, 1_000_000, 1_500_000, 2_000_000];
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn unavailable() -> ApiError {
    (
        StatusCode::BAD_GATEWAY,
        "Couldn't prepare network fees right now. Nothing has been sent; try again shortly.".into(),
    )
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Preview {
    pub amount: u64,
    pub fee: u64,
    pub gas_value: u128,
    pub min_lamports: u128,
}
impl Preview {
    pub fn cash(&self) -> u128 {
        u128::from(self.amount) + u128::from(self.fee)
    }
    pub fn network_fee(&self) -> u128 {
        self.cash().saturating_sub(self.gas_value)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Reserve {
    pub transfer: kora::Transfer,
    pub deposit: String,
    pub signature: Option<String>,
    pub expires: u64,
    pub finish_by: u64,
    #[serde(default)]
    pub next_transaction: Option<String>,
    #[serde(default)]
    pub next_expires: u64,
}
fn checked_quote(quote: &Quote, native: u128, input: u64) -> Result<(u128, u128), ApiError> {
    let amount = quote.amount_in.parse::<u64>().map_err(|_| unavailable())?;
    let minimum = quote
        .min_amount_out
        .as_deref()
        .and_then(|n| n.parse::<u128>().ok())
        .ok_or_else(unavailable)?;
    let output = quote
        .amount_out
        .parse::<u128>()
        .map_err(|_| unavailable())?;
    if amount != input
        || minimum == 0
        || minimum > output
        || minimum < output.saturating_mul(99) / 100
        || minimum.saturating_add(native) < FLOOR
        || quote.deposit_memo.is_some()
    {
        return Err(unavailable());
    }
    let value = display_micros(&quote.amount_out_usd)?;
    Ok((minimum, value.min(u128::from(input))))
}
async fn quote(client: &Client, owner: &str, amount: u64, dry: bool) -> Result<Quote, ApiError> {
    client
        .quote(&QuoteRequest::exact_input(
            CASH,
            SOL,
            &amount.to_string(),
            owner,
            owner,
            &near_intents::deadline_utc(300),
            dry,
        ))
        .await
        .map_err(|error| {
            let kind = match error {
                engine_execution::near_intents::Error::Http(_) => "transport",
                engine_execution::near_intents::Error::Venue(_, _) => "rejected",
                engine_execution::near_intents::Error::InvalidQuote => "invalid quote",
            };
            eprintln!("network-fee reserve: 1Click {kind}, dry={dry}");
            unavailable()
        })
}
// Dollar values are indicative. Keep integer transfer amounts exact; truncate only the display.
fn display_micros(text: &str) -> Result<u128, ApiError> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(unavailable());
    }
    let whole = whole.parse::<u128>().map_err(|_| unavailable())?;
    let fraction = &fraction[..fraction.len().min(6)];
    whole
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(format!("{fraction:0<6}").parse::<u128>().ok()?))
        .ok_or_else(unavailable)
}
pub(super) async fn preview(
    state: &AppState,
    owner: &str,
    force: bool,
) -> Result<Option<Preview>, ApiError> {
    preview_with(
        &state.solana_mainnet,
        &state.markets.kora,
        &state.markets.gas_near,
        owner,
        force,
    )
    .await
}
async fn preview_with(
    solana: &engine_execution::solana::SolanaAtaPreflight,
    kora: &kora::Client,
    near: &Client,
    owner: &str,
    force: bool,
) -> Result<Option<Preview>, ApiError> {
    let native = solana
        .owner_sol_balance(owner)
        .await
        .map_err(|_| unavailable())?;
    if native >= FLOOR && !force {
        return Ok(None);
    }
    let fee = kora
        .estimate_new_recipient(solana, owner, AMOUNTS[0])
        .await
        .map_err(|_| {
            eprintln!("network-fee reserve: fee estimate unavailable");
            unavailable()
        })?
        .fee_units;
    // The exact transfer includes this small, visible buffer. Repricing cannot exceed it.
    let fixed = fee.saturating_mul(11).div_ceil(10).saturating_add(1_000);
    if fixed > 1_000_000 {
        return Err(unavailable());
    }
    for amount in AMOUNTS {
        let q = quote(near, owner, amount, true).await?;
        if let Ok((minimum, value)) = checked_quote(&q, native, amount) {
            if native >= FLOOR && minimum < FLOOR {
                continue;
            }
            return Ok(Some(Preview {
                amount,
                fee: fixed,
                gas_value: value,
                min_lamports: minimum,
            }));
        }
    }
    Err(unavailable())
}
pub(super) async fn prepare(
    state: &AppState,
    owner: &str,
    preview: &Preview,
    required_cash: u128,
) -> Result<Reserve, ApiError> {
    let held = state
        .solana_mainnet
        .owner_mint_balance(owner, engine_execution::solana::MAINNET_USDC_MINT, 6)
        .await
        .map_err(|_| unavailable())?;
    if held < required_cash {
        return Err((
            StatusCode::CONFLICT,
            "Your balance changed. Refresh the quote; this network-fee step has not been sent."
                .into(),
        ));
    }
    prepare_with(
        &state.solana_mainnet,
        &state.markets.kora,
        &state.markets.gas_near,
        owner,
        preview,
    )
    .await
}
async fn prepare_with(
    solana: &engine_execution::solana::SolanaAtaPreflight,
    kora: &kora::Client,
    near: &Client,
    owner: &str,
    preview: &Preview,
) -> Result<Reserve, ApiError> {
    let native = solana
        .owner_sol_balance(owner)
        .await
        .map_err(|_| unavailable())?;
    let q = quote(near, owner, preview.amount, false).await?;
    let (minimum, _) = checked_quote(&q, native, preview.amount)?;
    if minimum < preview.min_lamports * 99 / 100 {
        return Err((
            StatusCode::CONFLICT,
            "Network fees changed; refresh this quote. Nothing has been sent.".into(),
        ));
    }
    let deposit = q.deposit_address.ok_or_else(unavailable)?;
    let transfer = kora
        .prepare(solana, owner, &deposit, preview.amount, preview.fee)
        .await
        .map_err(|error| {
            let reason = match error {
                kora::Error::Rpc { operation, code } => format!("{operation} code {code}"),
                kora::Error::FeeChanged => "fee changed".into(),
                kora::Error::Cash => "cash not available".into(),
                kora::Error::Solana(
                    engine_execution::solana::SolanaPreflightError::SwapSimulation(reason),
                ) => format!("simulation {reason}"),
                kora::Error::Solana(_) => "RPC or transfer build".into(),
                kora::Error::Invalid => "invalid response or signature".into(),
                _ => "provider unavailable".into(),
            };
            eprintln!("network-fee reserve: prepare {reason}");
            unavailable()
        })?;
    Ok(Reserve {
        transfer,
        deposit,
        signature: None,
        expires: now() + 45_000,
        finish_by: now() + 300_000,
        next_transaction: None,
        next_expires: 0,
    })
}
pub(super) fn step(reserve: &Reserve) -> Value {
    json!({"chain":"solana","transaction":reserve.transfer.transaction,"submit":"engine"})
}
pub(super) enum Progress {
    Pending,
    Ready,
    Failed(&'static str),
}
pub(super) async fn progress(
    state: &AppState,
    owner: &str,
    reserve: &Reserve,
) -> Result<Progress, ApiError> {
    let Some(signature) = &reserve.signature else {
        return Ok(Progress::Pending);
    };
    // A provider signature is not settlement. Verify the cash transfer first.
    let status = state
        .solana_mainnet
        .signature_status(signature)
        .await
        .map_err(|_| unavailable())?;
    match status {
        Some(Err(_)) => {
            return Ok(Progress::Failed(
                "The network-fee transfer failed. Your cash is still in your wallet.",
            ))
        }
        None => {
            if state
                .solana_mainnet
                .block_height()
                .await
                .map_err(|_| unavailable())?
                > reserve.transfer.last_valid_block_height
            {
                return Ok(Progress::Failed("The network-fee transfer expired without landing. Your cash is still in your wallet."));
            }
            return Ok(Progress::Pending);
        }
        _ => {}
    }
    let status = match state.markets.gas_near.status(&reserve.deposit, None).await {
        Ok(s) => s,
        Err(_) => return Ok(Progress::Pending),
    };
    match status.status.as_str() {
        "SUCCESS" => {
            if state.solana_mainnet.owner_sol_balance(owner).await.map_err(|_| unavailable())? < FLOOR {
                return Ok(Progress::Failed("Your network-fee reserve arrived but is no longer available. Your unspent cash is still in your wallet."));
            }
            Ok(Progress::Ready)
        },
        "REFUNDED" => Ok(Progress::Failed("The network-fee reserve was refunded to your wallet, less the transfer fees. The purchase was not sent.")),
        "FAILED" => Ok(Progress::Failed("The network-fee reserve could not be completed. Check the transfer in history before trying again; the purchase was not sent.")),
        // Processing, refunding and an unknown outcome remain pending; never collect again.
        _ => Ok(Progress::Pending),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gas_preview_separates_money_kept_from_network_cost() {
        let p = Preview {
            amount: 500_000,
            fee: 300_000,
            gas_value: 487_000,
            min_lamports: 3_990_000,
        };
        assert_eq!(p.cash(), 800_000);
        assert_eq!(p.network_fee(), 313_000);
    }
    #[test]
    fn reserve_requires_the_confirmed_input_and_enough_real_native_output() {
        let mut quote: Quote = serde_json::from_value(json!({"amountIn":"500000","amountOut":"3995537","minAmountOut":"3955581","amountOutUsd":"0.487295692520"})).unwrap();
        // Display values round down at six decimals; transfer amounts do not.
        assert!(checked_quote(&quote, 1_188_500, 500_000).is_ok());
        assert_eq!(
            checked_quote(&quote, 1_188_500, 500_000).unwrap().1,
            487_295
        );
        assert!(checked_quote(&quote, 0, 500_000).is_err());
        assert!(display_micros("-1").is_err());
        assert!(display_micros("1.2e3").is_err());
        quote.amount_in = "500001".into();
        assert!(checked_quote(&quote, 1_188_500, 500_000).is_err());
        quote.amount_in = "500000".into();
        quote.min_amount_out = Some("1000000".into());
        assert!(checked_quote(&quote, 1_188_500, 500_000).is_err());
    }
    #[tokio::test]
    #[ignore]
    async fn live_cash_reserve_unsigned_plan() {
        let solana = engine_execution::solana::SolanaAtaPreflight::new(
            engine_execution::solana::SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let kora = kora::Client::new(kora::PUBLIC_MAINNET, None).unwrap();
        let near = Client::new(std::env::var("NEAR_INTENTS_API_KEY").ok()).unwrap();
        let owner = "6metVveeGpQN6YoXYmevmvtQp7k5CvCgaKBRuQUgevKR";
        let native = solana.owner_sol_balance(owner).await.unwrap();
        let cash = solana
            .owner_mint_balance(owner, engine_execution::solana::MAINNET_USDC_MINT, 6)
            .await
            .unwrap();
        println!("Live reserve wallet read: native={native}, cash={cash}");
        let preview = preview_with(&solana, &kora, &near, owner, true)
            .await
            .unwrap()
            .unwrap();
        println!(
            "Live reserve preview: input={}, fee={}, minimum native={}",
            preview.amount, preview.fee, preview.min_lamports
        );
        let reserve = prepare_with(&solana, &kora, &near, owner, &preview)
            .await
            .unwrap();
        kora::checked(
            &reserve.transfer.transaction,
            &reserve.transfer.transaction,
            owner,
            &reserve.transfer.payer,
            false,
        )
        .unwrap();
        println!("Mainnet reserve: native={} lamports, cash={} USDC units, reserve input={}, provider fee={}, native minimum={}, estimated kept value={} USDC units; fresh 1Click deposit built; exact transfer simulated; operator signature verified; user signatures=0; broadcasts=0",native,cash,preview.amount,preview.fee,preview.min_lamports,preview.gas_value);
    }
}
