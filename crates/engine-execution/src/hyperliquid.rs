//! Hyperliquid reads: the perps markets, an account's value and positions, its fills, candles, and
//! which agents it has approved. Trading itself is signed in the Privy bridge (hyperliquid.mjs).
//! An account is the user's own EVM address. No API key. Besides Hyperliquid's own perps, it lists
//! the `xyz` dex (stocks, commodities, indices, currencies), which keeps its own margin per account.
use reqwest::{Client, Url};
use serde_json::{json, Value};
use std::time::Duration;

const INFO: &str = "https://api.hyperliquid.xyz/info";
// Builder-deployed dexes Atlas lists, with their place in Hyperliquid's `perpDexs` list: their asset
// ids are 100000 + place × 10000 + index (live check: `live_hyperliquid_markets`).
pub const DEXES: [(&str, u32); 1] = [("xyz", 1)];

#[derive(Debug, thiserror::Error)]
pub enum HyperliquidError {
    #[error("Hyperliquid request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Hyperliquid returned an unexpected answer: {0}")]
    InvalidResponse(&'static str),
}

#[derive(Clone)]
pub struct HyperliquidClient {
    http: Client,
    info: Url,
}

/// One perps market right now. `asset` is the id orders use; `coin` names it ("BTC", "xyz:TSLA").
#[derive(Clone, Debug, PartialEq)]
pub struct Market {
    pub coin: String,
    // "" for Hyperliquid's own perps, else the dex holding this market's margin.
    pub dex: String,
    pub asset: u32,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    pub mark: f64,
    pub prev_day: f64,
    // Per hour (Hyperliquid funds hourly).
    pub funding: f64,
    pub day_volume: f64,
    // Margin only per position (no cross margin), and the taker fee as a multiple of the base fee.
    pub isolated_only: bool,
    pub fee_scale: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    pub coin: String,
    // Signed size: positive long, negative short.
    pub size: f64,
    pub entry: f64,
    pub value: f64,
    pub unrealized_pnl: f64,
    pub return_on_equity: f64,
    pub liquidation: Option<f64>,
    pub margin: f64,
    pub leverage: u32,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Account {
    // USDC (dollars): everything in the account, and what can leave it now.
    pub value: f64,
    pub withdrawable: f64,
    pub positions: Vec<Position>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub coin: String,
    pub time_ms: u64,
    // The position before this fill: 0 means this fill opened it.
    pub start_position: f64,
    pub price: f64,
    pub size: f64,
    pub closed_pnl: f64,
    pub fee: f64,
}

/// A resting take-profit or stop-loss on a whole position (Hyperliquid's "position TP/SL").
#[derive(Clone, Debug, PartialEq)]
pub struct PositionTrigger {
    pub coin: String,
    pub oid: u64,
    // "tp" or "sl".
    pub kind: &'static str,
    pub trigger_price: f64,
}

/// A fill as the watcher reads it: which order it came from, what it did ("Close Long",
/// "Open Short"…), and whether it was the account being liquidated.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountFill {
    pub coin: String,
    pub oid: u64,
    pub tid: u64,
    pub time_ms: u64,
    pub dir: String,
    pub price: f64,
    pub size: f64,
    pub closed_pnl: f64,
    pub liquidated: bool,
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

impl HyperliquidClient {
    pub fn new() -> Result<Self, HyperliquidError> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(15)).build()?,
            info: Url::parse(INFO).map_err(|_| HyperliquidError::InvalidResponse("URL"))?,
        })
    }

    async fn info(&self, body: Value) -> Result<Value, HyperliquidError> {
        Ok(self
            .http
            .post(self.info.clone())
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Every live perps market, Hyperliquid's own and the listed dexes'.
    pub async fn markets(&self) -> Result<Vec<Market>, HyperliquidError> {
        let main = self.info(json!({"type":"metaAndAssetCtxs"}));
        let (dex, place) = DEXES[0];
        let other = self.info(json!({"type":"metaAndAssetCtxs","dex":dex}));
        let (main, other) = tokio::try_join!(main, other)?;
        let mut markets = parse_markets(&main, "", 0)?;
        markets.extend(parse_markets(&other, dex, 100_000 + place * 10_000)?);
        Ok(markets)
    }

    /// The account on Hyperliquid's own perps.
    pub async fn account(&self, user: &str) -> Result<Account, HyperliquidError> {
        self.account_on(user, "").await
    }

    /// The account's margin and positions on one dex ("" for Hyperliquid's own perps).
    pub async fn account_on(&self, user: &str, dex: &str) -> Result<Account, HyperliquidError> {
        let mut body = json!({"type":"clearinghouseState","user":user});
        if !dex.is_empty() {
            body["dex"] = json!(dex);
        }
        parse_account(&self.info(body).await?)
    }

    /// The account's latest fills, newest first.
    pub async fn fills(&self, user: &str) -> Result<Vec<Fill>, HyperliquidError> {
        let body = self.info(json!({"type":"userFills","user":user})).await?;
        Ok(body
            .as_array()
            .ok_or(HyperliquidError::InvalidResponse("fills"))?
            .iter()
            .filter_map(|f| {
                Some(Fill {
                    coin: f["coin"].as_str()?.to_string(),
                    time_ms: f["time"].as_u64()?,
                    start_position: num(&f["startPosition"])?,
                    price: num(&f["px"])?,
                    size: num(&f["sz"])?,
                    closed_pnl: num(&f["closedPnl"]).unwrap_or(0.0),
                    fee: num(&f["fee"]).unwrap_or(0.0),
                })
            })
            .collect())
    }

    /// Closing prices: (open time ms, close) per candle, oldest first.
    pub async fn closes(
        &self,
        coin: &str,
        interval: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<Vec<(u64, f64)>, HyperliquidError> {
        let body = self
            .info(
                json!({"type":"candleSnapshot","req":{"coin":coin,"interval":interval,
                "startTime":start_ms,"endTime":end_ms}}),
            )
            .await?;
        Ok(body
            .as_array()
            .ok_or(HyperliquidError::InvalidResponse("candles"))?
            .iter()
            .filter_map(|c| Some((c["t"].as_u64()?, num(&c["c"])?)))
            .collect())
    }

    /// The position TP/SLs resting on one dex ("" for Hyperliquid's own perps).
    pub async fn position_triggers(
        &self,
        user: &str,
        dex: &str,
    ) -> Result<Vec<PositionTrigger>, HyperliquidError> {
        let mut body = json!({"type":"frontendOpenOrders","user":user});
        if !dex.is_empty() {
            body["dex"] = json!(dex);
        }
        Ok(parse_triggers(&self.info(body).await?))
    }

    /// Fills since `start_ms`, oldest first, across the account's dexes.
    pub async fn fills_since(
        &self,
        user: &str,
        start_ms: u64,
    ) -> Result<Vec<AccountFill>, HyperliquidError> {
        let body = self
            .info(json!({"type":"userFillsByTime","user":user,"startTime":start_ms}))
            .await?;
        Ok(parse_account_fills(&body, user))
    }

    /// What kind of order `oid` was: "Take Profit Market", "Stop Market", "Limit"…
    pub async fn order_type(&self, user: &str, oid: u64) -> Result<String, HyperliquidError> {
        let body = self
            .info(json!({"type":"orderStatus","user":user,"oid":oid}))
            .await?;
        Ok(body["order"]["order"]["orderType"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// The agents this account has approved (lowercase addresses).
    pub async fn agents(&self, user: &str) -> Result<Vec<String>, HyperliquidError> {
        let body = self.info(json!({"type":"extraAgents","user":user})).await?;
        Ok(body
            .as_array()
            .ok_or(HyperliquidError::InvalidResponse("agents"))?
            .iter()
            .filter_map(|a| a["address"].as_str().map(str::to_ascii_lowercase))
            .collect())
    }
}

fn parse_markets(
    body: &Value,
    dex: &str,
    first_asset: u32,
) -> Result<Vec<Market>, HyperliquidError> {
    let universe = body[0]["universe"]
        .as_array()
        .ok_or(HyperliquidError::InvalidResponse("markets"))?;
    let ctxs = body[1]
        .as_array()
        .ok_or(HyperliquidError::InvalidResponse("market prices"))?;
    Ok(universe
        .iter()
        .zip(ctxs)
        .enumerate()
        .filter(|(_, (meta, _))| !meta["isDelisted"].as_bool().unwrap_or(false))
        .filter_map(|(index, (meta, ctx))| {
            // A builder dex's fee multiple (Hyperliquid's rule), a tenth of it in growth mode.
            let fee_scale = if dex.is_empty() {
                1.0
            } else {
                let deployer = num(&meta["deployerFeeScale"]).unwrap_or(1.0);
                let scale = if deployer < 1.0 {
                    deployer + 1.0
                } else {
                    deployer * 2.0
                };
                if meta["growthMode"].as_str() == Some("enabled") {
                    scale / 10.0
                } else {
                    scale
                }
            };
            Some(Market {
                coin: meta["name"].as_str()?.to_string(),
                dex: dex.to_string(),
                asset: first_asset + u32::try_from(index).ok()?,
                sz_decimals: u32::try_from(meta["szDecimals"].as_u64()?).ok()?,
                max_leverage: u32::try_from(meta["maxLeverage"].as_u64()?).ok()?,
                mark: num(&ctx["markPx"]).filter(|p| *p > 0.0)?,
                prev_day: num(&ctx["prevDayPx"]).unwrap_or(0.0),
                funding: num(&ctx["funding"]).unwrap_or(0.0),
                day_volume: num(&ctx["dayNtlVlm"]).unwrap_or(0.0),
                isolated_only: meta["onlyIsolated"].as_bool().unwrap_or(false)
                    || matches!(
                        meta["marginMode"].as_str(),
                        Some("noCross" | "strictIsolated")
                    ),
                fee_scale,
            })
        })
        .collect())
}

fn parse_triggers(body: &Value) -> Vec<PositionTrigger> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["isTrigger"] == true && o["isPositionTpsl"] == true)
        .filter_map(|o| {
            let kind = match o["orderType"].as_str()? {
                t if t.starts_with("Take Profit") => "tp",
                t if t.starts_with("Stop") => "sl",
                _ => return None,
            };
            Some(PositionTrigger {
                coin: o["coin"].as_str()?.to_string(),
                oid: o["oid"].as_u64()?,
                kind,
                trigger_price: num(&o["triggerPx"])?,
            })
        })
        .collect()
}

fn parse_account_fills(body: &Value, user: &str) -> Vec<AccountFill> {
    let mut fills: Vec<AccountFill> = body
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let dir = f["dir"].as_str().unwrap_or_default().to_string();
            let liquidated = f["liquidation"]["liquidatedUser"]
                .as_str()
                .is_some_and(|u| u.eq_ignore_ascii_case(user))
                || dir.starts_with("Liquidat");
            Some(AccountFill {
                coin: f["coin"].as_str()?.to_string(),
                oid: f["oid"].as_u64()?,
                tid: f["tid"].as_u64().unwrap_or(0),
                time_ms: f["time"].as_u64()?,
                dir,
                price: num(&f["px"])?,
                size: num(&f["sz"])?,
                closed_pnl: num(&f["closedPnl"]).unwrap_or(0.0),
                liquidated,
            })
        })
        .collect();
    fills.sort_by_key(|f| (f.time_ms, f.tid));
    fills
}

fn parse_account(body: &Value) -> Result<Account, HyperliquidError> {
    let value = num(&body["marginSummary"]["accountValue"])
        .ok_or(HyperliquidError::InvalidResponse("account value"))?;
    let positions = body["assetPositions"]
        .as_array()
        .ok_or(HyperliquidError::InvalidResponse("positions"))?
        .iter()
        .filter_map(|p| {
            let p = &p["position"];
            let size = num(&p["szi"])?;
            (size != 0.0).then_some(())?;
            Some(Position {
                coin: p["coin"].as_str()?.to_string(),
                size,
                entry: num(&p["entryPx"])?,
                value: num(&p["positionValue"]).unwrap_or(0.0),
                unrealized_pnl: num(&p["unrealizedPnl"]).unwrap_or(0.0),
                return_on_equity: num(&p["returnOnEquity"]).unwrap_or(0.0),
                liquidation: num(&p["liquidationPx"]),
                margin: num(&p["marginUsed"]).unwrap_or(0.0),
                leverage: u32::try_from(p["leverage"]["value"].as_u64().unwrap_or(1)).unwrap_or(1),
            })
        })
        .collect();
    Ok(Account {
        value,
        withdrawable: num(&body["withdrawable"]).unwrap_or(0.0),
        positions,
    })
}

/// A price Hyperliquid accepts: at most 5 significant figures and (6 − size decimals) decimals.
pub fn order_price(price: f64, sz_decimals: u32) -> String {
    let decimals = 6u32.saturating_sub(sz_decimals) as i32;
    let magnitude = if price > 0.0 {
        price.log10().floor() as i32 + 1
    } else {
        1
    };
    let places = (5 - magnitude).clamp(0, decimals);
    trim(format!("{price:.*}", places as usize))
}

/// A size rounded down to the market's size decimals.
pub fn order_size(size: f64, sz_decimals: u32) -> String {
    let scale = 10f64.powi(sz_decimals as i32);
    trim(format!(
        "{:.*}",
        sz_decimals as usize,
        (size * scale).floor() / scale
    ))
}

fn trim(text: String) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_position_tpsl_and_liquidations() {
        // Shapes captured from Hyperliquid's live info API on 2026-10-03.
        let orders = json!([
            {"coin":"BTC","oid":564383182882u64,"orderType":"Stop Market","triggerPx":"85848.0","isPositionTpsl":true,"isTrigger":true,"reduceOnly":true,"side":"B","sz":"0.0"},
            {"coin":"BTC","oid":2u64,"orderType":"Take Profit Market","triggerPx":"70000","isPositionTpsl":true,"isTrigger":true},
            {"coin":"ETH","oid":3u64,"orderType":"Limit","isPositionTpsl":false,"isTrigger":false,"triggerPx":"0.0"}
        ]);
        let t = parse_triggers(&orders);
        assert_eq!(t.len(), 2);
        assert_eq!(
            (t[0].kind, t[0].trigger_price, t[0].oid),
            ("sl", 85848.0, 564383182882)
        );
        assert_eq!(t[1].kind, "tp");
        let user = "0xabc0000000000000000000000000000000000001";
        let fills = json!([
            {"coin":"ONDO","oid":9u64,"tid":2u64,"dir":"Close Long","px":"0.5","sz":"10","closedPnl":"1.5","time":200u64},
            {"coin":"BTC","oid":8u64,"tid":1u64,"dir":"Close Short","px":"90000","sz":"0.01","closedPnl":"-30","time":100u64,
             "liquidation":{"liquidatedUser":"0xABC0000000000000000000000000000000000001","markPx":"90010","method":"market"}}
        ]);
        let f = parse_account_fills(&fills, user);
        assert_eq!(f[0].coin, "BTC");
        assert!(f[0].liquidated);
        assert!(!f[1].liquidated);
        assert_eq!(f[1].closed_pnl, 1.5);
    }

    #[test]
    fn reads_markets_and_skips_delisted_ones() {
        let body = json!([
            {"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40},
                {"name":"OLD","szDecimals":0,"maxLeverage":3,"isDelisted":true},
                {"name":"NEAR","szDecimals":1,"maxLeverage":10}]},
            [{"markPx":"83547.0","prevDayPx":"82000.0","funding":"0.0000125","dayNtlVlm":"2914300000"},
             {"markPx":"1.0"},
             {"markPx":"5.3423","prevDayPx":"5.1","funding":"-0.00001","dayNtlVlm":"348700000"}]
        ]);
        let markets = parse_markets(&body, "", 0).unwrap();
        assert_eq!(markets.len(), 2);
        assert_eq!(markets[1].coin, "NEAR");
        // The asset id is the position in the full list, delisted ones included.
        assert_eq!(markets[1].asset, 2);
        assert_eq!(markets[1].sz_decimals, 1);
        assert_eq!(markets[1].fee_scale, 1.0);
        assert!(!markets[1].isolated_only);
    }

    #[test]
    fn reads_a_builder_dex_with_its_asset_ids_fees_and_margin_rules() {
        // Shape from the live `xyz` dex on 2026-10-01 (trimmed).
        let body = json!([
            {"universe":[{"szDecimals":3,"name":"xyz:TSLA","maxLeverage":20,"growthMode":"enabled",
                    "deployerFeeScale":"1.0"},
                {"szDecimals":4,"name":"xyz:GOLD","maxLeverage":25,"deployerFeeScale":"1.0"},
                {"szDecimals":3,"name":"xyz:HOOD","maxLeverage":10,"onlyIsolated":true,
                    "marginMode":"noCross","deployerFeeScale":"0.5"}],
             "collateralToken":0},
            [{"markPx":"356.8"},{"markPx":"4174.2"},{"markPx":"113.21"}]
        ]);
        let markets = parse_markets(&body, "xyz", 110_000).unwrap();
        assert_eq!(
            markets.iter().map(|m| m.asset).collect::<Vec<_>>(),
            [110_000, 110_001, 110_002]
        );
        assert_eq!(markets[0].dex, "xyz");
        assert!((markets[0].fee_scale - 0.2).abs() < 1e-9);
        assert_eq!(markets[1].fee_scale, 2.0);
        assert_eq!(markets[2].fee_scale, 1.5);
        assert!(markets[2].isolated_only && !markets[1].isolated_only);
    }

    #[test]
    fn reads_an_account_with_a_position() {
        // Shape from Hyperliquid's live API on 2026-10-01.
        let body = json!({"marginSummary":{"accountValue":"20000.5"},"withdrawable":"372.7",
            "assetPositions":[{"type":"oneWay","position":{"coin":"BTC","szi":"-2.10308",
                "leverage":{"type":"cross","value":10},"entryPx":"84020.4","positionValue":"175745.98",
                "unrealizedPnl":"-955.8","returnOnEquity":"-0.054","liquidationPx":"75172.78",
                "marginUsed":"19627.77"}},
                {"type":"oneWay","position":{"coin":"ETH","szi":"0.0","entryPx":"1"}}]});
        let account = parse_account(&body).unwrap();
        assert_eq!(account.positions.len(), 1);
        let p = &account.positions[0];
        assert_eq!(p.size, -2.10308);
        assert_eq!(p.leverage, 10);
        assert_eq!(p.liquidation, Some(75172.78));
        assert_eq!(account.withdrawable, 372.7);
    }

    #[test]
    fn prices_and_sizes_fit_the_exchange_rules() {
        assert_eq!(order_price(83547.42, 5), "83547");
        assert_eq!(order_price(5.34237, 1), "5.3424");
        assert_eq!(order_price(0.0042713, 0), "0.004271");
        assert_eq!(order_price(1234.5678, 2), "1234.6");
        assert_eq!(order_size(2.00987, 1), "2");
        assert_eq!(order_size(0.0132999, 4), "0.0132");
        assert_eq!(order_size(12.0, 0), "12");
    }

    // Network: live markets and BTC candles.
    // cargo test -p engine-execution live_hyperliquid -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_hyperliquid_markets() {
        let client = HyperliquidClient::new().unwrap();
        let markets = client.markets().await.unwrap();
        println!("{} markets", markets.len());
        let near = markets.iter().find(|m| m.coin == "NEAR").unwrap();
        println!("{near:?}");
        let tsla = markets.iter().find(|m| m.coin == "xyz:TSLA").unwrap();
        println!("{tsla:?}");
        // The dexes sit where DEXES says, so their asset ids are right.
        let dexes = client.info(json!({"type":"perpDexs"})).await.unwrap();
        for (dex, place) in DEXES {
            assert_eq!(dexes[place as usize]["name"], dex);
        }
        let account = client
            .account_on("0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97", "xyz")
            .await
            .unwrap();
        println!("xyz account {account:?}");
        let now = 1_790_830_000_000;
        let closes = client
            .closes("BTC", "1h", now - 86_400_000, now)
            .await
            .unwrap();
        assert!(closes.len() > 20);
    }
}
