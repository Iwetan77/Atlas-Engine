//! Atlas Predictions: one confirmation, phone signatures, and venue-confirmed settlement.
use super::*;
use axum::extract::Query;
use engine_execution::swaps::uniswap::BASE_USDC;
use serde_json::{json, Value};

#[derive(Clone)]
pub(super) struct PredictionState {
    pg: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<String, Value>>>,
}
impl PredictionState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let pg = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("predictions database connection ended");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_predictions (
                id TEXT PRIMARY KEY, owner TEXT NOT NULL, payload TEXT NOT NULL)",
                )
                .await?;
            client.batch_execute("ALTER TABLE atlas_predictions ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT FALSE;
                CREATE UNIQUE INDEX IF NOT EXISTS atlas_predictions_one_active ON atlas_predictions(owner) WHERE active").await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            pg,
            memory: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    async fn get(&self, id: &str, owner: &str) -> Result<Value, ApiError> {
        let value = if let Some(pg) = &self.pg {
            pg.query_opt(
                "SELECT payload FROM atlas_predictions WHERE id=$1 AND owner=$2",
                &[&id, &owner],
            )
            .await
            .map_err(internal)?
            .map(|r| serde_json::from_str::<Value>(r.get::<_, &str>(0)).map_err(internal))
            .transpose()?
        } else {
            self.memory
                .lock()
                .map_err(internal)?
                .get(id)
                .filter(|v| v["owner"] == owner)
                .cloned()
        };
        value.ok_or((StatusCode::NOT_FOUND, "Prediction not found".into()))
    }
    async fn put(&self, id: &str, value: &Value) -> Result<(), ApiError> {
        let owner = text(value, "owner");
        let active = id.starts_with("prediction-")
            && !id.starts_with("prediction-quote-")
            && value["status"]["state"] == "pending";
        if let Some(pg) = &self.pg {
            let payload = serde_json::to_string(value).map_err(internal)?;
            pg.execute(
                "INSERT INTO atlas_predictions (id,owner,payload,active) VALUES ($1,$2,$3,$4)
                ON CONFLICT (id) DO UPDATE SET payload=$3,active=$4 WHERE atlas_predictions.owner=$2",
                &[&id, &owner, &payload, &active],
            )
            .await
            .map_err(|_|conflict("A prediction is already processing. Finish it or check Activity first."))?;
        } else {
            let mut memory = self.memory.lock().map_err(internal)?;
            if active
                && memory.iter().any(|(key, v)| {
                    key != id
                        && v["owner"] == owner
                        && v["status"]["state"] == "pending"
                        && key.starts_with("prediction-")
                        && !key.starts_with("prediction-quote-")
                })
            {
                return Err(conflict(
                    "A prediction is already processing. Finish it or check Activity first.",
                ));
            }
            memory.insert(id.into(), value.clone());
        }
        Ok(())
    }
    // Compare the entire old payload, including the preparation id: repeated reports cannot claim a later step.
    async fn replace(&self, id: &str, old: &Value, new: &Value) -> Result<bool, ApiError> {
        if let Some(pg) = &self.pg {
            let a = serde_json::to_string(old).map_err(internal)?;
            let b = serde_json::to_string(new).map_err(internal)?;
            let active =
                !id.starts_with("prediction-quote-") && new["status"]["state"] == "pending";
            return Ok(pg
                .execute(
                    "UPDATE atlas_predictions SET payload=$2,active=$4 WHERE id=$1 AND payload=$3",
                    &[&id, &b, &a, &active],
                )
                .await
                .map_err(internal)?
                == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        if memory.get(id) != Some(old) {
            return Ok(false);
        }
        memory.insert(id.into(), new.clone());
        Ok(true)
    }
    async fn release_previews(&self, owner: &str) -> Result<(), ApiError> {
        if let Some(pg) = &self.pg {
            pg.execute(r#"UPDATE atlas_predictions SET active=FALSE,
                payload=jsonb_set(jsonb_set(payload::jsonb,'{status,state}','"failed"'),'{status,error}',
                '"This unconfirmed preview was replaced. Nothing was charged."')::text
                WHERE owner=$1 AND active AND payload::jsonb#>>'{status,stage}'='validate'"#,&[&owner]).await.map_err(internal)?;
        } else {
            for v in self
                .memory
                .lock()
                .map_err(internal)?
                .values_mut()
                .filter(|v| v["owner"] == owner && v["status"]["stage"] == "validate")
            {
                v["status"]["state"] = json!("failed");
                v["status"]["error"] = json!("Unconfirmed preview replaced. Nothing was charged.");
            }
        }
        Ok(())
    }
    async fn has_activity(&self, owner: &str) -> Result<bool, ApiError> {
        if let Some(pg) = &self.pg {
            return Ok(pg.query_one("SELECT EXISTS(SELECT 1 FROM atlas_predictions WHERE owner=$1 AND id LIKE 'prediction-%' AND id NOT LIKE 'prediction-quote-%')",&[&owner])
                .await.map_err(internal)?.get(0));
        }
        Ok(self
            .memory
            .lock()
            .map_err(internal)?
            .iter()
            .any(|(id, v)| !id.starts_with("prediction-quote-") && v["owner"] == owner))
    }
    pub(super) async fn pending(&self, owner: &str) -> Result<Vec<Value>, ApiError> {
        let values = if let Some(pg) = &self.pg {
            pg.query(
                "SELECT payload FROM atlas_predictions WHERE owner=$1 AND id LIKE 'prediction-%'",
                &[&owner],
            )
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| serde_json::from_str::<Value>(r.get::<_, &str>(0)).map_err(internal))
            .collect::<Result<Vec<_>, _>>()?
        } else {
            self.memory
                .lock()
                .map_err(internal)?
                .values()
                .filter(|v| v["owner"] == owner)
                .cloned()
                .collect()
        };
        Ok(values.into_iter().filter(|v| v["status"]["state"] == "pending" && v["status"]["stage"] == "sign")
            .map(|v| json!({"intentId":v["status"]["intentId"],"kind":v["kind"],
                "title":"Finish your prediction","detail":"Use the money already set aside","symbol":v["quote"]["outcome"]})).collect())
    }
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
fn number(value: &Value, key: &str) -> Result<u128, ApiError> {
    text(value, key).parse().map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "Predictions returned an invalid amount".into(),
        )
    })
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        now(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}
fn conflict(reason: impl Into<String>) -> ApiError {
    (StatusCode::CONFLICT, reason.into())
}
fn money(units: u128, currency: &str, rate: u128) -> Value {
    json!({"amount":markets::format_units(units.saturating_mul(rate)/1_000_000,6),"currency":currency})
}
fn venue_money(value: &str, currency: &str, rate: u128) -> Result<Value, ApiError> {
    let usd: f64 = value
        .parse()
        .map_err(|_| internal("Position value unavailable"))?;
    if !usd.is_finite() {
        return Err(internal("Position value unavailable"));
    }
    Ok(json!({"amount":format!("{:.6}",usd*rate as f64/1_000_000.0),"currency":currency}))
}
async fn bridge(
    state: &AppState,
    headers: &HeaderMap,
    route: &str,
    mut data: Value,
) -> Result<Value, ApiError> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err(conflict("Predictions needs your Atlas wallet"));
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "Sign in to Atlas".into()))?;
    data["accessToken"] = json!(token);
    let response = http
        .post(format!("{bridge_url}/predictions/{route}"))
        .json(&data)
        .send()
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_GATEWAY,
                "Predictions did not answer. Check Activity before trying again.".into(),
            )
        })?;
    let response_status = response.status();
    let ok = response_status.is_success();
    let body: Value = response
        .json()
        .await
        .map_err(|_| internal("Predictions response unavailable"))?;
    if !ok {
        if let Some(minimum) = body["minimumUnits"]
            .as_str()
            .and_then(|v| v.parse::<u128>().ok())
        {
            let currency = text(&data, "currency");
            let currency = if currency.is_empty() { "NGN" } else { currency };
            let rate = app_balance::fx_rate(currency).await?;
            return Err(conflict(format!(
                "This needs at least {}.",
                markets::say_money(minimum, currency, rate)
            )));
        }
        return Err((
            if response_status.is_server_error() {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::CONFLICT
            },
            body["error"]
                .as_str()
                .unwrap_or("Predictions unavailable")
                .into(),
        ));
    }
    Ok(body)
}
#[derive(Deserialize)]
pub(super) struct Browse {
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: u32,
}
pub(super) async fn markets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<Browse>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        bridge(
            &state,
            &headers,
            "markets",
            json!({"q":query.q,"offset":query.offset}),
        )
        .await?,
    ))
}
pub(super) async fn market(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        bridge(&state, &headers, "market", json!({"marketId":id})).await?,
    ))
}
pub(super) async fn availability(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        bridge(&state, &headers, "availability", json!({})).await?,
    ))
}
#[derive(Deserialize)]
pub(super) struct Currency {
    currency: Option<String>,
}
pub(super) async fn account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<Currency>,
) -> Result<Json<Value>, ApiError> {
    let currency = query.currency.as_deref().unwrap_or("NGN");
    markets::checked_currency(currency)?;
    let rate = app_balance::fx_rate(currency).await?;
    let mut data = bridge(&state, &headers, "account", json!({})).await?;
    data["cash"] = money(number(&data, "cashUnits")?, currency, rate);
    for p in data["positions"].as_array_mut().into_iter().flatten() {
        p["value"] = venue_money(text(p, "valueUsd"), currency, rate)?;
        p["pnl"] = venue_money(text(p, "pnlUsd"), currency, rate)?;
    }
    Ok(Json(data))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Request {
    market_id: Option<String>,
    token_id: Option<String>,
    side: String,
    amount: Option<Amount>,
    shares: Option<String>,
    from: Option<String>,
    geo_allowed: bool,
}
#[derive(Deserialize)]
pub(super) struct Amount {
    amount: String,
    currency: String,
}
pub(super) async fn quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Request>,
) -> Result<Json<Value>, ApiError> {
    if !input.geo_allowed {
        return Err((
            StatusCode::FORBIDDEN,
            "Predictions is unavailable in your location".into(),
        ));
    }
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let amount = input.amount.ok_or_else(|| conflict("Choose an amount"))?;
    let currency = amount.currency.as_str();
    markets::checked_currency(currency)?;
    let rate = app_balance::fx_rate(currency).await?;
    let requested = markets::parse_micros(&amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| conflict("Amount too large"))?
        / rate;
    if matches!(input.side.as_str(), "buy" | "withdraw") {
        markets::check_limits(requested, currency, rate)?;
    }
    let account = bridge(&state, &headers, "account", json!({})).await?;
    let cash = number(&account, "cashUnits")?;
    let mut q = if input.side == "redeem" {
        bridge(
            &state,
            &headers,
            "redemption",
            json!({"tokenId":input.token_id}),
        )
        .await?
    } else if input.side == "withdraw" {
        let from = input
            .from
            .as_deref()
            .unwrap_or(if user.solana_wallet.is_some() {
                "solana"
            } else {
                "base"
            });
        if requested > cash {
            return Err(markets::not_enough_cash(cash, currency, rate));
        }
        let mut q = bridge(
            &state,
            &headers,
            "bridge-quote",
            json!({"units":requested.to_string(),"from":from,"withdraw":true,"currency":currency}),
        )
        .await?;
        q["side"] = json!("withdraw");
        q["maximumSpend"] = json!(requested.to_string());
        q["minimumReceive"] = q["receiveUnits"].clone();
        q["question"] = json!("Return Predictions cash");
        q["expiresAtUnixMs"] = json!(now() + 60_000);
        q
    } else {
        if !matches!(input.side.as_str(), "buy" | "sell") {
            return Err(conflict("Choose buy or sell"));
        }
        let units = if input.side == "sell" {
            markets::parse_micros(
                input
                    .shares
                    .as_deref()
                    .ok_or_else(|| conflict("Choose how many shares to sell"))?,
            )?
        } else {
            requested
        };
        bridge(
            &state,
            &headers,
            "preview",
            json!({"marketId":input.market_id,"tokenId":input.token_id,
            "side":input.side,"units":units.to_string(),"currency":currency}),
        )
        .await?
    };
    if input.side == "sell" {
        markets::check_limits(number(&q, "notional")?, currency, rate)?;
    }
    q["owner"] = json!(user.user_id);
    q["currency"] = json!(currency);
    q["rate"] = json!(rate.to_string());
    q["cashBefore"] = json!(cash.to_string());
    q["funding"] = Value::Null;
    if input.side == "buy" && cash < number(&q, "maximumSpend")? {
        // Keep the whole user budget: the bridge fee comes out before the order, never on top.
        let shortfall = requested.saturating_sub(cash);
        let evm = user
            .evm_wallet
            .as_deref()
            .ok_or_else(|| conflict("Your Atlas wallet is not ready"))?;
        let base = state
            .markets
            .base
            .balance_of(BASE_USDC, evm)
            .await
            .map_err(internal)?;
        let sol = if let Some(sol) = user.solana_wallet.as_deref() {
            markets::solana_cash(&state, sol).await
        } else {
            0
        };
        let from = if sol >= shortfall && user.solana_wallet.is_some() {
            "solana"
        } else if base >= shortfall {
            "base"
        } else {
            return Err(markets::not_enough_cash(base + sol + cash, currency, rate));
        };
        let reserve = funding_reserve(&state, &user, from).await?;
        let amount = shortfall
            .checked_sub(reserve)
            .filter(|v| *v > 0)
            .ok_or_else(|| markets::short_of_gas(currency, rate))?;
        let mut funding = bridge(
            &state,
            &headers,
            "bridge-quote",
            json!({"units":amount.to_string(),"from":from,"currency":currency}),
        )
        .await?;
        funding["gasReserve"] = json!(reserve.to_string());
        let effective = cash + number(&funding, "receiveUnits")?;
        q = bridge(
            &state,
            &headers,
            "preview",
            json!({"marketId":input.market_id,"tokenId":input.token_id,
            "side":"buy","units":effective.to_string(),"currency":currency}),
        )
        .await?;
        q["owner"] = json!(user.user_id);
        q["currency"] = json!(currency);
        q["rate"] = json!(rate.to_string());
        q["cashBefore"] = json!(cash.to_string());
        q["funding"] = funding;
    }
    let quote_id = id("prediction-quote");
    q["quoteId"] = json!(quote_id);
    q["spendBudget"] = json!(requested.to_string());
    state.predictions.put(&quote_id, &q).await?;
    let pay = if input.side == "buy" && !q["funding"].is_null() {
        requested
    } else {
        number(&q, "maximumSpend")?
    };
    let received = if input.side == "buy" {
        number(&q, "shares")?
    } else {
        number(&q, "minimumReceive")?
    };
    let fee = number(&q, "feeUnits")?
        + q["funding"]["feeUnits"]
            .as_str()
            .and_then(|v| v.parse::<u128>().ok())
            .unwrap_or(0);
    Ok(Json(
        json!({"quoteId":quote_id,"marketId":q["marketId"],"tokenId":q["tokenId"],"side":input.side,
        "question":q["question"],"outcome":q["outcome"],"shares":q["shares"],
        "pay":money(pay,currency,rate),"receive":money(received,currency,rate),
        "potentialPayout":if input.side=="buy"{money(received,currency,rate)}else{Value::Null},
        "price":money(q["limit"].as_str().and_then(|s|s.parse().ok()).unwrap_or(0),currency,rate),
        "fee":money(fee,currency,rate),
        "gasReserve":money(q["funding"]["gasReserve"].as_str().and_then(|s|s.parse().ok()).unwrap_or(0),currency,rate),
        "expiresAtUnixMs":q["expiresAtUnixMs"]}),
    ))
}
async fn prepare(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    current: &mut Value,
    kind: &str,
) -> Result<(), ApiError> {
    let q = &current["quote"];
    let result = bridge(state,headers,"prepare",json!({"intentId":id,"kind":kind,"quote":q,
        "units":q["units"],"from":q["from"],"minimumOut":q["minimumReceive"],"currency":q["currency"]})).await?;
    current["approval"] = result;
    current["step"] = json!(kind);
    current["status"]["stage"] = json!("sign");
    Ok(())
}
pub(super) async fn execute(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(quote_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let q = state.predictions.get(&quote_id, &user.user_id).await?;
    if let Some(existing) = q["intentId"].as_str() {
        let current = state.predictions.get(existing, &user.user_id).await?;
        return Ok(Json(plan(existing, &current)));
    }
    if q["expiresAtUnixMs"].as_u64().unwrap_or(0) <= now() {
        return Err(conflict("Price expired. Request a new quote."));
    }
    let intent_id = id("prediction");
    let mut intent = json!({"owner":user.user_id,"quote":q,"kind":if q["side"]=="withdraw"{"send"}else if q["side"]=="redeem"{"sell"}else{text(&q,"side")},
        "step":"auth","status":{"intentId":intent_id,"stage":"validate","state":"pending","txIds":[],"error":null},
        "credentials":null,"child":null,"progress":null});
    prepare(&state, &headers, &intent_id, &mut intent, "auth").await?;
    intent["status"]["stage"] = json!("validate");
    state.predictions.release_previews(&user.user_id).await?;
    state.predictions.put(&intent_id, &intent).await?;
    let mut marked = q.clone();
    marked["intentId"] = json!(intent_id);
    if !state.predictions.replace(&quote_id, &q, &marked).await? {
        intent["status"]["state"] = json!("failed");
        state.predictions.put(&intent_id, &intent).await?;
        return Err(conflict("Quote is already being confirmed"));
    }
    let plan = plan(&intent_id, &intent);
    let mut receipt = transactions::Receipt::plan(&user.user_id, &plan);
    receipt.title = format!("Prediction · {}", text(&q, "question"));
    receipt.icon_url = q["iconUrl"].as_str().map(str::to_string);
    receipt.set_usdc(number(&q, "maximumSpend")?);
    state.history.put(&receipt).await?;
    Ok(Json(plan))
}
fn plan(id: &str, current: &Value) -> Value {
    let q = &current["quote"];
    let currency = text(q, "currency");
    let rate = text(q, "rate").parse().unwrap_or(1_000_000);
    let mut summary = vec![
        json!({"label":"Market","value":q["question"]}),
        json!({"label":"Your choice","value":q["outcome"].as_str().unwrap_or("Return cash")}),
    ];
    if q["side"] == "buy" {
        let payment = if q["funding"].is_null() {
            number(q, "maximumSpend").unwrap_or(0)
        } else {
            number(q, "spendBudget").unwrap_or(0)
        };
        summary.push(json!({"label":"You pay","value":markets::say_money(payment,currency,rate)}));
        summary.push(json!({"label":"If your choice wins","value":markets::say_money(number(q,"shares").unwrap_or(0),currency,rate)}));
    } else {
        summary.push(json!({"label":"You receive","value":markets::say_money(number(q,"minimumReceive").unwrap_or(0),currency,rate)}));
    }
    let fees = number(q, "feeUnits").unwrap_or(0)
        + q["funding"]["feeUnits"]
            .as_str()
            .and_then(|v| v.parse::<u128>().ok())
            .unwrap_or(0);
    summary.push(json!({"label":"Fees included","value":markets::say_money(fees,currency,rate)}));
    if let Some(reserve) = q["funding"]["gasReserve"]
        .as_str()
        .and_then(|v| v.parse::<u128>().ok())
        .filter(|v| *v > 0)
    {
        summary.push(json!({"label":"Network fee reserve included","value":markets::say_money(reserve,currency,rate)}));
    }
    json!({"intentId":id,"kind":current["kind"],"stage":current["status"]["stage"],"summary":summary,
        "transactions":current["approval"]["transactions"],"expiresAtUnixMs":current["approval"]["expiresAtUnixMs"]})
}
async fn owned(state: &AppState, headers: &HeaderMap, id: &str) -> Result<Value, ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    state.predictions.get(id, &user.user_id).await
}
fn status_of(value: &Value) -> Result<markets::IntentStatus, ApiError> {
    serde_json::from_value(value["status"].clone()).map_err(internal)
}
async fn record(state: &AppState, id: &str, current: &Value) -> Result<(), ApiError> {
    if let Some(mut receipt) = state.history.find(text(current, "owner"), id).await? {
        receipt.set_state(
            text(&current["status"], "state"),
            text(&current["status"], "stage"),
            current["status"]["error"].as_str().map(str::to_string),
        );
        for tx in current["status"]["txIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            receipt.add_tx(tx.into());
        }
        state.history.put(&receipt).await?;
    }
    Ok(())
}
pub(super) async fn signed(
    state: AppState,
    headers: HeaderMap,
    id: String,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let old = owned(&state, &headers, &id).await?;
    if old["status"]["state"] != "pending"
        || !matches!(text(&old["status"], "stage"), "validate" | "sign")
    {
        return Ok(Json(status_of(&old)?));
    }
    let mut current = old.clone();
    if let Some(child) = old["child"].as_str().filter(|_| old["step"] == "funding") {
        // The child verifies only the planned cash transfer; the parent never guesses a tx hash.
        let result = Box::pin(markets::signed(
            State(state.clone()),
            Path(child.into()),
            headers.clone(),
            Json(body),
        ))
        .await?
        .0;
        current["status"]["stage"] = json!(if result.stage == "sign" {
            "sign"
        } else {
            "fund"
        });
        current["status"]["txIds"] = json!(result.tx_ids);
        if result.state == "failed" {
            current["status"]["state"] = json!("failed");
            current["status"]["error"] = json!(result.error);
        }
        if state.predictions.replace(&id, &old, &current).await? {
            record(&state, &id, &current).await?;
        } else {
            current = owned(&state, &headers, &id).await?;
        }
        return Ok(Json(status_of(&current)?));
    }
    if !body.sent.is_empty() || body.signed.len() != 1 || body.signed[0].index != 0 {
        return Err(conflict("Approval does not match this purchase"));
    }
    if old["approval"]["expiresAtUnixMs"].as_u64().unwrap_or(0) <= now() {
        return Err(conflict(
            "Approval expired. Your unused money is safe; finish from Predictions.",
        ));
    }
    bridge(
        &state,
        &headers,
        "check",
        json!({"intentId":id,"prepareId":old["approval"]["prepareId"],
        "signature":body.signed[0].transaction}),
    )
    .await?;
    current["status"]["stage"] = json!("execute");
    if old["step"] == "order" {
        current["step"] = json!("order_wait");
        current["progress"] = json!({"orderId":old["approval"]["expectedOrderId"]});
    }
    if !state.predictions.replace(&id, &old, &current).await? {
        return Ok(Json(status_of(&owned(&state, &headers, &id).await?)?));
    }
    let claimed = current.clone();
    let answer = bridge(
        &state,
        &headers,
        "commit",
        json!({"intentId":id,"prepareId":old["approval"]["prepareId"],
        "signature":body.signed[0].transaction,"credentials":old["credentials"]}),
    )
    .await;
    match answer {
        Ok(answer) => {
            let step = text(&old, "step");
            if step == "auth" {
                current["credentials"] = answer["credentials"].clone();
                if answer["deployId"].is_string() {
                    current["progress"] = json!({"relayId":answer["deployId"]});
                    current["step"] = json!("deploy");
                } else {
                    let kind = if current["quote"]["side"] == "withdraw" {
                        "withdraw"
                    } else if current["quote"]["side"] == "redeem" {
                        "redeem"
                    } else {
                        "setup"
                    };
                    if let Err((_, reason)) =
                        prepare(&state, &headers, &id, &mut current, kind).await
                    {
                        current["status"]["state"] = json!("failed");
                        current["status"]["error"] = json!(format!(
                            "{reason} Your unused cash and shares stay in Predictions."
                        ));
                    }
                }
            } else {
                current["progress"] = answer;
                current["step"] = json!(if step == "setup" {
                    "setup_wait"
                } else if step == "redeem" {
                    "redeem_wait"
                } else if step == "withdraw" {
                    "withdraw_wait"
                } else {
                    "order_wait"
                });
            }
            if current["status"]["stage"] != "sign" {
                current["status"]["stage"] = json!("settle");
            }
        }
        Err((code, reason)) => {
            // Submission may have reached the venue. Never re-send a prepared approval.
            current["status"]["error"] = json!(format!(
                "{reason} Check Predictions cash and Activity before trying again."
            ));
            if code == StatusCode::BAD_GATEWAY && old["step"] == "order" {
                current["status"]["stage"] = json!("settle");
            } else {
                current["status"]["state"] = json!("failed");
            }
        }
    }
    if state.predictions.replace(&id, &claimed, &current).await? {
        record(&state, &id, &current).await?;
    } else {
        current = owned(&state, &headers, &id).await?;
    }
    Ok(Json(status_of(&current)?))
}
// The same half-dollar top-up used by Atlas's funding helpers stays inside the user's budget.
async fn funding_reserve(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    from: &str,
) -> Result<u128, ApiError> {
    let pays = if from == "solana" {
        let wallet = user
            .solana_wallet
            .as_deref()
            .ok_or_else(|| conflict("Cash wallet unavailable"))?;
        state
            .solana_mainnet
            .owner_sol_balance(wallet)
            .await
            .map_err(internal)?
            >= 3_000_000
    } else {
        let wallet = user
            .evm_wallet
            .as_deref()
            .ok_or_else(|| conflict("Cash wallet unavailable"))?;
        markets::base_eth(state, wallet)
            .await
            .ok_or_else(|| internal("The network fee could not be checked"))?
            >= 40_000_000_000_000
    };
    Ok(if pays {
        0
    } else {
        markets::BASE_GAS_REFILL_USDC
    })
}
async fn funding(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    current: &mut Value,
) -> Result<(), ApiError> {
    let q = current["quote"].clone();
    if q["funding"].is_null() {
        prepare(state, headers, id, current, "order").await?;
        return Ok(());
    }
    let user = app_balance::verified_wallets(state, headers).await?;
    if funding_reserve(state, &user, text(&q["funding"], "from")).await?
        > number(&q["funding"], "gasReserve")?
    {
        return Err(conflict("The network fee reserve changed. Request a new quote; nothing was paid for this purchase."));
    }
    let deposit = bridge(state, headers, "deposit", json!({})).await?;
    let amount = number(&q["funding"], "units")?;
    let (child, steps) = if q["funding"]["from"] == "solana" {
        markets::plan_solana_transfer(
            state,
            user.user_id,
            user.solana_wallet
                .ok_or_else(|| conflict("Cash wallet unavailable"))?,
            text(&deposit, "svm"),
            amount,
        )
        .await?
    } else {
        let evm = user
            .evm_wallet
            .ok_or_else(|| conflict("Cash wallet unavailable"))?;
        let recipient = text(&deposit, "evm");
        if recipient.len() != 42
            || !recipient.starts_with("0x")
            || !recipient[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(conflict("Deposit address unavailable"));
        }
        let transfer = format!("0xa9059cbb{:0>64}{:064x}", &recipient[2..], amount);
        let (child, steps, _) = markets::plan_base_with_cash(
            state,
            user.user_id,
            evm,
            user.solana_wallet,
            vec![(BASE_USDC.into(), transfer)],
            amount,
            text(&q, "currency"),
            text(&q, "rate").parse().unwrap_or(1_000_000),
        )
        .await?;
        (child, steps)
    };
    current["fundingStartedAt"] = json!(now());
    current["child"] = json!(child);
    current["depositAddress"] = json!(if q["funding"]["from"] == "solana" {
        text(&deposit, "svm")
    } else {
        text(&deposit, "evm")
    });
    current["step"] = json!("funding");
    current["status"]["stage"] = json!("sign");
    current["approval"] = json!({"transactions":steps,"expiresAtUnixMs":now()+120_000});
    Ok(())
}
fn bridge_matches(row: &Value, intent: &Value, withdraw: bool) -> bool {
    let q = &intent["quote"];
    let source = if withdraw {
        "137"
    } else if q["funding"]["from"] == "solana" {
        "1151111081099710"
    } else {
        "8453"
    };
    let destination = if !withdraw {
        "137"
    } else if q["from"] == "solana" {
        "1151111081099710"
    } else {
        "8453"
    };
    let amount = if withdraw {
        text(q, "units")
    } else {
        text(&q["funding"], "units")
    };
    let started = if withdraw {
        intent["approval"]["expiresAtUnixMs"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(180_000)
    } else {
        intent["fundingStartedAt"].as_u64().unwrap_or(u64::MAX)
    };
    text(row, "fromChainId") == source
        && text(row, "toChainId") == destination
        && text(row, "fromAmountBaseUnit") == amount
        && text(row, "fromTokenAddress").eq_ignore_ascii_case(if withdraw {
            "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB"
        } else if source == "8453" {
            BASE_USDC
        } else {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        })
        && text(row, "toTokenAddress").eq_ignore_ascii_case(if !withdraw {
            "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB"
        } else if destination == "8453" {
            BASE_USDC
        } else {
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        })
        && row["createdTimeMs"]
            .as_u64()
            .is_some_and(|t| t + 30_000 >= started)
}
pub(super) async fn status(
    state: AppState,
    headers: HeaderMap,
    id: String,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let old = owned(&state, &headers, &id).await?;
    let mut current = old.clone();
    if current["status"]["state"] != "pending"
        || matches!(text(&current["status"], "stage"), "sign" | "validate")
    {
        return Ok(Json(status_of(&current)?));
    }
    let step = text(&current, "step").to_string();
    if !matches!(
        step.as_str(),
        "funding"
            | "deploy"
            | "setup_wait"
            | "withdraw_wait"
            | "withdraw_bridge"
            | "order_wait"
            | "redeem_wait"
    ) {
        return Ok(Json(status_of(&current)?));
    }
    if step == "funding" {
        let child = text(&current, "child");
        let child = Box::pin(markets::intent_status(
            State(state.clone()),
            Path(child.into()),
            headers.clone(),
        ))
        .await?
        .0;
        current["status"]["txIds"] = json!(child.tx_ids);
        if child.state == "failed" {
            current["status"]["state"] = json!("failed");
            current["status"]["error"] = json!(child.error);
        } else if child.stage == "sign" {
            current["status"]["stage"] = json!("sign");
        } else if child.state == "filled" {
            let result = bridge(
                &state,
                &headers,
                "progress",
                json!({"depositAddress":current["depositAddress"]}),
            )
            .await?;
            let matched = result["bridge"]["transactions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| bridge_matches(r, &current, false));
            if matched.is_some_and(|r| r["status"] == "COMPLETED") {
                let account = bridge(&state, &headers, "account", json!({})).await?;
                if number(&account, "cashUnits")? >= number(&current["quote"], "maximumSpend")? {
                    let prepared = prepare(&state, &headers, &id, &mut current, "order").await;
                    if let Err((code, reason)) = prepared {
                        current["status"]["error"] = json!(reason);
                        if code == StatusCode::CONFLICT {
                            current["status"]["state"] = json!("failed");
                        }
                    }
                }
            } else if matched.is_some_and(|r| r["status"] == "FAILED") {
                current["status"]["state"] = json!("failed");
                current["status"]["error"] =
                    json!("Your cash transfer needs review. Follow it in Activity.");
            }
            if current["status"]["stage"] != "sign" {
                current["status"]["stage"] = json!("fund");
            }
        }
    } else if step == "withdraw_bridge" {
        let result = bridge(
            &state,
            &headers,
            "progress",
            json!({"depositAddress":current["progress"]["depositAddress"]}),
        )
        .await?;
        let rows = result["bridge"]["transactions"].as_array();
        if rows.is_some_and(|rows| {
            rows.iter()
                .any(|r| bridge_matches(r, &current, true) && r["status"] == "COMPLETED")
        }) {
            current["status"]["state"] = json!("filled");
            current["status"]["stage"] = json!("settle");
            for row in rows
                .into_iter()
                .flatten()
                .filter(|r| bridge_matches(r, &old, true) && r["status"] == "COMPLETED")
            {
                if let Some(tx) = row["txHash"].as_str() {
                    if let Some(txs) = current["status"]["txIds"].as_array_mut() {
                        let tx = json!(tx);
                        if !txs.contains(&tx) {
                            txs.push(tx);
                        }
                    }
                }
            }
        } else if rows.is_some_and(|rows| {
            rows.iter()
                .any(|r| bridge_matches(r, &current, true) && r["status"] == "FAILED")
        }) {
            current["status"]["state"] = json!("failed");
            current["status"]["error"] = json!("The cash return needs review. Check Activity.");
        }
    } else {
        let mut input = current["progress"].clone();
        input["credentials"] = current["credentials"].clone();
        input["quote"] = current["quote"].clone();
        let result = bridge(&state, &headers, "progress", input).await?;
        if let Some(txs) = result["txIds"].as_array() {
            let existing = current["status"]["txIds"].as_array_mut().expect("tx ids");
            for tx in txs {
                if !existing.contains(tx) {
                    existing.push(tx.clone());
                }
            }
        }
        if result["state"] == "filled" {
            match step.as_str() {
                "deploy" => {
                    let kind = if current["quote"]["side"] == "withdraw" {
                        "withdraw"
                    } else if current["quote"]["side"] == "redeem" {
                        "redeem"
                    } else {
                        "setup"
                    };
                    if let Err((_, reason)) =
                        prepare(&state, &headers, &id, &mut current, kind).await
                    {
                        current["status"]["state"] = json!("failed");
                        current["status"]["error"] = json!(format!(
                            "{reason} Your unused cash and shares stay in Predictions."
                        ));
                    }
                }
                "setup_wait" => {
                    if let Err((_, reason)) = funding(&state, &headers, &id, &mut current).await {
                        current["status"]["state"] = json!("failed");
                        current["status"]["error"] = json!(format!("{reason} No purchase was submitted. Check your cash in Predictions and Activity."));
                    }
                }
                "withdraw_wait" => {
                    current["step"] = json!("withdraw_bridge");
                    current["status"]["stage"] = json!("fund");
                }
                "redeem_wait" | "order_wait" => {
                    current["status"]["state"] = json!("filled");
                    current["status"]["stage"] = json!("settle");
                }
                _ => {}
            }
        } else if result["state"] == "failed" {
            current["status"]["state"] = json!("failed");
            current["status"]["error"] =
                json!("The action did not settle. Your unused cash or shares are in Predictions.");
        }
    }
    if state.predictions.replace(&id, &old, &current).await? {
        record(&state, &id, &current).await?;
    } else {
        current = owned(&state, &headers, &id).await?;
    }
    Ok(Json(status_of(&current)?))
}
pub(super) async fn next(
    state: AppState,
    headers: HeaderMap,
    id: String,
) -> Result<Json<Value>, ApiError> {
    let old = owned(&state, &headers, &id).await?;
    let mut current = old.clone();
    if current["status"]["stage"] != "sign" || current["status"]["state"] != "pending" {
        return Err(conflict("Nothing waiting to sign"));
    }
    if current["step"] == "funding" {
        return Box::pin(markets::next_transactions(
            State(state.clone()),
            Path(text(&current, "child").into()),
            headers,
        ))
        .await;
    }
    if current["approval"]["expiresAtUnixMs"].as_u64().unwrap_or(0) <= now() {
        let step = text(&current, "step").to_string();
        prepare(&state, &headers, &id, &mut current, &step).await?;
        if state.predictions.replace(&id, &old, &current).await? {
            record(&state, &id, &current).await?;
        } else {
            current = owned(&state, &headers, &id).await?;
        }
    }
    Ok(Json(
        json!({"kind":current["kind"],"transactions":current["approval"]["transactions"]}),
    ))
}
pub(super) async fn resume(
    state: AppState,
    headers: HeaderMap,
    id: String,
) -> Result<Json<Value>, ApiError> {
    let current = owned(&state, &headers, &id).await?;
    if current["status"]["stage"] != "sign" {
        return Err(conflict("This action is still processing. Check Activity."));
    }
    let steps = next(state, headers, id.clone()).await?.0;
    let mut plan = plan(&id, &current);
    plan["stage"] = json!("sign");
    plan["transactions"] = steps["transactions"].clone();
    plan["expiresAtUnixMs"] = json!(now() + 60_000);
    Ok(Json(plan))
}
pub(super) async fn portfolio(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
) -> Result<Vec<Value>, ApiError> {
    if !state.predictions.has_activity(owner).await? {
        return Ok(vec![]);
    }
    let account = bridge(state, headers, "account", json!({})).await?;
    let mut rows = Vec::new();
    let cash = number(&account, "cashUnits")?;
    if cash > 0 {
        rows.push(json!({"assetId":"prediction:cash","symbol":"CASH","name":"Predictions cash",
        "kind":"cash","amount":markets::format_units(cash,6),"units":cash.to_string(),"iconUrl":null}));
    }
    for p in account["positions"].as_array().into_iter().flatten() {
        let value: f64 = text(p, "valueUsd")
            .parse()
            .map_err(|_| internal("Prediction value unavailable"))?;
        if !value.is_finite() || value < 0.0 || value > u128::MAX as f64 / 1_000_000.0 {
            return Err(internal("Prediction value unavailable"));
        }
        rows.push(json!({"assetId":format!("prediction:{}:{}",text(p,"marketId"),text(p,"tokenId")),
            "symbol":p["outcome"],"name":p["question"],"kind":"crypto","amount":p["shares"],"units":((value*1_000_000.0) as u128).to_string(),"iconUrl":p["iconUrl"]}));
    }
    Ok(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_report_can_only_claim_its_exact_preparation() {
        let store = PredictionState {
            pg: None,
            memory: Arc::new(Mutex::new(HashMap::new())),
        };
        let old = json!({"owner":"a","approval":"one","stage":"sign"});
        store.put("prediction-one", &old).await.unwrap();
        let new = json!({"owner":"a","approval":"two","stage":"sign"});
        assert!(store.replace("prediction-one", &old, &new).await.unwrap());
        assert!(!store.replace("prediction-one", &old, &new).await.unwrap());
        assert!(store.get("prediction-one", "b").await.is_err());
    }
    #[tokio::test]
    async fn a_cancelled_preview_releases_only_its_unconfirmed_slot() {
        let store = PredictionState {
            pg: None,
            memory: Arc::new(Mutex::new(HashMap::new())),
        };
        let preview = json!({"owner":"a","status":{"state":"pending","stage":"validate"}});
        store.put("prediction-first", &preview).await.unwrap();
        assert!(store.put("prediction-second", &preview).await.is_err());
        store.release_previews("a").await.unwrap();
        store.put("prediction-second", &preview).await.unwrap();
        let mut confirmed = preview.clone();
        confirmed["status"]["stage"] = json!("execute");
        assert!(store
            .replace("prediction-second", &preview, &confirmed)
            .await
            .unwrap());
        store.release_previews("a").await.unwrap();
        assert!(store.put("prediction-third", &preview).await.is_err());
    }
    #[test]
    fn bridge_completion_must_match_this_payment_and_tokens() {
        let intent = json!({"quote":{"funding":{"from":"base","units":"10000000"}},"fundingStartedAt":100000});
        let row = json!({"fromChainId":"8453","toChainId":"137","fromAmountBaseUnit":"10000000",
            "fromTokenAddress":BASE_USDC,"toTokenAddress":"0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB","createdTimeMs":100010});
        assert!(bridge_matches(&row, &intent, false));
        for (key, value) in [
            ("fromChainId", json!("1")),
            ("toTokenAddress", json!(BASE_USDC)),
            ("fromAmountBaseUnit", json!("20000000")),
            ("createdTimeMs", json!(60000)),
        ] {
            let mut changed = row.clone();
            changed[key] = value;
            assert!(!bridge_matches(&changed, &intent, false));
        }
    }
    #[test]
    fn display_money_keeps_decimal_strings() {
        assert_eq!(
            money(1234567, "NGN", 1_600_000_000),
            json!({"amount":"1975.3072","currency":"NGN"})
        );
    }
}
