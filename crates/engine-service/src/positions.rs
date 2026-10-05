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

// An unavailable asset is not a sold asset. Other positions can still refresh.
#[derive(Default)]
struct Refresh {
    positions: Vec<Value>,
    unavailable: Vec<String>,
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
    // A slow Solana venue must not delay the Sui/NEAR cards, or vice versa.
    let (other, near) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(15),
            valued(
                &state.markets,
                &state.solana_mainnet,
                wallets,
                &trades,
                &currency,
                rate
            ),
        ),
        near_valued(&state, &headers, &user, &trades, &currency, rate),
    );
    let mut refreshed = match other {
        Ok(Ok(refreshed)) => refreshed,
        _ => Refresh {
            positions: vec![],
            unavailable: fold(&trades)
                .into_iter()
                .filter(|(id, p)| p.units > 0 && !id.starts_with("near:"))
                .map(|(id, _)| id)
                .collect(),
        },
    };
    let near = near?;
    refreshed.unavailable.extend(near.unavailable);
    let mut positions = refreshed.positions;
    positions.extend(near.positions);
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
    Ok(Json(
        json!({"positions": positions, "unavailableAssetIds": refreshed.unavailable, "asOfUnixMs": as_of}),
    ))
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
) -> Result<Refresh, ApiError> {
    let open: Vec<(String, Position)> = fold(trades)
        .into_iter()
        .filter(|(id, p)| p.units > 0 && !id.starts_with("near:"))
        .collect();
    let mut assets = Vec::new();
    let mut unavailable = Vec::new();
    for (asset_id, position) in open {
        // A token Atlas can no longer find or price has no live value to show.
        if let Ok(asset) = markets::find_asset(markets, &asset_id).await {
            assets.push((asset, position));
        } else {
            unavailable.push(asset_id);
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
                unavailable.push(asset.id.clone());
                continue;
            };
            (
                markets::money_from_usd(*usd, currency, rate)?,
                (held as f64 / 10f64.powi(asset.decimals as i32) * usd * 1_000_000.0) as u128,
            )
        } else {
            let Some(per_dollar) = markets::base_rate(markets, asset).await else {
                unavailable.push(asset.id.clone());
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
    Ok(Refresh {
        positions: result,
        unavailable,
    })
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
) -> Result<Refresh, ApiError> {
    let mut reads = tokio::task::JoinSet::new();
    for (asset_id, p) in fold(trades) {
        if p.units == 0 || !asset_id.starts_with("near:") {
            continue;
        }
        let (state, headers, user) = (state.clone(), headers.clone(), user.clone());
        reads.spawn(async move {
            let coin = tokio::time::timeout(
                Duration::from_secs(15),
                near_intents::position_coin(&state, &headers, &user, &asset_id),
            )
            .await
            .ok()
            .flatten();
            (asset_id, p, coin)
        });
    }
    let mut refreshed = Refresh::default();
    while let Some(read) = reads.join_next().await {
        let (asset_id, p, coin) = read.map_err(internal)?;
        append_near_position(&mut refreshed, asset_id, p, coin, currency, rate)?;
    }
    Ok(refreshed)
}

fn append_near_position(
    refreshed: &mut Refresh,
    asset_id: String,
    p: Position,
    coin: Option<near_intents::NearCoin>,
    currency: &str,
    rate: u128,
) -> Result<(), ApiError> {
    let Some(coin) = coin else {
        refreshed.unavailable.push(asset_id);
        return Ok(());
    };
    let held = coin.held.min(p.units);
    if held == 0 {
        return Ok(());
    }
    if !coin.price.is_finite() || coin.price <= 0.0 {
        refreshed.unavailable.push(asset_id);
        return Ok(());
    }
    let invested = mul_div(p.cost, held, p.units);
    let value = (held as f64 / 10f64.powi(coin.decimals as i32) * coin.price * 1e6) as u128;
    let pnl = value as i128 - invested as i128;
    let pnl_pct = (invested > 0).then(|| format!("{:.2}", pnl as f64 / invested as f64 * 100.0));
    let entry = (invested > 0)
        .then(|| markets::unit_price(invested, held, coin.decimals, currency, rate))
        .transpose()?;
    refreshed.positions.push(json!({
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

    Ok(())
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
    fn one_failed_coin_keeps_other_pnl_and_a_zero_balance_removes_only_that_coin() {
        let mut refreshed = Refresh::default();
        let position = Position {
            units: 10_000_000,
            cost: 1_000_000,
            opened_at_ms: 1,
            ..Position::default()
        };
        let coin = |held, price| near_intents::NearCoin {
            symbol: "DEEP".into(),
            name: "DeepBook Token".into(),
            chain: "sui".into(),
            icon: None,
            decimals: 6,
            price,
            held,
        };
        append_near_position(
            &mut refreshed,
            "near:unavailable".into(),
            position.clone(),
            None,
            "USD",
            1_000_000,
        )
        .unwrap();
        append_near_position(
            &mut refreshed,
            "near:deep".into(),
            position.clone(),
            Some(coin(5_000_000, 0.12)),
            "USD",
            1_000_000,
        )
        .unwrap();
        assert_eq!(refreshed.positions.len(), 1);
        let row = &refreshed.positions[0];
        assert_eq!(row["amount"], "5");
        assert_eq!(row["invested"]["amount"], "0.5");
        assert_eq!(row["value"]["amount"], "0.6");
        assert_eq!(row["pnlPct"], "20.00");
        assert_eq!(refreshed.unavailable, ["near:unavailable"]);
        append_near_position(
            &mut refreshed,
            "near:sold".into(),
            position.clone(),
            Some(coin(0, 0.0)),
            "USD",
            1_000_000,
        )
        .unwrap();
        assert_eq!(refreshed.unavailable, ["near:unavailable"]);
        for price in [0.0, f64::NAN, f64::INFINITY] {
            append_near_position(
                &mut refreshed,
                "near:no-price".into(),
                position.clone(),
                Some(coin(10, price)),
                "USD",
                1_000_000,
            )
            .unwrap();
            assert_eq!(refreshed.positions.len(), 1);
            assert_eq!(refreshed.unavailable.last().unwrap(), "near:no-price");
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
            .unwrap()
            .positions;
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

    // Read-only mainnet check. The loopback bridge supplies fixture authentication and verified
    // addresses, but /sui/balance still uses the production SDK. No transaction is built or sent.
    // Trades below are test cost bases, never records from the user's database.
    #[tokio::test]
    #[ignore]
    async fn live_sui_and_near_pnl_survives_an_unavailable_ref_coin() {
        let bridge_url =
            env::var("ATLAS_PNL_READONLY_BRIDGE").expect("read-only loopback bridge required");
        let user_id = env::var("ATLAS_PNL_READONLY_USER").expect("fixture user required");
        let url = reqwest::Url::parse(&bridge_url).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.scheme(), "http");
        let state = AppState {
            user_id: user_id.clone(),
            base_wallet: String::new(),
            solana_owner: String::new(),
            solana_mainnet: SolanaAtaPreflight::new(
                SolanaNetwork::Mainnet,
                "https://api.mainnet-beta.solana.com",
                "",
            )
            .unwrap(),
            auth: AuthMode::Privy {
                bridge_url,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .unwrap(),
            },
            markets: markets::MarketState::new().unwrap(),
            near: near_intents::NearState::new().await.unwrap(),
            layerswap: engine_execution::layerswap::LayerswapClient::new().unwrap(),
            relay_link: engine_execution::relay_link::RelayClient::new(None).unwrap(),
            cow: engine_execution::cow::CowClient::new().unwrap(),
            links: cashlinks::LinkStore::new().await.unwrap(),
            comments: comments::CommentStore::new().await.unwrap(),
            emails: emails::EmailState::new().await.unwrap(),
            hl: hl::HlState::new().await.unwrap(),
            earn: earn::EarnState::default(),
            social: social::SocialState::new().await.unwrap(),
            trades: TradeBook::default(),
            history: transactions::HistoryStore::new().await.unwrap(),
            daya: daya::DayaState::new().await.unwrap(),
            predictions: predictions::PredictionState::new().await.unwrap(),
            pin: pin::PinState::new().await.unwrap(),
        };
        for (n, id, units, cost) in [
            (1, "near:sui:0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP", 63_146_677, 1_000_000),
            (2, "near:nep141:sui.omft.near", 953_939_794, 1_000_000),
            (3, "near:nep141:wrap.near", 452_112_000_000_000_000_000_000, 2_000_000),
            (4, "near:ref:unavailable.near", 1, 1),
        ] {
            let mut t = trade(n, "buy", units, cost);
            t.asset_id = id.into(); t.user_id = user_id.clone();
            state.trades.record(&t).await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/v1/positions/spot", axum::routing::get(spot))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let started = std::time::Instant::now();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/v1/positions/spot?currency=USD"))
            .bearer_auth("pnl-read-only")
            .timeout(Duration::from_secs(22))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        server.abort();
        let rows = body["positions"].as_array().unwrap();
        println!(
            "mainnet P&L HTTP 200 in {}ms (fixture cost bases): {}",
            started.elapsed().as_millis(),
            body
        );
        assert_eq!(
            rows.len(),
            3,
            "A failed Ref lookup must not hide the three healthy mainnet positions"
        );
        assert_eq!(
            body["unavailableAssetIds"],
            json!(["near:ref:unavailable.near"])
        );
        for (symbol, amount) in [
            ("DEEP", "63.146677"),
            ("SUI", "0.953939794"),
            ("wNEAR", "0.452112"),
        ] {
            let row = rows.iter().find(|row| row["symbol"] == symbol).unwrap();
            assert_eq!(row["amount"], amount);
            assert!(row["pnlPct"]
                .as_str()
                .unwrap()
                .parse::<f64>()
                .unwrap()
                .is_finite());
            assert!(
                row["price"]["amount"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
                    > 0.0
            );
        }
    }

    #[test]
    fn huge_token_units_do_not_overflow() {
        let big = u128::MAX / 3;
        assert!(mul_div(big, big, big) > 0);
    }
}
