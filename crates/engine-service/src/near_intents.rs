use super::*;
use base64::Engine;
use engine_execution::near_intents::{ChainTx, Client, QuoteRequest, Token};
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
    recoveries: Arc<Mutex<HashMap<String, SuiRecovery>>>,
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
    sale: Option<SuiSale>,
    // The Atlas asset id traded (for the trade book), and a Monad sale (the phone sends the coin).
    asset_id: String,
    monad_sale: bool,
}
#[derive(Clone)]
struct SuiRecovery {
    intent: StoredIntent,
    input: u128,
    expected: u128,
    expires: u64,
    currency: String,
    rate: u128,
    // The swap built when the user tapped Buy, waiting for their device's approval (bridge
    // /sui/swap/prepare); /signed brings the approval and the bridge commits it.
    prepare_id: Option<String>,
}
// A paid Sui buy whose swap failed can be finished from the SUI already received, as long as
// that SUI is still in the wallet: a swap that went through would have spent it. The wallet is
// read on-chain before every resume, so the error's wording never decides it.
const NOTHING_SENT: &str = "nothing was sent";
fn recoverable_sui(i: &StoredIntent) -> bool {
    !i.sell_sui
        && i.status.state == "failed"
        && i.status.stage == "execute"
        && i.then_swap.as_ref().is_some_and(|s| s.network != "near")
}
// Privy refuses our server's request to sign as the user (both login tokens, 2026-10-01). Buys whose
// later step signs that way would strand the money halfway (a paid DEEP buy stopped as SUI), so they're
// refused up front until that step moves to the user's device approving it, as the finish-a-paid-buy
// path already does.
pub(super) const SERVER_SIGNS_AS_USER: bool = false;
pub(super) const PAUSED_FOR_SIGNING: &str =
    "Buying this is paused for a short while, so nothing was charged. Your money is safe.";
// A bridge failure that happened before anything could reach Sui.
fn nothing_sent(reason: &str) -> bool {
    reason.contains(NOTHING_SENT) || reason.contains("nothing was signed")
}
// The reason a bridge call gave, short enough for a message ("Sui swap unavailable: " dropped).
fn short_reason(reason: &str) -> String {
    reason
        .trim_start_matches("Sui swap unavailable: ")
        .chars()
        .take(260)
        .collect()
}
// The second leg of an unlisted Sui buy.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SuiSwap {
    #[serde(default)]
    network: String,
    coin_type: String,
    symbol: String,
    name: String,
    decimals: u32,
    icon_url: Option<String>,
    // SUI (MIST) to swap, after keeping SUI_GAS_RESERVE for gas.
    sui_in: u128,
    expected_out: u128,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SuiSale {
    #[serde(default)]
    network: String,
    coin_type: String,
    symbol: String,
    name: String,
    decimals: u32,
    icon_url: Option<String>,
    amount: u128,
    minimum_sui: u128,
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
    #[serde(default)]
    sale: Option<SuiSale>,
    #[serde(default)]
    sale_permission_used: bool,
    #[serde(default)]
    ref_wallet: Option<String>,
    // The Atlas asset id traded, so a fill goes into the trade book.
    #[serde(default)]
    asset_id: String,
    // Monad: the deposit goes out from the phone on Monad (`expected_value` wei with it), and the
    // wallet's nonce when the plan was handed out: once it moves on, the plan may have gone out.
    #[serde(default)]
    origin_chain: String,
    #[serde(default)]
    expected_value: String,
    #[serde(default)]
    handed_nonce: Option<u64>,
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
            recoveries: Arc::new(Mutex::new(HashMap::new())),
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
    pub(super) async fn listed(&self) -> Arc<Listed> {
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
        intent.status.stage = if intent.sale.is_some() {
            "fund"
        } else {
            "execute"
        }
        .into();
        if let Some(pg) = &self.postgres {
            let payload = serde_json::to_string(&intent).map_err(internal)?;
            let changed = pg.execute(
                "UPDATE atlas_near_intents SET payload=$2,stage=$3 WHERE intent_id=$1 AND stage='validate'",
                &[&id, &payload, &intent.status.stage],
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
            let units = self.monad_balance(&asset, wallet).await?;
            if units > 0 {
                result.push((asset, units));
            }
        }
        Ok(result)
    }
    // What `wallet` holds of a Monad coin (native MON, or an ERC-20), in base units.
    async fn monad_balance(&self, asset: &Token, wallet: &str) -> Result<u128, ApiError> {
        if !is_evm(wallet) {
            return Err(venue("invalid Monad wallet"));
        }
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
        hex_units(&value).ok_or_else(|| venue("Monad RPC returned no balance"))
    }
    // The wallet's next Monad nonce, counting what's still pending.
    async fn monad_nonce(&self, wallet: &str) -> Result<u64, ApiError> {
        let value = self
            .monad_call("eth_getTransactionCount", json!([wallet, "pending"]))
            .await?;
        hex_units(&value)
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| venue("Monad RPC returned no nonce"))
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
            if let Some(sale) = intent.sale.as_ref().filter(|s| s.network != "near") {
                coins.insert(
                    sale.coin_type.clone(),
                    SuiSwap {
                        network: "sui".into(),
                        coin_type: sale.coin_type.clone(),
                        symbol: sale.symbol.clone(),
                        name: sale.name.clone(),
                        decimals: sale.decimals,
                        icon_url: sale.icon_url.clone(),
                        sui_in: 0,
                        expected_out: 0,
                    },
                );
            }
            let landed = intent.status.state == "filled"
                || intent.status.stage == "execute"
                || intent
                    .status
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("Your SUI arrived"));
            if let (true, Some(swap)) = (landed, intent.then_swap.filter(|s| s.network != "near")) {
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
fn hex_units(value: &Value) -> Option<u128> {
    u128::from_str_radix(value.as_str()?.strip_prefix("0x")?, 16).ok()
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
    sell_route(&token.blockchain).is_some()
        && !matches!(token.symbol.as_str(), "USDC" | "USDT" | "USDT0")
}
// How a coin on each chain goes back to cash. A chain without one isn't bought at all.
fn sell_route(blockchain: &str) -> Option<&'static str> {
    match blockchain {
        "near" => Some("1Click from the NEAR wallet, or Ref then 1Click"),
        "monad" => Some("1Click from the Monad wallet, sent by the phone"),
        "sui" => Some("1Click from the Sui wallet, after Cetus for other coins"),
        _ => None,
    }
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
        let details = intent
            .then_swap
            .as_ref()
            .filter(|s| s.network == "near")
            .map(|s| (&s.coin_type, &s.symbol, s.decimals))
            .or_else(|| {
                intent
                    .sale
                    .as_ref()
                    .filter(|s| s.network == "near")
                    .map(|s| (&s.coin_type, &s.symbol, s.decimals))
            });
        let through_ref = details.is_some();
        if let Some((contract, symbol, decimals)) = details {
            let asset = Token {
                asset_id: format!("ref:{contract}"),
                blockchain: "near".into(),
                symbol: symbol.clone(),
                decimals,
                contract_address: Some(contract.clone()),
                price: None,
                coingecko_id: None,
            };
            tracked.insert(asset.asset_id.clone(), asset);
        }
        // A Ref swap's wNEAR (bought for it, or the proceeds of a sale) counts too, whether or not
        // the rest went through: if a step failed, that's where the money is.
        if intent.status.state == "filled" || through_ref {
            if let Some(asset) = intent.asset.filter(|asset| asset.blockchain == "near") {
                tracked.insert(asset.asset_id.clone(), asset);
            }
        }
    }
    if tracked.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = state.near.tokens().await.unwrap_or_default();
    let mut priced = HashMap::new();
    for (id, mut asset) in tracked {
        if let Some(contract) = asset.asset_id.strip_prefix("ref:") {
            // A Ref coin nobody can price right now is left out, not the whole balance.
            let Some(fresh) = ref_assets(state, contract)
                .await
                .ok()
                .and_then(|list| list.into_iter().find(|a| a["token"] == contract))
            else {
                continue;
            };
            asset.price = Some(fresh["price"].clone());
            priced.insert(id, asset);
            continue;
        }
        let fresh = catalog
            .iter()
            .find(|token| token.asset_id == asset.asset_id)
            .ok_or_else(|| venue("An asset price is temporarily unavailable"))?;
        priced.insert(id, fresh.clone());
    }
    let tracked = priced;
    if tracked.is_empty() {
        return Ok(Vec::new());
    }
    let wallet = destination(state, headers, tracked.values().next().unwrap(), user).await?;
    let mut held = Vec::new();
    for asset in tracked.into_values() {
        let contract = asset
            .contract_address
            .as_deref()
            .ok_or_else(|| venue("An asset balance is temporarily unavailable"))?;
        if contract
            != asset
                .asset_id
                .trim_start_matches("nep141:")
                .trim_start_matches("ref:")
        {
            return Err(venue("An asset balance is temporarily unavailable"));
        }
        let units = near_coin_held(state, contract, &wallet).await?;
        if units > 0 {
            held.push((asset, units));
        }
    }
    Ok(held)
}

// NEAR RPC: one `query`, answered as the RPC sends it (errors included, for the caller to read).
async fn near_query(state: &AppState, params: Value) -> Result<Value, ApiError> {
    let rpc = env_url("ATLAS_NEAR_MAINNET_RPC_URL", "https://rpc.mainnet.near.org");
    state
        .near
        .icon_http
        .post(&rpc)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"query","params":params}))
        .send()
        .await
        .map_err(venue)?
        .error_for_status()
        .map_err(venue)?
        .json()
        .await
        .map_err(venue)
}
// A NEP-141 token balance of `account`, in base units.
async fn ft_balance(state: &AppState, contract: &str, account: &str) -> Result<u128, ApiError> {
    let args = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&json!({"account_id":account})).map_err(internal)?);
    let body = near_query(
        state,
        json!({"request_type":"call_function","finality":"final","account_id":contract,
            "method_name":"ft_balance_of","args_base64":args}),
    )
    .await?;
    let bytes = body["result"]["result"]
        .as_array()
        .and_then(|raw| {
            raw.iter()
                .map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| venue("An asset balance is temporarily unavailable"))?;
    let units: String = serde_json::from_slice(&bytes).map_err(venue)?;
    units.parse::<u128>().map_err(venue)
}
// Native NEAR (yocto) in `account`; one that doesn't exist yet holds none.
async fn near_native(state: &AppState, account: &str) -> Result<u128, ApiError> {
    let body = near_query(
        state,
        json!({"request_type":"view_account","finality":"final","account_id":account}),
    )
    .await?;
    if body["error"].to_string().contains("UNKNOWN_ACCOUNT")
        || body["result"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("does not exist"))
    {
        return Ok(0);
    }
    body["result"]["amount"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| venue("Your NEAR balance is temporarily unavailable"))
}
// What the user holds of a NEAR coin. 1Click pays NEAR out as native NEAR (it unwraps wNEAR on the
// way), so for NEAR itself the native balance counts with any wNEAR.
async fn near_coin_held(state: &AppState, contract: &str, account: &str) -> Result<u128, ApiError> {
    let tokens = ft_balance(state, contract, account).await?;
    if contract == WRAP_NEAR {
        return Ok(tokens.saturating_add(near_native(state, account).await?));
    }
    Ok(tokens)
}
const WRAP_NEAR: &str = "wrap.near";
// What the user holds on Sui from Atlas buys (SUI left for gas included), valued in USD:
// (asset id, symbol, name, decimals, units, USDC units, icon). Nothing to read, nothing asked.
fn sui_coin_key(value: &str) -> String {
    match value.split_once("::") {
        Some((address, rest)) => format!(
            "{}::{rest}",
            address
                .trim_start_matches("0x")
                .trim_start_matches('0')
                .to_ascii_lowercase()
        ),
        None => value.into(),
    }
}
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
    let balances = bridge(state, headers, "/sui/balances", json!({})).await?;
    if balances["address"].as_str() != Some(&owner) {
        return Err(venue("Balance wallet did not match"));
    }
    let held = |coin: &str| -> u128 {
        balances["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| {
                b["coinType"]
                    .as_str()
                    .is_some_and(|t| sui_coin_key(t) == sui_coin_key(coin))
            })
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
    // Each hop's transactions with links to see them: the deposit on its own chain, the swap on
    // NEAR, the payout on Solana. Only https links pass.
    let details = status.swap_details.as_ref();
    let hop = |txs: &[ChainTx]| -> Vec<Value> {
        txs.iter()
            .filter(|t| t.hash.len() <= 128)
            .map(|t| {
                let url = t
                    .explorer_url
                    .as_deref()
                    .filter(|u| u.starts_with("https://") && u.len() <= 300);
                json!({"hash": t.hash, "url": url})
            })
            .collect()
    };
    let near_hop: Vec<Value> = details
        .map(|d| {
            d.near_tx_hashes
                .iter()
                .filter(|h| {
                    !h.is_empty() && h.len() <= 64 && h.bytes().all(|b| b.is_ascii_alphanumeric())
                })
                .map(|h| json!({"hash": h, "url": format!("https://nearblocks.io/txns/{h}")}))
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "state": state_name,
        "journey": {
            "deposit": details.map(|d| hop(&d.origin_chain_tx_hashes)).unwrap_or_default(),
            "swap": near_hop,
            "payout": details.map(|d| hop(&d.destination_chain_tx_hashes)).unwrap_or_default(),
        }
    })))
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
    body["identityToken"] = json!(app_balance::identity_token(headers));
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

async fn sui_recovery_quote(
    state: &AppState,
    headers: &HeaderMap,
    req: &markets::QuoteRequest,
    user: &app_balance::VerifiedWallets,
    coin: &str,
) -> Result<Option<Value>, ApiError> {
    let intents: Vec<StoredIntent> = if let Some(pg) = &state.near.postgres {
        pg.query(
            "SELECT payload FROM atlas_near_intents WHERE owner=$1 AND stage='execute'",
            &[&user.user_id],
        )
        .await
        .map_err(internal)?
        .into_iter()
        .map(|row| serde_json::from_str(row.get::<_, &str>(0)).map_err(internal))
        .collect::<Result<_, _>>()?
    } else {
        state
            .near
            .intents
            .lock()
            .map_err(internal)?
            .values()
            .filter(|i| i.owner == user.user_id)
            .cloned()
            .collect()
    };
    let Some(intent) = intents.into_iter().find(|i| {
        i.status.stage == "execute"
            && i.status.state != "filled"
            && i.then_swap
                .as_ref()
                .is_some_and(|s| sui_coin_key(&s.coin_type) == sui_coin_key(coin))
    }) else {
        return Ok(None);
    };
    if !recoverable_sui(&intent) {
        return Err(conflict("Your previous purchase needs checking before another payment. Check your asset balance; do not pay again."));
    }
    let swap = intent
        .then_swap
        .as_ref()
        .ok_or_else(|| conflict("purchase cannot be resumed"))?;
    let balance = bridge(state, headers, "/sui/balance", json!({})).await?;
    if balance["address"].as_str()
        != intent
            .ref_wallet
            .as_deref()
            .or(intent.sui_wallet.as_deref())
    {
        return Err(conflict(
            "receiving wallet changed; purchase was not resumed",
        ));
    }
    let held = balance["result"]["totalBalance"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .ok_or_else(|| venue("Your asset balance is unavailable"))?;
    // The SUI this buy received has to still be there: if it isn't, a swap already used it.
    if held < swap.sui_in {
        return Err(conflict(&format!(
            "This purchase's SUI has already been used. Check your {} balance; don't pay again.",
            swap.symbol
        )));
    }
    let input = held.saturating_sub(SUI_GAS_RESERVE).min(swap.sui_in);
    if input == 0 {
        return Err(conflict(
            "The funds from this purchase are no longer available to finish it",
        ));
    }
    let routed = bridge(
        state,
        headers,
        "/sui/quote",
        json!({"coinType":coin,"amount":input.to_string()}),
    )
    .await?;
    let expected = routed["amountOut"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| venue("A fresh price is unavailable; nothing else was spent"))?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let quote_id = id("q");
    let expires = now() + 30_000;
    let response = json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"buy",
        "pay":{"amount":"0","symbol":"USDC","value":money(0,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(expected,swap.decimals),"symbol":swap.symbol,
            "value":money(intent.amount,&req.amount.currency,rate)},
        "price":unit_price(intent.amount,expected,swap.decimals,&req.amount.currency,rate),
        "fee":money(0,&req.amount.currency,rate),"expiresAtUnixMs":expires,
        "warning":"Finish your already-paid purchase. No new cash payment; network fees come from the received funds."});
    let mut recoveries = state.near.recoveries.lock().map_err(internal)?;
    recoveries.retain(|_, r| r.expires > now());
    recoveries.insert(
        quote_id,
        SuiRecovery {
            intent,
            input,
            expected,
            expires,
            currency: req.amount.currency.clone(),
            rate,
            prepare_id: None,
        },
    );
    Ok(Some(response))
}

async fn resume_sui_buy(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    mut current: StoredIntent,
    recovery: SuiRecovery,
    approval: String,
) -> Result<markets::IntentStatus, ApiError> {
    if current.owner != recovery.intent.owner {
        return Err((
            StatusCode::FORBIDDEN,
            "purchase belongs to another user".into(),
        ));
    }
    if !recoverable_sui(&current) {
        return Ok(current.status);
    }
    if recovery.expires <= now() {
        return Err(conflict("quote expired; request a fresh quote"));
    }
    let before = current
        .status
        .error
        .clone()
        .ok_or_else(|| conflict("purchase cannot be resumed"))?;
    // Claim before any signing; a lost response is never permission to submit another swap.
    current.status.state = "pending".into();
    current.status.error = None;
    let after = serde_json::to_string(&current).map_err(internal)?;
    let claimed = if let Some(pg) = &state.near.postgres {
        pg.execute("UPDATE atlas_near_intents SET payload=$3 WHERE intent_id=$1 AND stage='execute' AND payload::jsonb->'status'->>'state'='failed' AND payload::jsonb->'status'->>'error'=$2",
            &[&id,&before,&after]).await.map_err(internal)?==1
    } else {
        let mut intents = state.near.intents.lock().map_err(internal)?;
        if intents.get(id).is_some_and(recoverable_sui) {
            intents.insert(id.into(), current.clone());
            true
        } else {
            false
        }
    };
    if !claimed {
        return Ok(state
            .near
            .get_intent(id)
            .await?
            .ok_or_else(|| conflict("purchase changed"))?
            .status);
    }
    state.near.recoveries.lock().map_err(internal)?.remove(id);
    let swap = current
        .then_swap
        .as_ref()
        .ok_or_else(|| conflict("purchase cannot be resumed"))?;
    // The user's device approved this exact swap; the bridge passes that to Privy and sends it.
    let result = bridge(
        state,
        headers,
        "/sui/swap/commit",
        json!({"prepareId": recovery.prepare_id, "signature": approval}),
    )
    .await;
    match result {
        Ok(body)
            if body["ok"].as_bool() == Some(true)
                && body["amountOut"]
                    .as_str()
                    .and_then(|s| s.parse::<u128>().ok())
                    .is_some_and(|n| n > 0) =>
        {
            let bought = body["amountOut"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let digest = body["digest"].as_str().map(str::to_owned);
            if let Some(tx) = &digest {
                current.status.tx_ids.push(tx.clone());
            }
            current.status.stage = "settle".into();
            current.status.state = "filled".into();
            record_fill(state, id, &current, "buy", bought, current.amount, digest).await;
        }
        // Stopped before anything reached Sui (a price move, Privy refusing to sign…): the SUI is
        // untouched and the buy can be finished again.
        Err((_, reason)) if nothing_sent(&reason) => {
            eprintln!("intent {id}: Sui recovery stopped before sending: {reason}");
            current.status.state = "failed".into();
            current.status.error = Some(format!(
                "Couldn't finish yet ({}). Nothing was sent: your SUI is still in your Sui wallet. Ask for {} again to retry.",
                short_reason(&reason),
                swap.symbol
            ));
        }
        Err((_, reason)) => {
            eprintln!("intent {id}: Sui recovery may have been sent: {reason}");
            current.status.state = "failed".into();
            current.status.error = Some(format!(
                "The swap's result isn't known yet ({}). Don't pay again: check your {} balance in a minute.",
                short_reason(&reason),
                swap.symbol
            ));
        }
        Ok(body) => {
            let reason = body["error"].as_str().unwrap_or("swap failed").to_string();
            eprintln!("intent {id}: Sui recovery swap failed on Sui: {reason}");
            current.status.state = "failed".into();
            current.status.error = Some(format!(
                "The swap didn't go through on Sui ({}). Your SUI is still in your Sui wallet: ask for {} again to retry.",
                short_reason(&reason),
                swap.symbol
            ));
        }
    }
    state.near.save_intent(id, current.clone()).await?;
    Ok(current.status)
}

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
    if let Some(recovery) = sui_recovery_quote(&state, &headers, &req, &user, coin).await? {
        return Ok(Json(recovery));
    }
    if !SERVER_SIGNS_AS_USER && sui_coin_key(coin) != sui_coin_key("0x2::sui::SUI") {
        return Err(conflict(PAUSED_FOR_SIGNING));
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
            sale: None,
            from_solana,
            network_fee,
            then_swap: Some(SuiSwap {
                network: "sui".into(),
                coin_type: coin.into(),
                symbol: symbol.clone(),
                name,
                decimals,
                icon_url,
                sui_in,
                expected_out,
            }),
            asset_id: req.asset_id.clone(),
            monad_sale: false,
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

async fn ref_buy_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    coin: &str,
) -> Result<Json<Value>, ApiError> {
    if !near_account(coin) {
        return Err(bad("Invalid token contract"));
    }
    if !SERVER_SIGNS_AS_USER {
        return Err(conflict(PAUSED_FOR_SIGNING));
    }
    let sui = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| t.blockchain == "near" && t.contract_address.as_deref() == Some("wrap.near"))
        .ok_or_else(|| venue("The cash route is unavailable right now"))?;
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
    let minimum: u128 = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    if minimum <= NEAR_GAS_RESERVE * 2 || minimum > sui_out {
        return Err(bad(
            "That amount is too small after the network fee. Try a bigger amount.",
        ));
    }
    // The swap spends what's sure to arrive (1Click's minimum), not its estimate, less NEAR for gas.
    let sui_in = minimum - NEAR_GAS_RESERVE;
    let routed = bridge(
        &state,
        &headers,
        "/near/ref/quote",
        json!({"token":coin,"amount":sui_in.to_string(),"sell":false}),
    )
    .await?;
    let metadata = &routed["metadata"];
    let symbol = metadata["symbol"]
        .as_str()
        .ok_or_else(|| venue("Token details unavailable"))?
        .to_owned();
    let name = metadata["name"].as_str().unwrap_or(&symbol).to_owned();
    let decimals = metadata["decimals"]
        .as_u64()
        .filter(|d| *d <= 24)
        .ok_or_else(|| venue("Token decimals unavailable"))? as u32;
    let icon_url = metadata["icon"]
        .as_str()
        .filter(|v| v.starts_with("https://"))
        .map(str::to_owned);
    let expected_out: u128 = routed["amountOut"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| venue("No liquid route for this token"))?;
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
            sale: None,
            from_solana,
            network_fee,
            then_swap: Some(SuiSwap {
                network: "near".into(),
                coin_type: coin.into(),
                symbol: symbol.clone(),
                name,
                decimals,
                icon_url,
                sui_in,
                expected_out,
            }),
            asset_id: req.asset_id.clone(),
            monad_sale: false,
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

const NEAR_GAS_RESERVE: u128 = 50_000_000_000_000_000_000_000;
fn near_account(value: &str) -> bool {
    (2..=64).contains(&value.len())
        && value.ends_with(".near")
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
async fn ref_assets(state: &AppState, query: &str) -> Result<Vec<Value>, ApiError> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Ok(vec![]);
    };
    let response: Value = http
        .post(format!("{bridge_url}/near/ref/search"))
        .timeout(Duration::from_secs(20))
        .json(&json!({"query":query,"identityToken":null}))
        .send()
        .await
        .map_err(venue)?
        .error_for_status()
        .map_err(venue)?
        .json()
        .await
        .map_err(venue)?;
    Ok(response["assets"].as_array().cloned().unwrap_or_default())
}
async fn search_ref_unlisted(
    state: &AppState,
    query: &str,
    currency: &str,
    rate: u128,
) -> Vec<Value> {
    let Ok(found) = ref_assets(state, query).await else {
        return vec![];
    };
    // Verified only when CoinGecko lists that exact contract on NEAR, like pasted coins elsewhere.
    let listed = state.near.listed().await;
    found.into_iter().filter_map(|a|{
        let token=a["token"].as_str()?;
        let price=a["price"].as_str()?.parse::<f64>().ok()?;
        if !price.is_finite() || price<=0.0 {return None}
        let display=price*rate as f64/1_000_000.0;
        Some(json!({"assetId":format!("near:ref:{token}"),"symbol":a["symbol"],"name":a["name"],"kind":"crypto",
            "price":{"amount":format!("{display:.12}").trim_end_matches('0').trim_end_matches('.'),"currency":currency},
            "iconUrl":a["icon"].as_str().filter(|v|v.starts_with("https://")),"verified":listed.has("near",token),"tradeable":a["tradeable"],"change24hPct":null}))
    }).collect()
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
    let list = state.near.tokens().await.unwrap_or_default();
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
            "name":format!("{} on {}",t.symbol,t.blockchain),"kind":"crypto","chain":t.blockchain,
            "price":{"amount":format!("{display:.12}").trim_end_matches('0').trim_end_matches('.'),
                "currency":currency},"change24hPct":null,"iconUrl":icon,"verified":true}),
        );
        if out.len() >= 30 {
            break;
        }
    }
    out.extend(search_sui_unlisted(state, &query, currency, rate).await);
    out.extend(search_ref_unlisted(state, &query, currency, rate).await);
    Ok(out)
}
// A NEAR Intents coin's live price and what the user holds of it, for its position card: NEAR and
// Monad coins from 1Click's list, and NEAR coins found on Ref.
pub(super) struct NearCoin {
    pub(super) symbol: String,
    pub(super) name: String,
    pub(super) chain: String,
    pub(super) icon: Option<String>,
    pub(super) decimals: u32,
    pub(super) price: f64,
    pub(super) held: u128,
}
pub(super) async fn position_coin(
    state: &AppState,
    headers: &HeaderMap,
    user: &app_balance::VerifiedWallets,
    asset_id: &str,
) -> Option<NearCoin> {
    let id = asset_id.strip_prefix("near:")?;
    let near_wallet = |contract: &str| Token {
        asset_id: format!("nep141:{contract}"),
        blockchain: "near".into(),
        symbol: String::new(),
        decimals: 0,
        contract_address: Some(contract.into()),
        price: None,
        coingecko_id: None,
    };
    if let Some(contract) = id.strip_prefix("ref:") {
        let found = ref_assets(state, contract)
            .await
            .ok()?
            .into_iter()
            .find(|a| a["token"] == contract)?;
        let wallet = destination(state, headers, &near_wallet(contract), user)
            .await
            .ok()?;
        let symbol = found["symbol"].as_str()?.to_owned();
        return Some(NearCoin {
            name: found["name"].as_str().unwrap_or(&symbol).to_owned(),
            symbol,
            chain: "near".into(),
            icon: found["icon"]
                .as_str()
                .filter(|v| v.starts_with("https://"))
                .map(str::to_owned),
            decimals: u32::try_from(found["decimals"].as_u64()?).ok()?,
            price: found["price"].as_str()?.parse().ok()?,
            held: ft_balance(state, contract, &wallet).await.ok()?,
        });
    }
    let token = state
        .near
        .tokens()
        .await
        .ok()?
        .into_iter()
        .find(|t| t.asset_id == id && supported(t))?;
    let held = match token.blockchain.as_str() {
        "near" => {
            let contract = token.contract_address.clone()?;
            let wallet = destination(state, headers, &token, user).await.ok()?;
            near_coin_held(state, &contract, &wallet).await.ok()?
        }
        "monad" => {
            let wallet = user.evm_wallet.as_deref()?;
            state.near.monad_balance(&token, wallet).await.ok()?
        }
        _ => return None,
    };
    Some(NearCoin {
        name: format!("{} on {}", token.symbol, token.blockchain),
        icon: state.near.icon_for(&token),
        chain: token.blockchain.clone(),
        decimals: token.decimals,
        price: token_usd(&token),
        symbol: token.symbol,
        held,
    })
}
// Where every sale's cash lands: USDC in the user's own Solana wallet, or Base without one. Kept in
// this one place (and its twin `cashDestination` in the bridge) so naira payouts can follow on later.
fn cash_target(user: &app_balance::VerifiedWallets) -> Result<(&'static str, &str), ApiError> {
    if let Some(wallet) = user.solana_wallet.as_deref() {
        return Ok((SOLANA_USDC_1CLICK, wallet));
    }
    user.evm_wallet
        .as_deref()
        .filter(|w| is_evm(w))
        .map(|w| (BASE_USDC_1CLICK, w))
        .ok_or_else(|| conflict("Your cash wallet is not ready"))
}
fn sell_units(
    held: u128,
    value: u128,
    price: u128,
    decimals: u32,
    all: bool,
) -> Result<u128, ApiError> {
    if all {
        return (held > 0)
            .then_some(held)
            .ok_or_else(|| bad("No holding to sell"));
    }
    if price == 0 || decimals > 24 {
        return Err(venue("Price is unavailable"));
    }
    let units = value
        .checked_mul(10u128.pow(decimals))
        .ok_or_else(|| bad("Amount too large"))?
        / price;
    if units == 0 || units > held {
        return Err(bad("The sell amount exceeds your holding"));
    }
    Ok(units)
}
// MON kept back for the network fee when selling it: a plain transfer costs about 0.002 MON.
const MONAD_GAS_RESERVE: u128 = 20_000_000_000_000_000;
const NEAR_GAS_SHORT: &str = "Selling needs about 0.05 NEAR in your NEAR wallet for network fees. Buy a little NEAR first, then sell.";

// A 1Click coin's USD price, as the token list gives it (memecoins sit far below a micro-dollar).
fn token_usd(token: &Token) -> f64 {
    token
        .price
        .as_ref()
        .and_then(|p| p.as_f64().or_else(|| p.as_str()?.parse().ok()))
        .filter(|p| p.is_finite() && *p > 0.0)
        .unwrap_or(0.0)
}
// Base units of a coin worth `usd_micros`, and what base units are worth (USD micros).
fn units_worth(usd_micros: u128, price: f64, decimals: u32) -> u128 {
    if price <= 0.0 {
        return 0;
    }
    (usd_micros as f64 / 1e6 / price * 10f64.powi(decimals as i32)) as u128
}
fn worth_of(units: u128, price: f64, decimals: u32) -> u128 {
    (units as f64 / 10f64.powi(decimals as i32) * price * 1e6) as u128
}
// How much to sell: the whole holding for Max, else what the asked value buys at the coin's price.
fn sale_units(
    held: u128,
    value: u128,
    price: f64,
    decimals: u32,
    all: bool,
) -> Result<u128, ApiError> {
    if all {
        return (held > 0)
            .then_some(held)
            .ok_or_else(|| bad("No holding to sell"));
    }
    if price <= 0.0 {
        return Err(venue("Price is unavailable"));
    }
    let units = units_worth(value, price, decimals);
    if units == 0 || units > held {
        return Err(bad("The sell amount exceeds your holding"));
    }
    Ok(units)
}
// 1Click's quote for exactly `amount` of `origin` to the user's cash, refunded to `refund_to` on the
// coin's own chain if it can't go through.
async fn cash_quote(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    origin: &Token,
    amount: u128,
    refund_to: &str,
    dry: bool,
) -> Result<engine_execution::near_intents::Quote, ApiError> {
    let (cash_asset, cash_wallet) = cash_target(user)?;
    let deadline = deadline_utc(if dry { 180 } else { 240 });
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        &origin.asset_id,
        cash_asset,
        &units,
        cash_wallet,
        refund_to,
        &deadline,
        dry,
    );
    let q = state.near.client.quote(&request).await.map_err(venue)?;
    if q.amount_in.parse::<u128>().ok() != Some(amount) {
        return Err(venue("1Click changed the exact input amount"));
    }
    Ok(q)
}
fn min_out(q: &engine_execution::near_intents::Quote) -> Result<u128, ApiError> {
    q.min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)
}
// Whether Atlas can sell `token` back to cash right now (about `usd_micros` of it): Atlas won't buy
// a coin it can't sell.
async fn sellable(
    state: &AppState,
    headers: &HeaderMap,
    user: &app_balance::VerifiedWallets,
    token: &Token,
    usd_micros: u128,
) -> bool {
    if token.blockchain == "sui" {
        return true;
    }
    let units = units_worth(usd_micros, token_usd(token), token.decimals);
    if units == 0 {
        return false;
    }
    let Ok(refund) = destination(state, headers, token, user).await else {
        return false;
    };
    if cash_quote(state, user, token, units, &refund, true)
        .await
        .is_ok()
    {
        return true;
    }
    // A NEAR coin 1Click won't take directly can still go out through Ref.
    match token.contract_address.as_deref() {
        Some(contract) if token.blockchain == "near" && near_account(contract) => {
            ref_assets(state, contract).await.is_ok_and(|found| {
                found
                    .iter()
                    .any(|a| a["token"] == contract && a["tradeable"] == true)
            })
        }
        _ => false,
    }
}
#[allow(clippy::too_many_arguments)]
fn sale_json(
    quote_id: &str,
    asset_id: &str,
    paid: (u128, u32, &str),
    value: u128,
    receive: u128,
    currency: &str,
    rate: u128,
    expires: u64,
) -> Value {
    let (amount, decimals, symbol) = paid;
    json!({"quoteId":quote_id,"assetId":asset_id,"side":"sell",
        "pay":{"amount":markets::format_units(amount,decimals),"symbol":symbol,"value":money(value,currency,rate)},
        "receive":{"amount":markets::format_units(receive,6),"symbol":"USDC","value":money(receive,currency,rate)},
        "price":unit_price(receive,amount,decimals,currency,rate),
        "fee":money(value.saturating_sub(receive),currency,rate),"expiresAtUnixMs":expires})
}

// Selling a NEAR coin Atlas bought through 1Click: straight to 1Click from the user's NEAR wallet,
// or through Ref to wNEAR first when 1Click can't take it or Ref leaves more cash. Either way the
// bridge signs once for exactly what's confirmed here (prepare → commit), and the cash lands as USDC.
async fn near_coin_sell_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    token: Token,
) -> Result<Json<Value>, ApiError> {
    let contract = token
        .contract_address
        .clone()
        .filter(|c| near_account(c))
        .ok_or_else(|| bad("Atlas can't sell this coin"))?;
    let wallet = destination(&state, &headers, &token, &user).await?;
    if near_native(&state, &wallet).await? < NEAR_GAS_RESERVE {
        return Err(conflict(NEAR_GAS_SHORT));
    }
    let mut held = near_coin_held(&state, &contract, &wallet).await?;
    if contract == WRAP_NEAR {
        // NEAR keeps its own fee money.
        held = held.saturating_sub(NEAR_GAS_RESERVE);
    }
    let price = token_usd(&token);
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let desired = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("Amount too large"))?
        / rate;
    let amount = sale_units(held, desired, price, token.decimals, req.all)?;
    let value = worth_of(amount, price, token.decimals);
    markets::check_limits(value, &req.amount.currency, rate)?;
    let direct = cash_quote(&state, &user, &token, amount, &wallet, true).await;
    let via_ref = if contract == WRAP_NEAR {
        None
    } else {
        let quoted = ref_sell_quote(
            state.clone(),
            headers.clone(),
            req.clone(),
            user.clone(),
            &contract,
        )
        .await
        .ok();
        quoted.and_then(|json| {
            let id = json["quoteId"].as_str()?.to_owned();
            let minimum = state.near.quotes.lock().ok()?.get(&id)?.minimum_out;
            Some((json, id, minimum))
        })
    };
    let direct_minimum = match &direct {
        Ok(q) => Some(min_out(q)?),
        Err(_) => None,
    };
    // Ref leaves more (or 1Click can't take it): Ref's quote stands as it is.
    if let Some((json, id, ref_minimum)) = via_ref {
        if direct_minimum.is_none_or(|d| ref_minimum > d) {
            return Ok(json);
        }
        state.near.quotes.lock().map_err(internal)?.remove(&id);
    }
    let (Ok(q), Some(minimum)) = (direct, direct_minimum) else {
        return Err(conflict(&format!(
            "Atlas can't sell {} to cash right now. Try again later.",
            token.symbol
        )));
    };
    let receive: u128 = q.amount_out.parse().map_err(internal)?;
    let (from_solana, paying) = match user.evm_wallet.clone() {
        Some(evm) => (false, evm),
        None => (
            true,
            user.solana_wallet
                .clone()
                .ok_or_else(|| conflict("Your cash wallet is not ready"))?,
        ),
    };
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id.clone(),
            wallet: paying,
            recipient: wallet,
            asset: token.clone(),
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: true,
            from_solana,
            network_fee: value.saturating_sub(receive),
            sale: Some(SuiSale {
                network: "nearintents".into(),
                coin_type: contract,
                symbol: token.symbol.clone(),
                name: token.symbol.clone(),
                decimals: token.decimals,
                icon_url: None,
                amount,
                minimum_sui: minimum,
            }),
            asset_id: req.asset_id.clone(),
            monad_sale: false,
        },
    );
    Ok(Json(sale_json(
        &quote_id,
        &req.asset_id,
        (amount, token.decimals, &token.symbol),
        value,
        receive,
        &req.amount.currency,
        rate,
        expires,
    )))
}

// Selling a Monad coin: the phone sends it to 1Click's deposit address (one plain transfer, MON
// itself or an ERC-20 `transfer`), and the cash lands as USDC.
async fn monad_sell_quote(
    state: AppState,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    token: Token,
) -> Result<Json<Value>, ApiError> {
    let wallet = user
        .evm_wallet
        .clone()
        .filter(|w| is_evm(w))
        .ok_or_else(|| conflict("Your wallet is still being set up. Try again in a moment."))?;
    let held = state.near.monad_balance(&token, &wallet).await?;
    let native = token.contract_address.is_none();
    let gas = if native {
        held
    } else {
        let mon = Token {
            contract_address: None,
            ..token.clone()
        };
        state.near.monad_balance(&mon, &wallet).await?
    };
    if gas < MONAD_GAS_RESERVE {
        return Err(conflict(
            "Selling needs a little MON in your wallet for the network fee (about 0.02 MON).",
        ));
    }
    let available = if native {
        held - MONAD_GAS_RESERVE
    } else {
        held
    };
    let price = token_usd(&token);
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let desired = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("Amount too large"))?
        / rate;
    let amount = sale_units(available, desired, price, token.decimals, req.all)?;
    let value = worth_of(amount, price, token.decimals);
    markets::check_limits(value, &req.amount.currency, rate)?;
    let q = cash_quote(&state, &user, &token, amount, &wallet, true)
        .await
        .map_err(|_| {
            conflict(&format!(
                "{} can't be sold to cash right now: NEAR Intents isn't taking it. Try again later.",
                token.symbol
            ))
        })?;
    let receive: u128 = q.amount_out.parse().map_err(internal)?;
    let minimum = min_out(&q)?;
    let quote_id = id("q");
    let expires = now() + 30_000;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id.clone(),
            wallet: wallet.clone(),
            recipient: wallet,
            asset: token.clone(),
            amount,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: false,
            from_solana: false,
            network_fee: value.saturating_sub(receive),
            sale: None,
            asset_id: req.asset_id.clone(),
            monad_sale: true,
        },
    );
    Ok(Json(sale_json(
        &quote_id,
        &req.asset_id,
        (amount, token.decimals, &token.symbol),
        value,
        receive,
        &req.amount.currency,
        rate,
        expires,
    )))
}

async fn sui_coin_sell_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    coin: &str,
) -> Result<Json<Value>, ApiError> {
    if !sui_coin_type(coin) {
        return Err(bad("Invalid asset"));
    }
    let token = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| t.blockchain == "sui" && t.symbol == "SUI")
        .ok_or_else(|| venue("Cashout is unavailable"))?;
    let recipient = destination(&state, &headers, &token, &user).await?;
    let (symbol, name, decimals, icon_url) = sui_meta(&state, coin)
        .await
        .ok_or_else(|| venue("Asset details unavailable"))?;
    let body = bridge(&state, &headers, "/sui/balance", json!({"coinType":coin})).await?;
    if body["address"].as_str() != Some(&recipient) {
        return Err(venue("Balance wallet did not match"));
    }
    let held = body["result"]["totalBalance"]
        .as_str()
        .and_then(|v| v.parse::<u128>().ok())
        .ok_or_else(|| venue("Your holding is unavailable"))?;
    let pairs: Value = state
        .near
        .icon_http
        .get(format!("https://api.dexscreener.com/tokens/v1/sui/{coin}"))
        .send()
        .await
        .map_err(venue)?
        .error_for_status()
        .map_err(venue)?
        .json()
        .await
        .map_err(venue)?;
    let price = pairs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["baseToken"]["address"].as_str() == Some(coin))
        .max_by(|a, b| {
            a["liquidity"]["usd"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["liquidity"]["usd"].as_f64().unwrap_or(0.0))
        })
        .and_then(|p| p["priceUsd"].as_str())
        .map(usd_micros)
        .transpose()?
        .unwrap_or(0);
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let desired = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("Amount too large"))?
        / rate;
    let amount = sell_units(held, desired, price, decimals, req.all)?;
    let value = amount
        .checked_mul(price)
        .ok_or_else(|| bad("Amount too large"))?
        / 10u128.pow(decimals);
    markets::check_limits(value, &req.amount.currency, rate)?;
    let routed = bridge(
        &state,
        &headers,
        "/sui/sale/quote",
        json!({"coinType":coin,"amount":amount.to_string()}),
    )
    .await?;
    if routed["userId"].as_str() != Some(&user.user_id)
        || routed["address"].as_str() != Some(&recipient)
    {
        return Err(venue("Sale wallet did not match"));
    }
    let output = routed["amountOut"]
        .as_str()
        .and_then(|v| v.parse::<u128>().ok())
        .filter(|v| *v > SUI_GAS_RESERVE * 2)
        .ok_or_else(|| bad("The sale is too small after network fees"))?;
    let min_sui = output
        .checked_mul(99)
        .ok_or_else(|| bad("Amount too large"))?
        / 100;
    // Price cashout conservatively, allowing gas; execution uses the actual net swap credit.
    let cashout = min_sui - SUI_GAS_RESERVE;
    let (cash_asset, cash_wallet) = cash_target(&user)?;
    let deadline = deadline_utc(180);
    let units = cashout.to_string();
    let q=state.near.client.quote(&QuoteRequest::exact_input(&token.asset_id,cash_asset,&units,cash_wallet,
        &recipient,&deadline,true)).await.map_err(|_|bad("The cashout is below the available route minimum or unavailable. Try a larger amount."))?;
    let receive: u128 = q.amount_out.parse().map_err(internal)?;
    let minimum = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    let quote_id = id("q");
    let expires = now() + 30_000;
    let from_solana = user.evm_wallet.is_none();
    let wallet = if from_solana {
        user.solana_wallet.clone()
    } else {
        user.evm_wallet.clone()
    }
    .ok_or_else(|| conflict("Your cash wallet is not ready"))?;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            wallet,
            recipient,
            asset: token,
            amount: cashout,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: true,
            from_solana,
            network_fee: value.saturating_sub(receive),
            sale: Some(SuiSale {
                network: "sui".into(),
                coin_type: coin.into(),
                symbol: symbol.clone(),
                name,
                decimals,
                icon_url,
                amount,
                minimum_sui: min_sui,
            }),
            asset_id: req.asset_id.clone(),
            monad_sale: false,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"sell",
        "pay":{"amount":markets::format_units(amount,decimals),"symbol":symbol,"value":money(value,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(receive,6),"symbol":"USDC","value":money(receive,&req.amount.currency,rate)},
        "price":unit_price(receive,amount,decimals,&req.amount.currency,rate),
        "fee":money(value.saturating_sub(receive),&req.amount.currency,rate),"expiresAtUnixMs":expires}),
    ))
}

async fn ref_sell_quote(
    state: AppState,
    headers: HeaderMap,
    req: markets::QuoteRequest,
    user: app_balance::VerifiedWallets,
    coin: &str,
) -> Result<Json<Value>, ApiError> {
    if !near_account(coin) {
        return Err(bad("Invalid asset"));
    }
    let token = state
        .near
        .tokens()
        .await?
        .into_iter()
        .find(|t| t.blockchain == "near" && t.contract_address.as_deref() == Some("wrap.near"))
        .ok_or_else(|| venue("Cashout is unavailable"))?;
    let recipient = destination(&state, &headers, &token, &user).await?;
    let details = ref_assets(&state, coin)
        .await?
        .into_iter()
        .find(|a| a["token"] == coin && a["tradeable"] == true)
        .ok_or_else(|| bad("This token does not have enough liquidity to trade"))?;
    let symbol = details["symbol"]
        .as_str()
        .ok_or_else(|| venue("Token details unavailable"))?
        .to_owned();
    let name = details["name"].as_str().unwrap_or(&symbol).to_owned();
    let decimals = details["decimals"]
        .as_u64()
        .filter(|d| *d <= 24)
        .ok_or_else(|| venue("Token details unavailable"))? as u32;
    let icon_url = details["icon"]
        .as_str()
        .filter(|s| s.starts_with("https://"))
        .map(str::to_owned);
    let price = usd_micros(details["price"].as_str().unwrap_or("0"))?;
    let balance = bridge(&state, &headers, "/near/ref/balance", json!({"token":coin})).await?;
    if balance["userId"] != user.user_id || balance["address"] != recipient {
        return Err(venue("Holding wallet did not match"));
    }
    let held = balance["amount"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .ok_or_else(|| venue("Your holding is unavailable"))?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let desired = markets::parse_micros(&req.amount.amount)?
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("Amount too large"))?
        / rate;
    let amount = sell_units(held, desired, price, decimals, req.all)?;
    let value = amount
        .checked_mul(price)
        .ok_or_else(|| bad("Amount too large"))?
        / 10u128.pow(decimals);
    markets::check_limits(value, &req.amount.currency, rate)?;
    let routed = bridge(
        &state,
        &headers,
        "/near/ref/quote",
        json!({"token":coin,"amount":amount.to_string(),"sell":true}),
    )
    .await?;
    if routed["userId"].as_str() != Some(&user.user_id)
        || routed["address"].as_str() != Some(&recipient)
    {
        return Err(venue("Sale wallet did not match"));
    }
    let output = routed["amountOut"]
        .as_str()
        .and_then(|v| v.parse::<u128>().ok())
        .filter(|v| *v > NEAR_GAS_RESERVE * 2)
        .ok_or_else(|| bad("The sale is too small after network fees"))?;
    let min_sui = output
        .checked_mul(99)
        .ok_or_else(|| bad("Amount too large"))?
        / 100;
    // Price cashout conservatively, allowing gas; execution uses the actual net swap credit.
    let cashout = min_sui - NEAR_GAS_RESERVE;
    let (cash_asset, cash_wallet) = cash_target(&user)?;
    let deadline = deadline_utc(180);
    let units = cashout.to_string();
    let request = QuoteRequest::exact_input(
        &token.asset_id,
        cash_asset,
        &units,
        cash_wallet,
        &recipient,
        &deadline,
        true,
    );
    let q = state.near.client.quote(&request).await.map_err(|_| {
        bad("The cashout is below the available route minimum or unavailable. Try a larger amount.")
    })?;
    let receive: u128 = q.amount_out.parse().map_err(internal)?;
    let minimum = q
        .min_amount_out
        .as_deref()
        .unwrap_or(&q.amount_out)
        .parse()
        .map_err(internal)?;
    let quote_id = id("q");
    let expires = now() + 30_000;
    let from_solana = user.evm_wallet.is_none();
    let wallet = if from_solana {
        user.solana_wallet.clone()
    } else {
        user.evm_wallet.clone()
    }
    .ok_or_else(|| conflict("Your cash wallet is not ready"))?;
    state.near.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        StoredQuote {
            owner: user.user_id,
            wallet,
            recipient,
            asset: token,
            amount: cashout,
            minimum_out: minimum,
            currency: req.amount.currency.clone(),
            expires,
            then_swap: None,
            sell_sui: true,
            from_solana,
            network_fee: value.saturating_sub(receive),
            sale: Some(SuiSale {
                network: "near".into(),
                coin_type: coin.into(),
                symbol: symbol.clone(),
                name,
                decimals,
                icon_url,
                amount,
                minimum_sui: min_sui,
            }),
            asset_id: req.asset_id.clone(),
            monad_sale: false,
        },
    );
    Ok(Json(
        json!({"quoteId":quote_id,"assetId":req.asset_id,"side":"sell",
        "pay":{"amount":markets::format_units(amount,decimals),"symbol":symbol,"value":money(value,&req.amount.currency,rate)},
        "receive":{"amount":markets::format_units(receive,6),"symbol":"USDC","value":money(receive,&req.amount.currency,rate)},
        "price":unit_price(receive,amount,decimals,&req.amount.currency,rate),
        "fee":money(value.saturating_sub(receive),&req.amount.currency,rate),"expiresAtUnixMs":expires}),
    ))
}

// Called by the bridge, with this same user's identity. The confirmed intent is the sole source
// of the coin and bounds; consuming it in the database survives bridge restarts.
pub(super) async fn sale_permission(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut intent = state
        .near
        .get_intent(&id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Sale not found".into()))?;
    if intent.owner != user.user_id {
        return Err((StatusCode::FORBIDDEN, "Sale belongs to another user".into()));
    }
    let ref_buy = intent
        .then_swap
        .as_ref()
        .is_some_and(|s| s.network == "near")
        && !intent.sell_sui;
    let permitted_stage = if ref_buy { "execute" } else { "fund" };
    if (!intent.sell_sui && !ref_buy)
        || intent.status.stage != permitted_stage
        || intent.status.state != "pending"
        || intent.expires <= now()
        || intent.sale_permission_used
    {
        return Err(conflict("Sale permission expired or used"));
    }
    let sale = if ref_buy {
        let swap = intent
            .then_swap
            .as_ref()
            .ok_or_else(|| bad("Swap unavailable"))?;
        SuiSale {
            network: "near".into(),
            coin_type: swap.coin_type.clone(),
            symbol: swap.symbol.clone(),
            name: swap.name.clone(),
            decimals: swap.decimals,
            icon_url: swap.icon_url.clone(),
            amount: swap.sui_in,
            minimum_sui: swap.expected_out * 99 / 100,
        }
    } else {
        intent.sale.clone().ok_or_else(|| bad("Not a coin sale"))?
    };
    intent.sale_permission_used = true;
    if let Some(pg) = &state.near.postgres {
        let payload = serde_json::to_string(&intent).map_err(internal)?;
        let changed=pg.execute("UPDATE atlas_near_intents SET payload=$2 WHERE intent_id=$1 AND owner=$3 AND stage=$4 AND NOT COALESCE((payload::jsonb->>'sale_permission_used')::boolean,false)",
            &[&id,&payload,&user.user_id,&permitted_stage]).await.map_err(internal)?;
        if changed != 1 {
            return Err(conflict("Sale permission already used"));
        }
    } else {
        let mut intents = state.near.intents.lock().map_err(internal)?;
        let current = intents
            .get_mut(&id)
            .ok_or_else(|| conflict("Sale not found"))?;
        if current.sale_permission_used || current.status.stage != permitted_stage {
            return Err(conflict("Sale permission already used"));
        }
        current.sale_permission_used = true;
    }
    Ok(Json(
        json!({"userId":user.user_id,"address":if ref_buy {intent.ref_wallet} else {intent.sui_wallet},"quoteId":id,
        "network":sale.network,"side":if ref_buy {"buy"} else {"sell"},"coinType":sale.coin_type,"amount":sale.amount.to_string(),"minimumOut":sale.minimum_sui.to_string(),
        "expiresAtUnixMs":intent.expires}),
    ))
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
                && (req.asset_id == format!("near:{}", t.asset_id)
                    || req.asset_id == "near:sui:0x2::sui::SUI")
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
    let mut amount = value_usdc
        .checked_mul(1_000_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / price_micros;
    if amount == 0 || amount > u64::MAX as u128 {
        return Err(bad("amount is outside the supported range"));
    }
    let balance = bridge(
        &state,
        &headers,
        "/sui/balance",
        json!({"coinType":"0x2::sui::SUI"}),
    )
    .await?;
    if balance["address"].as_str() != Some(&sui_wallet) {
        return Err(venue("Balance wallet did not match"));
    }
    let held = balance["result"]["totalBalance"]
        .as_str()
        .and_then(|raw| raw.parse::<u128>().ok())
        .ok_or_else(|| venue("Your balance is temporarily unavailable"))?;
    if req.all {
        amount = held.saturating_sub(SUI_GAS_RESERVE);
    }
    if amount == 0 || held < amount.saturating_add(SUI_GAS_RESERVE) {
        return Err(markets::short_of_cash());
    }
    let deadline = deadline_utc(180);
    let units = amount.to_string();
    let request = QuoteRequest::exact_input(
        &token.asset_id,
        cash_target(&user)?.0,
        &units,
        cash_target(&user)?.1,
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
            sale: None,
            from_solana: false,
            network_fee: 0,
            asset_id: req.asset_id.clone(),
            monad_sale: false,
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
    if let Some(token) = req.asset_id.strip_prefix("near:ref:").map(str::to_owned) {
        if req.side == "buy" {
            return ref_buy_quote(state, headers, req, user, &token).await;
        }
        return ref_sell_quote(state, headers, req, user, &token).await;
    }
    if req.side == "sell" {
        if let Some(coin) = req.asset_id.strip_prefix("near:sui:") {
            if coin != "0x2::sui::SUI" {
                let coin = coin.to_owned();
                return sui_coin_sell_quote(state, headers, req, user, &coin).await;
            }
            return sui_sell_quote(state, headers, req, user).await;
        }
        let token = state
            .near
            .tokens()
            .await?
            .into_iter()
            .find(|t| format!("near:{}", t.asset_id) == req.asset_id && supported(t))
            .ok_or((StatusCode::NOT_FOUND, "1Click asset not found".into()))?;
        return match (token.blockchain.as_str(), sell_route(&token.blockchain)) {
            ("near", Some(_)) => near_coin_sell_quote(state, headers, req, user, token).await,
            ("monad", Some(_)) => monad_sell_quote(state, req, user, token).await,
            ("sui", Some(_)) => sui_sell_quote(state, headers, req, user).await,
            _ => Err(bad("Atlas can't sell this coin")),
        };
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
    // Selling a NEAR coin signs as the user from the server: no buying one until that works.
    if !SERVER_SIGNS_AS_USER && token.blockchain == "near" {
        return Err(conflict(PAUSED_FOR_SIGNING));
    }
    let destination = destination(&state, &headers, &token, &user).await?;
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let micros = markets::parse_micros(&req.amount.amount)?;
    let amount = micros
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?
        / rate;
    markets::check_limits(amount, &req.amount.currency, rate)?;
    // A NEAR coin's sale is paid for in NEAR. With none in the wallet, the coin is bought through
    // NEAR and Ref, which keeps 0.05 NEAR back for those fees.
    if let Some(contract) = token
        .contract_address
        .clone()
        .filter(|c| token.blockchain == "near" && c != WRAP_NEAR && near_account(c))
    {
        if near_native(&state, &destination).await? < NEAR_GAS_RESERVE {
            let symbol = token.symbol.clone();
            return ref_buy_quote(state, headers, req, user, &contract)
                .await
                .map_err(|_| {
                    conflict(&format!(
                        "Buying {symbol} needs a little NEAR in your NEAR wallet for the fees of selling it later. Buy a little NEAR first."
                    ))
                });
        }
    }
    // Atlas won't buy a coin it can't sell back to cash.
    if !sellable(&state, &headers, &user, &token, amount).await {
        return Err(conflict(&format!(
            "Atlas can't sell {} back to cash right now, so it won't buy it. Try again later.",
            token.symbol
        )));
    }
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
            sale: None,
            from_solana,
            network_fee,
            asset_id: req.asset_id.clone(),
            monad_sale: false,
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
// A Monad sale's plan: one transfer to 1Click's deposit address, for the phone to send (MON itself,
// or an ERC-20 `transfer`). The wallet's nonce is noted: once it moves on, the plan may have gone out.
async fn execute_monad_sale(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    stored: StoredQuote,
) -> Result<Json<Value>, ApiError> {
    let token = stored.asset.clone();
    let held = state.near.monad_balance(&token, &stored.wallet).await?;
    let gas = if token.contract_address.is_none() {
        MONAD_GAS_RESERVE
    } else {
        0
    };
    if held < stored.amount.saturating_add(gas) {
        return Err(conflict(&format!(
            "You no longer hold that much {}.",
            token.symbol
        )));
    }
    let q = cash_quote(state, user, &token, stored.amount, &stored.wallet, false).await?;
    if min_out(&q)? < stored.minimum_out.saturating_mul(99) / 100 {
        return Err(conflict("route price changed; request a fresh quote"));
    }
    let deposit = q
        .deposit_address
        .clone()
        .filter(|d| is_evm(d) && q.deposit_memo.is_none())
        .ok_or_else(|| venue("1Click returned an unsupported deposit destination"))?;
    let (to, data, value) = monad_transfer(&token, &deposit, stored.amount)?;
    let nonce = state.near.monad_nonce(&stored.wallet).await?;
    let intent_id = id("intent");
    let expires = now() + 120_000;
    state
        .near
        .insert_intent(
            &intent_id,
            StoredIntent {
                owner: user.user_id.clone(),
                wallet: stored.wallet.clone(),
                expected_to: to.clone(),
                expected_data: data.clone(),
                deposit_address: deposit,
                deposit_memo: None,
                asset: Some(token.clone()),
                expires,
                status: markets::IntentStatus {
                    intent_id: intent_id.clone(),
                    stage: "validate".into(),
                    state: "pending".into(),
                    tx_ids: Vec::new(),
                    error: None,
                },
                then_swap: None,
                sell_sui: false,
                amount: stored.amount,
                minimum_out: stored.minimum_out,
                sui_wallet: None,
                from_solana: false,
                gas_request_id: None,
                gas_topup: false,
                gas_order: None,
                sale: None,
                sale_permission_used: false,
                ref_wallet: None,
                asset_id: stored.asset_id.clone(),
                origin_chain: "monad".into(),
                expected_value: format!("0x{value:x}"),
                handed_nonce: Some(nonce),
            },
        )
        .await?;
    let rate = app_balance::fx_rate(&stored.currency).await?;
    let mut summary = vec![
        json!({"label":"You sell","value":format!("{} {}",markets::format_units(stored.amount,token.decimals),token.symbol)}),
        json!({"label":"You receive (at least)","value":markets::say_money(stored.minimum_out,&stored.currency,rate)}),
    ];
    if stored.network_fee > 0 {
        summary.push(json!({"label":"Fees","value":markets::say_money(stored.network_fee,&stored.currency,rate)}));
    }
    Ok(Json(
        json!({"intentId":intent_id,"kind":"sell","summary":summary,
        "transactions":[{"chain":"monad","chainId":MONAD_CHAIN_ID,"to":to,"data":data,"value":value.to_string()}],
        "expiresAtUnixMs":expires}),
    ))
}
const MONAD_CHAIN_ID: u64 = 143;
// The transfer that pays 1Click on Monad: (to, calldata, wei). MON goes as value; an ERC-20 goes as
// a `transfer(deposit, amount)` call to its contract.
fn monad_transfer(
    token: &Token,
    deposit: &str,
    amount: u128,
) -> Result<(String, String, u128), ApiError> {
    if !is_evm(deposit) || amount == 0 {
        return Err(bad("invalid transfer"));
    }
    let deposit = deposit.to_ascii_lowercase();
    match token.contract_address.as_deref() {
        None => Ok((deposit, "0x".into(), amount)),
        Some(contract) if is_evm(contract) => Ok((
            contract.to_ascii_lowercase(),
            format!("0xa9059cbb{:0>64}{:064x}", &deposit[2..], amount),
            0,
        )),
        Some(_) => Err(venue("invalid Monad token contract")),
    }
}
// A filled trade goes into the trade book, so positions and P&L follow it. Keeping it never fails
// the trade the user already made.
async fn record_fill(
    state: &AppState,
    intent_id: &str,
    intent: &StoredIntent,
    side: &str,
    token_units: u128,
    usdc_units: u128,
    tx_id: Option<String>,
) {
    if intent.asset_id.is_empty() || token_units == 0 {
        return;
    }
    let trade = positions::Trade {
        intent_id: intent_id.into(),
        user_id: intent.owner.clone(),
        asset_id: intent.asset_id.clone(),
        side: side.into(),
        token_units,
        usdc_units,
        tx_id,
        filled_at_ms: now(),
    };
    if let Err(error) = state.trades.record(&trade).await {
        eprintln!("trade {intent_id} not kept: {}", error.1);
    }
}
async fn execute_sui_sell(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    quote: StoredQuote,
) -> Result<Json<Value>, ApiError> {
    let intent_id = id("intent");
    let expires = now() + 120_000;
    let rate = app_balance::fx_rate(&quote.currency).await?;
    let sell_label = quote
        .sale
        .as_ref()
        .map(|sale| {
            format!(
                "{} {}",
                markets::format_units(sale.amount, sale.decimals),
                sale.symbol
            )
        })
        .unwrap_or_else(|| format!("{} SUI", markets::format_units(quote.amount, 9)));
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
                sale: quote.sale,
                amount: quote.amount,
                minimum_out: quote.minimum_out,
                sui_wallet: Some(quote.recipient),
                from_solana: false,
                gas_request_id: None,
                gas_topup: false,
                gas_order: None,
                sale_permission_used: false,
                ref_wallet: None,
                asset_id: quote.asset_id.clone(),
                origin_chain: String::new(),
                expected_value: String::new(),
                handed_nonce: None,
            },
        )
        .await?;
    Ok(Json(json!({"intentId":intent_id,"kind":"sell",
        "summary":[
            {"label":"You sell","value":sell_label},
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
    let recovery = state
        .near
        .recoveries
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned();
    if let Some(r) = recovery {
        if r.intent.owner != user.user_id {
            return Err((
                StatusCode::FORBIDDEN,
                "quote belongs to another user".into(),
            ));
        }
        if r.expires <= now() {
            return Err(conflict("quote expired; request a fresh quote"));
        }
        let intent_id = r.intent.status.intent_id.clone();
        let swap = r
            .intent
            .then_swap
            .as_ref()
            .ok_or_else(|| conflict("purchase cannot be resumed"))?;
        // Build the swap now; the user's device approves this exact Privy request in the confirm.
        let prepared = bridge(
            &state,
            &headers,
            "/sui/swap/prepare",
            json!({"coinType":swap.coin_type,"amount":r.input.to_string(),
                "reserve":SUI_GAS_RESERVE.to_string(),"minimumOut":(r.expected * 97 / 100).to_string(),
                "expiresAtUnixMs":r.expires,
                "expectedWallet":r.intent.ref_wallet.as_ref().or(r.intent.sui_wallet.as_ref())}),
        )
        .await?;
        let (Some(prepare_id), true) = (
            prepared["prepareId"].as_str().map(str::to_owned),
            prepared["request"].is_object(),
        ) else {
            return Err(venue("Couldn't prepare the swap; nothing was spent"));
        };
        let expires = now() + 150_000;
        let plan = json!({"intentId":intent_id,"kind":"buy",
            "transactions":[{"chain":"privy","request":prepared["request"]}],
            "summary":[{"label":"Already paid","value":markets::say_money(r.intent.amount,&r.currency,r.rate)},
                {"label":"New cash payment","value":markets::say_money(0,&r.currency,r.rate)},
                {"label":"You get about","value":format!("{} {}",markets::format_units(r.expected,swap.decimals),swap.symbol)}],
            "expiresAtUnixMs":expires});
        let r = SuiRecovery {
            expires,
            prepare_id: Some(prepare_id),
            ..r
        };
        // A newer confirm replaces an older one: only the latest prepared swap can be committed.
        let mut recoveries = state.near.recoveries.lock().map_err(internal)?;
        recoveries.remove(&quote_id);
        recoveries.insert(intent_id, r);
        return Ok(Json(plan));
    }
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
    if stored.monad_sale {
        return execute_monad_sale(&state, &user, stored).await;
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
                sale: None,
                amount: stored.amount,
                minimum_out: stored.minimum_out,
                sui_wallet: None,
                from_solana: stored.from_solana,
                gas_request_id,
                gas_topup,
                gas_order: None,
                sale_permission_used: false,
                ref_wallet: Some(stored.recipient.clone()),
                asset_id: stored.asset_id.clone(),
                origin_chain: String::new(),
                expected_value: String::new(),
                handed_nonce: None,
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
    current.status.stage = if current.sale.is_some() {
        "fund"
    } else {
        "execute"
    }
    .into();
    let response = current.status.clone();
    let (state, headers, id, user) = (
        state.clone(),
        headers.clone(),
        id.to_owned(),
        user.user_id.clone(),
    );
    tokio::spawn(async move {
        if let Err((_, reason)) = run_sui_sell(&state, &headers, &id, &user, current).await {
            eprintln!("sale worker {id}: {reason}");
            if let Ok(Some(mut latest)) = state.near.get_intent(&id).await {
                if latest.status.state == "pending" && latest.deposit_address.is_empty() {
                    latest.status.state = "failed".into();
                    latest.status.error = Some(
                        "The sale could not be verified. Check your wallet before trying again."
                            .into(),
                    );
                    let _ = state.near.save_intent(&id, latest).await;
                }
            }
        }
    });
    Ok(Json(response))
}
async fn run_sui_sell(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    user: &str,
    mut current: StoredIntent,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let is_near = current
        .sale
        .as_ref()
        .is_some_and(|sale| matches!(sale.network.as_str(), "near" | "nearintents"));
    // A NEAR coin sold straight to 1Click: no swap first, the coin itself is the deposit.
    let direct = current
        .sale
        .as_ref()
        .filter(|sale| sale.network == "nearintents")
        .map(|sale| sale.symbol.clone());
    if let Some(sale) = current.sale.clone().filter(|_| direct.is_none()) {
        let prepared = bridge(
            state,
            headers,
            if is_near {
                "/near/ref/prepare"
            } else {
                "/sui/sale/prepare"
            },
            json!({"intentId":id}),
        )
        .await;
        current.sale_permission_used = true;
        let prepared = match prepared {
            Ok(p) => p,
            Err(_) => {
                current.status.state = "failed".into();
                current.status.error = Some(format!(
                    "The sale could not start. Your {} is still in your wallet.",
                    sale.symbol
                ));
                state.near.save_intent(id, current.clone()).await?;
                return Ok(Json(current.status));
            }
        };
        let sent = bridge(
            state,
            headers,
            if is_near {
                "/near/ref/commit"
            } else {
                "/sui/sale/commit"
            },
            json!({"saleId":prepared["saleId"],"scope":prepared["scope"]}),
        )
        .await;
        match sent {
            Ok(body)
                if body["ok"].as_bool() == Some(true)
                    && body["userId"].as_str() == Some(user)
                    && body["address"].as_str() == current.sui_wallet.as_deref() =>
            {
                if let Some(hash) = body["digest"].as_str() {
                    current.status.tx_ids.push(hash.into());
                }
                current.amount = body["amountOut"]
                    .as_str()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if current.amount == 0 {
                    current.status.state = "failed".into();
                    current.status.error = Some(
                        "The sale settled but its proceeds could not be read. Check your wallet."
                            .into(),
                    );
                    state.near.save_intent(id, current.clone()).await?;
                    return Ok(Json(current.status));
                }
                state.near.save_intent(id, current.clone()).await?;
            }
            result => {
                current.status.state = "failed".into();
                current.status.error = Some(if result.is_ok() {
                    format!(
                        "The sale did not settle. Your {} is still in your wallet.",
                        sale.symbol
                    )
                } else {
                    "The sale outcome could not be verified. Check your wallet before trying again."
                        .into()
                });
                state.near.save_intent(id, current.clone()).await?;
                return Ok(Json(current.status));
            }
        }
    }
    // A direct sale's bounds come only from this intent (the bridge asks for its permission).
    let prepared = if direct.is_some() {
        bridge(state, headers, "/near/sell/prepare", json!({"intentId":id})).await
    } else {
        bridge(
            state,
            headers,
            if is_near {
                "/near/cashout/prepare"
            } else {
                "/sui/cashout/prepare"
            },
            json!({"amount":current.amount.to_string(),
                "minimumOut":current.minimum_out.to_string()}),
        )
        .await
    };
    current.sale_permission_used |= direct.is_some();
    let prepared = match prepared {
        Ok(value) => value,
        Err(_) => {
            current.status.state = "failed".into();
            current.status.error = Some(if let Some(symbol) = &direct {
                format!("The sale didn't start. Your {symbol} is still in your NEAR wallet.")
            } else if current.sale.is_some() {
                "Your sale settled, but cashout is unavailable. The proceeds are still in your wallet.".into()
            } else {
                "Cashout route is unavailable; nothing was sent".into()
            });
            state.near.save_intent(id, current.clone()).await?;
            return Ok(Json(current.status));
        }
    };
    let expected_wallet = current.sui_wallet.as_deref().unwrap_or("");
    let address = prepared["depositAddress"].as_str().unwrap_or("");
    let cashout_id = prepared["cashoutId"].as_str().unwrap_or("");
    if prepared["userId"].as_str() != Some(user)
        || prepared["address"].as_str() != Some(expected_wallet)
        || (if is_near {
            address.len() < 2
                || address.len() > 128
                || !address
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        } else {
            address.len() != 66
                || !address.starts_with("0x")
                || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
        })
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
        if direct.is_some() {
            "/near/sell/commit"
        } else if is_near {
            "/near/cashout/commit"
        } else {
            "/sui/cashout/commit"
        },
        json!({"cashoutId":cashout_id,"scope":prepared["scope"]}),
    )
    .await;
    current.status.stage = "settle".into();
    match result {
        Ok(sent)
            if sent["ok"].as_bool() == Some(true)
                && sent["userId"].as_str() == Some(user)
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
    let recovery = state
        .near
        .recoveries
        .lock()
        .map_err(internal)?
        .get(&id)
        .cloned();
    if let Some(recovery) = recovery {
        // No new payment: only the device's approval of the prepared swap comes back.
        let [approval] = body.signed.as_slice() else {
            return Err(bad(
                "finishing this buy needs one approval and no new payment",
            ));
        };
        if !body.sent.is_empty() || approval.index != 0 {
            return Err(bad(
                "finishing this buy needs one approval and no new payment",
            ));
        }
        let approval = approval.transaction.clone();
        return resume_sui_buy(&state, &headers, &id, current, recovery, approval)
            .await
            .map(Json);
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
    // A Monad sale: the phone sent the coin to 1Click; status follows it on Monad, then 1Click.
    if current.origin_chain == "monad" {
        if !body.signed.is_empty()
            || body.sent.len() != 1
            || body.sent[0].chain != "monad"
            || !valid_hash(&body.sent[0].id)
        {
            return Err(bad("signed report does not match the sale plan"));
        }
        current.status.stage = "settle".into();
        current.status.tx_ids = vec![body.sent[0].id.clone()];
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
            if now() > current.expires && result.stage == "validate" {
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
                let cash = delivered(&venue_status).unwrap_or(current.minimum_out);
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
                let sold = current.sale.as_ref().map_or(current.amount, |s| s.amount);
                let tx = result.tx_ids.first().cloned();
                record_fill(&state, &id, &current, "sell", sold, cash, tx).await;
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
    // A Monad sale nobody reported: past its window, and the wallet's nonce still where it was when
    // the plan was handed out, nothing went out. If the nonce moved, it may have: follow 1Click.
    if current.origin_chain == "monad"
        && result.stage == "validate"
        && result.state == "pending"
        && now() > current.expires
    {
        let moved = match current.handed_nonce {
            Some(handed) => state.near.monad_nonce(&current.wallet).await? > handed,
            None => true,
        };
        if !moved {
            result.state = "failed".into();
            result.error = Some(format!(
                "Nothing was sent, so your {} is still in your wallet.",
                current.asset.as_ref().map_or("coin", |a| a.symbol.as_str())
            ));
            current.status = result.clone();
            state.near.save_intent(&id, current).await?;
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
                result.stage = "settle".into();
                result.state = "filled".into();
                let cash = delivered(&venue_status).unwrap_or(current.minimum_out);
                record_fill(&state, &id, &current, "sell", current.amount, cash, None).await;
            }
            "FAILED" | "REFUNDED" => {
                result.state = "failed".into();
                result.error = Some(format!("1Click {}", venue_status.status));
            }
            _ => {
                result.error = Some(
                    "It may have gone through. Checking with NEAR Intents before saying more."
                        .into(),
                );
                return Ok(Json(result));
            }
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
                    let got = delivered(&venue_status).unwrap_or(current.minimum_out);
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
                        if swap.network == "near" {
                            let claimed = if let Some(pg) = &state.near.postgres {
                                {
                                    let mut claimed = current.clone();
                                    claimed.status.stage = "execute".into();
                                    claimed.expires = now() + 180_000;
                                    let payload =
                                        serde_json::to_string(&claimed).map_err(internal)?;
                                    pg.execute("UPDATE atlas_near_intents SET stage='execute',payload=$3 WHERE intent_id=$1 AND owner=$2 AND stage='settle'", &[&id,&current.owner,&payload]).await.map_err(internal)?==1
                                }
                            } else {
                                let mut intents = state.near.intents.lock().map_err(internal)?;
                                if let Some(i) = intents.get_mut(&id) {
                                    if i.status.stage == "settle" {
                                        i.status.stage = "execute".into();
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    false
                                }
                            };
                            if !claimed {
                                return Ok(Json(
                                    state
                                        .near
                                        .get_intent(&id)
                                        .await?
                                        .map_or(result, |i| i.status),
                                ));
                            }
                        }
                        result.stage = "execute".into();
                        current.expires = now() + 180_000;
                        current.status = result.clone();
                        state.near.save_intent(&id, current.clone()).await?;
                        let swapped = if swap.network == "near" {
                            let prepared = bridge(
                                &state,
                                &headers,
                                "/near/ref/prepare",
                                json!({"intentId":id}),
                            )
                            .await;
                            current.sale_permission_used = true;
                            match prepared {
                                Ok(p) => {
                                    bridge(
                                        &state,
                                        &headers,
                                        "/near/ref/commit",
                                        json!({"saleId":p["saleId"],"scope":p["scope"]}),
                                    )
                                    .await
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            bridge(&state,&headers,"/sui/swap",json!({"coinType":swap.coin_type,"amount":swap.sui_in.to_string(),"reserve":SUI_GAS_RESERVE.to_string()})).await
                        };
                        match swapped {
                            Ok(body) if body["ok"].as_bool() == Some(true) => {
                                if let Some(digest) = body["digest"].as_str() {
                                    result.tx_ids.push(digest.into());
                                }
                                result.stage = "settle".into();
                                result.state = "filled".into();
                                let bought = body["amountOut"]
                                    .as_str()
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(swap.expected_out);
                                let tx = result.tx_ids.last().cloned();
                                record_fill(
                                    &state,
                                    &id,
                                    &current,
                                    "buy",
                                    bought,
                                    current.amount,
                                    tx,
                                )
                                .await;
                            }
                            Ok(body) => {
                                result.state = "failed".into();
                                result.error = Some(format!(
                                    "Your cash arrived, but the swap to {} didn't go through ({}). The proceeds are in your wallet.",
                                    swap.symbol,
                                    body["error"].as_str().unwrap_or("swap failed")
                                ));
                            }
                            Err((_, reason)) => {
                                result.state = "failed".into();
                                result.error = Some(format!(
                                    "The swap to {} could not be verified ({reason}). Check your token and cash balances before trying again.",
                                    swap.symbol
                                ));
                            }
                        }
                    } else {
                        result.state = "filled".into();
                        let tx = result.tx_ids.first().cloned();
                        if current.origin_chain == "monad" {
                            record_fill(&state, &id, &current, "sell", current.amount, got, tx)
                                .await;
                        } else {
                            record_fill(&state, &id, &current, "buy", got, current.amount, tx)
                                .await;
                        }
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

// What 1Click says reached the recipient, once settled.
fn delivered(status: &engine_execution::near_intents::Status) -> Option<u128> {
    status
        .swap_details
        .as_ref()?
        .amount_out
        .as_deref()?
        .parse()
        .ok()
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
    if current.origin_chain == "monad" {
        return monad_deposit_landed(state, current, hash).await;
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

// A Monad sale's transfer: exactly the planned one (from the wallet, to, data and value, on chain
// 143), and it succeeded.
async fn monad_deposit_landed(
    state: &AppState,
    current: &StoredIntent,
    hash: &str,
) -> Result<Result<bool, String>, ApiError> {
    let tx = state
        .near
        .monad_call("eth_getTransactionByHash", json!([hash]))
        .await?;
    if tx.is_null() {
        return Ok(Ok(false));
    }
    if !monad_tx_matches(&tx, current) {
        return Ok(Err(
            "reported transaction does not match the sale plan".into()
        ));
    }
    let receipt = state
        .near
        .monad_call("eth_getTransactionReceipt", json!([hash]))
        .await?;
    if receipt.is_null() {
        return Ok(Ok(false));
    }
    if receipt["status"].as_str() != Some("0x1") {
        return Ok(Err(format!(
            "The transfer didn't go through, so your {} is still in your wallet.",
            current.asset.as_ref().map_or("coin", |a| a.symbol.as_str())
        )));
    }
    Ok(Ok(true))
}
fn monad_tx_matches(tx: &Value, current: &StoredIntent) -> bool {
    let field = |name: &str, want: &str| {
        tx[name]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(want))
    };
    let value = hex_units(&tx["value"]);
    field("from", &current.wallet)
        && field("to", &current.expected_to)
        && (field("input", &current.expected_data)
            || (current.expected_data == "0x" && tx["input"].as_str() == Some("0x")))
        && value.is_some()
        && value == hex_units(&json!(current.expected_value))
        && tx["chainId"].as_str() == Some("0x8f")
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

    #[test]
    fn only_a_failed_sui_buy_can_resume_whatever_its_error() {
        let mut intent: StoredIntent = serde_json::from_value(json!({
            "owner":"o","wallet":"w","expected_to":"t","expected_data":"0x",
            "deposit_address":"d","deposit_memo":null,"expires":1,
            "then_swap":{"network":"sui","coin_type":"0x2::coin::COIN","symbol":"COIN","name":"Coin",
                "decimals":9,"icon_url":null,"sui_in":100,"expected_out":200},
            "status":{"intentId":"near-intent-test","stage":"execute","state":"failed","txIds":["deposit"],
                "error":"Sui RPC suix_getBalance: Method not found"}
        })).unwrap();
        assert!(recoverable_sui(&intent));
        assert_eq!(
            short_reason("Sui swap unavailable: 400 Invalid JWT token provided; nothing was sent"),
            "400 Invalid JWT token provided; nothing was sent"
        );
        intent.status.state = "pending".into();
        assert!(!recoverable_sui(&intent));
        intent.status.state = "filled".into();
        assert!(!recoverable_sui(&intent));
        // The wording no longer decides: the wallet's SUI is read on-chain before any resume.
        intent.status.state = "failed".into();
        intent.status.error = Some("submit timed out".into());
        assert!(recoverable_sui(&intent));
        intent.then_swap.as_mut().unwrap().network = "near".into();
        assert!(!recoverable_sui(&intent));
        intent.then_swap.as_mut().unwrap().network = "sui".into();
        intent.sell_sui = true;
        assert!(!recoverable_sui(&intent));
    }
    #[test]
    fn grpc_coin_addresses_match_short_catalog_addresses() {
        assert_eq!(
            super::sui_coin_key("0x2::sui::SUI"),
            super::sui_coin_key(&format!("0x{:0>64}::sui::SUI", "2"))
        );
        assert_ne!(
            super::sui_coin_key("0x2::sui::SUI"),
            super::sui_coin_key("0x3::sui::SUI")
        );
    }

    use super::*;
    #[test]
    #[ignore = "live dry Ref quotes; no signing"]
    fn live_ref_quotes() {
        let output = std::process::Command::new("node")
            .args(["privy-bridge/live-assets.mjs", "near"])
            .output()
            .expect("node must be available");
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn token(
        chain: &str,
        symbol: &str,
        contract: Option<&str>,
        decimals: u32,
        price: f64,
    ) -> Token {
        Token {
            asset_id: format!("nep141:{symbol}"),
            blockchain: chain.into(),
            symbol: symbol.into(),
            decimals,
            contract_address: contract.map(str::to_owned),
            price: Some(json!(price)),
            coingecko_id: None,
        }
    }
    #[test]
    fn nothing_is_bought_on_a_chain_atlas_cant_sell_from() {
        for chain in ["near", "monad", "sui"] {
            assert!(supported(&token(chain, "COIN", None, 18, 1.0)), "{chain}");
            assert!(sell_route(chain).is_some());
        }
        for chain in ["eth", "base", "sol", "arb", "btc", "aptos"] {
            assert!(!supported(&token(chain, "COIN", None, 18, 1.0)), "{chain}");
        }
        // Cash itself isn't traded.
        assert!(!supported(&token(
            "monad",
            "USDC",
            Some("0x754704bc059f8c67012fed69bc8a327a5aafb603"),
            6,
            1.0
        )));
    }
    #[test]
    fn memecoins_far_below_a_microdollar_still_price() {
        // BLACKDRAGON: about $0.00000002 a coin, 24 decimals.
        let price = 2.0065261919009766e-8;
        let units = units_worth(1_500_000, price, 24);
        assert!((units as f64 / 1e24 - 74_756_000.0).abs() < 100_000.0);
        let back = worth_of(units, price, 24);
        assert!((back as i128 - 1_500_000).abs() < 10);
        assert_eq!(
            sale_units(units, 1_500_000, price, 24, false).unwrap(),
            units
        );
        assert!(sale_units(units / 2, 1_500_000, price, 24, false).is_err());
        assert_eq!(sale_units(123, 0, 0.0, 24, true).unwrap(), 123);
        assert!(sale_units(0, 0, price, 24, true).is_err());
        assert_eq!(token_usd(&token("near", "X", None, 24, price)), price);
        let mut quoted = token("near", "X", None, 24, 0.0);
        quoted.price = Some(json!("0.25"));
        assert_eq!(token_usd(&quoted), 0.25);
    }
    #[test]
    fn a_monad_sale_is_one_plain_transfer_to_the_deposit() {
        let deposit = "0x1111111111111111111111111111111111111111";
        let mon = token("monad", "MON", None, 18, 0.03);
        assert_eq!(
            monad_transfer(&mon, deposit, 5).unwrap(),
            (deposit.into(), "0x".into(), 5)
        );
        let usd = token(
            "monad",
            "COIN",
            Some("0x754704BC059F8C67012FED69BC8A327A5AAFB603"),
            6,
            1.0,
        );
        let (to, data, value) = monad_transfer(&usd, deposit, 255).unwrap();
        assert_eq!(to, "0x754704bc059f8c67012fed69bc8a327a5aafb603");
        assert_eq!(value, 0);
        assert_eq!(
            data,
            format!(
                "0xa9059cbb{:0>64}{:064x}",
                "1111111111111111111111111111111111111111", 255
            )
        );
        assert!(monad_transfer(&mon, "not-an-address", 5).is_err());
        assert!(monad_transfer(&mon, deposit, 0).is_err());
    }
    #[test]
    fn a_reported_monad_transfer_must_be_exactly_the_plan() {
        let mut intent: StoredIntent = serde_json::from_value(json!({
            "owner":"o","wallet":"0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97","expected_to":"0x1111111111111111111111111111111111111111",
            "expected_data":"0x","deposit_address":"0x1111111111111111111111111111111111111111","deposit_memo":null,
            "expires":0,"status":{"intentId":"i","stage":"settle","state":"pending","txIds":[],"error":null},
            "origin_chain":"monad","expected_value":"0x2386f26fc10000"}))
        .unwrap();
        let tx = json!({"from":"0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97","to":"0x1111111111111111111111111111111111111111",
            "input":"0x","value":"0x2386f26fc10000","chainId":"0x8f"});
        assert!(monad_tx_matches(&tx, &intent));
        for (field, value) in [
            ("to", "0x2222222222222222222222222222222222222222"),
            ("value", "0x2386f26fc10001"),
            ("chainId", "0x2105"),
            ("from", "0x0000000000000000000000000000000000000001"),
            ("input", "0xa9059cbb"),
        ] {
            let mut changed = tx.clone();
            changed[field] = json!(value);
            assert!(!monad_tx_matches(&changed, &intent), "{field}");
        }
        intent.expected_value = "0x0".into();
        assert!(!monad_tx_matches(&tx, &intent));
    }
    #[test]
    fn a_captured_near_coin_sale_quote_reads_its_minimum_and_deposit() {
        // From 1Click on 2026-10-01: 2,000 SHITZU to Solana USDC (ORIGIN_CHAIN, not funded).
        let body = json!({"quote":{"amountIn":"2000000000000000000000","amountOut":"7792945",
            "amountOutUsd":"7.79","minAmountOut":"7715015",
            "depositAddress":"7478b3f96ffcaffb848a85a269195600a6201f30658fbf671029a5480da89c7d",
            "depositMemo":null,"timeEstimate":20,"deadline":"2026-10-04T17:06:27.000Z"}});
        let quote: engine_execution::near_intents::QuoteResponse =
            serde_json::from_value(body).unwrap();
        assert_eq!(min_out(&quote.quote).unwrap(), 7_715_015);
        assert_eq!(
            quote.quote.deposit_address.as_deref().map(str::len),
            Some(64)
        );
        // What settled: 1Click's own amount, for the trade book.
        let status: engine_execution::near_intents::Status = serde_json::from_value(json!({
            "status":"SUCCESS","swapDetails":{"amountOut":"7790001","destinationChainTxHashes":[]}}))
        .unwrap();
        assert_eq!(delivered(&status), Some(7_790_001));
    }
    // Network: dry quotes only, nothing is sent.
    // cargo test -p engine-service live_near_and_monad_sales -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_near_and_monad_sales() {
        let client = Client::new(std::env::var("NEAR_INTENTS_API_KEY").ok()).unwrap();
        let deadline = deadline_utc(180);
        let solana = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
        for (origin, amount, refund, label) in [
            (
                "nep141:token.0xshitzu.near",
                "2000000000000000000000",
                "atlas.near",
                "2,000 SHITZU",
            ),
            (
                "nep141:blackdragon.tkn.near",
                "100000000000000000000000000000000",
                "atlas.near",
                "100M BLACKDRAGON",
            ),
            (
                "nep141:wrap.near",
                "1000000000000000000000000",
                "atlas.near",
                "1 NEAR",
            ),
            (
                "nep245:v2_1.omni.hot.tg:143_11111111111111111111",
                "100000000000000000000",
                "0x4838B106FCe9647Bdf1E7877BF73cE8B0BAD5f97",
                "100 MON",
            ),
        ] {
            let request = QuoteRequest::exact_input(
                origin,
                SOLANA_USDC_1CLICK,
                amount,
                solana,
                refund,
                &deadline,
                true,
            );
            match client.quote(&request).await {
                Ok(q) => println!(
                    "LIVE DRY: {label} -> {} USDC units on Solana (at least {})",
                    q.amount_out,
                    q.min_amount_out.unwrap_or_default()
                ),
                Err(error) => println!("LIVE DRY: {label} -> not quoted ({error})"),
            }
        }
    }
    #[test]
    fn max_sale_uses_the_exact_onchain_holding() {
        assert_eq!(sell_units(123456789, 1, 0, 9, true).unwrap(), 123456789);
        assert_eq!(sell_units(1000, 50, 100, 2, false).unwrap(), 50);
        assert!(sell_units(10, 500, 100, 2, false).is_err());
    }
    #[test]
    #[ignore = "live dry Cetus and 1Click quotes; no signing"]
    fn live_sui_sale_quotes() {
        let result = std::process::Command::new("node")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/privy-bridge/live-assets.mjs"
            ))
            .arg("sui")
            .output()
            .expect("node runtime");
        println!("{}", String::from_utf8_lossy(&result.stdout));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
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
