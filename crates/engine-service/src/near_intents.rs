use super::*;
use base64::Engine;
use engine_execution::near_intents::{Client, QuoteRequest, Token};
use engine_execution::swaps::uniswap::BASE_USDC;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
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
    // The tokens CoinGecko has reviewed and lists, per chain Atlas supports (lowercase addresses),
    // refreshed daily: a token found by search counts as verified only when it's among them, so a
    // look-alike with the same name never does. Held with the time it's good until.
    listed: Arc<Mutex<Option<(Instant, Arc<Listed>)>>>,
    monad_rpc: reqwest::Url,
    postgres: Option<Arc<tokio_postgres::Client>>,
}
#[derive(Clone)]
struct StoredQuote {
    owner: String,
    wallet: String,
    recipient: String,
    asset: Token,
    amount: u128,
    minimum_out: u128,
    currency: String,
    expires: u64,
    // An unlisted Sui coin: 1Click delivers SUI, then Cetus swaps it into this coin.
    then_swap: Option<SuiSwap>,
    sell_sui: bool,
    // Paid from Solana cash (`wallet` is the Solana wallet), and what that costs on top (USD micros).
    from_solana: bool,
    network_fee: u128,
}
// The second leg of an unlisted Sui buy.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SuiSwap {
    coin_type: String,
    symbol: String,
    name: String,
    decimals: u32,
    icon_url: Option<String>,
    // SUI (MIST) to swap, after keeping SUI_GAS_RESERVE for gas.
    sui_in: u128,
    expected_out: u128,
}
// What the user reads when their deposit to 1Click didn't go out: nothing left the balance.
const NOT_SENT: &str = "The payment didn't go through, so nothing left your balance. Try again.";
// SUI kept in the Sui wallet for gas: 0.02 SUI pays for dozens of swaps.
const SUI_GAS_RESERVE: u128 = 20_000_000;
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
    #[serde(default)]
    then_swap: Option<SuiSwap>,
    #[serde(default)]
    sell_sui: bool,
    #[serde(default)]
    amount: u128,
    #[serde(default)]
    minimum_out: u128,
    #[serde(default)]
    sui_wallet: Option<String>,
    // Paid from Solana cash: the engine lands the transfer (`expected_to` is 1Click's address), after
    // the gas top-up when there is one.
    #[serde(default)]
    from_solana: bool,
    #[serde(default)]
    gas_request_id: Option<String>,
    // Paid from Base with no ETH: a gasless CoW top-up first (its order once placed), then the
    // transfer is handed out by /next.
    #[serde(default)]
    gas_topup: bool,
    #[serde(default)]
    gas_order: Option<String>,
}
// CoinGecko's chain names for the chains Atlas supports, keyed by Atlas's.
const LISTED_CHAINS: [(&str, &str); 8] = [
    ("sui", "sui"),
    ("near", "near-protocol"),
    ("monad", "monad"),
    ("arc", "arc"),
    ("base", "base"),
    ("solana", "solana"),
    ("ethereum", "ethereum"),
    ("arbitrum", "arbitrum-one"),
];
#[derive(Default)]
pub(super) struct Listed(HashMap<&'static str, HashSet<String>>);
impl Listed {
    fn from_coins(coins: &[Value]) -> Self {
        let mut by_chain: HashMap<&'static str, HashSet<String>> = HashMap::new();
        for coin in coins {
            for (chain, platform) in LISTED_CHAINS {
                if let Some(address) = coin["platforms"][platform]
                    .as_str()
                    .filter(|a| !a.is_empty())
                {
                    by_chain
                        .entry(chain)
                        .or_default()
                        .insert(address.to_ascii_lowercase());
                }
            }
        }
        Self(by_chain)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    // Whether CoinGecko lists this exact token on this chain.
    pub(super) fn has(&self, chain: &str, address: &str) -> bool {
        self.0
            .get(chain)
            .is_some_and(|set| set.contains(&address.to_ascii_lowercase()))
    }
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
            listed: Arc::new(Mutex::new(None)),
            postgres,
            monad_rpc: env_url("ATLAS_MONAD_MAINNET_RPC_URL", "https://rpc.monad.xyz").parse()?,
        })
    }
    async fn listed(&self) -> Arc<Listed> {
        let last = self.listed.lock().ok().and_then(|held| held.clone());
        if let Some((until, listed)) = &last {
            if Instant::now() < *until {
                return listed.clone();
            }
        }
        let fetched = async {
            let body: Value = self
                .icon_http
                .get("https://api.coingecko.com/api/v3/coins/list")
                .query(&[("include_platform", "true")])
                // CoinGecko refuses requests without one.
                .header(reqwest::header::USER_AGENT, "Atlas/1.0")
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()?;
            Some(Listed::from_coins(body.as_array()?))
        }
        .await;
        // Unreachable: keep the last list, however old, and try again in ten minutes.
        let (fresh_for, listed) = match fetched {
            Some(listed) if !listed.is_empty() => (24 * 60 * 60, Arc::new(listed)),
            _ => (10 * 60, last.map(|(_, listed)| listed).unwrap_or_default()),
        };
        if let Ok(mut held) = self.listed.lock() {
            *held = Some((
                Instant::now() + Duration::from_secs(fresh_for),
                listed.clone(),
            ));
        }
        listed
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
    // Only one confirmed sell may start signing, including across concurrent Render requests.
    async fn claim_sui_sell(&self, id: &str, mut intent: StoredIntent) -> Result<bool, ApiError> {
        intent.status.stage = "execute".into();
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            let changed = pg.execute(
                "UPDATE atlas_near_intents SET payload=$2,stage='execute' WHERE intent_id=$1 AND stage='validate'",
                &[&id, &payload],
            ).await.map_err(internal)?;
            return Ok(changed == 1);
        }
        let mut intents = self.intents.lock().map_err(internal)?;
        if intents
            .get(id)
            .is_none_or(|saved| saved.status.stage != "validate")
        {
            return Ok(false);
        }
        intents.insert(id.into(), intent);
        Ok(true)
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
    // Sui coins the user bought through Atlas (from settled intents).
    async fn sui_coins(&self, owner: &str) -> Result<Vec<SuiSwap>, ApiError> {
        let intents: Vec<StoredIntent> = if let Some(pg) = &self.postgres {
            pg.query(
                "SELECT payload FROM atlas_near_intents WHERE owner=$1",
                &[&owner],
            )
            .await
            .map_err(internal)?
            .into_iter()
            .filter_map(|row| serde_json::from_str::<StoredIntent>(row.get::<_, &str>(0)).ok())
            .collect()
        } else {
            self.intents
                .lock()
                .map_err(internal)?
                .values()
                .filter(|i| i.owner == owner)
                .cloned()
                .collect()
        };
        let mut coins = HashMap::<String, SuiSwap>::new();
        for intent in intents {
            let landed = intent.status.state == "filled"
                || intent.status.stage == "execute"
                || intent
                    .status
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("Your SUI arrived"));
            if let (true, Some(swap)) = (landed, intent.then_swap) {
                coins.insert(swap.coin_type.clone(), swap);
            }
        }
        Ok(coins.into_values().collect())
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
            .filter(|i| {
                i.owner == owner
                    && !i.from_solana
                    && i.expires > now()
                    && if i.gas_topup {
                        i.status.stage == "sign"
                    } else {
                        i.status.stage == "validate"
                    }
            })
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
async fn destination(
    state: &AppState,
    headers: &HeaderMap,
    token: &Token,
    user: &app_balance::VerifiedWallets,
) -> Result<String, ApiError> {
    if !matches!(token.blockchain.as_str(), "sui" | "near") {
        return recipient(token, user).map(str::to_owned);
    }
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err(conflict("Privy identity is required for this destination"));
    };
    let access_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    let response = http
        .post(format!("{bridge_url}/wallet/ensure"))
        .json(&json!({"accessToken":access_token,"chainType":token.blockchain}))
        .send()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Privy receiving wallet unavailable".into(),
            )
        })?;
    if !response.status().is_success() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy receiving wallet unavailable".into(),
        ));
    }
    let body: Value = response.json().await.map_err(internal)?;
    if body["userId"].as_str() != Some(user.user_id.as_str())
        || body["chainType"].as_str() != Some(token.blockchain.as_str())
    {
        return Err(venue("Privy receiving wallet identity mismatch"));
    }
    let address = body["address"]
        .as_str()
        .ok_or_else(|| venue("Privy omitted receiving wallet"))?;
    let valid = match token.blockchain.as_str() {
        "sui" => {
            address.len() == 66
                && address.starts_with("0x")
                && address[2..].bytes().all(|b| b.is_ascii_hexdigit())
        }
        "near" => address.len() == 64 && address.bytes().all(|b| b.is_ascii_hexdigit()),
        _ => false,
    };
    if !valid {
        return Err(venue("Privy returned an invalid receiving wallet address"));
    }
    Ok(address.to_owned())
}

// A settled 1Click buy lands in the user's own NEAR account. Read the NEP-141 contract,
// not an inferred 1Click amount: later sends and partial sells must change Home immediately.
pub(super) async fn near_holdings(
    state: &AppState,
    headers: &HeaderMap,
    user: &app_balance::VerifiedWallets,
) -> Result<Vec<(Token, u128)>, ApiError> {
    let intents: Vec<StoredIntent> = if let Some(pg) = &state.near.postgres {
        pg.query(
            "SELECT payload FROM atlas_near_intents WHERE owner=$1",
            &[&user.user_id],
        )
        .await
        .map_err(internal)?
        .into_iter()
        .map(|row| serde_json::from_str::<StoredIntent>(row.get::<_, &str>(0)).map_err(internal))
        .collect::<Result<_, _>>()?
    } else {
        state
            .near
            .intents
            .lock()
            .map_err(internal)?
            .values()
            .filter(|intent| intent.owner == user.user_id)
            .cloned()
            .collect()
    };
    let mut tracked = HashMap::<String, Token>::new();
    for intent in intents {
        if intent.status.state == "filled" {
            if let Some(asset) = intent.asset.filter(|asset| asset.blockchain == "near") {
                tracked.insert(asset.asset_id.clone(), asset);
            }
        }
    }
    if tracked.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = state.near.tokens().await?;
    for asset in tracked.values_mut() {
        let fresh = catalog
            .iter()
            .find(|token| token.asset_id == asset.asset_id)
            .ok_or_else(|| venue("An asset price is temporarily unavailable"))?;
        *asset = fresh.clone();
    }
    let wallet = destination(state, headers, tracked.values().next().unwrap(), user).await?;
    let rpc = env_url("ATLAS_NEAR_MAINNET_RPC_URL", "https://rpc.mainnet.near.org");
    let mut held = Vec::new();
    for asset in tracked.into_values() {
        let contract = asset
            .contract_address
            .as_deref()
            .ok_or_else(|| venue("An asset balance is temporarily unavailable"))?;
        if contract != asset.asset_id.trim_start_matches("nep141:") {
            return Err(venue("An asset balance is temporarily unavailable"));
        }
        let args = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&json!({"account_id":wallet})).map_err(internal)?);
        let body: Value = state
            .near
            .icon_http
            .post(&rpc)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"query","params":{
                "request_type":"call_function","finality":"final","account_id":contract,
                "method_name":"ft_balance_of","args_base64":args}}))
            .send()
            .await
            .map_err(venue)?
            .error_for_status()
            .map_err(venue)?
            .json()
            .await
            .map_err(venue)?;
        let raw = body["result"]["result"]
            .as_array()
            .ok_or_else(|| venue("An asset balance is temporarily unavailable"))?;
        let bytes = raw
            .iter()
            .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| venue("An asset balance is temporarily unavailable"))?;
        let units: String = serde_json::from_slice(&bytes).map_err(venue)?;
        let units = units.parse::<u128>().map_err(venue)?;
        if units > 0 {
            held.push((asset, units));
        }
    }
    Ok(held)
}
// What the user holds on Sui from Atlas buys (SUI left for gas included), valued in USD:
// (asset id, symbol, name, decimals, units, USDC units, icon). Nothing to read, nothing asked.
pub(super) async fn sui_holdings(
    state: &AppState,
    headers: &HeaderMap,
    user: &app_balance::VerifiedWallets,
) -> Result<Vec<(String, String, String, u32, u128, u128, Option<String>)>, ApiError> {
    let coins = state.near.sui_coins(&user.user_id).await?;
    let tokens = state.near.tokens().await?;
    let Some(sui) = tokens
        .iter()
        .find(|t| t.blockchain == "sui" && t.symbol == "SUI")
    else {
        return Ok(Vec::new());
    };
    let owner = destination(state, headers, sui, user).await?;
    let balances: Value = state
        .near
        .icon_http
        .post("https://fullnode.mainnet.sui.io:443")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"suix_getAllBalances","params":[owner]}))
        .send()
        .await
        .map_err(venue)?
        .json()
        .await
        .map_err(venue)?;
    let held = |coin: &str| -> u128 {
        balances["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["coinType"].as_str() == Some(coin))
            .filter_map(|b| b["totalBalance"].as_str()?.parse::<u128>().ok())
            .sum()
    };
    let price_of = |v: &Option<Value>| {
        v.as_ref()
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .filter(|p: &f64| p.is_finite() && *p > 0.0)
    };
    let mut out = Vec::new();
    let sui_units = held("0x2::sui::SUI");
    if let (true, Some(price)) = (sui_units > 0, price_of(&sui.price)) {
        let value = (sui_units as f64 / 1e9 * price * 1e6) as u128;
        out.push((
            "near:sui:0x2::sui::SUI".into(),
            "SUI".into(),
            "Sui".into(),
            9,
            sui_units,
            value,
            state.near.icon_for(sui),
        ));
    }
    for coin in coins {
        let units = held(&coin.coin_type);
        if units == 0 {
            continue;
        }
        // DexScreener's price for the coin's most liquid Sui pair.
        let pairs: Value = match state
            .near
            .icon_http
            .get(format!(
                "https://api.dexscreener.com/tokens/v1/sui/{}",
                coin.coin_type
            ))
            .send()
            .await
        {
            Ok(r) => r.json().await.unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        let price = pairs
            .as_array()
            .into_iter()
            .flatten()
            .max_by(|a, b| {
                let l = |p: &Value| p["liquidity"]["usd"].as_f64().unwrap_or(0.0);
                l(a).total_cmp(&l(b))
            })
            .and_then(|p| p["priceUsd"].as_str()?.parse::<f64>().ok())
            .filter(|p| p.is_finite() && *p > 0.0);
        let Some(price) = price else {
            continue;
        };
        let value = (units as f64 / 10f64.powi(coin.decimals as i32) * price * 1e6) as u128;
        out.push((
            format!("near:sui:{}", coin.coin_type),
            coin.symbol,
            coin.name,
            coin.decimals,
            units,
            value,
            coin.icon_url,
        ));
    }
    Ok(out)
}

// What people can deposit besides USDC to their own Base or Solana address: each is a 1Click asset
// that becomes USDC in the user's Solana wallet (dollar coins one for one, other coins at the
// market). Logos: the coin, and the chain it's on.
struct DepositOption {
    id: &'static str,
    label: &'static str,
    network: &'static str,
    asset: &'static str,
    asset_id: &'static str,
    dollar: bool,
    asset_icon: &'static str,
    chain_icon: Option<&'static str>,
    // Shown in the first list; the rest sit under "See more".
    featured: bool,
}
const DEPOSIT_NETWORKS: &[DepositOption] = &[
    DepositOption {
        id: "tron-usdt",
        label: "USDT on Tron (TRC20)",
        network: "Tron",
        asset: "USDT",
        asset_id: "nep141:tron-d28a265909efecdcee7c5028585214ea0b96f015.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/tron_mainnet.png"),
        featured: true,
    },
    DepositOption {
        id: "bsc-usdt",
        label: "USDT on BNB Chain (BEP20)",
        network: "BNB Chain",
        asset: "USDT",
        asset_id: "nep245:v2_1.omni.hot.tg:56_2CMMyVTGZkeyNZTSvS5sarzfir6g",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/bsc_mainnet.png"),
        featured: true,
    },
    DepositOption {
        id: "sui-usdc",
        label: "USDC on Sui",
        network: "Sui",
        asset: "USDC",
        asset_id: "nep141:sui-c1b81ecaf27933252d31a963bc5e9458f13c18ce.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some(
            "https://coin-images.coingecko.com/coins/images/26375/large/sui-ocean-square.png",
        ),
        featured: true,
    },
    DepositOption {
        id: "sol",
        label: "SOL",
        network: "Solana",
        asset: "SOL",
        asset_id: "nep141:sol.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/sol.png",
        chain_icon: None,
        featured: true,
    },
    DepositOption {
        id: "eth-usdt",
        label: "USDT on Ethereum (ERC20)",
        network: "Ethereum",
        asset: "USDT",
        asset_id: "nep141:eth-0xdac17f958d2ee523a2206206994597c13d831ec7.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/ethereum_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "eth-usdc",
        label: "USDC on Ethereum",
        network: "Ethereum",
        asset: "USDC",
        asset_id: "nep141:eth-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/ethereum_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "arb-usdc",
        label: "USDC on Arbitrum",
        network: "Arbitrum",
        asset: "USDC",
        asset_id: "nep141:arb-0xaf88d065e77c8cc2239327c5edb3a432268e5831.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/arbitrum_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "pol-usdt",
        label: "USDT on Polygon",
        network: "Polygon",
        asset: "USDT",
        asset_id: "nep245:v2_1.omni.hot.tg:137_3hpYoaLtt8MP1Z2GH1U473DMRKgr",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/polygon_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "ton-usdt",
        label: "USDT on TON",
        network: "TON",
        asset: "USDT",
        asset_id: "nep245:v2_1.omni.hot.tg:1117_3tsdfyziyc7EJbP2aULWSKU4toBaAcN4FdTgfm5W1mC4ouR",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/ton_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "sol-usdt",
        label: "USDT on Solana",
        network: "Solana",
        asset: "USDT",
        asset_id: "nep141:sol-c800a4bd850783ccb82c2b2c7e84175443606352.omft.near",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdt.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/solana_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "monad-usdc",
        label: "USDC on Monad",
        network: "Monad",
        asset: "USDC",
        asset_id: "nep245:v2_1.omni.hot.tg:143_2dmLwYWkCQKyTjeUPAsGJuiVLbFx",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/monad_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "near-usdc",
        label: "USDC on NEAR",
        network: "NEAR",
        asset: "USDC",
        asset_id: "nep141:17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://coin-images.coingecko.com/coins/images/10365/large/near.jpg"),
        featured: false,
    },
    DepositOption {
        id: "op-usdc",
        label: "USDC on Optimism",
        network: "Optimism",
        asset: "USDC",
        asset_id: "nep245:v2_1.omni.hot.tg:10_A2ewyUyDp6qsue1jqZsGypkCxRJ",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/optimism_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "avax-usdc",
        label: "USDC on Avalanche",
        network: "Avalanche",
        asset: "USDC",
        asset_id: "nep245:v2_1.omni.hot.tg:43114_3atVJH3r5c4GqiSYmg9fECvjc47o",
        dollar: true,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/usdc.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/avax_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "eth",
        label: "ETH on Ethereum",
        network: "Ethereum",
        asset: "ETH",
        asset_id: "nep141:eth.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/eth.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/ethereum_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "base-eth",
        label: "ETH on Base",
        network: "Base",
        asset: "ETH",
        asset_id: "nep141:base.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/eth.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/base_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "arb-eth",
        label: "ETH on Arbitrum",
        network: "Arbitrum",
        asset: "ETH",
        asset_id: "nep141:arb.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/eth.png",
        chain_icon: Some("https://cdn.layerswap.io/layerswap/networks/arbitrum_mainnet.png"),
        featured: false,
    },
    DepositOption {
        id: "btc",
        label: "BTC",
        network: "Bitcoin",
        asset: "BTC",
        asset_id: "nep141:btc.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/btc.png",
        chain_icon: None,
        featured: false,
    },
    DepositOption {
        id: "bnb",
        label: "BNB",
        network: "BNB Chain",
        asset: "BNB",
        asset_id: "nep245:v2_1.omni.hot.tg:56_11111111111111111111",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/bnb.png",
        chain_icon: None,
        featured: false,
    },
    DepositOption {
        id: "trx",
        label: "TRX",
        network: "Tron",
        asset: "TRX",
        asset_id: "nep141:tron.omft.near",
        dollar: false,
        asset_icon: "https://cdn.layerswap.io/layerswap/currencies/trx.png",
        chain_icon: None,
        featured: false,
    },
    DepositOption {
        id: "sui",
        label: "SUI",
        network: "Sui",
        asset: "SUI",
        asset_id: "nep141:sui.omft.near",
        dollar: false,
        asset_icon:
            "https://coin-images.coingecko.com/coins/images/26375/large/sui-ocean-square.png",
        chain_icon: None,
        featured: false,
    },
    DepositOption {
        id: "xrp",
        label: "XRP",
        network: "XRP Ledger",
        asset: "XRP",
        asset_id: "nep141:xrp.omft.near",
        dollar: false,
        asset_icon:
            "https://coin-images.coingecko.com/coins/images/44/large/xrp-symbol-white-128.png",
        chain_icon: None,
        featured: false,
    },
    DepositOption {
        id: "doge",
        label: "DOGE",
        network: "Dogecoin",
        asset: "DOGE",
        asset_id: "nep141:doge.omft.near",
        dollar: false,
        asset_icon: "https://coin-images.coingecko.com/coins/images/5/large/dogecoin.png",
        chain_icon: None,
        featured: false,
    },
];
// How long a deposit address waits for the money.
const DEPOSIT_WINDOW_SECS: u64 = 2 * 60 * 60;
// Deposits from other networks land as USDC on Solana, where Atlas never needs to pay gas.
const SOLANA_USDC_1CLICK: &str = "nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near";

pub(super) async fn deposit_networks() -> Json<Value> {
    Json(json!({"networks": DEPOSIT_NETWORKS.iter().map(|o| {
        json!({"id":o.id,"label":o.label,"network":o.network,"asset":o.asset,
            "assetIcon":o.asset_icon,"chainIcon":o.chain_icon,"featured":o.featured})
    }).collect::<Vec<_>>()}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DepositRequest {
    network_id: String,
    amount: markets::Money,
}

// "Temporary swap limits: minimum swap amount is $100" → 100.
fn venue_minimum_usd(message: &str) -> Option<u128> {
    let tail = message.split("minimum swap amount is $").nth(1)?;
    let digits: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    digits.parse().ok()
}

pub(super) async fn deposit_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let option = DEPOSIT_NETWORKS
        .iter()
        .find(|o| o.id == req.network_id)
        .ok_or((StatusCode::NOT_FOUND, "unknown deposit network".into()))?;
    let (label, network, asset, asset_id) =
        (option.label, option.network, option.asset, option.asset_id);
    let wallet = user
        .solana_wallet
        .clone()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| conflict("Your wallet is still being set up. Try again in a moment."))?;
    let tokens = state.near.tokens().await?;
    let origin = tokens
        .iter()
        .find(|t| t.asset_id == *asset_id)
        .ok_or_else(|| venue(format!("Deposits of {label} aren't available right now")))?;
    let near = tokens
        .iter()
        .find(|t| t.blockchain == "near")
        .ok_or_else(|| venue("NEAR Intents is unavailable right now"))?;
    // Refunds (a deposit below the band, or one that can't be swapped) land in their own NEAR account.
    let refund_to = destination(&state, &headers, near, &user).await?;
    let currency = req.amount.currency.clone();
    markets::checked_currency(&currency)?;
    let rate = app_balance::fx_rate(&currency).await?;
    let usd = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(usd, &currency, rate)?;
    // Dollar coins: the dollar amount in the coin's own decimals. Other coins: at 1Click's price.
    let units = if option.dollar {
        if origin.decimals >= 6 {
            usd.checked_mul(10u128.pow(origin.decimals - 6))
        } else {
            Some(usd / 10u128.pow(6 - origin.decimals))
        }
    } else {
        let price = origin
            .price
            .as_ref()
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .filter(|p: &f64| p.is_finite() && *p > 0.0)
            .ok_or_else(|| venue(format!("No price for {asset} right now")))?;
        let whole = usd as f64 / 1_000_000.0 / price;
        let units = whole * 10f64.powi(origin.decimals as i32);
        (units.is_finite() && units >= 1.0).then_some(units as u128)
    }
    .ok_or_else(|| bad("amount too large"))?;
    let deadline = deadline_utc(DEPOSIT_WINDOW_SECS);
    let amount = units.to_string();
    let request = QuoteRequest::flex_deposit(
        asset_id,
        SOLANA_USDC_1CLICK,
        &amount,
        &wallet,
        &refund_to,
        &deadline,
    );
    let q = match state.near.client.quote(&request).await {
        Ok(q) => q,
        Err(engine_execution::near_intents::Error::Venue(_, text)) => {
            if let Some(min) = venue_minimum_usd(&text) {
                return Err(bad(&format!(
                    "The smallest deposit from {network} right now is {} (${min}).",
                    markets::say_money(min * 1_000_000, &currency, rate)
                )));
            }
            return Err(venue(format!(
                "Deposits from {network} aren't available right now"
            )));
        }
        Err(e) => return Err(venue(e)),
    };
    let address = q
        .deposit_address
        .clone()
        .ok_or_else(|| venue("1Click omitted the deposit address"))?;
    let receive: u128 = q.amount_out.parse().map_err(internal)?;
    let minimum: u128 = q
        .min_amount_in
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(units.saturating_mul(99) / 100);
    Ok(Json(json!({
        "address": address,
        "memo": q.deposit_memo,
        "network": network,
        "label": label,
        "asset": asset,
        "sendAmount": markets::format_units(units, origin.decimals),
        "minAmount": markets::format_units(minimum, origin.decimals),
        "receive": money(receive, &currency, rate),
        "timeEstimateSec": q.time_estimate,
        "expiresAtUnixMs": now() + DEPOSIT_WINDOW_SECS * 1000,
    })))
}

#[derive(Deserialize)]
pub(super) struct DepositStatusQuery {
    address: String,
    memo: Option<String>,
}

// Where a deposit from another network stands: waiting for it, swapping, done, or refunded.
pub(super) async fn deposit_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<DepositStatusQuery>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    if q.address.is_empty() || q.address.len() > 128 {
        return Err(bad("invalid deposit address"));
    }
    let status = state
        .near
        .client
        .status(&q.address, q.memo.as_deref())
        .await
        .map_err(venue)?;
    let state_name = match status.status.as_str() {
        "PENDING_DEPOSIT" => "waiting",
        "KNOWN_DEPOSIT_TX" | "PROCESSING" => "processing",
        "SUCCESS" => "done",
        "INCOMPLETE_DEPOSIT" => "incomplete",
        "REFUNDED" => "refunded",
        _ => "failed",
    };
    Ok(Json(json!({"state": state_name})))
}

fn sui_coin_type(value: &str) -> bool {
    let parts: Vec<_> = value.split("::").collect();
    parts.len() == 3
        && parts[0].starts_with("0x")
        && (3..=66).contains(&parts[0].len())
        && parts[0][2..].bytes().all(|b| b.is_ascii_hexdigit())
        && parts[1..].iter().all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

// Calls the Privy bridge with the user's own access token (it signs for their wallets only with it).
async fn bridge(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    mut body: Value,
) -> Result<Value, ApiError> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err(conflict("Privy identity is required for this"));
    };
    let access_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    body["accessToken"] = json!(access_token);
    let response = http
        .post(format!("{bridge_url}{path}"))
        .timeout(Duration::from_secs(90))
        .json(&body)
        .send()
        .await
        .map_err(|_| venue("Privy bridge unavailable"))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body["error"].as_str().unwrap_or("bridge error").to_owned();
        return Err((StatusCode::BAD_GATEWAY, reason));
    }
    Ok(body)
}

// A Sui coin's symbol, name, decimals and icon from Sui's own metadata.
async fn sui_meta(state: &AppState, coin: &str) -> Option<(String, String, u32, Option<String>)> {
    let graphql = json!({
        "query":"query($coinType:String!){coinMetadata(coinType:$coinType){symbol name decimals iconUrl}}",
        "variables":{"coinType":coin}
    });
    let body: Value = state
        .near
        .icon_http
        .post("https://graphql.mainnet.sui.io/graphql")
        .json(&graphql)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let meta = &body["data"]["coinMetadata"];
    let symbol = meta["symbol"].as_str().filter(|s| !s.is_empty())?;
    let name = meta["name"].as_str().filter(|s| !s.is_empty())?;
    let decimals = u32::try_from(meta["decimals"].as_u64()?)
        .ok()
        .filter(|d| *d <= 18)?;
    let icon = meta["iconUrl"]
        .as_str()
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned);
    Some((symbol.into(), name.into(), decimals, icon))
}

// Buying an unlisted Sui coin: 1Click turns the Base USDC into SUI in the user's own Sui wallet
// (keeping a little for gas), then Cetus swaps the rest into the coin once it lands.
async fn sui_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    coin: &str,
) -> Result<Json<Value>, ApiError> {
    if !sui_coin_type(coin) {
        return Err(bad("not a Sui coin type"));
    }
    let sui = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| t.blockchain == "sui" && t.symbol == "SUI")
        .ok_or_else(|| venue("1Click doesn't list SUI right now"))?;
    let destination = destination(&state, &headers, &sui, &user).await?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let amount = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(amount, &req.amount.currency, rate)?;
    let (wallet, from_solana, network_fee, origin) =
        pay_from(&state, &user, amount, &req.amount.currency, rate).await?;
    let deadline = deadline_utc(180);
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        origin,
        &sui.asset_id,
        &units,
        &destination,
        &wallet,
        &deadline,
        true,
    );
    let q = state.near.client.quote(&request).await.map_err(venue)?;
    let sui_out: u128 = q.amount_out.parse().map_err(internal)?;
    if sui_out <= SUI_GAS_RESERVE * 2 {
        return Err(bad("That's too small to buy on Sui. Try a bigger amount."));
    }
    let sui_in = sui_out - SUI_GAS_RESERVE;
    let (symbol, name, decimals, icon_url) = sui_meta(&state, coin)
        .await
        .ok_or_else(|| venue("Sui coin details unavailable"))?;
    let routed = bridge(
        &state,
        &headers,
        "/sui/quote",
        json!({"coinType":coin,"amount":sui_in.to_string()}),
    )
    .await?;
    let expected_out: u128 = routed["amountOut"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| venue("no Sui route for this coin"))?;
    let minimum: u128 = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    let sui_usd = usd_micros(&q.amount_out_usd)?;
    let value_usd = sui_usd.saturating_mul(sui_in) / sui_out;
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            wallet,
            recipient: destination,
            asset: sui,
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            sell_sui: false,
            from_solana,
            network_fee,
            then_swap: Some(SuiSwap {
                coin_type: coin.into(),
                symbol: symbol.clone(),
                name,
                decimals,
                icon_url,
                sui_in,
                expected_out,
            }),
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"buy",
        "pay":{"amount":markets::format_units(amount,6),"symbol":"USDC","value":money(amount,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(expected_out,decimals),"symbol":symbol,
            "value":money(value_usd,&req.amount.currency,rate)},
        "price":unit_price(amount,expected_out,decimals,&req.amount.currency,rate),
        "fee":money(network_fee,&req.amount.currency,rate),"expiresAtUnixMs":expires}),
    ))
}

async fn search_sui_unlisted(
    state: &AppState,
    query: &str,
    currency: &str,
    rate: u128,
) -> Vec<Value> {
    if query.len() < 2 || query.len() > 160 {
        return Vec::new();
    }
    let Ok(response) = state
        .near
        .icon_http
        .get("https://api.dexscreener.com/latest/dex/search")
        .query(&[("q", query)])
        .send()
        .await
    else {
        return Vec::new();
    };
    let Ok(body) = response.error_for_status() else {
        return Vec::new();
    };
    let Ok(body) = body.json::<Value>().await else {
        return Vec::new();
    };
    let Some(pairs) = body["pairs"].as_array() else {
        return Vec::new();
    };
    let listed = state.near.listed().await;
    let mut candidates = HashMap::<String, (f64, f64)>::new();
    for pair in pairs {
        if pair["chainId"].as_str() != Some("sui") {
            continue;
        }
        let Some(coin) = pair["baseToken"]["address"].as_str() else {
            continue;
        };
        let symbol = pair["baseToken"]["symbol"].as_str().unwrap_or("");
        let name = pair["baseToken"]["name"].as_str().unwrap_or("");
        if !sui_coin_type(coin)
            || !(coin.eq_ignore_ascii_case(query)
                || symbol.eq_ignore_ascii_case(query)
                || symbol
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase())
                || name
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase()))
        {
            continue;
        }
        let liquidity = pair["liquidity"]["usd"].as_f64().unwrap_or_default();
        let price = pair["priceUsd"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or_default();
        if liquidity < 5_000.0 || !liquidity.is_finite() || price <= 0.0 || !price.is_finite() {
            continue;
        }
        let entry = candidates.entry(coin.to_owned()).or_insert((0.0, 0.0));
        if liquidity > entry.0 {
            *entry = (liquidity, price);
        }
    }
    let mut candidates: Vec<_> = candidates.into_iter().collect();
    candidates.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
    let mut result = Vec::new();
    for (coin, (_, price)) in candidates.into_iter().take(5) {
        let graphql = json!({
            "query":"query($coinType:String!){coinMetadata(coinType:$coinType){symbol name decimals iconUrl}}",
            "variables":{"coinType":coin}
        });
        let Ok(response) = state
            .near
            .icon_http
            .post("https://graphql.mainnet.sui.io/graphql")
            .json(&graphql)
            .send()
            .await
        else {
            continue;
        };
        let Ok(body) = response.error_for_status() else {
            continue;
        };
        let Ok(body) = body.json::<Value>().await else {
            continue;
        };
        let meta = &body["data"]["coinMetadata"];
        let (Some(symbol), Some(name), Some(decimals)) = (
            meta["symbol"].as_str(),
            meta["name"].as_str(),
            meta["decimals"].as_u64(),
        ) else {
            continue;
        };
        if symbol.is_empty() || name.is_empty() || decimals > 18 {
            continue;
        }
        let display = price * (rate as f64 / 1_000_000.0);
        let icon = meta["iconUrl"]
            .as_str()
            .filter(|url| url.starts_with("https://"));
        let verified = listed.has("sui", &coin);
        result.push(json!({"assetId":format!("near:sui:{coin}"),"symbol":symbol,
            "name":format!("{name} on sui"),"kind":"crypto","chain":"sui",
            "price":{"amount":format!("{display:.12}").trim_end_matches('0').trim_end_matches('.'),
                "currency":currency},"change24hPct":null,"iconUrl":icon,"verified":verified,
            "tradeable":true}));
    }
    result
}

// Where a 1Click asset's price history lives on GeckoTerminal: (network, token address). None
// when the chain or the token (a native coin without a wrapped twin there) has no pools to read.
pub(super) async fn chart_token(
    state: &AppState,
    asset_id: &str,
) -> Result<Option<(&'static str, String)>, ApiError> {
    if let Some(coin) = asset_id.strip_prefix("near:sui:") {
        return Ok(sui_coin_type(coin).then(|| ("sui-network", coin.to_string())));
    }
    let Some(id) = asset_id.strip_prefix("near:") else {
        return Ok(None);
    };
    let Some(token) = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| t.asset_id == id)
    else {
        return Ok(None);
    };
    let network = match token.blockchain.as_str() {
        "sui" => "sui-network",
        "near" => "near",
        "monad" => "monad",
        "eth" => "eth",
        "base" => "base",
        "arb" => "arbitrum",
        "bsc" => "bsc",
        "sol" => "solana",
        _ => return Ok(None),
    };
    let address = match (token.contract_address.clone(), token.blockchain.as_str()) {
        (Some(address), _) if !address.is_empty() => address,
        (_, "sui") => "0x2::sui::SUI".into(),
        (_, "near") => "wrap.near".into(),
        _ => return Ok(None),
    };
    Ok(Some((network, address)))
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
    out.extend(search_sui_unlisted(state, &query, currency, rate).await);
    Ok(out)
}
async fn sui_sell_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
) -> Result<Json<Value>, ApiError> {
    let token = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| {
            t.blockchain == "sui"
                && t.symbol == "SUI"
                && req.asset_id == format!("near:{}", t.asset_id)
        })
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "Selling this asset is not ready yet".into(),
        ))?;
    let wallet = user
        .evm_wallet
        .as_deref()
        .filter(|w| is_evm(w))
        .ok_or_else(|| conflict("Your cash wallet is not ready"))?;
    let sui_wallet = destination(&state, &headers, &token, &user).await?;
    let price = token
        .price
        .as_ref()
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|p: &f64| p.is_finite() && *p > 0.0)
        .ok_or_else(|| venue("Asset price is temporarily unavailable"))?;
    let price_micros = (price * 1_000_000.0).round() as u128;
    if price_micros == 0 {
        return Err(venue("Asset price is temporarily unavailable"));
    }
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let value_usdc = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(value_usdc, &req.amount.currency, rate)?;
    let amount = value_usdc
        .checked_mul(1_000_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / price_micros;
    if amount == 0 || amount > u64::MAX as u128 {
        return Err(bad("amount is outside the supported range"));
    }
    let balance: Value = state
        .near
        .icon_http
        .post("https://fullnode.mainnet.sui.io:443")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"suix_getBalance",
            "params":[sui_wallet,"0x2::sui::SUI"]}))
        .send()
        .await
        .map_err(venue)?
        .error_for_status()
        .map_err(venue)?
        .json()
        .await
        .map_err(venue)?;
    let held = balance["result"]["totalBalance"]
        .as_str()
        .and_then(|raw| raw.parse::<u128>().ok())
        .ok_or_else(|| venue("Your balance is temporarily unavailable"))?;
    if held < amount.saturating_add(SUI_GAS_RESERVE) {
        return Err(markets::short_of_cash());
    }
    let deadline = deadline_utc(180);
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        &token.asset_id,
        BASE_USDC_1CLICK,
        &units,
        wallet,
        &sui_wallet,
        &deadline,
        true,
    );
    let q = state.near.client.quote(&request).await.map_err(venue)?;
    if q.amount_in.parse::<u128>().ok() != Some(amount) {
        return Err(venue("The route changed the sell amount"));
    }
    let output: u128 = q.amount_out.parse().map_err(internal)?;
    let minimum: u128 = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            wallet: wallet.into(),
            recipient: sui_wallet,
            asset: token.clone(),
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: true,
            from_solana: false,
            network_fee: 0,
        },
    );
    Ok(Json(json!({
        "quoteId":quote_id,"assetId":req.asset_id,"side":"sell",
        "pay":{"amount":markets::format_units(amount,9),"symbol":"SUI",
            "value":money(value_usdc,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(output,6),"symbol":"USDC",
            "value":money(output,&req.amount.currency,rate)},
        "price":unit_price(output,amount,9,&req.amount.currency,rate),
        "fee":{"amount":"0","currency":req.amount.currency},"expiresAtUnixMs":expires
    })))
}
// Which cash pays a 1Click buy of `amount`: Base when it covers it (a plain USDC transfer; an empty
// gas tank fills itself first), else Solana (gas topped up gaslessly, but 1Click's one-off deposit
// address needs its USDC account opened, about 0.002 SOL of rent, shown as the network fee).
// Returns the paying wallet, whether it's Solana, the network fee (USD micros) and 1Click's asset.
async fn pay_from(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    amount: u128,
    currency: &str,
    rate: u128,
) -> Result<(String, bool, u128, &'static str), ApiError> {
    let evm = user.evm_wallet.as_deref().filter(|w| is_evm(w));
    let base_cash = match evm {
        Some(w) => state
            .markets
            .base
            .balance_of(BASE_USDC, w)
            .await
            .unwrap_or(0),
        None => 0,
    };
    // Base covers it and its gas is paid (own ETH, or USDC to spare for a CoW top-up).
    if let (Some(w), true) = (evm, base_cash >= amount) {
        if markets::wallet_pays_gas(state, w).await
            || markets::base_topup_fits(state, w, amount).await
        {
            return Ok((w.into(), false, 0, BASE_USDC_1CLICK));
        }
    }
    let solana = user.solana_wallet.as_deref().filter(|w| !w.is_empty());
    let solana_cash = match solana {
        Some(s) => markets::solana_cash(state, s).await,
        None => 0,
    };
    if let Some(s) = solana {
        if solana_cash >= amount && markets::solana_can_send(state, s, amount).await {
            let fee = markets::account_rent_usd(state).await;
            return Ok((s.into(), true, fee, SOLANA_USDC_1CLICK));
        }
    }
    if base_cash >= amount {
        return Err(markets::short_of_gas(currency, rate));
    }
    Err(markets::not_enough_cash(
        base_cash + solana_cash,
        currency,
        rate,
    ))
}

// A Solana address (base58, 32 bytes).
fn is_solana(address: &str) -> bool {
    (32..=44).contains(&address.len())
        && address
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() && !b"0OIl".contains(&b))
}

pub(super) async fn quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
) -> Result<Json<Value>, ApiError> {
    markets::checked_currency(&req.amount.currency)?;
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if req.side == "sell" {
        return sui_sell_quote(state, headers, req, user).await;
    }
    if req.side != "buy" {
        return Err(bad("unsupported trade side"));
    }
    if let Some(coin) = req.asset_id.strip_prefix("near:sui:").map(str::to_owned) {
        return sui_quote(state, headers, req, user, &coin).await;
    }
    let token = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| format!("near:{}", t.asset_id) == req.asset_id && supported(t))
        .ok_or((StatusCode::NOT_FOUND, "1Click asset not found".into()))?;
    let destination = destination(&state, &headers, &token, &user).await?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let micros = markets::parse_micros(&req.amount.amount)?;
    let amount = micros
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(amount, &req.amount.currency, rate)?;
    let (wallet, from_solana, network_fee, origin) =
        pay_from(&state, &user, amount, &req.amount.currency, rate).await?;
    let deadline = deadline_utc(180);
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        origin,
        &token.asset_id,
        &units,
        &destination,
        &wallet,
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
            wallet,
            recipient: destination,
            asset: token.clone(),
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: false,
            from_solana,
            network_fee,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"buy",
        "pay":{"amount":markets::format_units(input,6),"symbol":"USDC","value":money(input,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(output,token.decimals),"symbol":token.symbol,
            "value":money(output_usd,&req.amount.currency,rate)},
        "price":unit_price(input,output,token.decimals,&req.amount.currency,rate),
        "fee":money(network_fee,&req.amount.currency,rate),"expiresAtUnixMs":expires}),
    ))
}
async fn execute_sui_sell(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    quote: StoredQuote,
) -> Result<Json<Value>, ApiError> {
    let intent_id = id("intent");
    let expires = now() + 120_000;
    let rate = app_balance::fx_rate(&quote.currency).await?;
    let status = markets::IntentStatus {
        intent_id: intent_id.clone(),
        stage: "validate".into(),
        state: "pending".into(),
        tx_ids: Vec::new(),
        error: None,
    };
    state
        .near
        .insert_intent(
            &intent_id,
            StoredIntent {
                owner: user.user_id.clone(),
                wallet: quote.wallet,
                expected_to: String::new(),
                expected_data: String::new(),
                deposit_address: String::new(),
                deposit_memo: None,
                asset: Some(quote.asset),
                expires,
                status,
                then_swap: None,
                sell_sui: true,
                amount: quote.amount,
                minimum_out: quote.minimum_out,
                sui_wallet: Some(quote.recipient),
                from_solana: false,
                gas_request_id: None,
                gas_topup: false,
                gas_order: None,
            },
        )
        .await?;
    Ok(Json(json!({"intentId":intent_id,"kind":"sell",
        "summary":[
            {"label":"You sell","value":format!("{} SUI",markets::format_units(quote.amount,9))},
            {"label":"You receive (at least)","value":markets::say_money(quote.minimum_out,&quote.currency,rate)}
        ],
        "transactions":[],"expiresAtUnixMs":expires
    })))
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
    let destination = destination(&state, &headers, &stored.asset, &user).await?;
    if destination != stored.recipient {
        return Err(conflict("receiving wallet changed; request a fresh quote"));
    }
    let paying = if stored.from_solana {
        user.solana_wallet.as_deref()
    } else {
        user.evm_wallet.as_deref()
    };
    if paying != Some(&stored.wallet) {
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
    if stored.sell_sui {
        return execute_sui_sell(&state, &user, stored).await;
    }
    let balance = if stored.from_solana {
        markets::solana_cash(&state, &stored.wallet).await
    } else {
        state
            .markets
            .base
            .balance_of(BASE_USDC, &stored.wallet)
            .await
            .map_err(venue)?
    };
    if balance < stored.amount {
        return Err(markets::short_of_cash());
    }
    // No ETH on Base: a gasless CoW top-up goes first, so the deposit gets more time.
    let gas_topup = !stored.from_solana
        && markets::base_topup_fits(&state, &stored.wallet, stored.amount).await;
    if !stored.from_solana && !gas_topup && !markets::wallet_pays_gas(&state, &stored.wallet).await
    {
        let rate = app_balance::fx_rate(&stored.currency).await?;
        return Err(markets::short_of_gas(&stored.currency, rate));
    }
    let deadline = deadline_utc(if gas_topup { 20 * 60 } else { 240 });
    let amount = stored.amount.to_string();
    let req = QuoteRequest::exact_input(
        if stored.from_solana {
            SOLANA_USDC_1CLICK
        } else {
            BASE_USDC_1CLICK
        },
        &stored.asset.asset_id,
        &amount,
        &destination,
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
    let expected_chain = if stored.from_solana {
        is_solana(&deposit)
    } else {
        is_evm(&deposit)
    };
    if !expected_chain || fresh.deposit_memo.is_some() {
        return Err(venue("1Click returned an unsupported deposit destination"));
    }
    // What the app signs now: from Solana, [gas top-up?, transfer] that the engine lands; from Base,
    // the transfer (nothing yet when the tank tops up first; /next hands it out).
    let (expected_to, expected_data, transactions, gas_request_id) = if stored.from_solana {
        let (gas, transfer) =
            markets::solana_usdc_transfer(&state, &stored.wallet, &deposit, stored.amount).await?;
        let mut txs = Vec::new();
        if let Some((_, gas_tx)) = &gas {
            txs.push(json!({"chain":"solana","transaction":gas_tx,"submit":"engine"}));
        }
        txs.push(json!({"chain":"solana","transaction":transfer,"submit":"engine"}));
        (deposit.clone(), String::new(), txs, gas.map(|(id, _)| id))
    } else {
        let tx = state
            .markets
            .base
            .transfer_transaction(BASE_USDC, &stored.wallet, &deposit, stored.amount)
            .map_err(venue)?;
        let txs = if gas_topup {
            Vec::new()
        } else {
            vec![json!({"chain":"base","chainId":8453,"to":tx.to,"data":tx.data,"value":"0"})]
        };
        (
            tx.to.to_ascii_lowercase(),
            tx.data.to_ascii_lowercase(),
            txs,
            None,
        )
    };
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
                expected_to,
                expected_data,
                deposit_address: deposit,
                deposit_memo: fresh.deposit_memo,
                asset: Some(stored.asset.clone()),
                expires,
                status,
                then_swap: stored.then_swap.clone(),
                sell_sui: false,
                amount: stored.amount,
                minimum_out: stored.minimum_out,
                sui_wallet: None,
                from_solana: stored.from_solana,
                gas_request_id,
                gas_topup,
                gas_order: None,
            },
        )
        .await?;
    let rate = app_balance::fx_rate(&stored.currency).await?;
    let mut chain = stored.asset.blockchain.clone();
    if let Some(first) = chain.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let get = match &stored.then_swap {
        Some(swap) => format!(
            "{} {}",
            markets::format_units(swap.expected_out, swap.decimals),
            swap.symbol
        ),
        None => format!(
            "{} {}",
            markets::format_units(
                fresh.amount_out.parse().map_err(internal)?,
                stored.asset.decimals
            ),
            stored.asset.symbol
        ),
    };
    let mut summary = vec![
        json!({"label":"You pay","value":markets::say_money(stored.amount,&stored.currency,rate)}),
        json!({"label":"You get (about)","value":get}),
        json!({"label":"Lands on","value":chain}),
    ];
    if stored.network_fee > 0 {
        summary.push(json!({"label":"Network fee",
            "value":markets::say_money(stored.network_fee,&stored.currency,rate)}));
    }
    Ok(Json(
        json!({"intentId":intent_id,"kind":"buy","summary":summary,
        "transactions":transactions,"expiresAtUnixMs":expires}),
    ))
}
async fn signed_sui_sell(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    user: &app_balance::VerifiedWallets,
    mut current: StoredIntent,
    body: markets::Submission,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    if current.expires < now() {
        return Err(conflict("quote expired; request a fresh quote"));
    }
    if !body.sent.is_empty() || !body.signed.is_empty() {
        return Err(bad(
            "cashout confirmation must not include app transactions",
        ));
    }
    if !state.near.claim_sui_sell(id, current.clone()).await? {
        let latest = state
            .near
            .get_intent(id)
            .await?
            .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
        return Ok(Json(latest.status));
    }
    current.status.stage = "execute".into();
    let prepared = bridge(
        state,
        headers,
        "/sui/cashout/prepare",
        json!({"amount":current.amount.to_string(),
            "minimumOut":current.minimum_out.to_string()}),
    )
    .await;
    let prepared = match prepared {
        Ok(value) => value,
        Err(_) => {
            current.status.state = "failed".into();
            current.status.error = Some("Cashout route is unavailable; nothing was sent".into());
            state.near.save_intent(id, current.clone()).await?;
            return Ok(Json(current.status));
        }
    };
    let expected_wallet = current.sui_wallet.as_deref().unwrap_or("");
    let address = prepared["depositAddress"].as_str().unwrap_or("");
    let cashout_id = prepared["cashoutId"].as_str().unwrap_or("");
    if prepared["userId"].as_str() != Some(user.user_id.as_str())
        || prepared["address"].as_str() != Some(expected_wallet)
        || address.len() != 66
        || !address.starts_with("0x")
        || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
        || cashout_id.len() != 36
    {
        current.status.state = "failed".into();
        current.status.error = Some("Cashout preparation was invalid; nothing was sent".into());
        state.near.save_intent(id, current.clone()).await?;
        return Ok(Json(current.status));
    }
    current.deposit_address = address.into();
    current.expires = now() + 240_000;
    // Persist before any signing: a lost bridge response can still be followed by deposit address.
    state.near.save_intent(id, current.clone()).await?;
    let result = bridge(
        state,
        headers,
        "/sui/cashout/commit",
        json!({"cashoutId":cashout_id}),
    )
    .await;
    current.status.stage = "settle".into();
    match result {
        Ok(sent)
            if sent["ok"].as_bool() == Some(true)
                && sent["userId"].as_str() == Some(user.user_id.as_str())
                && sent["address"].as_str() == Some(expected_wallet) =>
        {
            if let Some(digest) = sent["digest"].as_str() {
                current.status.tx_ids.push(digest.into());
            }
        }
        Ok(sent) if sent["ok"].as_bool() == Some(false) => {
            current.status.state = "failed".into();
            current.status.error = Some("Cashout transfer did not complete".into());
        }
        _ => {
            // The bridge may have sent before losing its response. Never retry the transfer.
            current.status.error = Some("Checking your cashout transfer".into());
        }
    }
    state.near.save_intent(id, current.clone()).await?;
    Ok(Json(current.status))
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
    // After a gas top-up, the Base transfer is reported from the sign stage.
    let second_step = current.gas_topup && current.status.stage == "sign";
    if current.status.stage != "validate" && !second_step {
        return Ok(Json(current.status));
    }
    if current.sell_sui {
        return signed_sui_sell(&state, &headers, &id, &user, current, body).await;
    }
    if current.from_solana {
        let main = usize::from(current.gas_request_id.is_some());
        if !body.sent.is_empty() || body.signed.len() != main + 1 || body.signed[main].index != main
        {
            return Err(bad("signed report does not match 1Click deposit plan"));
        }
        // The stage moves first, so a repeated report can't land it twice.
        current.status.stage = "execute".into();
        state.near.save_intent(&id, current.clone()).await?;
        if let (Some(gas_id), true) = (&current.gas_request_id, main == 1) {
            markets::land_gas_topup(&state, gas_id, &body.signed[0].transaction).await;
        }
        current.status.stage = "settle".into();
        match state
            .solana_mainnet
            .send_signed(&body.signed[main].transaction)
            .await
        {
            Ok(signature) => current.status.tx_ids = vec![signature],
            Err(error) => {
                eprintln!("intent {id}: 1Click deposit from Solana not sent: {error}");
                current.status.state = "failed".into();
                current.status.error = Some(NOT_SENT.into());
            }
        }
        let result = current.status.clone();
        state.near.save_intent(&id, current).await?;
        return Ok(Json(result));
    }
    if current.gas_topup && !second_step {
        if !body.sent.is_empty() || !body.signed.is_empty() {
            return Err(bad("signed report does not match 1Click deposit plan"));
        }
        current.status.stage = "fund".into();
        state.near.save_intent(&id, current.clone()).await?;
        match markets::run_base_topup(&state, &headers, &current.wallet).await {
            Ok(uid) => current.gas_order = Some(uid),
            Err(reason) => {
                eprintln!("intent {id}: gas top-up not started: {reason}");
                current.status.state = "failed".into();
                current.status.error = Some(markets::GAS_NOT_READY.into());
            }
        }
        let result = current.status.clone();
        state.near.save_intent(&id, current).await?;
        return Ok(Json(result));
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
    if current.sell_sui && result.state == "pending" {
        if current.deposit_address.is_empty() {
            if now() > current.expires {
                result.state = "failed".into();
                result.error = Some("Cashout confirmation expired; nothing was sent".into());
                current.status = result.clone();
                state.near.save_intent(&id, current).await?;
            }
            return Ok(Json(result));
        }
        let venue_status = state
            .near
            .client
            .status(&current.deposit_address, None)
            .await
            .map_err(venue)?;
        match venue_status.status.as_str() {
            "SUCCESS" => {
                let hashes = venue_status
                    .swap_details
                    .map(|details| {
                        details
                            .destination_chain_tx_hashes
                            .into_iter()
                            .map(|tx| tx.hash)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if hashes.is_empty() {
                    return Ok(Json(result));
                }
                result.tx_ids.extend(hashes);
                result.stage = "settle".into();
                result.state = "filled".into();
                result.error = None;
            }
            "REFUNDED" | "FAILED" | "EXPIRED" => {
                result.stage = "settle".into();
                result.state = "failed".into();
                result.error = Some("Cashout did not settle; check Activity for the refund".into());
            }
            _ => return Ok(Json(result)),
        }
        current.status = result.clone();
        state.near.save_intent(&id, current).await?;
        return Ok(Json(result));
    }
    // A gas top-up filling: CoW says when; then the Base transfer is handed out (/next).
    if let (Some(uid), "fund", "pending") = (
        current.gas_order.clone(),
        result.stage.as_str(),
        result.state.as_str(),
    ) {
        match state.cow.order_state(&uid).await {
            Ok(engine_execution::layerswap::SwapState::Completed) => {
                result.stage = "sign".into();
                current.expires = now() + 120_000;
            }
            Ok(engine_execution::layerswap::SwapState::Failed(reason)) => {
                eprintln!("intent {id}: gas top-up {reason}");
                result.state = "failed".into();
                result.error = Some(markets::GAS_NOT_READY.into());
            }
            _ => return Ok(Json(result)),
        }
        current.status = result.clone();
        state.near.save_intent(&id, current).await?;
        return Ok(Json(result));
    }
    if result.stage != "settle" || result.state != "pending" {
        return Ok(Json(result));
    }
    let hash = result.tx_ids[0].clone();
    match deposit_landed(&state, &current, &hash).await? {
        Ok(false) => return Ok(Json(result)),
        Err(message) => {
            result.state = "failed".into();
            result.error = Some(message);
        }
        Ok(true) => {
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
                    if let Some(swap) = current.then_swap.clone() {
                        // The SUI is in the user's Sui wallet: swap it now, once (the stage moves
                        // first, so a second poll can't start another swap).
                        result.stage = "execute".into();
                        current.status = result.clone();
                        state.near.save_intent(&id, current.clone()).await?;
                        match bridge(
                            &state,
                            &headers,
                            "/sui/swap",
                            json!({"coinType":swap.coin_type,"amount":swap.sui_in.to_string(),
                                "reserve":SUI_GAS_RESERVE.to_string()}),
                        )
                        .await
                        {
                            Ok(body) if body["ok"].as_bool() == Some(true) => {
                                if let Some(digest) = body["digest"].as_str() {
                                    result.tx_ids.push(digest.into());
                                }
                                result.stage = "settle".into();
                                result.state = "filled".into();
                            }
                            Ok(body) => {
                                result.state = "failed".into();
                                result.error = Some(format!(
                                    "Your SUI arrived, but the swap to {} didn't go through ({}). The SUI is in your balance.",
                                    swap.symbol,
                                    body["error"].as_str().unwrap_or("swap failed")
                                ));
                            }
                            Err((_, reason)) => {
                                result.state = "failed".into();
                                result.error = Some(format!(
                                    "Your SUI arrived, but the swap to {} didn't go through ({reason}). The SUI is in your balance.",
                                    swap.symbol
                                ));
                            }
                        }
                    } else {
                        result.state = "filled".into();
                    }
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

// Whether the user's deposit to 1Click has landed: Ok(true) once it has, as planned; Ok(false)
// until then; Err with what the user reads when it failed.
async fn deposit_landed(
    state: &AppState,
    current: &StoredIntent,
    hash: &str,
) -> Result<Result<bool, String>, ApiError> {
    if current.from_solana {
        return Ok(
            match state
                .solana_mainnet
                .signature_status(hash)
                .await
                .map_err(venue)?
            {
                None => Ok(false),
                Some(Ok(())) => Ok(true),
                Some(Err(_)) => Err(NOT_SENT.into()),
            },
        );
    }
    let tx = markets::base_rpc(&state.markets, "eth_getTransactionByHash", json!([hash])).await?;
    if tx.is_null() {
        return Ok(Ok(false));
    }
    let field = |name: &str, want: &str| {
        tx[name]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(want))
    };
    if !field("from", &current.wallet)
        || !field("to", &current.expected_to)
        || !field("input", &current.expected_data)
    {
        return Ok(Err(
            "reported transaction does not match the deposit plan".into()
        ));
    }
    let receipt =
        markets::base_rpc(&state.markets, "eth_getTransactionReceipt", json!([hash])).await?;
    if receipt.is_null() {
        return Ok(Ok(false));
    }
    if receipt["status"].as_str() != Some("0x1") {
        return Ok(Err(NOT_SENT.into()));
    }
    Ok(Ok(true))
}

// The Base transfer to 1Click once an empty gas tank has been topped up (GET /v1/intents/{id}/next).
pub(super) async fn next(
    state: AppState,
    headers: HeaderMap,
    id: String,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let current = state
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
    if !current.gas_topup || current.status.stage != "sign" || current.status.state != "pending" {
        return Err(conflict("nothing to sign for this intent"));
    }
    Ok(Json(json!({"transactions":[{"chain":"base","chainId":8453,
        "to":current.expected_to,"data":current.expected_data,"value":"0"}]})))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_the_venues_minimum() {
        assert_eq!(
            venue_minimum_usd(
                r#"{"message":"Temporary swap limits: minimum swap amount is $100"}"#
            ),
            Some(100)
        );
        assert_eq!(
            venue_minimum_usd("minimum swap amount is $1,000"),
            Some(1000)
        );
        assert_eq!(
            venue_minimum_usd("Quoting for this pair is not available"),
            None
        );
    }
    #[test]
    fn listed_tokens_are_matched_by_exact_address_per_chain() {
        let listed = Listed::from_coins(&[
            json!({"id":"deep","platforms":{"sui":"0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP"}}),
            json!({"id":"usd-coin","platforms":{"base":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "near-protocol":"17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1"}}),
        ]);
        assert!(listed.has(
            "sui",
            "0xDEEB7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP"
        ));
        assert!(listed.has("base", "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"));
        assert!(listed.has(
            "near",
            "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1"
        ));
        // A look-alike with the same name but another address isn't listed.
        assert!(!listed.has("sui", "0x1234::deep::DEEP"));
        assert!(!listed.has("monad", "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"));
    }
    #[test]
    fn deposit_addresses_are_checked_per_chain() {
        // A 1Click Solana deposit address captured on 2026-09-30.
        assert!(is_solana("9r5gSYHALmVWGnhGtR5VTibkN9Yn94TX1uLqX3n7APHK"));
        assert!(!is_solana("0x845c22a46398E0a702733e556bEB6aFcB2E92132"));
        assert!(!is_solana("9r5gSYHALmVWGnhGtR5VTibkN9Yn94TX1uLqX3n7AP0K"));
        assert!(!is_solana("short"));
    }
    #[test]
    fn intents_saved_before_solana_payments_still_load() {
        let old: StoredIntent = serde_json::from_value(json!({"owner":"o","wallet":"0xw",
            "expected_to":"0xt","expected_data":"0x","deposit_address":"0xd","deposit_memo":null,
            "expires":1,"status":{"intentId":"i","stage":"settle","state":"pending","txIds":["0xh"],
            "error":null}}))
        .unwrap();
        assert!(!old.from_solana && !old.gas_topup);
        assert!(old.gas_order.is_none() && old.gas_request_id.is_none());
    }
    #[test]
    fn deposit_options_are_distinct_with_the_featured_first() {
        let ids: std::collections::HashSet<_> = DEPOSIT_NETWORKS.iter().map(|o| o.id).collect();
        assert_eq!(ids.len(), DEPOSIT_NETWORKS.len());
        let first_more = DEPOSIT_NETWORKS.iter().position(|o| !o.featured).unwrap();
        assert!(DEPOSIT_NETWORKS[first_more..].iter().all(|o| !o.featured));
        assert!(DEPOSIT_NETWORKS
            .iter()
            .filter(|o| o.dollar)
            .all(|o| ["USDC", "USDT"].contains(&o.asset)));
        assert!(ids.contains("sui-usdc"));
    }
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
    fn sui_discovery_accepts_coin_types_but_not_wallet_addresses() {
        assert!(sui_coin_type(
            "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP"
        ));
        assert!(!sui_coin_type(
            "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270"
        ));
        assert!(!sui_coin_type("javascript:evil::deep::DEEP"));
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
