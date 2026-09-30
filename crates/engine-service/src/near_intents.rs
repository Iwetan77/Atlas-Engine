use super::*;
use engine_execution::near_intents::{Client, QuoteRequest, Token};
use engine_execution::swaps::uniswap::BASE_USDC;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
const BASE_USDC_1CLICK: &str = "nep141:base-0x833589fcd6edb6e08f4c7c32d4f71b54bda02913.omft.near";
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(super) struct NearState {
    client: Client,
    quotes: Arc<Mutex<HashMap<String, StoredQuote>>>,
    intents: Arc<Mutex<HashMap<String, StoredIntent>>>,
    tokens: Arc<Mutex<Option<(Instant, Vec<Token>)>>>,
    icons: Arc<Mutex<HashMap<String, String>>>,
    icon_http: reqwest::Client,
    monad_rpc: reqwest::Url,
    postgres: Option<Arc<tokio_postgres::Client>>,
}
#[derive(Clone)]
struct StoredQuote {
    owner: String,
    wallet: String,
    asset: Token,
    amount: u128,
    minimum_out: u128,
    currency: String,
    display_amount: String,
    expires: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct StoredIntent {
    owner: String,
    wallet: String,
    expected_to: String,
    expected_data: String,
    deposit_address: String,
    deposit_memo: Option<String>,
    #[serde(default)]
    asset: Option<Token>,
    expires: u64,
    status: markets::IntentStatus,
}
impl NearState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("near intents database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_near_intents (
                intent_id TEXT PRIMARY KEY, owner TEXT NOT NULL, payload TEXT NOT NULL,
                stage TEXT NOT NULL, expires_at_ms BIGINT NOT NULL)",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            client: Client::new(env::var("NEAR_INTENTS_API_KEY").ok())?,
            quotes: Arc::new(Mutex::new(HashMap::new())),
            intents: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(None)),
            icons: Arc::new(Mutex::new(HashMap::new())),
            icon_http: reqwest::Client::builder()
                .timeout(Duration::from_secs(6))
                .build()?,
            postgres,
            monad_rpc: env::var("ATLAS_MONAD_MAINNET_RPC_URL")
                .unwrap_or_else(|_| "https://rpc.monad.xyz".into())
                .parse()?,
        })
    }
    async fn tokens(&self) -> Result<Vec<Token>, ApiError> {
        if let Some((at, list)) = self.tokens.lock().map_err(internal)?.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(list.clone());
            }
        }
        let list = self.client.tokens().await.map_err(venue)?;
        // CoinGecko IDs are supplied by 1Click. Fetch images in one bounded request.
        let ids: Vec<&str> = list
            .iter()
            .filter(|t| supported(t))
            .filter_map(|t| t.coingecko_id.as_deref())
            .collect();
        if !ids.is_empty() {
            if let Ok(response) = self
                .icon_http
                .get("https://api.coingecko.com/api/v3/coins/markets")
                .query(&[("vs_currency", "usd"), ("ids", &ids.join(","))])
                .send()
                .await
            {
                if let Ok(rows) = response.json::<Vec<Value>>().await {
                    let mut icons = self.icons.lock().map_err(internal)?;
                    for row in rows {
                        if let (Some(id), Some(url)) = (row["id"].as_str(), row["image"].as_str()) {
                            if url.starts_with("https://coin-images.coingecko.com/") {
                                icons.insert(id.into(), url.into());
                            }
                        }
                    }
                }
            }
        }
        *self.tokens.lock().map_err(internal)? = Some((Instant::now(), list.clone()));
        Ok(list)
    }
    async fn insert_intent(&self, id: &str, intent: StoredIntent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            pg.execute("INSERT INTO atlas_near_intents (intent_id,owner,payload,stage,expires_at_ms) VALUES ($1,$2,$3,$4,$5)",
                &[&id,&intent.owner,&payload,&intent.status.stage,&i64::try_from(intent.expires).map_err(internal)?])
                .await.map_err(internal)?;
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .insert(id.into(), intent);
        }
        Ok(())
    }
    async fn get_intent(&self, id: &str) -> Result<Option<StoredIntent>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT payload FROM atlas_near_intents WHERE intent_id=$1",
                    &[&id],
                )
                .await
                .map_err(internal)?;
            return row
                .map(|r| {
                    serde_json::from_str::<StoredIntent>(r.get::<_, &str>(0)).map_err(internal)
                })
                .transpose();
        }
        Ok(self.intents.lock().map_err(internal)?.get(id).cloned())
    }
    async fn save_intent(&self, id: &str, intent: StoredIntent) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            pg.execute(
                "UPDATE atlas_near_intents SET payload=$2,stage=$3 WHERE intent_id=$1",
                &[&id, &payload, &intent.status.stage],
            )
            .await
            .map_err(internal)?;
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .insert(id.into(), intent);
        }
        Ok(())
    }
    pub(super) async fn monad_holdings(
        &self,
        owner: &str,
        wallet: &str,
    ) -> Result<Vec<(Token, u128)>, ApiError> {
        let intents: Vec<StoredIntent> = if let Some(pg) = &self.postgres {
            pg.query(
                "SELECT payload FROM atlas_near_intents WHERE owner=$1",
                &[&owner],
            )
            .await
            .map_err(internal)?
            .into_iter()
            .map(|row| {
                serde_json::from_str::<StoredIntent>(row.get::<_, &str>(0)).map_err(internal)
            })
            .collect::<Result<_, _>>()?
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .values()
                .filter(|i| i.owner == owner)
                .cloned()
                .collect()
        };
        let mut assets = HashMap::<String, Token>::new();
        for intent in intents {
            if intent.status.state == "filled" {
                if let Some(asset) = intent.asset.filter(|a| a.blockchain == "monad") {
                    assets.insert(asset.asset_id.clone(), asset);
                }
            }
        }
        if assets.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.tokens().await?;
        for asset in assets.values_mut() {
            let fresh = catalog
                .iter()
                .find(|t| t.asset_id == asset.asset_id)
                .ok_or_else(|| venue(format!("no live 1Click price for {}", asset.symbol)))?;
            *asset = fresh.clone();
        }
        let chain = self.monad_call("eth_chainId", json!([])).await?;
        if chain.as_str() != Some("0x8f") {
            return Err(venue("Monad RPC returned a different chain ID"));
        }
        let mut result = Vec::new();
        for asset in assets.into_values() {
            let value = if let Some(contract) = asset.contract_address.as_deref() {
                if !is_evm(contract) {
                    return Err(venue("invalid Monad token contract"));
                }
                let data = format!("0x70a08231{:0>64}", &wallet[2..].to_ascii_lowercase());
                self.monad_call("eth_call", json!([{"to":contract,"data":data},"latest"]))
                    .await?
            } else {
                self.monad_call("eth_getBalance", json!([wallet, "latest"]))
                    .await?
            };
            let hex = value
                .as_str()
                .and_then(|v| v.strip_prefix("0x"))
                .ok_or_else(|| venue("Monad RPC returned no balance"))?;
            let units = u128::from_str_radix(hex, 16).map_err(internal)?;
            if units > 0 {
                result.push((asset, units));
            }
        }
        Ok(result)
    }
    async fn monad_call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let response = self
            .icon_http
            .post(self.monad_rpc.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(venue)?;
        if !response.status().is_success() {
            return Err(venue(format!("Monad RPC HTTP {}", response.status())));
        }
        let body: Value = response.json().await.map_err(venue)?;
        if !body["error"].is_null() {
            return Err(venue("Monad RPC returned an error"));
        }
        Ok(body["result"].clone())
    }
    pub(super) fn icon_for(&self, asset: &Token) -> Option<String> {
        asset.coingecko_id.as_deref()
            .and_then(|id| self.icons.lock().ok().and_then(|m|m.get(id).cloned()))
            .or_else(||match asset.coingecko_id.as_deref() {
                Some("monad")=>Some("https://coin-images.coingecko.com/coins/images/38927/small/mon.png".into()),
                Some("sui")=>Some("https://coin-images.coingecko.com/coins/images/26375/small/sui-ocean-square.png".into()),
                _=>None
            })
    }
    pub(super) async fn planned_base_txs(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<Vec<(String, String)>, ApiError> {
        Ok(self
            .get_intent(id)
            .await?
            .filter(|i| i.owner == owner && i.status.stage == "validate" && i.expires > now())
            .map(|i| vec![(i.expected_to.clone(), i.expected_data.clone())])
            .unwrap_or_default())
    }
}
fn venue(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn conflict(message: &str) -> ApiError {
    (StatusCode::CONFLICT, message.into())
}
fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn id(prefix: &str) -> String {
    format!(
        "near-{prefix}-{}-{}",
        now(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn is_evm(address: &str) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 66 && hash.starts_with("0x") && hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
}
// UTC civil date from Unix days; no shell clock or local timezone dependence.
fn deadline_utc(seconds_from_now: u64) -> String {
    let unix = now() / 1000 + seconds_from_now;
    let z = (unix / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    if m <= 2 {
        y += 1;
    }
    let s = unix % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}
fn money(amount: u128, currency: &str, rate: u128) -> Value {
    let micros = amount.saturating_mul(rate) / 1_000_000;
    json!({"amount":markets::format_units(micros,6),"currency":currency})
}
fn usd_micros(value: &str) -> Result<u128, ApiError> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(venue("1Click returned an invalid USD valuation"));
    }
    let units = whole
        .parse::<u128>()
        .ok()
        .and_then(|n| n.checked_mul(1_000_000))
        .and_then(|n| {
            format!("{:0<6}", &fraction[..fraction.len().min(6)])
                .parse::<u128>()
                .ok()
                .and_then(|part| n.checked_add(part))
        })
        .ok_or_else(|| venue("1Click USD valuation is too large"))?;
    Ok(units)
}
fn unit_price(
    amount_in: u128,
    amount_out: u128,
    decimals: u32,
    currency: &str,
    rate: u128,
) -> Value {
    let scale = 10u128.checked_pow(decimals).unwrap_or(1);
    let value = amount_in.saturating_mul(scale).saturating_mul(rate) / amount_out.max(1);
    json!({"amount":markets::format_units(value,12),"currency":currency})
}
fn supported(token: &Token) -> bool {
    matches!(token.blockchain.as_str(), "monad" | "sui" | "near")
        && !matches!(token.symbol.as_str(), "USDC" | "USDT" | "USDT0")
}
fn recipient<'a>(
    token: &Token,
    user: &'a app_balance::VerifiedWallets,
) -> Result<&'a str, ApiError> {
    match token.blockchain.as_str() {
        "monad" => user
            .evm_wallet
            .as_deref()
            .filter(|w| is_evm(w))
            .ok_or_else(|| conflict("Privy EVM wallet is required for Monad")),
        "sui" => Err(conflict(
            "A verified Sui wallet is required before buying Sui assets",
        )),
        "near" => Err(conflict(
            "A verified NEAR wallet is required before buying NEAR assets",
        )),
        _ => Err(bad("unsupported destination chain")),
    }
}
pub(super) async fn search_assets(
    state: &AppState,
    query: &str,
    currency: &str,
    rate: u128,
) -> Result<Vec<Value>, ApiError> {
    let list = state.near.tokens().await?;
    let query = query.to_ascii_lowercase();
    let mut out = Vec::new();
    for t in list.iter().filter(|t| supported(t)) {
        if !query.is_empty()
            && !t.symbol.to_ascii_lowercase().contains(&query)
            && !t.blockchain.to_ascii_lowercase().contains(&query)
            && !t
                .contract_address
                .as_deref()
                .unwrap_or("")
                .eq_ignore_ascii_case(&query)
        {
            continue;
        }
        let usd = t
            .price
            .as_ref()
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_f64().map(|p| p.to_string()))
            })
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or_default();
        if usd <= 0.0 || !usd.is_finite() {
            continue;
        }
        let display = usd * (rate as f64 / 1_000_000.0);
        let icon = state.near.icon_for(t);
        out.push(
            json!({"assetId":format!("near:{}", t.asset_id),"symbol":t.symbol,
            "name":format!("{} on {}",t.symbol,t.blockchain),"kind":"crypto",
            "price":{"amount":format!("{display:.12}").trim_end_matches('0').trim_end_matches('.'),
                "currency":currency},"change24hPct":null,"iconUrl":icon,"verified":true}),
        );
        if out.len() >= 30 {
            break;
        }
    }
    Ok(out)
}
pub(super) async fn quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
) -> Result<Json<Value>, ApiError> {
    if req.side != "buy" {
        return Err(bad("1Click sell routing is not available yet"));
    }
    markets::checked_currency(&req.amount.currency)?;
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let token = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| format!("near:{}", t.asset_id) == req.asset_id && supported(t))
        .ok_or((StatusCode::NOT_FOUND, "1Click asset not found".into()))?;
    let destination = recipient(&token, &user)?;
    let wallet = user
        .evm_wallet
        .as_deref()
        .filter(|w| is_evm(w))
        .ok_or_else(|| conflict("Privy Base wallet is required"))?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let micros = markets::parse_micros(&req.amount.amount)?;
    let amount = micros
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(amount, &req.amount.currency, rate)?;
    // More than the cash on Base: say so at the quote, in their currency.
    if let Ok(held) = state.markets.base.balance_of(BASE_USDC, wallet).await {
        if held < amount {
            return Err(markets::short_of_cash(
                held,
                "Base",
                &req.amount.currency,
                rate,
            ));
        }
    }
    let deadline = deadline_utc(180);
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        BASE_USDC_1CLICK,
        &token.asset_id,
        &units,
        destination,
        wallet,
        &deadline,
        true,
    );
    let q = state.near.client.quote(&request).await.map_err(venue)?;
    let input: u128 = q.amount_in.parse().map_err(internal)?;
    let output: u128 = q.amount_out.parse().map_err(internal)?;
    let output_usd = usd_micros(&q.amount_out_usd)?;
    let minimum: u128 = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    if input != amount {
        return Err(venue("1Click changed the exact input amount"));
    }
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            wallet: wallet.into(),
            asset: token.clone(),
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            display_amount: req.amount.amount,
            expires,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"buy",
        "pay":{"amount":markets::format_units(input,6),"symbol":"USDC","value":money(input,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(output,token.decimals),"symbol":token.symbol,
            "value":money(output_usd,&req.amount.currency,rate)},
        "price":unit_price(input,output,token.decimals,&req.amount.currency,rate),
        "fee":{"amount":"0","currency":req.amount.currency},"expiresAtUnixMs":expires}),
    ))
}
pub(super) async fn execute(
    state: AppState,
    headers: HeaderMap,
    quote_id: String,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let stored = state
        .near
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "quote not found".into()))?;
    if stored.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "quote belongs to another user".into(),
        ));
    }
    if stored.expires < now() {
        return Err(conflict("quote expired; request a fresh quote"));
    }
    let destination = recipient(&stored.asset, &user)?;
    if user.evm_wallet.as_deref() != Some(&stored.wallet) {
        return Err(conflict("Privy wallet changed; request a fresh quote"));
    }
    // A quote can generate only one deposit plan, even if the client retries execute.
    if state
        .near
        .quotes
        .lock()
        .map_err(internal)?
        .remove(&quote_id)
        .is_none()
    {
        return Err(conflict("quote already used; request a fresh quote"));
    }
    let balance = state
        .markets
        .base
        .balance_of(BASE_USDC, &stored.wallet)
        .await
        .map_err(venue)?;
    if balance < stored.amount {
        let rate = app_balance::fx_rate(&stored.currency).await?;
        return Err(markets::short_of_cash(
            balance,
            "Base",
            &stored.currency,
            rate,
        ));
    }
    let deadline = deadline_utc(240);
    let amount = stored.amount.to_string();
    let req = QuoteRequest::exact_input(
        BASE_USDC_1CLICK,
        &stored.asset.asset_id,
        &amount,
        destination,
        &stored.wallet,
        &deadline,
        false,
    );
    let fresh = state.near.client.quote(&req).await.map_err(venue)?;
    if fresh.amount_in.parse::<u128>().ok() != Some(stored.amount)
        || fresh
            .min_amount_out
            .as_deref()
            .unwrap_or(&fresh.amount_out)
            .parse::<u128>()
            .unwrap_or_default()
            < stored.minimum_out.saturating_mul(99) / 100
    {
        return Err(conflict("route price changed; request a fresh quote"));
    }
    let deposit = fresh
        .deposit_address
        .ok_or_else(|| venue("1Click omitted deposit address"))?;
    if !is_evm(&deposit) || fresh.deposit_memo.is_some() {
        return Err(venue(
            "1Click returned an unsupported Base deposit destination",
        ));
    }
    let tx = state
        .markets
        .base
        .transfer_transaction(BASE_USDC, &stored.wallet, &deposit, stored.amount)
        .map_err(venue)?;
    let intent_id = id("intent");
    let status = markets::IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: Vec::new(),
        error: None,
    };
    let expires = now() + 120_000;
    state
        .near
        .insert_intent(
            &intent_id,
            StoredIntent {
                owner: user.user_id,
                wallet: stored.wallet,
                expected_to: tx.to.to_ascii_lowercase(),
                expected_data: tx.data.to_ascii_lowercase(),
                deposit_address: deposit,
                deposit_memo: fresh.deposit_memo,
                asset: Some(stored.asset.clone()),
                expires,
                status,
            },
        )
        .await?;
    Ok(Json(json!({"intentId":intent_id,"kind":"buy",
        "summary":[{"label":"Pay","value":format!("{} USDC",markets::format_units(stored.amount,6))},
        {"label":"Receive (estimated)","value":format!("{} {}",markets::format_units(
            fresh.amount_out.parse().map_err(internal)?,stored.asset.decimals),stored.asset.symbol)},
        {"label":"Destination","value":stored.asset.blockchain},
        {"label":"Requested value","value":stored.display_amount},
        {"label":"Display currency","value":stored.currency}],
        "transactions":[{"chain":"base","chainId":8453,"to":tx.to,"data":tx.data,"value":"0"}],
        "expiresAtUnixMs":expires})))
}
pub(super) async fn signed(
    state: AppState,
    headers: HeaderMap,
    id: String,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut current = state
        .near
        .get_intent(&id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    if current.status.stage != "validate" {
        return Ok(Json(current.status));
    }
    if !body.signed.is_empty()
        || body.sent.len() != 1
        || body.sent[0].chain != "base"
        || !valid_hash(&body.sent[0].id)
    {
        return Err(bad("signed report does not match 1Click deposit plan"));
    }
    // A sent transaction must still be tracked even if the reporting request arrives after expiry.
    current.status.stage = "settle".into();
    current.status.tx_ids = vec![body.sent[0].id.clone()];
    let result = current.status.clone();
    state.near.save_intent(&id, current).await?;
    Ok(Json(result))
}
pub(super) async fn status(
    state: AppState,
    headers: HeaderMap,
    id: String,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut current = state
        .near
        .get_intent(&id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    if current.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "intent belongs to another user".into(),
        ));
    }
    let mut result = current.status.clone();
    if result.stage != "settle" || result.state != "pending" {
        return Ok(Json(result));
    }
    let hash = &result.tx_ids[0];
    let tx = markets::base_rpc(&state.markets, "eth_getTransactionByHash", json!([hash])).await?;
    if tx.is_null() {
        return Ok(Json(result));
    }
    if !tx["from"]
        .as_str()
        .is_some_and(|v| v.eq_ignore_ascii_case(&current.wallet))
        || !tx["to"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(&current.expected_to))
        || !tx["input"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(&current.expected_data))
    {
        result.state = "failed".into();
        result.error = Some("reported transaction does not match the deposit plan".into());
    } else {
        let receipt =
            markets::base_rpc(&state.markets, "eth_getTransactionReceipt", json!([hash])).await?;
        if receipt.is_null() {
            return Ok(Json(result));
        }
        if receipt["status"].as_str() != Some("0x1") {
            result.state = "failed".into();
            result.error = Some("Base deposit reverted".into());
        } else {
            let venue_status = state
                .near
                .client
                .status(&current.deposit_address, current.deposit_memo.as_deref())
                .await
                .map_err(venue)?;
            match venue_status.status.as_str() {
                "SUCCESS" => {
                    let hashes = venue_status
                        .swap_details
                        .map(|s| {
                            s.destination_chain_tx_hashes
                                .into_iter()
                                .map(|tx| tx.hash)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if hashes.is_empty() {
                        return Ok(Json(result));
                    }
                    result.tx_ids.extend(hashes);
                    result.state = "filled".into();
                }
                "FAILED" | "REFUNDED" => {
                    result.state = "failed".into();
                    result.error = Some(format!("1Click {}", venue_status.status));
                }
                "PENDING_DEPOSIT" | "KNOWN_DEPOSIT_TX" | "PROCESSING" | "INCOMPLETE_DEPOSIT" => {}
                _ => {
                    return Err(venue(format!(
                        "unknown 1Click status: {}",
                        venue_status.status
                    )));
                }
            }
        }
    }
    current.status = result.clone();
    state.near.save_intent(&id, current).await?;
    Ok(Json(result))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_deadline_is_iso8601() {
        let d = deadline_utc(180);
        assert!(d.ends_with('Z') && d.len() == 20);
    }
    #[test]
    fn quoted_unit_price_uses_usdc_and_display_currency_scales() {
        let p = unit_price(1_000_000, 35_000_000_000_000_000_000, 18, "USD", 1_000_000);
        assert_eq!(p["amount"], "0.028571428571");
    }
    #[test]
    fn venue_usd_value_preserves_six_decimal_places() {
        assert_eq!(usd_micros("0.987654321").unwrap(), 987_654);
        assert_eq!(usd_micros("12").unwrap(), 12_000_000);
        assert!(usd_micros("NaN").is_err());
    }
    #[test]
    fn destination_requires_verified_wallet() {
        let sui = Token {
            asset_id: "sui".into(),
            blockchain: "sui".into(),
            symbol: "SUI".into(),
            decimals: 9,
            contract_address: None,
            price: None,
            coingecko_id: None,
        };
        let user = app_balance::VerifiedWallets {
            user_id: "u".into(),
            evm_wallet: Some("0x845c22a46398E0a702733e556bEB6aFcB2E92132".into()),
            solana_wallet: None,
        };
        assert!(recipient(&sui, &user).is_err());
    }
}
