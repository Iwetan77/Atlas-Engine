//! Receipts are views of saved actions, never a second execution path.
use super::*;
use axum::extract::Query;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Receipt {
    owner: String,
    pub(super) id: String,
    intent_id: Option<String>,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) symbol: String,
    pub(super) icon_url: Option<String>,
    asset_id: Option<String>,
    created_at_unix_ms: u64,
    state: String,
    stage: String,
    tx_ids: Vec<String>,
    error: Option<String>,
    pub(super) usdc_units: Option<String>,
    summary: Vec<Value>,
    #[serde(default)]
    deposit_address: Option<String>,
    #[serde(default)]
    deposit_memo: Option<String>,
}
impl Receipt {
    pub(super) fn plan(owner: &str, plan: &Value) -> Self {
        let id = text(plan, "intentId");
        let kind = text(plan, "kind");
        let summary: Vec<Value> = plan["summary"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(json!({"label":r["label"].as_str()?,"value":r["value"].as_str()?}))
            })
            .collect();
        let symbol = summary
            .iter()
            .find(|r| r["label"] == "Market")
            .and_then(|r| r["value"].as_str())
            .unwrap_or("")
            .to_owned();
        Self {
            owner: owner.into(),
            intent_id: Some(id.clone()),
            created_at_unix_ms: created_ms(&id),
            id,
            title: title(&kind, &symbol),
            kind,
            symbol,
            icon_url: None,
            asset_id: None,
            state: "pending".into(),
            stage: "validate".into(),
            tx_ids: vec![],
            error: None,
            usdc_units: None,
            summary,
            deposit_address: None,
            deposit_memo: None,
        }
    }
    pub(super) fn deposit(
        owner: &str,
        address: &str,
        memo: Option<String>,
        symbol: &str,
        receive: u128,
    ) -> Self {
        let mut r = Self::plan(
            owner,
            &json!({"intentId":format!("deposit-{address}"),"kind":"deposit"}),
        );
        r.intent_id = None;
        r.created_at_unix_ms = now();
        r.symbol = symbol.into();
        r.stage = "waiting".into();
        r.deposit_address = Some(address.into());
        r.deposit_memo = memo;
        r.usdc_units = Some(receive.to_string());
        r.title = title(&r.kind, &r.symbol);
        r
    }
    // A bank transfer through Daya: no intent of its own, updated by Daya's webhook and polling.
    pub(super) fn ramp(
        owner: &str,
        id: &str,
        kind: &str,
        summary: Vec<Value>,
        usdc_units: u128,
    ) -> Self {
        let mut r = Self::plan(owner, &json!({"intentId":id,"kind":kind,"summary":summary}));
        r.intent_id = None;
        r.created_at_unix_ms = now();
        r.stage = "waiting".into();
        r.usdc_units = Some(usdc_units.to_string());
        r
    }
    pub(super) fn set_state(&mut self, state: &str, stage: &str, error: Option<String>) {
        self.state = state.into();
        self.stage = stage.into();
        self.error = error;
    }
    pub(super) fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }
    pub(super) fn set_usdc(&mut self, units: u128) {
        self.usdc_units = Some(units.to_string());
    }
    // Replaces the summary line with this label, or adds it.
    pub(super) fn set_line(&mut self, label: &str, value: String) {
        let line = json!({"label":label,"value":value});
        match self.summary.iter_mut().find(|l| l["label"] == label) {
            Some(existing) => *existing = line,
            None => self.summary.push(line),
        }
    }
    pub(super) fn add_tx(&mut self, tx: String) {
        if !self.tx_ids.contains(&tx) {
            self.tx_ids.push(tx);
        }
    }
    fn public(&self, currency: &str, rate: u128) -> Value {
        let amount = self.usdc_units.as_deref().and_then(|u| u.parse::<u128>().ok())
            .and_then(|u| u.checked_mul(rate)).map(|u| json!({"amount": markets::format_units(u / 1_000_000, 6), "currency": currency}));
        // Older perps plans wrote their moved margin in dollars; receipts use the chosen currency.
        let summary: Vec<_> = self
            .summary
            .iter()
            .map(|line| {
                let mut line = line.clone();
                for key in ["label", "value"] {
                    if let Some(plain) = line[key].as_str().and_then(unbranded) {
                        line[key] = json!(plain);
                    }
                }
                if let Some(usd) = line["value"]
                    .as_str()
                    .and_then(|v| v.strip_prefix('$'))
                    .and_then(|v| markets::parse_micros(v).ok())
                {
                    line["value"] = json!(markets::say_money(usd, currency, rate));
                }
                line
            })
            .collect();
        let error = self.error.as_deref().map(|e| unbranded(e).unwrap_or(e));
        json!({"id":self.id,"intentId":self.intent_id,"kind":self.kind,"title":self.title,
            "symbol":self.symbol,"assetId":self.asset_id,"iconUrl":self.icon_url,
            "createdAtUnixMs":self.created_at_unix_ms,"state":self.state,"stage":self.stage,
            "amount":amount,"txIds":self.tx_ids,"error":error,"summary":summary})
    }
}
// Older bank-transfer receipts named the payment partner; receipts just say what happened.
fn unbranded(text: &str) -> Option<&'static str> {
    Some(match text {
        "Daya fee" => "Fee",
        "Being checked by Daya" => "Being checked",
        "The money arrived after the account expired, so Daya is reviewing it. Contact support if it isn't sorted within a day." => "The money arrived after the account expired, so it's being reviewed. Contact support if it isn't sorted within a day.",
        "Daya is checking this payment. It usually clears; contact support if it takes more than a day." => "This payment is being checked. It usually clears; contact support if it takes more than a day.",
        "Daya reversed this payment. Contact support." => "This payment was reversed. Contact support.",
        _ => return None,
    })
}
fn text(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").into()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const DRAFT_MS: u64 = 10 * 60 * 1000;
fn created_ms(id: &str) -> u64 {
    // Market ids use hex milliseconds, cross-chain ids use decimal milliseconds.
    id.split('-')
        .find_map(|s| {
            let t = if s.len() == 13 {
                s.parse().ok()
            } else if s.len() == 11 {
                u64::from_str_radix(s, 16).ok()
            } else {
                None
            };
            t.filter(|t| (1_600_000_000_000..4_000_000_000_000).contains(t))
        })
        .unwrap_or(0)
}
pub(super) fn title(kind: &str, symbol: &str) -> String {
    let action = match kind {
        "buy" => "Buy",
        "sell" => "Sell",
        "earn_deposit" => "Save with",
        "earn_withdraw" => "Withdraw from",
        "perp_open" => "Open position",
        "perp_close" => "Close position",
        "send" => "Send money",
        "cashlink" => "Cash link",
        "deposit" => "Deposit",
        "onramp" => "Add money",
        "offramp" => "Cash out",
        _ => "Transaction",
    };
    if symbol.is_empty() {
        action.into()
    } else {
        format!("{action} {symbol}")
    }
}
#[derive(Clone, Default)]
pub(super) struct HistoryStore {
    postgres: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<String, Receipt>>>,
    refreshing: Arc<Mutex<HashMap<String, Instant>>>,
}
impl HistoryStore {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut store = Self::default();
        if let Ok(url) = env::var("DATABASE_URL") {
            let (pg, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("history database connection ended");
                }
            });
            pg.batch_execute("CREATE TABLE IF NOT EXISTS atlas_receipts (id TEXT PRIMARY KEY, owner TEXT NOT NULL, payload TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS atlas_receipts_owner ON atlas_receipts(owner);
                CREATE INDEX IF NOT EXISTS atlas_intents_owner ON atlas_intents(owner);
                CREATE INDEX IF NOT EXISTS atlas_near_intents_owner ON atlas_near_intents(owner);
                CREATE INDEX IF NOT EXISTS atlas_hl_intents_owner ON atlas_hl_intents(owner)").await?;
            store.postgres = Some(Arc::new(pg));
        }
        Ok(store)
    }
    pub(super) async fn put(&self, receipt: &Receipt) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_receipts(id,owner,payload) VALUES($1,$2,$3)
                ON CONFLICT(id) DO UPDATE SET payload=$3 WHERE atlas_receipts.owner=$2",
                &[
                    &receipt.id,
                    &receipt.owner,
                    &serde_json::to_string(receipt).map_err(internal)?,
                ],
            )
            .await
            .map_err(internal)?;
        } else {
            let mut memory = self.memory.lock().map_err(internal)?;
            if memory
                .get(&receipt.id)
                .is_some_and(|r| r.owner != receipt.owner)
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    "receipt belongs to another user".into(),
                ));
            }
            memory.insert(receipt.id.clone(), receipt.clone());
        }
        Ok(())
    }
    pub(super) async fn observe_deposit(
        &self,
        owner: &str,
        address: &str,
        status: &engine_execution::near_intents::Status,
    ) -> Result<(), ApiError> {
        let Some(mut r) = self
            .owned(owner)
            .await?
            .into_iter()
            .find(|r| r.deposit_address.as_deref() == Some(address))
        else {
            return Ok(());
        };
        match status.status.as_str() {
            "SUCCESS" => {
                r.state = "filled".into();
                r.stage = "settle".into();
            }
            "REFUNDED" => {
                r.state = "failed".into();
                r.stage = "refunded".into();
                r.error=Some("The deposit was refunded. Check the receiving wallet shown when you deposited.".into());
            }
            "FAILED" => {
                r.state = "failed".into();
                r.error =
                    Some("The deposit could not be completed. Check its transaction IDs.".into());
            }
            "PENDING_DEPOSIT" => {}
            _ => {
                r.stage = "settle".into();
            }
        }
        if let Some(d) = &status.swap_details {
            r.tx_ids = d
                .origin_chain_tx_hashes
                .iter()
                .map(|t| t.hash.clone())
                .chain(d.near_tx_hashes.iter().cloned())
                .chain(d.destination_chain_tx_hashes.iter().map(|t| t.hash.clone()))
                .collect();
            if r.state == "filled" {
                r.usdc_units = d.amount_out.clone();
            }
        }
        self.put(&r).await
    }
    pub(super) async fn find(&self, owner: &str, id: &str) -> Result<Option<Receipt>, ApiError> {
        if let Some(pg) = &self.postgres {
            return pg
                .query_opt(
                    "SELECT payload FROM atlas_receipts WHERE id=$1 AND owner=$2",
                    &[&id, &owner],
                )
                .await
                .map_err(internal)?
                .map(|r| serde_json::from_str(r.get::<_, &str>(0)).map_err(internal))
                .transpose();
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .get(id)
            .filter(|r| r.owner == owner)
            .cloned())
    }
    async fn owned(&self, owner: &str) -> Result<Vec<Receipt>, ApiError> {
        if let Some(pg) = &self.postgres {
            return pg
                .query(
                    "SELECT payload FROM atlas_receipts WHERE owner=$1",
                    &[&owner],
                )
                .await
                .map_err(internal)?
                .iter()
                .map(|r| serde_json::from_str(r.get::<_, &str>(0)).map_err(internal))
                .collect();
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .values()
            .filter(|r| r.owner == owner)
            .cloned()
            .collect())
    }
}
fn snapshot(owner: &str, v: &Value, source: &str) -> Receipt {
    let mut r = Receipt::plan(owner, &json!({"intentId":v["status"]["intentId"]}));
    r.state = text(&v["status"], "state");
    r.stage = text(&v["status"], "stage");
    r.error = v["status"]["error"].as_str().map(str::to_owned);
    r.icon_url = v["historyIcon"].as_str().map(str::to_owned);
    r.tx_ids = v["status"]["txIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if source == "near" {
        r.kind = if v["sell_sui"] == true || v["origin_chain"] == "monad" {
            "sell"
        } else {
            "buy"
        }
        .into();
        for key in ["then_swap", "sale", "ref_buy", "ref_sale", "asset"] {
            if let Some(symbol) = v[key]["symbol"].as_str() {
                r.symbol = symbol.into();
                r.icon_url = v[key]["icon_url"]
                    .as_str()
                    .map(str::to_owned)
                    .or(r.icon_url);
                break;
            }
        }
        r.asset_id = v["asset_id"].as_str().map(str::to_owned);
        let amount = if r.kind == "buy" {
            &v["amount"]
        } else {
            &v["minimum_out"]
        };
        r.usdc_units = amount.as_u64().map(|n| n.to_string());
    } else if source == "hl" {
        r.kind = if v["quote"]["close"] == true {
            "perp_close"
        } else {
            "perp_open"
        }
        .into();
        r.symbol = text(&v["quote"], "coin")
            .split(':')
            .next_back()
            .unwrap_or("")
            .into();
    } else if !v["trade"].is_null() {
        r.kind = text(&v["trade"], "side");
        r.asset_id = v["trade"]["asset_id"].as_str().map(str::to_owned);
        let k = if r.kind == "buy" {
            "pay_units"
        } else {
            "get_units"
        };
        r.usdc_units = v["trade"][k].as_number().map(ToString::to_string);
    }
    r.title = title(&r.kind, &r.symbol);
    r
}
async fn records(state: &AppState, owner: &str) -> Result<Vec<Receipt>, ApiError> {
    let mut rows: HashMap<String, Receipt> = state
        .history
        .owned(owner)
        .await?
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    let (market, near, hl) = tokio::try_join!(
        state.markets.history_rows(owner),
        near_intents::history_rows(state, owner),
        state.hl.history_rows(owner)
    )?;
    for (source, values) in [("market", market), ("near", near), ("hl", hl)] {
        for v in values {
            let mut r = snapshot(owner, &v, source);
            if let Some(saved) = rows.remove(&r.id) {
                r.created_at_unix_ms = saved.created_at_unix_ms;
                r.summary = saved.summary;
                if !saved.kind.is_empty() {
                    r.kind = saved.kind;
                }
                if !saved.symbol.is_empty() {
                    r.symbol = saved.symbol;
                }
                if saved.icon_url.is_some() {
                    r.icon_url = saved.icon_url;
                }
                if saved.usdc_units.is_some() {
                    r.usdc_units = saved.usdc_units;
                }
                if saved.asset_id.is_some() {
                    r.asset_id = saved.asset_id;
                }
            }
            rows.insert(r.id.clone(), r);
        }
    }
    // Filled trades carry actual proceeds, replacing a sell's estimated quote.
    for t in state.trades.for_user(owner).await? {
        let r = rows.entry(t.intent_id.clone()).or_insert_with(|| {
            Receipt::plan(owner, &json!({"intentId":t.intent_id,"kind":t.side}))
        });
        r.asset_id = Some(t.asset_id);
        r.usdc_units = Some(t.usdc_units.to_string());
        if r.created_at_unix_ms == 0 {
            r.created_at_unix_ms = t.filled_at_ms;
        }
        if r.kind.is_empty() {
            r.kind = t.side;
        }
        if r.tx_ids.is_empty() {
            if let Some(tx) = t.tx_id {
                r.tx_ids.push(tx);
            }
        }
        // Keep a live intent's state (a closed app may still owe a second step).
        if r.stage == "validate" && r.state == "pending" {
            r.state = "filled".into();
            r.stage = "settle".into();
        }
    }
    for r in rows.values_mut() {
        if let Some(asset) = r
            .asset_id
            .as_deref()
            .and_then(|id| state.markets.history_asset(id))
        {
            r.symbol = asset.symbol;
            r.icon_url = asset.icon_url.or(r.icon_url.take());
        }
        r.title = title(&r.kind, &r.symbol);
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    // A plan is saved when its confirmation opens. One nobody confirmed (cancelled, or left
    // unsigned) expires after two minutes and moved nothing, so after ten it's not history.
    let now = now();
    rows.retain(|r| {
        !(r.state == "pending"
            && r.stage == "validate"
            && r.tx_ids.is_empty()
            && now.saturating_sub(r.created_at_unix_ms) > DRAFT_MS)
    });
    rows.sort_by(|a, b| (b.created_at_unix_ms, &b.id).cmp(&(a.created_at_unix_ms, &a.id)));
    Ok(rows)
}
#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    currency: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}
fn page(
    mut rows: Vec<Receipt>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<Receipt>, Option<String>), ApiError> {
    if let Some(c) = cursor {
        let (ms, id) = c
            .split_once(':')
            .ok_or((StatusCode::BAD_REQUEST, "invalid history cursor".into()))?;
        let ms: u64 = ms
            .parse()
            .map_err(|_| (StatusCode::BAD_REQUEST, "invalid history cursor".into()))?;
        rows.retain(|r| (r.created_at_unix_ms, r.id.as_str()) < (ms, id));
    }
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        rows.last()
            .map(|r| format!("{}:{}", r.created_at_unix_ms, r.id))
    } else {
        None
    };
    Ok((rows, next))
}
fn refresh(state: &AppState, headers: &HeaderMap, owner: &str, rows: &[Receipt]) {
    let Ok(mut held) = state.history.refreshing.lock() else {
        return;
    };
    held.retain(|_, at| at.elapsed() < Duration::from_secs(30));
    if held.contains_key(owner) {
        return;
    }
    held.insert(owner.into(), Instant::now());
    drop(held);
    // Bank transfers through Daya: a naira deposit still on its way, or a cash-out in its first days
    // (its USDC send may be done while the bank payout isn't).
    for r in rows
        .iter()
        .filter(|r| {
            (r.kind == "onramp" && r.state == "pending")
                || (r.kind == "offramp" && now().saturating_sub(r.created_at_unix_ms) < 3 * DAY_MS)
        })
        .take(4)
    {
        let (state, owner, id) = (state.clone(), owner.to_owned(), r.id.clone());
        tokio::spawn(async move {
            let _ =
                tokio::time::timeout(Duration::from_secs(8), daya::observe(&state, &owner, &id))
                    .await;
        });
    }
    // Reading history stays fast. Observe pending transfers without starting new buys or signing.
    for r in rows
        .iter()
        .filter(|r| {
            r.state == "pending"
                && matches!(r.stage.as_str(), "fund" | "settle" | "execute" | "waiting")
        })
        .take(6)
    {
        if let Some(address) = r.deposit_address.clone() {
            let (state, owner, memo) = (state.clone(), owner.to_owned(), r.deposit_memo.clone());
            tokio::spawn(async move {
                if let Ok(Ok(status)) = tokio::time::timeout(
                    Duration::from_secs(8),
                    state.near.history_deposit_status(&address, memo.as_deref()),
                )
                .await
                {
                    let _ = state
                        .history
                        .observe_deposit(&owner, &address, &status)
                        .await;
                }
            });
            continue;
        }
        let Some(id) = r.intent_id.clone() else {
            continue;
        };
        let (state, headers) = (state.clone(), headers.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(8),
                markets::intent_status(State(state), Path(id), headers),
            )
            .await;
        });
    }
}
pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let currency = q.currency.as_deref().unwrap_or("NGN");
    markets::checked_currency(currency)?;
    let rate = app_balance::fx_rate(currency).await?;
    let rows = records(&state, &owner).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 50);
    let (rows, next) = page(rows, q.cursor.as_deref(), limit)?;
    refresh(&state, &headers, &owner, &rows);
    Ok(Json(
        json!({"transactions":rows.iter().map(|r|r.public(currency,rate)).collect::<Vec<_>>(),"nextCursor":next}),
    ))
}
pub(super) async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let currency = q.currency.as_deref().unwrap_or("NGN");
    markets::checked_currency(currency)?;
    let rows = records(&state, &owner).await?;
    let row = rows
        .iter()
        .find(|r| r.id == id)
        .ok_or((StatusCode::NOT_FOUND, "transaction not found".into()))?;
    refresh(&state, &headers, &owner, std::slice::from_ref(row));
    Ok(Json(
        row.public(currency, app_balance::fx_rate(currency).await?),
    ))
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Transaction history is unavailable. Try again shortly.".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_bank_receipts_do_not_name_the_payment_partner() {
        let mut r = Receipt::ramp(
            "alice",
            "onramp-1",
            "onramp",
            vec![
                json!({"label":"Daya fee","value":"₦100"}),
                json!({"label":"Status","value":"Being checked by Daya"}),
            ],
            1_000_000,
        );
        r.set_error(Some("Daya reversed this payment. Contact support.".into()));
        let public = r.public("NGN", 1_500_000_000);
        assert_eq!(public["summary"][0]["label"], "Fee");
        assert_eq!(public["summary"][1]["value"], "Being checked");
        assert_eq!(
            public["error"],
            "This payment was reversed. Contact support."
        );
        assert!(!public.to_string().contains("Daya"));
    }
    #[test]
    fn receipts_never_return_signing_payloads_or_owner() {
        let r = Receipt::plan(
            "private-user",
            &json!({"intentId":"intent-19a112abcde-1","kind":"buy","transactions":[{"request":{"secret":"no"}}],"summary":[{"label":"You pay","value":"₦2,000"}]}),
        );
        let public = r.public("NGN", 1_500_000_000).to_string();
        assert!(!public.contains("private-user"));
        assert!(!public.contains("secret"));
        assert!(!public.contains("request"));
        assert!(public.contains("₦2,000"));
    }
    #[test]
    fn paging_does_not_repeat_rows_with_the_same_time() {
        let mut rows: Vec<_> = (0..7)
            .rev()
            .map(|n| {
                let mut r = Receipt::plan("u", &json!({"intentId":format!("i{n}")}));
                r.created_at_unix_ms = 100;
                r
            })
            .collect();
        let (first, next) = page(rows.clone(), None, 6).unwrap();
        assert_eq!(first.len(), 6);
        let (last, next) = page(std::mem::take(&mut rows), next.as_deref(), 6).unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].id, "i0");
        assert!(next.is_none());
        assert!(page(vec![], Some("bad"), 6).is_err());
    }
    #[test]
    fn a_sui_buy_stays_pending_until_the_saved_intent_says_it_settled() {
        let v = json!({"status":{"intentId":"near-intent-1790921147000-1","stage":"fund","state":"pending","txIds":["deposit-tx"],"error":null},"amount":1_000_000,"then_swap":{"symbol":"DEEP","icon_url":"https://example.com/deep.png"},"asset_id":"near:sui:deep","approval":"private"});
        let mut r = snapshot("alice", &v, "near");
        assert_eq!(r.state, "pending");
        assert_eq!(r.symbol, "DEEP");
        assert_eq!(r.created_at_unix_ms, 1790921147000);
        assert_eq!(r.public("NGN", 1_500_000_000)["amount"]["amount"], "1500");
        assert_eq!(r.tx_ids, vec!["deposit-tx"]);
        assert!(!r
            .public("NGN", 1_500_000_000)
            .to_string()
            .contains("private"));
        r.state = "failed".into();
        assert_eq!(r.public("NGN", 1_500_000_000)["state"], "failed");
    }
    #[test]
    fn a_monad_sale_does_not_show_wei_as_cash() {
        let v:Value=serde_json::from_str(r#"{"status":{"intentId":"near-intent-1790921147000-2","state":"pending","stage":"validate"},"origin_chain":"monad","amount":100000000000000000000,"minimum_out":2000000,"asset":{"symbol":"MON"}}"#).unwrap();
        let r = snapshot("alice", &v, "near");
        assert_eq!(r.kind, "sell");
        assert_eq!(r.usdc_units.as_deref(), Some("2000000"));
    }
    #[test]
    fn legacy_perps_margin_uses_the_users_currency() {
        let r = Receipt::plan(
            "alice",
            &json!({"intentId":"hl-19a112abcde-1","kind":"perp_open","summary":[{"label":"Margin moved","value":"$2.00"}]}),
        );
        let public = r.public("NGN", 1_500_000_000);
        assert_eq!(
            public["summary"][0]["value"],
            markets::say_money(2_000_000, "NGN", 1_500_000_000)
        );
        assert!(!public.to_string().contains("$2.00"));
    }
    #[tokio::test]
    async fn deposits_use_the_actual_payout_and_cannot_update_another_user() {
        let store = HistoryStore::default();
        let r = Receipt::deposit("alice", "address", None, "SUI", 2_000_000);
        store.put(&r).await.unwrap();
        let status=serde_json::from_value(json!({"status":"SUCCESS","swapDetails":{"amountOut":"1950000","originChainTxHashes":[{"hash":"in"}],"destinationChainTxHashes":[{"hash":"out"}]}})).unwrap();
        store
            .observe_deposit("bob", "address", &status)
            .await
            .unwrap();
        assert_eq!(store.owned("alice").await.unwrap()[0].state, "pending");
        store
            .observe_deposit("alice", "address", &status)
            .await
            .unwrap();
        let actual = store.owned("alice").await.unwrap().remove(0);
        assert_eq!(actual.state, "filled");
        assert_eq!(actual.usdc_units.as_deref(), Some("1950000"));
        assert_eq!(actual.tx_ids, vec!["in", "out"]);
    }
    #[tokio::test]
    async fn history_is_owned_and_survives_updates_without_duplicates() {
        let store = HistoryStore::default();
        let mut r = Receipt::plan("alice", &json!({"intentId":"one","kind":"offramp"}));
        store.put(&r).await.unwrap();
        r.state = "filled".into();
        store.put(&r).await.unwrap();
        assert_eq!(store.owned("alice").await.unwrap().len(), 1);
        assert!(store.owned("bob").await.unwrap().is_empty());
        assert_eq!(store.owned("alice").await.unwrap()[0].state, "filled");
    }
}
