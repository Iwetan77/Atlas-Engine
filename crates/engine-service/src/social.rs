//! Privy-backed handles and direct Base USDC sends. Durable handle storage
//! uses Postgres or ATLAS_SOCIAL_STATE_PATH on a persistent volume.
use super::*;
use axum::extract::Query;
use engine_execution::swaps::uniswap::BASE_USDC;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
static NEXT_SEND_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(super) struct SocialState {
    path: Option<PathBuf>,
    postgres: Option<Arc<tokio_postgres::Client>>,
    ledger: Arc<Mutex<Ledger>>,
    quotes: Arc<Mutex<HashMap<String, SendQuote>>>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Ledger {
    handles: HashMap<String, HandleRecord>,
    // Profile photos by user id, as small data URLs.
    #[serde(default)]
    avatars: HashMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandleRecord {
    user_id: String,
    handle: String,
    display_name: Option<String>,
    evm_wallet: String,
    // Where friends can pay them on Solana (filled in when they claim a handle or open the app).
    #[serde(default)]
    solana_wallet: Option<String>,
}
#[derive(Clone)]
struct SendQuote {
    owner: String,
    sender_wallet: String,
    recipient_wallet: String,
    label: String,
    currency: String,
    usdc_units: u128,
    expires: u64,
    plan: Option<Value>,
    // Duplicate confirmation requests prepare one intent and one receipt at a time.
    preparation: Arc<tokio::sync::Mutex<()>>,
    // An Atlas Link: its escrow, the note, and what the claimer gets (`usdc_units` adds the claim fee).
    link: Option<(String, Option<String>, u128)>,
    // A bank withdrawal through Daya: where, the naira the bank gets, and Daya's fee on top.
    bank: Option<(daya::Payout, u128, u128)>,
    // Additional USDC charged for delivery, separately from the bank payout fee.
    network_units: u128,
}
// Sends, bank cash-outs and Atlas Links are paid from Spendable (USDC in the Base wallet) only.
// With ETH in the wallet the transfer pays its own gas (free here); without, Relay delivers exactly
// the amount gaslessly for a few cents, on top. Nothing ever buys ETH first for a send.
async fn network_fee(state: &AppState, evm: &str, to: &str, usdc: u128) -> Result<u128, ApiError> {
    if markets::wallet_pays_gas(state, evm).await {
        return Ok(0);
    }
    let moved = markets::base_gasless_payment(state, evm, to, usdc).await?;
    Ok(markets::fee_room(
        moved.amount_in_units.saturating_sub(usdc),
    ))
}

// The quoted fee already has room for Relay's price moving a hair, and is the most a send may
// spend on it. More than that needs a fresh look; nothing is sent.
fn checked_relay_fee(quoted: u128, actual: u128) -> Result<(), ApiError> {
    if actual <= quoted {
        return Ok(());
    }
    Err((
        StatusCode::CONFLICT,
        "The network fee changed. Check a fresh quote before sending; nothing was sent.".into(),
    ))
}

// The plan that sends `usdc` to `to` from Spendable: the planned transfer when the wallet pays its
// gas, else a gasless Relay payment of exactly `usdc`. Returns the intent, what the app signs, and
// the network fee in USDC (0 for a plain transfer).
#[allow(clippy::too_many_arguments)]
async fn spend_plan(
    state: &AppState,
    owner: String,
    evm: &str,
    to: &str,
    usdc: u128,
    quoted_network: u128,
    currency: &str,
    rate: u128,
) -> Result<(String, Vec<Value>, u128), ApiError> {
    if markets::wallet_pays_gas(state, evm).await {
        let tx = state
            .markets
            .base
            .transfer_transaction(BASE_USDC, evm, to, usdc)
            .map_err(internal)?;
        // The plain transfer only: the wallet's ETH pays, and no refill is bought on the side
        // (the quote said this send is free). Once the ETH runs low, sends go through Relay.
        let txs = vec![(tx.to.clone(), tx.data.clone())];
        let (intent_id, transactions, fee) = markets::plan_base_bank_with_cash(
            state,
            owner,
            evm.into(),
            None,
            txs,
            usdc,
            currency,
            rate,
        )
        .await?;
        return Ok((intent_id, transactions, fee.unwrap_or(0)));
    }
    let moved = markets::base_gasless_payment(state, evm, to, usdc).await?;
    let fee = moved.amount_in_units.saturating_sub(usdc);
    checked_relay_fee(quoted_network, fee)?;
    let cash = state
        .markets
        .base
        .balance_of(BASE_USDC, evm)
        .await
        .map_err(markets::unavailable)?;
    if cash < moved.amount_in_units {
        return Err(markets::not_enough_spendable(cash, 0, currency, rate));
    }
    let (intent_id, transactions) =
        markets::plan_relay_move(state, owner, evm.into(), "base", moved).await?;
    Ok((intent_id, transactions, fee))
}

fn checked_bank_network_fee(quoted: u128, actual: u128) -> Result<(), ApiError> {
    if actual <= quoted {
        return Ok(());
    }
    Err((
        StatusCode::CONFLICT,
        "The network fee changed. Check a fresh quote before sending; nothing was sent.".into(),
    ))
}

fn bank_network_naira(network: u128, fx: u128) -> u128 {
    network.saturating_mul(fx).div_ceil(1_000_000)
}

#[derive(Deserialize)]
pub(super) struct HandleBody {
    handle: String,
}
#[derive(Deserialize)]
pub(super) struct HandleQuery {
    handle: String,
}
#[derive(Deserialize)]
pub(super) struct BanksQuery {
    country: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BankResolve {
    bank_code: String,
    account_number: String,
}
#[derive(Deserialize)]
pub(super) struct SendRequest {
    destination: Destination,
    amount: Money,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Destination {
    Atlas {
        handle: String,
    },
    Bank {
        #[serde(rename = "bankCode")]
        bank_code: String,
        #[serde(rename = "accountNumber")]
        account_number: String,
    },
    Cashlink {
        message: Option<String>,
        // The link's escrow address, made on the sender's phone with the link's secret.
        escrow: Option<String>,
    },
}
#[derive(Deserialize)]
struct Money {
    amount: String,
    currency: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn invalid_handle(handle: &str) -> bool {
    !(3..=20).contains(&handle.len())
        || !handle
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}
fn unavailable(message: &str) -> ApiError {
    (StatusCode::SERVICE_UNAVAILABLE, message.into())
}
fn money_usdc(units: u128, currency: &str, rate: u128) -> Result<Value, ApiError> {
    let value = units
        .checked_mul(rate)
        .ok_or_else(|| bad("amount too large"))?
        / 1_000_000;
    let mut amount = format!("{}.{:06}", value / 1_000_000, value % 1_000_000);
    while amount.ends_with('0') {
        amount.pop();
    }
    if amount.ends_with('.') {
        amount.pop();
    }
    Ok(json!({"amount":amount,"currency":currency}))
}
fn parse_micros(value: &str) -> Result<u128, ApiError> {
    let (whole, frac) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || frac.len() > 6
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad(
            "amount must be a positive decimal with at most six places",
        ));
    }
    let w: u128 = whole.parse().map_err(|_| bad("amount too large"))?;
    let f: u128 = format!("{:0<6}", frac)
        .parse()
        .map_err(|_| bad("invalid amount"))?;
    w.checked_mul(1_000_000)
        .and_then(|x| x.checked_add(f))
        .filter(|x| *x > 0)
        .ok_or_else(|| bad("amount must be positive"))
}
impl SocialState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("social database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_handles (
                handle TEXT PRIMARY KEY,
                user_id TEXT NOT NULL UNIQUE,
                display_name TEXT,
                evm_wallet TEXT NOT NULL
            );
            ALTER TABLE atlas_handles ADD COLUMN IF NOT EXISTS solana_wallet TEXT;
            CREATE TABLE IF NOT EXISTS atlas_avatars (
                user_id TEXT PRIMARY KEY,
                image TEXT NOT NULL,
                updated_at BIGINT NOT NULL
            )",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        let path = if postgres.is_none() {
            env::var_os("ATLAS_SOCIAL_STATE_PATH").map(PathBuf::from)
        } else {
            None
        };
        let ledger = if let Some(path) = &path {
            if path.exists() {
                serde_json::from_slice(&fs::read(path)?)?
            } else {
                Ledger::default()
            }
        } else {
            Ledger::default()
        };
        Ok(Self {
            path,
            postgres,
            ledger: Arc::new(Mutex::new(ledger)),
            quotes: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    fn require_storage(&self) -> Result<(), ApiError> {
        if self.path.is_some() || self.postgres.is_some() {
            Ok(())
        } else {
            Err(unavailable("permanent handle storage is not configured"))
        }
    }
    async fn avatar(&self, user_id: &str) -> Result<Option<String>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT image FROM atlas_avatars WHERE user_id=$1",
                    &[&user_id],
                )
                .await
                .map_err(internal)?;
            return Ok(row.map(|r| r.get("image")));
        }
        Ok(self
            .ledger
            .lock()
            .map_err(internal)?
            .avatars
            .get(user_id)
            .cloned())
    }
    async fn set_avatar(&self, user_id: &str, image: Option<&str>) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            match image {
                Some(image) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(internal)?
                        .as_millis() as i64;
                    pg.execute(
                        "INSERT INTO atlas_avatars (user_id,image,updated_at) VALUES ($1,$2,$3)
                         ON CONFLICT (user_id) DO UPDATE SET image=EXCLUDED.image, updated_at=EXCLUDED.updated_at",
                        &[&user_id, &image, &now],
                    )
                    .await
                    .map_err(internal)?;
                }
                None => {
                    pg.execute("DELETE FROM atlas_avatars WHERE user_id=$1", &[&user_id])
                        .await
                        .map_err(internal)?;
                }
            }
            return Ok(());
        }
        let mut ledger = self.ledger.lock().map_err(internal)?;
        match image {
            Some(image) => ledger.avatars.insert(user_id.into(), image.into()),
            None => ledger.avatars.remove(user_id),
        };
        if let Some(path) = &self.path {
            fs::write(path, serde_json::to_vec(&*ledger).map_err(internal)?).map_err(internal)?;
        }
        Ok(())
    }
    async fn find_user(&self, user_id: &str) -> Result<Option<HandleRecord>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg.query_opt("SELECT user_id, handle, display_name, evm_wallet, solana_wallet FROM atlas_handles WHERE user_id=$1", &[&user_id]).await.map_err(internal)?;
            return Ok(row.map(record_from_row));
        }
        Ok(self
            .ledger
            .lock()
            .map_err(internal)?
            .handles
            .values()
            .find(|r| r.user_id == user_id)
            .cloned())
    }
    // The user's @handle, if they've picked one (comments are signed with it).
    pub(super) async fn handle_of(&self, user_id: &str) -> Result<Option<String>, ApiError> {
        Ok(self.find_user(user_id).await?.map(|r| r.handle))
    }
    // Who has this @handle, for telling them a friend paid them.
    pub(super) async fn user_of_handle(&self, handle: &str) -> Result<Option<String>, ApiError> {
        Ok(self.find_handle(handle).await?.map(|r| r.user_id))
    }
    async fn find_handle(&self, handle: &str) -> Result<Option<HandleRecord>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg.query_opt("SELECT user_id, handle, display_name, evm_wallet, solana_wallet FROM atlas_handles WHERE handle=$1", &[&handle]).await.map_err(internal)?;
            return Ok(row.map(record_from_row));
        }
        Ok(self
            .ledger
            .lock()
            .map_err(internal)?
            .handles
            .get(handle)
            .cloned())
    }
    async fn register_postgres(&self, record: &HandleRecord) -> Result<(), ApiError> {
        let pg = self
            .postgres
            .as_ref()
            .ok_or_else(|| unavailable("database not configured"))?;
        let result = pg.execute("INSERT INTO atlas_handles (handle,user_id,display_name,evm_wallet,solana_wallet) VALUES ($1,$2,$3,$4,$5)",
            &[&record.handle,&record.user_id,&record.display_name,&record.evm_wallet,&record.solana_wallet]).await;
        match result {
            Ok(1) => Ok(()),
            Ok(_) => Err(internal("handle insert affected no rows")),
            Err(error)
                if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) =>
            {
                Err((
                    StatusCode::CONFLICT,
                    "handle already taken or user already registered".into(),
                ))
            }
            Err(error) => Err(internal(error)),
        }
    }
    fn save(&self, ledger: &Ledger) -> Result<(), ApiError> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| unavailable("permanent handle storage is not configured"))?;
        let temporary = path.with_extension("tmp");
        let bytes = serde_json::to_vec(ledger).map_err(internal)?;
        let mut file = fs::File::create(&temporary).map_err(internal)?;
        use std::io::Write;
        file.write_all(&bytes).map_err(internal)?;
        file.sync_all().map_err(internal)?;
        fs::rename(temporary, path).map_err(internal)
    }
}

fn record_from_row(row: tokio_postgres::Row) -> HandleRecord {
    HandleRecord {
        user_id: row.get("user_id"),
        handle: row.get("handle"),
        display_name: row.get("display_name"),
        evm_wallet: row.get("evm_wallet"),
        solana_wallet: row.try_get("solana_wallet").ok().flatten(),
    }
}
pub(super) async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    // Where their money emails go (never fatal to opening the app).
    let _ = state.emails.remember(&user, None).await;
    state.social.require_storage()?;
    let record = state.social.find_user(&user.user_id).await?;
    // Handles claimed before Solana sends existed learn their Solana wallet here.
    if let (Some(r), Some(solana), Some(pg)) = (
        &record,
        user.solana_wallet.as_deref().filter(|w| !w.is_empty()),
        &state.social.postgres,
    ) {
        if r.solana_wallet.as_deref() != Some(solana) {
            pg.execute(
                "UPDATE atlas_handles SET solana_wallet=$2 WHERE user_id=$1",
                &[&user.user_id, &solana],
            )
            .await
            .map_err(internal)?;
        }
    }
    let avatar = state.social.avatar(&user.user_id).await?;

    Ok(Json(
        json!({"userId":user.user_id,"handle":record.as_ref().map(|r|&r.handle),"displayName":record.and_then(|r|r.display_name),"avatar":avatar}),
    ))
}

#[derive(Deserialize)]
pub(super) struct AvatarBody {
    image: Option<String>,
}
// Profile photos come from the app already cropped and shrunk; keep them small.
const AVATAR_MAX_CHARS: usize = 150_000;

fn valid_avatar(image: &str) -> bool {
    let Some(payload) = image
        .strip_prefix("data:image/jpeg;base64,")
        .or_else(|| image.strip_prefix("data:image/png;base64,"))
    else {
        return false;
    };
    image.len() <= AVATAR_MAX_CHARS
        && payload.len() > 100
        && payload.len() % 4 == 0
        && payload
            .trim_end_matches('=')
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

// Sets (or, with `image: null`, removes) the signed-in user's profile photo.
pub(super) async fn set_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AvatarBody>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    state.social.require_storage()?;
    if let Some(image) = &body.image {
        if !valid_avatar(image) {
            return Err(bad("photo must be a JPEG or PNG under about 110 KB"));
        }
    }
    state
        .social
        .set_avatar(&user.user_id, body.image.as_deref())
        .await?;
    Ok(Json(json!({"avatar":body.image})))
}
pub(super) async fn set_handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<HandleBody>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if invalid_handle(&body.handle) {
        return Err(bad(
            "handle must be 3–20 lowercase letters, digits, or underscores",
        ));
    }
    state.social.require_storage()?;
    let wallet = user.evm_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    if state.social.postgres.is_some() {
        if let Some(existing) = state.social.find_user(&user.user_id).await? {
            if existing.handle == body.handle {
                return Ok(Json(
                    json!({"userId":user.user_id,"handle":existing.handle,"displayName":existing.display_name}),
                ));
            }
            return Err((StatusCode::CONFLICT, "handle is permanent".into()));
        }
        if state.social.find_handle(&body.handle).await?.is_some() {
            return Err((StatusCode::CONFLICT, "handle already taken".into()));
        }
        let record = HandleRecord {
            user_id: user.user_id.clone(),
            handle: body.handle.clone(),
            display_name: None,
            evm_wallet: wallet,
            solana_wallet: user.solana_wallet.clone().filter(|w| !w.is_empty()),
        };
        state.social.register_postgres(&record).await?;
        return Ok(Json(
            json!({"userId":user.user_id,"handle":body.handle,"displayName":null}),
        ));
    }
    let mut ledger = state.social.ledger.lock().map_err(internal)?;
    if let Some(existing) = ledger.handles.values().find(|r| r.user_id == user.user_id) {
        if existing.handle == body.handle {
            return Ok(Json(
                json!({"userId":user.user_id,"handle":existing.handle,"displayName":existing.display_name}),
            ));
        }
        return Err((StatusCode::CONFLICT, "handle is permanent".into()));
    }
    if ledger.handles.contains_key(&body.handle) {
        return Err((StatusCode::CONFLICT, "handle already taken".into()));
    }
    let record = HandleRecord {
        user_id: user.user_id.clone(),
        handle: body.handle.clone(),
        display_name: None,
        evm_wallet: wallet,
        solana_wallet: user.solana_wallet.clone().filter(|w| !w.is_empty()),
    };
    let mut next = ledger.clone();
    next.handles.insert(body.handle.clone(), record);
    state.social.save(&next)?;
    *ledger = next;
    Ok(Json(
        json!({"userId":user.user_id,"handle":body.handle,"displayName":null}),
    ))
}
pub(super) async fn resolve_user(
    State(state): State<AppState>,
    Query(q): Query<HandleQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    state.social.require_storage()?;
    let record = state
        .social
        .find_handle(&q.handle)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "unknown handle".into()))?;

    let avatar = state.social.avatar(&record.user_id).await?;
    Ok(Json(
        json!({"handle":record.handle,"displayName":record.display_name,"avatar":avatar}),
    ))
}
pub(super) async fn banks(
    State(state): State<AppState>,
    Query(q): Query<BanksQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    if q.country.as_deref() != Some("NG") {
        return Err(bad("only NG banks are supported"));
    }
    Ok(Json(json!({"banks": *state.daya.banks().await?})))
}
pub(super) async fn resolve_bank(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BankResolve>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    match state
        .daya
        .account_name(&body.bank_code, &body.account_number)
        .await?
    {
        Some(name) => Ok(Json(json!({"accountName": name}))),
        None => Err((StatusCode::NOT_FOUND, "no such bank account".into())),
    }
}
pub(super) async fn send_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SendRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    state.social.require_storage()?;
    let solana = user.solana_wallet.clone().filter(|w| !w.is_empty());
    let wallet = user.evm_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Privy Ethereum wallet is not ready".into(),
    ))?;
    let mut link: Option<(String, Option<String>)> = None;
    let mut bank: Option<daya::Payout> = None;
    let (recipient, label) = match req.destination {
        Destination::Atlas { handle } => {
            let record = state
                .social
                .find_handle(&handle)
                .await?
                .ok_or((StatusCode::NOT_FOUND, "unknown handle".into()))?;

            if record.user_id == user.user_id {
                return Err(bad("cannot send to yourself"));
            }
            (record.evm_wallet.clone(), format!("@{}", record.handle))
        }
        // Paid to Daya's one-time address, opened when the user confirms.
        Destination::Bank {
            bank_code,
            account_number,
        } => {
            let payout = state.daya.payout_to(&bank_code, &account_number).await?;
            let label = payout.label.clone();
            bank = Some(payout);
            (String::new(), label)
        }
        Destination::Cashlink { message, escrow } => {
            let escrow = escrow
                .as_deref()
                .and_then(cashlinks::escrow_id)
                .ok_or_else(|| bad("a link needs its escrow address"))?;
            if state.links.get(&escrow).await?.is_some() {
                return Err(bad("this link already exists"));
            }
            let note = message
                .map(|m| m.trim().chars().take(80).collect::<String>())
                .filter(|m| !m.is_empty());
            link = Some((escrow.clone(), note));
            (escrow, "Atlas Link".to_string())
        }
    };
    if !DISPLAY_CURRENCIES.contains(&req.amount.currency.as_str()) {
        return Err(bad("unsupported display currency"));
    }
    let rate = app_balance::fx_rate(&req.amount.currency).await?;
    let amount = parse_micros(&req.amount.amount)?;
    // A bank withdrawal names what the bank gets, in naira; Daya's fee goes on top.
    let bank = match bank {
        Some(mut payout) => {
            if req.amount.currency != "NGN" {
                return Err(bad("Bank withdrawals are in naira"));
            }
            let (usdc, fee) = state.daya.payout_cost(&mut payout, amount).await?;
            Some((payout, usdc, fee))
        }
        None => None,
    };
    let gift_units = match &bank {
        Some((_, usdc, _)) => *usdc,
        None => {
            amount
                .checked_mul(1_000_000)
                .ok_or_else(|| bad("amount too large"))?
                / rate
        }
    };
    markets::check_limits(gift_units, &req.amount.currency, rate)?;
    // A link also carries the few cents that pay out its claim, so the friend gets it all.
    let fee_units = if link.is_some() {
        cashlinks::CLAIM_FEE_UNITS
    } else {
        0
    };
    let usdc_units = gift_units + fee_units;
    // Paid from Spendable only. Short there, the error says what Trading holds, so the app can offer
    // to move it back first.
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, &wallet)
        .await
        .map_err(markets::unavailable)?;
    // The network fee: nothing when the wallet's ETH pays, else Relay's few cents. A bank payout's
    // address only exists once confirmed; Relay prices a same-chain payment the same either way.
    let fee_to = if bank.is_some() { &wallet } else { &recipient };
    let network_units = network_fee(&state, &wallet, fee_to, usdc_units).await?;
    if base_cash < usdc_units.saturating_add(network_units) {
        let trading = match solana.as_deref() {
            Some(owner) => markets::solana_cash(&state, owner).await,
            None => 0,
        };
        return Err(markets::not_enough_spendable(
            base_cash,
            trading,
            &req.amount.currency,
            rate,
        ));
    }
    let quote_id = format!(
        "send-{:x}-{:x}",
        now(),
        NEXT_SEND_ID.fetch_add(1, Ordering::Relaxed)
    );
    let expires = now() + 30_000;
    state.social.quotes.lock().map_err(internal)?.insert(
        quote_id.clone(),
        SendQuote {
            owner: user.user_id,
            sender_wallet: wallet,
            recipient_wallet: recipient,
            label: label.clone(),
            currency: req.amount.currency.clone(),
            usdc_units,
            expires,
            plan: None,
            preparation: Arc::new(tokio::sync::Mutex::new(())),
            link: link.map(|(escrow, note)| (escrow, note, gift_units)),
            bank: bank
                .as_ref()
                .map(|(payout, _, fee)| (payout.clone(), amount, *fee)),
            network_units,
        },
    );
    // Fee is Daya's bank payout charge. Only a new USDC debit appears as Network fee;
    // rent or transaction fees paid from the existing SOL reserve add no USDC charge.
    if let Some((_, _, fee)) = bank {
        let network_ngn = bank_network_naira(network_units, rate);
        let total_fee = fee.saturating_add(network_ngn);
        let naira = |micros: u128| money_usdc(micros, "NGN", 1_000_000);
        return Ok(Json(json!({"quoteId":quote_id,"destinationLabel":label,
                "send":naira(amount.saturating_add(total_fee))?,"receive":naira(amount)?,
                "fee":naira(fee)?,"networkFee":naira(network_ngn)?,"feeBreakdown":[],
                "eta":"Usually within minutes","expiresAtUnixMs":expires})));
    }
    // The network fee goes on top: in Fee and "You pay" here, as on the confirmation.
    let send = money_usdc(usdc_units + network_units, &req.amount.currency, rate)?;
    let receive = money_usdc(gift_units, &req.amount.currency, rate)?;
    let fee = money_usdc(fee_units + network_units, &req.amount.currency, rate)?;
    let mut breakdown = Vec::new();
    if fee_units > 0 {
        breakdown.push(json!({"label":"Claim delivery reserve","amount":money_usdc(fee_units, &req.amount.currency, rate)?}));
    }
    if network_units > 0 {
        breakdown.push(json!({"label":"Network delivery","amount":money_usdc(network_units, &req.amount.currency, rate)?}));
    }
    Ok(Json(
        json!({"quoteId":quote_id,"destinationLabel":label,"send":send,"receive":receive,"fee":fee,
            "feeBreakdown":breakdown,"eta":"After confirmation","expiresAtUnixMs":expires}),
    ))
}
async fn execute_send_inner(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let quote = state
        .social
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?;
    if quote.owner != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "send quote belongs to another user".into(),
        ));
    }
    if let Some(plan) = &quote.plan {
        return Ok(Json(plan.clone()));
    }
    if quote.expires < now() {
        return Err((StatusCode::CONFLICT, "send quote expired".into()));
    }
    if user.evm_wallet.as_deref() != Some(quote.sender_wallet.as_str()) {
        return Err((StatusCode::CONFLICT, "Privy wallet changed".into()));
    }
    if let Some((payout, get, fee)) = &quote.bank {
        let (plan, funding_account, expires) =
            bank_plan(&state, &user, &quote_id, &quote, payout, *get, *fee).await?;
        // Tracking must be durable before this plan can leave the server and be signed. A failed
        // write aborts preparation; the user cannot send funds into an untracked payout account.
        let intent_id = plan["intentId"].as_str().unwrap_or("");
        state
            .daya
            .record_payout(
                &user.user_id,
                &funding_account,
                intent_id,
                quote.usdc_units,
                expires,
            )
            .await?;
        state
            .social
            .quotes
            .lock()
            .map_err(internal)?
            .get_mut(&quote_id)
            .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?
            .plan = Some(plan.clone());
        // Recents are cosmetic; their write may fail after the payout itself is safely tracked.
        if let Err((_, error)) = state.daya.paid(&user.user_id, payout).await {
            eprintln!("bank recipient not kept: {error}");
        }
        return Ok(Json(plan));
    }
    let rate = app_balance::fx_rate(&quote.currency).await?;
    // A repeated execute gets the same plan back.
    if let Some(plan) = state
        .social
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?
        .plan
        .clone()
    {
        return Ok(Json(plan));
    }
    // From Spendable only: the cash is never moved over from Trading behind the user's back.
    let (intent_id, transactions, fee) = spend_plan(
        &state,
        user.user_id.clone(),
        &quote.sender_wallet,
        &quote.recipient_wallet,
        quote.usdc_units,
        quote.network_units,
        &quote.currency,
        rate,
    )
    .await?;
    // Relay's fee is at most the quoted one; the confirmation and receipt show exactly what leaves.
    let fee = (fee > 0).then_some(fee);
    // An Atlas Link is recorded once its plan exists (the escrow is its id; a repeat finds it).
    if let Some((escrow, note, gift)) = &quote.link {
        let sender = state.social.find_user(&user.user_id).await?;
        let created = now();
        let recorded = state
            .links
            .insert(&cashlinks::Link {
                escrow: escrow.clone(),
                owner: user.user_id.clone(),
                sender_name: sender.as_ref().and_then(|r| r.display_name.clone()),
                sender_handle: sender.map(|r| r.handle),
                amount_units: *gift,
                currency: quote.currency.clone(),
                message: note.clone(),
                state: "open".into(),
                created_ms: created,
                expires_ms: created + cashlinks::LINK_DAYS * 24 * 60 * 60 * 1000,
                claim_started_ms: 0,
                request_id: None,
                claimer: None,
                settled: false,
            })
            .await?;
        if !recorded {
            if let Some(plan) = state
                .social
                .quotes
                .lock()
                .map_err(internal)?
                .get(&quote_id)
                .and_then(|q| q.plan.clone())
            {
                return Ok(Json(plan));
            }
            return Err(bad("this link already exists"));
        }
    }
    let mut quotes = state.social.quotes.lock().map_err(internal)?;
    let stored = quotes
        .get_mut(&quote_id)
        .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?;
    // Two executes racing: the first plan stands (the other intent is never signed).
    if let Some(plan) = &stored.plan {
        return Ok(Json(plan.clone()));
    }
    let mut summary = vec![
        json!({"label":"Send to","value":quote.label}),
        json!({"label":"Amount","value":markets::say_money(quote.usdc_units,&quote.currency,rate)}),
    ];
    if let Some((_, note, gift)) = &quote.link {
        summary[1] =
            json!({"label":"They get","value":markets::say_money(*gift,&quote.currency,rate)});
        summary.push(json!({"label":"Claim delivery reserve","value":markets::say_money(cashlinks::CLAIM_FEE_UNITS,&quote.currency,rate)}));
        if let Some(note) = note {
            summary.push(json!({"label":"Note","value":note}));
        }
    }
    if let Some(fee) = fee {
        summary.push(
            json!({"label":"Network fee","value":markets::say_money(fee,&quote.currency,rate)}),
        );
        summary.push(json!({"label":"You pay","value":markets::say_money(quote.usdc_units + fee,&quote.currency,rate)}));
    }
    let plan = json!({"intentId":intent_id,"kind":"send","summary":summary,"transactions":transactions,"expiresAtUnixMs":now()+120_000});
    stored.plan = Some(plan.clone());
    Ok(Json(plan))
}
// A bank withdrawal from Spendable: Daya opens a one-time USDC address on Base, and the phone sends
// the USDC there. Daya's idempotency key is the quote, so a retried execute gets the same address.
async fn bank_plan(
    state: &AppState,
    user: &app_balance::VerifiedWallets,
    quote_id: &str,
    quote: &SendQuote,
    payout: &daya::Payout,
    get: u128,
    fee: u128,
) -> Result<(Value, String, u64), ApiError> {
    let rate = app_balance::fx_rate(&quote.currency).await?;
    let (funding_account, address, expires) = state
        .daya
        .open_payout(user, payout, "BASE", &format!("atlas-{quote_id}"))
        .await?;
    let (intent_id, transactions, network) = spend_plan(
        state,
        user.user_id.clone(),
        &quote.sender_wallet,
        &address,
        quote.usdc_units,
        quote.network_units,
        &quote.currency,
        rate,
    )
    .await?;
    // Never give the phone a plan that spends more USDC for its network fee than they reviewed.
    checked_bank_network_fee(quote.network_units, network)?;
    let network_ngn = bank_network_naira(network, rate);
    let total_fee = fee.saturating_add(network_ngn);
    // Keep the bank charge separate from any fresh USDC spent to fund the network.
    let summary = vec![
        json!({"label":"Send to","value":quote.label}),
        json!({"label":"Bank gets","value":markets::say_micros(get, "NGN")}),
        json!({"label":"Fee","value":markets::say_micros(fee, "NGN")}),
        json!({"label":"Network fee","value":if network == 0 { "Free".into() } else { markets::say_micros(network_ngn, "NGN") }}),
        json!({"label":"You pay","value":markets::say_micros(get.saturating_add(total_fee), "NGN")}),
        json!({"label":"Rate","value":all_in_rate(get.saturating_add(fee), quote.usdc_units)}),
        json!({"label":"Bank payout","value":"Waiting for your USDC"}),
    ];
    eprintln!(
        "bank withdrawal {quote_id}: quoted_network_usdc={} prepared_network_usdc={network}",
        quote.network_units
    );
    let plan = json!({"intentId":intent_id,"kind":"send","summary":summary,"transactions":transactions,"expiresAtUnixMs":now()+120_000});
    Ok((plan, funding_account, expires))
}
// Naira (micros) per dollar of USDC (units) that leaves the balance: "₦1,487.76 per $1".
fn all_in_rate(naira: u128, usdc: u128) -> String {
    format!(
        "{} per $1",
        markets::say_micros(naira.saturating_mul(1_000_000) / usdc.max(1), "NGN")
    )
}
#[cfg(test)]
mod tests {
    #[test]
    fn a_send_never_spends_more_on_relay_than_the_fee_reviewed() {
        // Relay's same-chain fee was about 2.34 cents on 12 Oct.
        // Relay asked 2.34 cents; the quote shows it with room: 2.54 cents.
        let quoted = markets::fee_room(23_400);
        assert_eq!(quoted, 25_400);
        assert!(super::checked_relay_fee(quoted, 23_400).is_ok());
        assert!(super::checked_relay_fee(quoted, 25_400).is_ok());
        assert!(super::checked_relay_fee(quoted, 0).is_ok());
        // Never a hair past what they reviewed.
        assert!(super::checked_relay_fee(quoted, 25_401).is_err());
        // Quoted with the wallet's own ETH paying, then none left: a fresh look first.
        assert!(super::checked_relay_fee(0, 23_400).is_err());
    }

    #[test]
    fn expensive_fallback_requires_another_fee_review() {
        assert!(checked_bank_network_fee(22_006, 300_000).is_err());
        assert!(checked_bank_network_fee(22_006, 22_007).is_err());
        assert!(checked_bank_network_fee(22_006, 22_006).is_ok());
        assert!(checked_bank_network_fee(22_006, 0).is_ok());
        assert!(checked_bank_network_fee(0, 1).is_err());
    }

    #[test]
    fn bank_network_charge_is_added_once_to_the_total() {
        let delivery = bank_network_naira(22_006, 1_560_000_000);
        assert_eq!(delivery, 34_329_360); // ₦34.32936 at ₦1,560 per USDC.
        assert_eq!(707_000_000_u128 + delivery, 741_329_360);
        assert_eq!(bank_network_naira(0, 1_560_000_000), 0);
    }

    #[test]
    fn provider_rate_is_distinct_from_network_cash_charge() {
        // ₦700 to the bank + ₦7 Daya fee = 0.453205 USDC at ₦1,560; Relay adds 0.022006.
        assert_eq!(super::all_in_rate(707_000_000, 475_211), "₦1,487.76 per $1");
        // Cash already on Base: nothing to move, so it's Daya's own rate.
        assert_eq!(super::all_in_rate(707_000_000, 453_205), "₦1,560.00 per $1");
    }
    #[test]
    fn avatars_must_be_small_images() {
        let jpeg = format!("data:image/jpeg;base64,{}", "A".repeat(400));
        assert!(valid_avatar(&jpeg));
        assert!(!valid_avatar("data:image/gif;base64,AAAA"));
        assert!(!valid_avatar(&format!(
            "data:image/png;base64,{}",
            "A".repeat(200_000)
        )));
        assert!(!valid_avatar(&format!(
            "data:image/png;base64,{}<script>",
            "A".repeat(400)
        )));
    }
    use super::*;
    #[test]
    fn handle_validation() {
        for valid in ["ade", "a_3", "abc12345678901234567"] {
            assert!(!invalid_handle(valid));
        }
        for invalid in ["ab", "ABC", "a-b", "abc12345678901234567890"] {
            assert!(invalid_handle(invalid));
        }
    }
    #[test]
    fn send_amounts_are_exact_decimal() {
        assert_eq!(parse_micros("0.123456").unwrap(), 123456);
        assert!(parse_micros("0.1234567").is_err());
    }
}

// Save the confirmation's receipt before the phone signs; this does not submit an action.
pub(super) async fn execute_send(
    State(state): State<AppState>,
    Path(quote_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let quote = state
        .social
        .quotes
        .lock()
        .map_err(internal)?
        .get(&quote_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?;
    if quote.owner != owner {
        return Err((
            StatusCode::FORBIDDEN,
            "send quote belongs to another user".into(),
        ));
    }
    // Includes the durable receipt write: a second execute cannot race a new intent or reset a
    // progressing receipt after the first response has already been signed by the user.
    let _preparation = quote.preparation.lock().await;
    let plan =
        execute_send_inner(State(state.clone()), Path(quote_id), headers, Json(body)).await?;
    let intent_id = plan.0["intentId"]
        .as_str()
        .ok_or_else(|| internal("send plan missing intent"))?;
    if state.history.find(&owner, intent_id).await?.is_some() {
        return Ok(plan);
    }
    let mut receipt = transactions::Receipt::plan(&owner, &plan.0);
    receipt.usdc_units = Some(quote.usdc_units.to_string());
    if quote.link.is_some() {
        receipt.kind = "cashlink".into();
    }
    if quote.bank.is_some() {
        receipt.kind = "offramp".into();
    }
    receipt.title = transactions::title(&receipt.kind, &receipt.symbol);
    state.history.put(&receipt).await?;
    Ok(plan)
}
