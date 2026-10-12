//! The two balances behind the one on Home. Spendable is USDC in the user's Base wallet: what they
//! send, cash out and save. Trading is USDC in their Solana wallet: what trades, perps margin and
//! predictions are paid from. Moving between them is the user's own cash going from one of their
//! wallets to the other through Relay, a few cents and a few seconds, one confirm.
use super::*;
use engine_execution::swaps::uniswap::BASE_USDC;
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_MOVE_ID: AtomicU64 = AtomicU64::new(1);
const QUOTE_MS: u64 = 30_000;
// A cent of room under everything in the balance, so a Max move's next quote still fits.
const ROUNDING_UNITS: u128 = 10_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Direction {
    ToTrading,
    ToSpendable,
}

impl Direction {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "to_trading" => Some(Self::ToTrading),
            "to_spendable" => Some(Self::ToSpendable),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::ToTrading => "to_trading",
            Self::ToSpendable => "to_spendable",
        }
    }
    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::ToTrading => ("Spendable balance", "Trading balance"),
            Self::ToSpendable => ("Trading balance", "Spendable balance"),
        }
    }
}

#[derive(Clone)]
struct MoveQuote {
    owner: String,
    evm: String,
    solana: String,
    direction: Direction,
    currency: String,
    rate: u128,
    // What arrives in the other balance, and what leaves on top of it (Relay, any SOL top-up).
    land: u128,
    fee: u128,
    // Spendable → Trading: the exact gasless move they reviewed.
    gasless: Option<engine_execution::relay_link::GaslessMove>,
    expires: u64,
    plan: Option<Value>,
    preparation: Arc<tokio::sync::Mutex<()>>,
}

static QUOTES: std::sync::LazyLock<Mutex<HashMap<String, MoveQuote>>> =
    std::sync::LazyLock::new(Default::default);

#[derive(Deserialize)]
pub(super) struct MoveRequest {
    direction: String,
    amount: markets::Money,
    // Everything in the balance it leaves; `amount` is then only what it's worth.
    #[serde(default)]
    all: bool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn conflict(message: impl Into<String>) -> ApiError {
    (StatusCode::CONFLICT, message.into())
}
fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn not_moving() -> ApiError {
    markets::unavailable("Couldn't move your cash right now; try again shortly")
}
fn money(units: u128, currency: &str, rate: u128) -> Value {
    json!({"amount":markets::format_units(units.saturating_mul(rate) / 1_000_000, 6),"currency":currency})
}

// The user's own two wallets, as the server knows them. A move never goes anywhere else.
fn wallets(user: &app_balance::VerifiedWallets) -> Result<(String, String), ApiError> {
    let evm = user
        .evm_wallet
        .clone()
        .filter(|w| markets::looks_like_evm_address(w))
        .ok_or_else(|| conflict("Your wallet isn't ready yet. Try again in a moment."))?;
    let solana = user
        .solana_wallet
        .clone()
        .filter(|w| markets::looks_like_mint(w))
        .ok_or_else(|| conflict("Your wallet isn't ready yet. Try again in a moment."))?;
    Ok((evm, solana))
}

// Moving everything out of Trading: what can land once Relay's fee and any SOL top-up come out of
// it. Relay's Solana fee is a few cents and barely moves with the amount, so one more quote settles it.
async fn all_from_trading(
    state: &AppState,
    solana: &str,
    evm: &str,
    cash: u128,
) -> Result<(u128, u128), ApiError> {
    let mut land = cash;
    for _ in 0..3 {
        let (fee, topup) = markets::move_to_spendable_cost(state, solana, evm, land).await?;
        let cost = markets::fee_room(fee).saturating_add(topup);
        if land.saturating_add(cost) <= cash {
            return Ok((land, cost));
        }
        land = cash.saturating_sub(cost).saturating_sub(ROUNDING_UNITS);
        if land == 0 {
            break;
        }
    }
    Err(conflict(
        "There isn't enough in your trading balance to cover the move's fee.",
    ))
}

// POST /v1/moves/quote {direction: to_trading|to_spendable, amount, all?} → what arrives, the fee on
// top, and the total that leaves.
pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MoveRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let direction = Direction::parse(&req.direction)
        .ok_or_else(|| bad("direction must be to_trading or to_spendable"))?;
    let (evm, solana) = wallets(&user)?;
    let currency = req.amount.currency.clone();
    markets::checked_currency(&currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let typed = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    let (land, fee, gasless, available) = match direction {
        Direction::ToTrading => {
            let cash = state
                .markets
                .base
                .balance_of(BASE_USDC, &evm)
                .await
                .map_err(markets::unavailable)?;
            let moved = if req.all {
                if cash == 0 {
                    return Err(conflict(
                        "Your spendable balance is empty. Add money first.",
                    ));
                }
                state
                    .relay_link
                    .base_to_solana_all(&evm, &solana, cash)
                    .await
            } else {
                markets::check_limits(typed, &currency, rate)?;
                state.relay_link.base_to_solana(&evm, &solana, typed).await
            }
            .map_err(|error| {
                eprintln!("move to trading: Relay didn't quote ({error})");
                not_moving()
            })?;
            if moved.amount_in_units > cash {
                return Err(conflict(format!(
                    "That's more than you can move with the fee. You have {} spendable; tap Max to move it all.",
                    markets::say_money(cash, &currency, rate)
                )));
            }
            let land = moved.amount_out_units;
            let fee = moved.amount_in_units.saturating_sub(land);
            (land, fee, Some(moved), cash)
        }
        Direction::ToSpendable => {
            let cash = markets::solana_cash(&state, &solana).await;
            let (land, fee) = if req.all {
                if cash == 0 {
                    return Err(conflict("Your trading balance is empty."));
                }
                all_from_trading(&state, &solana, &evm, cash).await?
            } else {
                markets::check_limits(typed, &currency, rate)?;
                let (fee, topup) =
                    markets::move_to_spendable_cost(&state, &solana, &evm, typed).await?;
                (typed, markets::fee_room(fee).saturating_add(topup))
            };
            if land.saturating_add(fee) > cash {
                return Err(conflict(format!(
                    "That's more than you can move with the fee. You have {} in your trading balance; tap Max to move it all.",
                    markets::say_money(cash, &currency, rate)
                )));
            }
            (land, fee, None, cash)
        }
    };
    if land == 0 {
        return Err(conflict("That's too small to move."));
    }
    let quote_id = format!(
        "move-{:x}-{:x}",
        now(),
        NEXT_MOVE_ID.fetch_add(1, Ordering::Relaxed)
    );
    let expires = now() + QUOTE_MS;
    let mut quotes = QUOTES.lock().map_err(internal)?;
    quotes.retain(|_, q| q.expires + 10 * 60_000 > now());
    quotes.insert(
        quote_id.clone(),
        MoveQuote {
            owner: user.user_id,
            evm,
            solana,
            direction,
            currency: currency.clone(),
            rate,
            land,
            fee,
            gasless,
            expires,
            plan: None,
            preparation: Arc::new(tokio::sync::Mutex::new(())),
        },
    );
    let (from, to) = direction.labels();
    Ok(Json(json!({
        "quoteId":quote_id,"direction":direction.name(),"from":from,"to":to,
        "receive":money(land,&currency,rate),"fee":money(fee,&currency,rate),
        "send":money(land + fee,&currency,rate),"available":money(available,&currency,rate),
        "eta":"A few seconds","expiresAtUnixMs":expires
    })))
}

// POST /v1/moves/quotes/{id}/execute → the plan for the move they reviewed (PIN-gated, like every
// /execute). A repeat returns the same plan.
pub(super) async fn execute(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let quote = QUOTES
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "move quote not found".into()))?;
    if quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "move quote belongs to another user".into(),
        ));
    }
    let _preparing = quote.preparation.lock().await;
    // Asked twice at once: the first plan stands.
    if let Some(plan) = QUOTES
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .and_then(|q| q.plan.clone())
    {
        return Ok(Json(plan));
    }
    if quote.expires < now() {
        return Err(conflict("This quote expired. Check the new amount."));
    }
    if wallets(&user)? != (quote.evm.clone(), quote.solana.clone()) {
        return Err(conflict("Your wallet changed. Check a fresh quote."));
    }
    let (rate, currency) = (quote.rate, quote.currency.as_str());
    let (intent_id, transactions, fee) = match quote.direction {
        Direction::ToTrading => {
            let moved = quote.gasless.clone().ok_or_else(not_moving)?;
            let (intent_id, steps) = markets::plan_relay_move(
                &state,
                user.user_id.clone(),
                quote.solana.clone(),
                "solana",
                moved,
            )
            .await?;
            (intent_id, steps, quote.fee)
        }
        Direction::ToSpendable => {
            // Never more than the fee they reviewed (it has room for Relay's price moving a hair).
            // The confirmation and receipt show the exact fee that leaves.
            markets::plan_move_to_spendable(
                &state,
                user.user_id.clone(),
                quote.solana.clone(),
                &quote.evm,
                quote.land,
                quote.fee,
            )
            .await?
        }
    };
    let (from, to) = quote.direction.labels();
    let summary = vec![
        json!({"label":"You move","value":markets::say_money(quote.land + fee, currency, rate)}),
        json!({"label":"From","value":from}),
        json!({"label":"Into","value":to}),
        json!({"label":"Arrives","value":markets::say_money(quote.land, currency, rate)}),
        json!({"label":"Fee","value":markets::say_money(fee, currency, rate)}),
        json!({"label":"Time","value":"A few seconds"}),
    ];
    let plan = json!({"intentId":intent_id,"kind":"move","summary":summary,
        "transactions":transactions,"expiresAtUnixMs":now() + 120_000});
    let mut receipt = transactions::Receipt::plan(&user.user_id, &plan);
    receipt.kind = format!("move_{}", quote.direction.name());
    receipt.title = transactions::title(&receipt.kind, "");
    receipt.set_usdc(quote.land + fee);
    state.history.put(&receipt).await?;
    if let Some(stored) = QUOTES.lock().map_err(internal)?.get_mut(&quote_id) {
        stored.plan = Some(plan.clone());
    }
    Ok(Json(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_round_trip_and_name_both_balances() {
        for d in [Direction::ToTrading, Direction::ToSpendable] {
            assert_eq!(Direction::parse(d.name()), Some(d));
        }
        assert_eq!(Direction::parse("to_someone_else"), None);
        assert_eq!(
            Direction::ToTrading.labels(),
            ("Spendable balance", "Trading balance")
        );
        assert_eq!(
            Direction::ToSpendable.labels(),
            ("Trading balance", "Spendable balance")
        );
    }

    #[test]
    fn money_is_in_the_users_currency() {
        // $1.50 at ₦1,500 per dollar.
        assert_eq!(
            money(1_500_000, "NGN", 1_500_000_000),
            json!({"amount":"2250","currency":"NGN"})
        );
    }
}
