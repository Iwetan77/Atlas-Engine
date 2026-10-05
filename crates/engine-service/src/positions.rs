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

// Open spot positions with live value, from the same read as the balance (`app_balance::portfolio`):
// a coin on Home always has its gain or loss, and the two can't disagree. What the wallet still
// holds caps each one: tokens sent away outside Atlas leave the position (with their share of the
// cost).
pub(super) async fn spot(
    State(state): State<AppState>,
    Query(q): Query<SpotQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let currency = q.currency.unwrap_or_else(|| "NGN".into());
    markets::checked_currency(&currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let portfolio = app_balance::portfolio(&state, &headers, &user).await?;
    let positions = rows(&portfolio.holdings, &portfolio.trades, &currency, rate)?;
    let as_of = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis() as u64;
    // Every holding is in the read (a failed lookup keeps its last confirmed amount), so none is
    // ever reported unavailable; the field stays for apps that look for it.
    Ok(Json(
        json!({"positions": positions, "unavailableAssetIds": [], "asOfUnixMs": as_of}),
    ))
}

// One asset, however its id was written: Base addresses in either case, Sui coin types with or
// without leading zeros.
fn asset_key(id: &str) -> String {
    if id.starts_with("base:") {
        return id.to_ascii_lowercase();
    }
    if let Some(coin) = id.strip_prefix("near:sui:") {
        return format!("near:sui:{}", near_intents::sui_coin_key(coin));
    }
    id.to_owned()
}

// What Atlas bought and still tracks of each asset, by `asset_key`.
pub(super) fn open_positions(trades: &[Trade]) -> HashMap<String, Position> {
    let keyed: Vec<Trade> = trades
        .iter()
        .map(|t| Trade {
            asset_id: asset_key(&t.asset_id),
            ..t.clone()
        })
        .collect();
    fold(&keyed)
        .into_iter()
        .filter(|(_, p)| p.units > 0)
        .collect()
}

// A coin in the wallet that Atlas bought: (tracked units held, what went into them, what they're worth).
fn bought_part(
    h: &app_balance::Held,
    open: &HashMap<String, Position>,
) -> Option<(u128, u128, u128)> {
    if h.location != "wallet" || h.kind == "cash" || h.units == 0 {
        return None;
    }
    let p = open.get(&asset_key(&h.asset_id))?;
    let held = h.units.min(p.units);
    Some((
        held,
        mul_div(p.cost, held, p.units),
        mul_div(h.value_usdc, held, h.units),
    ))
}

fn percent(invested: u128, value: u128) -> Option<String> {
    (invested > 0).then(|| {
        format!(
            "{:.2}",
            (value as f64 - invested as f64) / invested as f64 * 100.0
        )
    })
}

// The gain or loss shown on a holding's card on Home.
pub(super) fn pnl_pct(h: &app_balance::Held, open: &HashMap<String, Position>) -> Option<String> {
    let (_, invested, value) = bought_part(h, open)?;
    percent(invested, value)
}

fn rows(
    holdings: &[app_balance::Held],
    trades: &[Trade],
    currency: &str,
    rate: u128,
) -> Result<Vec<Value>, ApiError> {
    let open = open_positions(trades);
    let mut rows = Vec::new();
    for h in holdings {
        let Some((held, invested, value)) = bought_part(h, &open) else {
            continue;
        };
        let p = &open[&asset_key(&h.asset_id)];
        let entry = (invested > 0)
            .then(|| markets::unit_price(invested, held, h.decimals, currency, rate))
            .transpose()?;
        // Per whole token, from what the holding is worth now (`bought_part` saw units > 0).
        let price = markets::unit_price(h.value_usdc, h.units, h.decimals, currency, rate)?;
        rows.push((
            value,
            json!({
                "assetId": h.asset_id,
                "symbol": h.symbol,
                "name": h.name,
                "kind": h.kind,
                "chain": h.chain,
                "iconUrl": h.icon_url,
                "amount": markets::format_units(held, h.decimals),
                "invested": signed_money(invested as i128, currency, rate),
                "value": signed_money(value as i128, currency, rate),
                "pnl": signed_money(value as i128 - invested as i128, currency, rate),
                "pnlPct": percent(invested, value),
                "entryPrice": entry,
                "price": price,
                "realizedPnl": signed_money(p.realized, currency, rate),
                "openedAtUnixMs": p.opened_at_ms,
            }),
        ));
    }
    // Biggest first.
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
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

    fn held(asset_id: &str, units: u128, decimals: u32, value_usdc: u128) -> app_balance::Held {
        app_balance::Held {
            asset_id: asset_id.into(),
            symbol: "X".into(),
            name: "X".into(),
            kind: "crypto".into(),
            chain: "near".into(),
            location: "wallet".into(),
            icon_url: None,
            amount: markets::format_units(units, decimals),
            units,
            decimals,
            value_usdc,
            seen_ms: 0,
        }
    }

    #[test]
    fn positions_come_from_the_balance_read_and_near_prices_in_naira() {
        // The holdings that lost their P&L: 0.452112 wNEAR bought for $2 (24 decimals), DEEP and SUI.
        let mut near = trade(1, "buy", 452_112_000_000_000_000_000_000, 2_000_000);
        near.asset_id = "near:nep141:wrap.near".into();
        let mut deep = trade(2, "buy", 63_146_677, 1_500_000);
        deep.asset_id = "near:sui:0x0deeb::deep::DEEP".into();
        let holdings = [
            held(
                "near:nep141:wrap.near",
                452_112_000_000_000_000_000_000,
                24,
                2_200_000,
            ),
            // The balance spells the coin type without the leading zero; it's the same coin.
            held("near:sui:0xdeeb::deep::DEEP", 63_146_677, 6, 1_485_000),
            // Bought elsewhere: no position, no percentage.
            held("near:nep141:sui.omft.near", 953_939_794, 9, 1_100_000),
        ];
        let rows = rows(&holdings, &[near, deep], "NGN", 1_328_440_000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["assetId"], "near:nep141:wrap.near");
        assert_eq!(rows[0]["pnlPct"], "10.00");
        assert_eq!(rows[0]["amount"], "0.452112");
        assert!(rows[0]["entryPrice"]["amount"]
            .as_str()
            .unwrap()
            .starts_with("5876.59"));
        assert_eq!(rows[1]["pnlPct"], "-1.00");
        let open = open_positions(&[]);
        assert_eq!(pnl_pct(&holdings[2], &open), None);
    }

    #[test]
    fn a_partly_sent_away_holding_counts_only_what_is_left() {
        let mut t = trade(1, "buy", 1_000, 10_000_000);
        t.asset_id = "base:0xABC".into();
        // Half left in the wallet, worth $6: half the cost ($5) went into it.
        let h = held("base:0xabc", 500, 0, 6_000_000);
        let open = open_positions(&[t]);
        assert_eq!(pnl_pct(&h, &open).as_deref(), Some("20.00"));
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

    #[test]
    fn huge_token_units_do_not_overflow() {
        let big = u128::MAX / 3;
        assert!(mul_div(big, big, big) > 0);
    }
}
