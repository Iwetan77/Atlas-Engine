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
    sender_solana: Option<String>,
    recipient_solana: Option<String>,
    label: String,
    currency: String,
    usdc_units: u128,
    expires: u64,
    plan: Option<Value>,
    // An Atlas Link: its escrow, the note, and what the claimer gets (`usdc_units` adds the claim fee).
    link: Option<(String, Option<String>, u128)>,
    // A bank withdrawal through Daya: where, the naira the bank gets, and Daya's fee on top.
    bank: Option<(daya::Payout, u128, u128)>,
    // How the quote expected its USDC to travel, and the network fee that costs (naira micros), so
    // the confirmation shows the same Fee and total the quote did.
    bank_network: Option<(BankRoute, u128)>,
    // A friend send or link: the network fee the quote showed (USDC units), in its Fee and total.
    network_units: u128,
}
// How a bank withdrawal's USDC reaches the payout address: straight from Base, straight from
// Solana (opening the address's USDC account costs rent), or hopped from Solana to Base first.
#[derive(Clone, Copy, PartialEq, Debug)]
enum BankRoute {
    Base,
    Solana,
    Hop,
}
async fn bank_route(state: &AppState, evm: &str, solana: Option<&str>, usdc: u128) -> BankRoute {
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, evm)
        .await
        .unwrap_or(0);
    if base_cash >= usdc {
        return BankRoute::Base;
    }
    // Straight from Solana only when the wallet can also pay that transfer's network fee.
    if let Some(from) = solana {
        if markets::solana_cash(state, from).await >= usdc
            && markets::solana_can_send(state, from, usdc).await
        {
            return BankRoute::Solana;
        }
    }
    BankRoute::Hop
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

    Ok(Json(
        json!({"handle":record.handle,"displayName":record.display_name}),
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
    let (recipient, recipient_solana, label) = match req.destination {
        Destination::Atlas { handle } => {
            let record = state
                .social
                .find_handle(&handle)
                .await?
                .ok_or((StatusCode::NOT_FOUND, "unknown handle".into()))?;

            if record.user_id == user.user_id {
                return Err(bad("cannot send to yourself"));
            }
            (
                record.evm_wallet.clone(),
                record.solana_wallet.clone(),
                format!("@{}", record.handle),
            )
        }
        // Paid to Daya's one-time address, opened when the user confirms.
        Destination::Bank {
            bank_code,
            account_number,
        } => {
            let payout = state.daya.payout_to(&bank_code, &account_number).await?;
            let label = payout.label.clone();
            bank = Some(payout);
            (String::new(), None, label)
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
            (escrow, None, "Atlas Link".to_string())
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
    // One balance. Friends are paid on the chain the cash is on: Base, or Solana straight to their
    // Solana wallet. Only when neither chain holds it all does cash move first; short on both, say so.
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, &wallet)
        .await
        .unwrap_or(0);
    // Daya takes USDC on Solana as well as Base.
    let route = match bank {
        Some(_) => Some(bank_route(&state, &wallet, solana.as_deref(), usdc_units).await),
        None => None,
    };
    let solana_covers = match (&solana, &recipient_solana) {
        (Some(from), Some(_)) => markets::solana_cash(&state, from).await >= usdc_units,
        (Some(_), None) => route == Some(BankRoute::Solana),
        _ => false,
    };
    let mut hop_fee = 0;
    if base_cash < usdc_units && !solana_covers {
        if let Some((_, fee)) = markets::cash_for_base(
            &state,
            &wallet,
            solana.as_deref(),
            usdc_units,
            &req.amount.currency,
            rate,
        )
        .await?
        {
            hop_fee = fee;
        }
    }
    // A friend send's network fee, shown from the quote on: opening the friend's USDC account on
    // Solana when they have none, or hopping cash to Base.
    let network_units = if bank.is_some() {
        0
    } else if base_cash < usdc_units && solana_covers {
        match &recipient_solana {
            Some(to) => markets::solana_transfer_fee_estimate(&state, to).await,
            None => 0,
        }
    } else {
        hop_fee
    };
    // A bank withdrawal's network fee in naira, in its Fee and "You pay" from the quote on: rent
    // for the payout address's new USDC account on Solana, or the cost of hopping cash to Base.
    let bank_network = match route {
        Some(route) => {
            let usdc = match route {
                BankRoute::Base => 0,
                BankRoute::Solana => markets::account_rent_usd(&state).await,
                BankRoute::Hop => hop_fee,
            };
            let ngn = usdc.saturating_mul(app_balance::fx_rate("NGN").await?) / 1_000_000;
            Some((route, ngn))
        }
        None => None,
    };
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
            sender_solana: solana,
            recipient_solana,
            label: label.clone(),
            currency: req.amount.currency.clone(),
            usdc_units,
            expires,
            plan: None,
            link: link.map(|(escrow, note)| (escrow, note, gift_units)),
            bank: bank
                .as_ref()
                .map(|(payout, _, fee)| (payout.clone(), amount, *fee)),
            bank_network,
            network_units,
        },
    );
    // The bank gets exactly what was asked; the user pays that plus one Fee: Daya's (at Daya's
    // rate) and the network's. The confirmation and the receipt show these same numbers.
    if let Some((_, _, fee)) = bank {
        let fee = fee + bank_network.map_or(0, |(_, ngn)| ngn);
        let naira = |micros: u128| money_usdc(micros, "NGN", 1_000_000);
        return Ok(Json(
            json!({"quoteId":quote_id,"destinationLabel":label,"send":naira(amount + fee)?,"receive":naira(amount)?,"fee":naira(fee)?,"eta":"Usually within minutes","expiresAtUnixMs":expires}),
        ));
    }
    // The network fee goes on top: in Fee and "You pay" here, as on the confirmation.
    let send = money_usdc(usdc_units + network_units, &req.amount.currency, rate)?;
    let receive = money_usdc(gift_units, &req.amount.currency, rate)?;
    let fee = money_usdc(fee_units + network_units, &req.amount.currency, rate)?;
    Ok(Json(
        json!({"quoteId":quote_id,"destinationLabel":label,"send":send,"receive":receive,"fee":fee,"eta":"After confirmation","expiresAtUnixMs":expires}),
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
        {
            let mut quotes = state.social.quotes.lock().map_err(internal)?;
            let stored = quotes
                .get_mut(&quote_id)
                .ok_or((StatusCode::NOT_FOUND, "send quote not found".into()))?;
            // Two executes racing: the first plan stands (both opened the same Daya address).
            if let Some(plan) = &stored.plan {
                return Ok(Json(plan.clone()));
            }
            stored.plan = Some(plan.clone());
        }
        // The account goes to the top of Send to bank's recents.
        if let Err((_, error)) = state.daya.paid(&user.user_id, payout).await {
            eprintln!("bank recipient not kept: {error}");
        }
        // Only the plan that stands is tied to the payout, so Daya's updates reach its receipt.
        let intent_id = plan["intentId"].as_str().unwrap_or("");
        if let Err((_, error)) = state
            .daya
            .record_payout(
                &user.user_id,
                &funding_account,
                intent_id,
                quote.usdc_units,
                expires,
            )
            .await
        {
            eprintln!("bank withdrawal {intent_id}: payout not recorded: {error}");
        }
        return Ok(Json(plan));
    }
    let tx = state
        .markets
        .base
        .transfer_transaction(
            BASE_USDC,
            &quote.sender_wallet,
            &quote.recipient_wallet,
            quote.usdc_units,
        )
        .map_err(internal)?;
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
    // Same chain first: Base if it holds the amount, else Solana to the friend's Solana wallet;
    // only then move cash across.
    let base_cash = state
        .markets
        .base
        .balance_of(BASE_USDC, &quote.sender_wallet)
        .await
        .unwrap_or(0);
    let solana_route = match (&quote.sender_solana, &quote.recipient_solana) {
        (Some(from), Some(to))
            if base_cash < quote.usdc_units
                && markets::solana_cash(&state, from).await >= quote.usdc_units =>
        {
            Some((from.clone(), to.clone()))
        }
        _ => None,
    };
    let (intent_id, transactions, fee) = if let Some((from, to)) = solana_route {
        let (intent_id, transactions, network) = markets::plan_solana_transfer(
            &state,
            user.user_id.clone(),
            from,
            &to,
            quote.usdc_units,
            true,
        )
        .await?;
        (intent_id, transactions, Some(network).filter(|n| *n > 0))
    } else {
        markets::plan_base_with_cash(
            &state,
            user.user_id.clone(),
            quote.sender_wallet.clone(),
            user.solana_wallet.clone().filter(|w| !w.is_empty()),
            vec![(tx.to.clone(), tx.data.clone())],
            quote.usdc_units,
            &quote.currency,
            rate,
        )
        .await?
    };
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
        summary.push(json!({"label":"Claim fee","value":markets::say_money(cashlinks::CLAIM_FEE_UNITS,&quote.currency,rate)}));
        if let Some(note) = note {
            summary.push(json!({"label":"Note","value":note}));
        }
    }
    if let Some(fee) = fee {
        // The quote's figure when it's the same cost priced moments later, so the totals match.
        let fee = if near(quote.network_units, fee) {
            quote.network_units
        } else {
            fee
        };
        summary.push(
            json!({"label":"Network fee","value":markets::say_money(fee,&quote.currency,rate)}),
        );
        summary.push(json!({"label":"You pay","value":markets::say_money(quote.usdc_units + fee,&quote.currency,rate)}));
    }
    let plan = json!({"intentId":intent_id,"kind":"send","summary":summary,"transactions":transactions,"expiresAtUnixMs":now()+120_000});
    stored.plan = Some(plan.clone());
    Ok(Json(plan))
}
// A bank withdrawal: Daya opens a one-time USDC address on the chain the cash is on (Base, or
// Solana when only Solana holds it), and the phone sends the USDC there. Daya's idempotency key is
// the quote, so a retried execute gets the same address.
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
    let route = bank_route(
        state,
        &quote.sender_wallet,
        quote.sender_solana.as_deref(),
        quote.usdc_units,
    )
    .await;
    let on_solana = route == BankRoute::Solana && quote.sender_solana.is_some();
    let chain = if on_solana { "SOLANA" } else { "BASE" };
    let (funding_account, address, expires) = state
        .daya
        .open_payout(user, payout, chain, &format!("atlas-{quote_id}"))
        .await?;
    let (intent_id, transactions, network_fee) = match (&quote.sender_solana, on_solana) {
        (Some(from), true) => {
            let (intent_id, transactions, network) = markets::plan_solana_transfer(
                state,
                user.user_id.clone(),
                from.clone(),
                &address,
                quote.usdc_units,
                true,
            )
            .await?;
            (intent_id, transactions, Some(network))
        }
        _ => {
            let tx = state
                .markets
                .base
                .transfer_transaction(BASE_USDC, &quote.sender_wallet, &address, quote.usdc_units)
                .map_err(internal)?;
            markets::plan_base_with_cash(
                state,
                user.user_id.clone(),
                quote.sender_wallet.clone(),
                user.solana_wallet.clone().filter(|w| !w.is_empty()),
                vec![(tx.to.clone(), tx.data.clone())],
                quote.usdc_units,
                &quote.currency,
                rate,
            )
            .await?
        }
    };
    // Moving cash between networks, or opening the payout address's USDC account on Solana, costs a
    // little more; "You pay" is everything that leaves the balance, that included. When the cash
    // travels the way the quote expected, it's the quote's figure, so the confirmation shows the
    // very Fee and total the screen did.
    let actual: Option<u128> = match network_fee {
        Some(usdc) => Some(usdc.saturating_mul(app_balance::fx_rate("NGN").await?) / 1_000_000),
        None => None,
    };
    let network_ngn = match (quote.bank_network, actual) {
        (Some((quoted, ngn)), Some(actual)) if quoted == route && near(ngn, actual) => ngn,
        (_, Some(actual)) => actual,
        (Some((quoted, ngn)), None) if quoted == route => ngn,
        (_, None) => 0,
    };
    // One Fee, as on the screen before: the payout partner's and the network's together.
    let mut summary = vec![
        json!({"label":"Send to","value":quote.label}),
        json!({"label":"Bank gets","value":markets::say_micros(get, "NGN")}),
        json!({"label":"Fee","value":markets::say_micros(fee + network_ngn, "NGN")}),
    ];
    summary.extend([
        json!({"label":"You pay","value":markets::say_micros(get + fee + network_ngn, "NGN")}),
        json!({"label":"Rate","value":payout.rate_line()}),
        json!({"label":"Bank payout","value":"Waiting for your USDC"}),
    ]);
    let plan = json!({"intentId":intent_id,"kind":"send","summary":summary,"transactions":transactions,"expiresAtUnixMs":now()+120_000});
    Ok((plan, funding_account, expires))
}
// The same cost priced a few seconds apart (SOL or the naira moved a hair): the quote's figure
// stands. A different cost (another way of paying it) is shown as it is.
fn near(quoted: u128, actual: u128) -> bool {
    actual > 0 && quoted.abs_diff(actual) * 50 <= quoted
}
#[cfg(test)]
mod tests {
    #[test]
    fn a_fee_priced_moments_apart_keeps_the_quoted_figure() {
        assert!(super::near(370_000_000, 371_000_000));
        assert!(!super::near(370_000_000, 410_000_000));
        assert!(!super::near(370_000_000, 0));
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
        .cloned();
    let plan =
        execute_send_inner(State(state.clone()), Path(quote_id), headers, Json(body)).await?;
    let mut receipt = transactions::Receipt::plan(&owner, &plan.0);

    if let Some(q) = quote {
        receipt.usdc_units = Some(q.usdc_units.to_string());
        if q.link.is_some() {
            receipt.kind = "cashlink".into();
        }
        if q.bank.is_some() {
            receipt.kind = "offramp".into();
        }
    }
    receipt.title = transactions::title(&receipt.kind, &receipt.symbol);
    state.history.put(&receipt).await?;
    Ok(plan)
}
