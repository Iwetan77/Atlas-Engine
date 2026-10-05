//! Spot positions. Every filled Atlas buy or sell is kept, so a holding has a real entry price,
//! what went in, how long it's been held and its live gain or loss (the meme cards on Home).
//! Cost is average cost: a partial sell takes out its share of the cost, so the entry price of
//! what's left doesn't move.

use super::*;
use axum::extract::Query;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Trade {
    pub(super) intent_id: String,
    pub(super) user_id: String,
    pub(super) asset_id: String,
    // buy | sell
    pub(super) side: String,
    pub(super) token_units: u128,
    pub(super) usdc_units: u128,
    pub(super) tx_id: Option<String>,
    pub(super) filled_at_ms: u64,
}

#[derive(Clone, Default)]
pub(super) struct TradeBook {
    postgres: Option<Arc<tokio_postgres::Client>>,
    // Without DATABASE_URL (local runs) trades live in memory.
    memory: Arc<Mutex<Vec<Trade>>>,
}

impl TradeBook {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut book = Self::default();
        if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("trades database connection ended: {error}");
                }
            });
            // Units are TEXT: token amounts can outgrow BIGINT.
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_trades (
                    intent_id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    asset_id TEXT NOT NULL,
                    side TEXT NOT NULL,
                    token_units TEXT NOT NULL,
                    usdc_units TEXT NOT NULL,
                    tx_id TEXT,
                    filled_at_ms BIGINT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS atlas_trades_user ON atlas_trades (user_id, filled_at_ms)",
                )
                .await?;
            book.postgres = Some(Arc::new(client));
        }
        Ok(book)
    }

    // Recording the same intent twice (two status polls racing) keeps the first.
    pub(super) async fn record(&self, trade: &Trade) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_trades (intent_id,user_id,asset_id,side,token_units,usdc_units,tx_id,filled_at_ms)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (intent_id) DO NOTHING",
                &[
                    &trade.intent_id,
                    &trade.user_id,
                    &trade.asset_id,
                    &trade.side,
                    &trade.token_units.to_string(),
                    &trade.usdc_units.to_string(),
                    &trade.tx_id,
                    &i64::try_from(trade.filled_at_ms).map_err(internal)?,
                ],
            )
            .await
            .map_err(internal)?;
        } else {
            let mut memory = self.memory.lock().map_err(internal)?;
            if !memory.iter().any(|t| t.intent_id == trade.intent_id) {
                memory.push(trade.clone());
            }
        }
        Ok(())
    }

    pub(super) async fn for_user(&self, user_id: &str) -> Result<Vec<Trade>, ApiError> {
        let mut trades = if let Some(pg) = &self.postgres {
            let rows = pg
                .query(
                    "SELECT intent_id,user_id,asset_id,side,token_units,usdc_units,tx_id,filled_at_ms
                     FROM atlas_trades WHERE user_id=$1",
                    &[&user_id],
                )
                .await
                .map_err(internal)?;
            rows.into_iter()
                .map(|row| {
                    Ok(Trade {
                        intent_id: row.get(0),
                        user_id: row.get(1),
                        asset_id: row.get(2),
                        side: row.get(3),
                        token_units: row.get::<_, String>(4).parse().map_err(internal)?,
                        usdc_units: row.get::<_, String>(5).parse().map_err(internal)?,
                        tx_id: row.get(6),
                        filled_at_ms: u64::try_from(row.get::<_, i64>(7)).map_err(internal)?,
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?
        } else {
            self.memory
                .lock()
                .map_err(internal)?
                .iter()
                .filter(|t| t.user_id == user_id)
                .cloned()
                .collect()
        };
        trades.sort_by(|a, b| {
            a.filled_at_ms
                .cmp(&b.filled_at_ms)
                .then(a.intent_id.cmp(&b.intent_id))
        });
        Ok(trades)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Position {
    pub(super) units: u128,
    // USDC base units (6 decimals) still invested in `units`.
    pub(super) cost: u128,
    // Gain or loss already taken by selling, in USDC base units.
    pub(super) realized: i128,
    // When the current run of holding began (a full sell ends it).
    pub(super) opened_at_ms: u64,
}

// Replays trades oldest first into one position per asset.
pub(super) fn fold(trades: &[Trade]) -> HashMap<String, Position> {
    let mut positions: HashMap<String, Position> = HashMap::new();
    for t in trades {
        let p = positions.entry(t.asset_id.clone()).or_default();
        if t.side == "buy" {
            if p.units == 0 {
                p.cost = 0;
                p.opened_at_ms = t.filled_at_ms;
            }
            p.units = p.units.saturating_add(t.token_units);
            p.cost = p.cost.saturating_add(t.usdc_units);
        } else if p.units > 0 && t.token_units > 0 {
            // Tokens that came from outside Atlas have no cost here; only the tracked part counts.
            let sold = t.token_units.min(p.units);
            let removed = mul_div(p.cost, sold, p.units);
            let proceeds = mul_div(t.usdc_units, sold, t.token_units);
            p.realized += proceeds as i128 - removed as i128;
            p.cost -= removed;
            p.units -= sold;
        }
    }
    positions
}

// a * b / c without overflowing on large token units.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    match a.checked_mul(b) {
        Some(n) => n / c,
        None => (a as f64 * b as f64 / c as f64) as u128,
    }
}

// Signed money in the display currency, from USDC base units.
fn signed_money(usdc: i128, currency: &str, rate: u128) -> Value {
    let scaled = usdc.unsigned_abs().saturating_mul(rate) / 1_000_000;
    let sign = if usdc < 0 && scaled > 0 { "-" } else { "" };
    json!({"amount": format!("{sign}{}", markets::format_units(scaled, 6)), "currency": currency})
}

#[derive(Deserialize)]
pub(super) struct SpotQuery {
    currency: Option<String>,
}

// Open spot positions with live value. What the wallet still holds caps each one: tokens sent
// away outside Atlas leave the position (with their share of the cost).
pub(super) async fn spot(
    State(state): State<AppState>,
    Query(q): Query<SpotQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let currency = q.currency.unwrap_or_else(|| "NGN".into());
    markets::checked_currency(&currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let trades = state.trades.for_user(&user.user_id).await?;
    let wallets = Wallets {
        solana: user.solana_wallet.as_deref().filter(|w| !w.is_empty()),
        evm: user.evm_wallet.as_deref().filter(|w| !w.is_empty()),
    };
    let mut positions = valued(
        &state.markets,
        &state.solana_mainnet,
        wallets,
        &trades,
        &currency,
        rate,
    )
    .await?;
    positions.extend(near_valued(&state, &headers, &user, &trades, &currency, rate).await?);
    positions.sort_by(|a, b| {
        let v = |x: &Value| {
            x["value"]["amount"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        v(b).total_cmp(&v(a))
    });
    let as_of = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis() as u64;
    Ok(Json(json!({"positions": positions, "asOfUnixMs": as_of})))
}

#[derive(Clone, Copy)]
struct Wallets<'a> {
    solana: Option<&'a str>,
    evm: Option<&'a str>,
}

async fn valued(
    markets: &markets::MarketState,
    solana: &SolanaAtaPreflight,
    wallets: Wallets<'_>,
    trades: &[Trade],
    currency: &str,
    rate: u128,
) -> Result<Vec<Value>, ApiError> {
    let open: Vec<(String, Position)> = fold(trades)
        .into_iter()
        .filter(|(_, p)| p.units > 0)
        .collect();
    let mut assets = Vec::new();
    for (asset_id, position) in open {
        // A token Atlas can no longer find or price has no live value to show.
        if let Ok(asset) = markets::find_asset(markets, &asset_id).await {
            assets.push((asset, position));
        }
    }
    // One read of the Solana wallet covers every Solana position; if it fails the tracked units stand.
    let solana_held = match wallets.solana {
        Some(owner) if assets.iter().any(|(a, _)| a.chain == "solana") => {
            let (tokens, native) = tokio::join!(
                solana.owner_token_balances(owner),
                solana.owner_sol_balance(owner),
            );
            tokens.ok().map(|mut held| {
                if let Ok(native) = native {
                    match held.iter_mut().find(|(m, _, _)| m == markets::SOL_MINT) {
                        Some(entry) => entry.1 = entry.1.saturating_add(native),
                        None => held.push((markets::SOL_MINT.into(), native, 9)),
                    }
                }
                held
            })
        }
        _ => None,
    };
    let mints: Vec<String> = assets
        .iter()
        .filter(|(a, _)| a.chain == "solana")
        .map(|(a, _)| a.token.clone())
        .collect();
    let prices = markets::usd_prices(markets, &mints).await?;
    let mut result = Vec::new();
    for (asset, p) in &assets {
        let on_chain = if asset.chain == "solana" {
            solana_held.as_ref().map(|held| {
                held.iter()
                    .filter(|(m, _, _)| *m == asset.token)
                    .map(|(_, units, _)| *units)
                    .sum::<u128>()
            })
        } else {
            match wallets.evm {
                Some(wallet) => markets.base.balance_of(&asset.token, wallet).await.ok(),
                None => None,
            }
        };
        let held = on_chain.map_or(p.units, |units| units.min(p.units));
        if held == 0 {
            continue;
        }
        let invested = mul_div(p.cost, held, p.units);
        let (price, value) = if asset.chain == "solana" {
            let Some((usd, _)) = prices.get(&asset.token) else {
                continue;
            };
            (
                markets::money_from_usd(*usd, currency, rate)?,
                (held as f64 / 10f64.powi(asset.decimals as i32) * usd * 1_000_000.0) as u128,
            )
        } else {
            let Some(per_dollar) = markets::base_rate(markets, asset).await else {
                continue;
            };
            (
                json!(markets::unit_price(
                    1_000_000,
                    per_dollar,
                    asset.decimals,
                    currency,
                    rate
                )?),
                mul_div(held, 1_000_000, per_dollar),
            )
        };
        let pnl = value as i128 - invested as i128;
        let pnl_pct = if invested > 0 {
            Some(format!("{:.2}", pnl as f64 / invested as f64 * 100.0))
        } else {
            None
        };
        let entry = if held > 0 && invested > 0 {
            Some(json!(markets::unit_price(
                invested,
                held,
                asset.decimals,
                currency,
                rate
            )?))
        } else {
            None
        };
        result.push(json!({
            "assetId": asset.id,
            "symbol": asset.symbol,
            "name": asset.name,
            "kind": asset.kind,
            "chain": asset.chain,
            "iconUrl": asset.icon_url,
            "amount": markets::format_units(held, asset.decimals),
            "invested": signed_money(invested as i128, currency, rate),
            "value": signed_money(value as i128, currency, rate),
            "pnl": signed_money(pnl, currency, rate),
            "pnlPct": pnl_pct,
            "entryPrice": entry,
            "price": price,
            "realizedPnl": signed_money(p.realized, currency, rate),
            "openedAtUnixMs": p.opened_at_ms,
        }));
    }
    // Biggest first.
    result.sort_by(|a, b| {
        let v = |x: &Value| {
            x["value"]["amount"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        v(b).total_cmp(&v(a))
    });
    Ok(result)
}

// Positions in NEAR Intents coins (NEAR and Monad coins, Ref coins): priced from 1Click's list or
// Ref, capped by what the wallet holds now, in the same shape as the rest.
async fn near_valued(
    state: &AppState,
    headers: &HeaderMap,
    user: &app_balance::VerifiedWallets,
    trades: &[Trade],
    currency: &str,
    rate: u128,
) -> Result<Vec<Value>, ApiError> {
    let mut result = Vec::new();
    for (asset_id, p) in fold(trades) {
        if p.units == 0 || !asset_id.starts_with("near:") {
            continue;
        }
        let coin = checked_position_coin(
            near_intents::position_coin(state, headers, user, &asset_id).await,
        )?;
        let held = coin.held.min(p.units);
        if held == 0 {
            continue;
        }
        let invested = mul_div(p.cost, held, p.units);
        let value = (held as f64 / 10f64.powi(coin.decimals as i32) * coin.price * 1e6) as u128;
        let pnl = value as i128 - invested as i128;
        let pnl_pct =
            (invested > 0).then(|| format!("{:.2}", pnl as f64 / invested as f64 * 100.0));
        let entry = (invested > 0)
            .then(|| markets::unit_price(invested, held, coin.decimals, currency, rate))
            .transpose()?;
        result.push(json!({
            "assetId": asset_id,
            "symbol": coin.symbol,
            "name": coin.name,
            "kind": "crypto",
            "chain": coin.chain,
            "iconUrl": coin.icon,
            "amount": markets::format_units(held, coin.decimals),
            "invested": signed_money(invested as i128, currency, rate),
            "value": signed_money(value as i128, currency, rate),
            "pnl": signed_money(pnl, currency, rate),
            "pnlPct": pnl_pct,
            "entryPrice": entry,
            "price": markets::money_from_usd(coin.price, currency, rate)?,
            "realizedPnl": signed_money(p.realized, currency, rate),
            "openedAtUnixMs": p.opened_at_ms,
        }));
    }
    Ok(result)
}

// Missing data is a failed refresh, not proof that a position was sold.
fn checked_position_coin(
    coin: Option<near_intents::NearCoin>,
) -> Result<near_intents::NearCoin, ApiError> {
    coin.filter(|c| c.price.is_finite() && c.price > 0.0)
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "Your profit and loss couldn't be updated. Try again in a moment.".into(),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(n: u64, side: &str, tokens: u128, usdc: u128) -> Trade {
        Trade {
            intent_id: format!("intent-{n}"),
            user_id: "did:privy:a".into(),
            asset_id: "bonk".into(),
            side: side.into(),
            token_units: tokens,
            usdc_units: usdc,
            tx_id: None,
            filled_at_ms: n,
        }
    }

    #[test]
    fn an_unavailable_position_is_not_reported_as_sold() {
        assert_eq!(
            checked_position_coin(None).err().unwrap().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let coin = |held, price| near_intents::NearCoin {
            symbol: "DEEP".into(),
            name: "DeepBook Token".into(),
            chain: "sui".into(),
            icon: None,
            decimals: 6,
            price,
            held,
        };
        assert_eq!(checked_position_coin(Some(coin(0, 0.03))).unwrap().held, 0);
        for price in [0.0, f64::NAN, f64::INFINITY] {
            assert!(checked_position_coin(Some(coin(10, price))).is_err());
        }
    }

    #[test]
    fn buys_average_and_a_partial_sell_keeps_the_entry_price() {
        let trades = [
            trade(1, "buy", 1_000, 10_000_000),
            trade(2, "buy", 1_000, 30_000_000),
            // Sell a quarter for $15: its cost was $10, so $5 is taken as gain.
            trade(3, "sell", 500, 15_000_000),
        ];
        let p = &fold(&trades)["bonk"];
        assert_eq!(p.units, 1_500);
        assert_eq!(p.cost, 30_000_000);
        assert_eq!(p.realized, 5_000_000);
        assert_eq!(p.opened_at_ms, 1);
        // Entry per token is unchanged: $40/2000 before, $30/1500 after.
        assert_eq!(p.cost * 2_000, 40_000_000 * p.units);
    }

    #[test]
    fn a_full_sell_ends_the_run_and_a_new_buy_starts_fresh() {
        let trades = [
            trade(1, "buy", 1_000, 10_000_000),
            trade(2, "sell", 1_000, 4_000_000),
            trade(3, "buy", 200, 1_000_000),
        ];
        let p = &fold(&trades)["bonk"];
        assert_eq!(p.units, 200);
        assert_eq!(p.cost, 1_000_000);
        assert_eq!(p.realized, -6_000_000);
        assert_eq!(p.opened_at_ms, 3);
    }

    #[test]
    fn selling_more_than_atlas_bought_only_counts_the_tracked_part() {
        // 1,000 bought here, 1,000 more arrived from elsewhere, all 2,000 sold for $30.
        let trades = [
            trade(1, "buy", 1_000, 10_000_000),
            trade(2, "sell", 2_000, 30_000_000),
        ];
        let p = &fold(&trades)["bonk"];
        assert_eq!(p.units, 0);
        assert_eq!(p.cost, 0);
        assert_eq!(p.realized, 5_000_000);
    }

    #[test]
    fn a_sell_with_nothing_tracked_is_ignored() {
        let p = &fold(&[trade(1, "sell", 1_000, 10_000_000)])["bonk"];
        assert_eq!(*p, Position::default());
    }

    #[test]
    fn money_keeps_its_sign() {
        assert_eq!(
            signed_money(-1_500_000, "USD", 1_000_000),
            json!({"amount":"-1.5","currency":"USD"})
        );
        assert_eq!(
            signed_money(2_000_000, "NGN", 1_500_000_000),
            json!({"amount":"3000","currency":"NGN"})
        );
        assert_eq!(
            signed_money(0, "USD", 1_000_000),
            json!({"amount":"0","currency":"USD"})
        );
    }

    // Real catalog, prices and Base route: a BONK and a BRETT position value without errors.
    #[tokio::test]
    #[ignore]
    async fn live_spot_positions() {
        let markets = markets::MarketState::new().unwrap();
        let solana = SolanaAtaPreflight::new(
            SolanaNetwork::Mainnet,
            "https://api.mainnet-beta.solana.com",
            "",
        )
        .unwrap();
        let catalog = markets::catalog(&markets).await.unwrap();
        let bonk = catalog
            .iter()
            .find(|a| a.symbol == "Bonk" || a.symbol == "BONK")
            .unwrap();
        let brett = catalog.iter().find(|a| a.symbol == "BRETT").unwrap();
        let mut trades = vec![
            trade(1, "buy", 10u128.pow(bonk.decimals) * 1_000_000, 20_000_000),
            trade(2, "sell", 10u128.pow(bonk.decimals) * 250_000, 6_000_000),
        ];
        let mut b = trade(3, "buy", 10u128.pow(brett.decimals) * 100, 3_000_000);
        b.asset_id = brett.id.clone();
        trades.push(b);
        for t in trades.iter_mut().take(2) {
            t.asset_id = bonk.id.clone();
        }
        let wallets = Wallets {
            solana: None,
            evm: None,
        };
        let rows = valued(&markets, &solana, wallets, &trades, "NGN", 1_500_000_000)
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        assert_eq!(rows.len(), 2);
        let bonk_row = rows
            .iter()
            .find(|r| r["assetId"] == bonk.id.as_str())
            .unwrap();
        assert_eq!(bonk_row["amount"], "750000");
        assert_eq!(bonk_row["invested"]["amount"], "22500");
        assert_eq!(bonk_row["realizedPnl"]["amount"], "1500");
        assert!(bonk_row["pnlPct"].as_str().is_some());
    }

    #[test]
    fn huge_token_units_do_not_overflow() {
        let big = u128::MAX / 3;
        assert!(mul_div(big, big, big) > 0);
    }
}
