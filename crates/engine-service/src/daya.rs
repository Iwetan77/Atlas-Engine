//! Daya (docs.daya.co): naira in and out through Nigerian banks, production only.
//!
//! Adding money opens a one-time Nigerian bank account; the naira paid into it becomes USDC on the
//! user's Base wallet. Cashing out opens a one-time Daya USDC address that pays naira to a bank
//! account the user named; the phone sends the USDC there. Atlas never pays anyone out of its own
//! Daya balance, so its key needs Business read, trade and write, and never withdraw.
use super::*;
use axum::{body::Bytes, extract::Query};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DAYA_API: &str = "https://api.daya.co";
// Daya pays USDC to the user's wallet on Base: a Base address needs no token account opened first.
const ONRAMP_CHAIN: &str = "BASE";
// The rate locks for about 30 minutes; a new account is only opened on a rate with this much left.
const RATE_MARGIN_MS: u64 = 15 * 60_000;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
// What the last background check of Daya found, for /health (no key, no amounts).
static CHECK: std::sync::LazyLock<Mutex<Value>> =
    std::sync::LazyLock::new(|| Mutex::new(json!({"ok": null})));

#[derive(Clone)]
pub(super) struct DayaState {
    http: reqwest::Client,
    key: Option<String>,
    webhook_secret: Option<String>,
    postgres: Option<Arc<tokio_postgres::Client>>,
    // Without DATABASE_URL, ramps live here (and are lost on restart).
    ramps: Arc<Mutex<HashMap<String, Ramp>>>,
    customers: Arc<Mutex<HashMap<String, (String, String)>>>,
    banks: Arc<Mutex<Option<(Instant, Arc<Vec<Value>>)>>>,
    names: Arc<Mutex<HashMap<String, (Instant, Option<String>)>>>,
    rates: Arc<Mutex<HashMap<&'static str, Rate>>>,
    fees: Arc<Mutex<Option<(Instant, Fees)>>>,
    polled: Arc<Mutex<HashMap<String, Poll>>>,
    updates: Arc<Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
    // Without DATABASE_URL, bank recipients live here: (owner, bank, number) → recipient.
    recipients: Arc<Mutex<HashMap<(String, String, String), Recipient>>>,
}

// A bank account someone has paid or saved: Send to bank lists them as recents and favorites.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    bank_code: String,
    bank_name: String,
    account_number: String,
    account_name: String,
    favorite: bool,
    // 0 when saved but never paid.
    last_used_at_unix_ms: u64,
}

// One Daya funding account Atlas opened for a user, and what became of it.
#[derive(Clone, Serialize, Deserialize)]
struct Ramp {
    funding_account: String,
    owner: String,
    // "onramp" (naira in) or "offramp" (naira out).
    kind: String,
    // The history receipt it updates.
    receipt: String,
    created_ms: u64,
    expires_ms: u64,
    // waiting, received, processing, review, completed, failed, expired.
    status: String,
    #[serde(default)]
    message: Option<String>,
    // Add money: the USDC expected, then what landed. Cash out: the USDC sent.
    #[serde(default)]
    usdc_units: Option<String>,
    // Cash out: the naira the bank was paid.
    #[serde(default)]
    paid_ngn: Option<String>,
    #[serde(default)]
    tx: Option<String>,
    // Once Daya knows the deposit, read it directly instead of scanning the merchant list.
    #[serde(default)]
    deposit_id: Option<String>,
    #[serde(default)]
    provider_updated_ms: u64,
    // Add money: the bank account to pay and the exact amount.
    #[serde(default)]
    account: Value,
}
impl Ramp {
    fn done(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "expired")
    }
}

struct Poll {
    at: Instant,
    busy: bool,
}
// Dropping a canceled or timed-out read releases its slot as well.
struct PollGuard {
    polled: Arc<Mutex<HashMap<String, Poll>>>,
    account: String,
}
impl Drop for PollGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.polled.lock() {
            if let Some(poll) = held.get_mut(&self.account) {
                poll.busy = false;
            }
        }
    }
}

fn customer_matches(customer: &Value, email: &str) -> bool {
    customer["email"]
        .as_str()
        .is_some_and(|actual| actual.trim().eq_ignore_ascii_case(email))
        && customer["id"].as_str().is_some_and(|id| !id.is_empty())
}

#[derive(Clone)]
struct Rate {
    id: String,
    // Naira micros for one USDC (Daya's spread included).
    ngn_per_usdc: u128,
    expires_ms: u64,
    min_ngn: u128,
    fetched: Instant,
}

// Daya's Business fee schedule, all in micros (naira, or dollars for the USDC payout fee).
#[derive(Clone, Copy)]
struct Fees {
    deposit_pct: u128,
    deposit_cap: u128,
    payout_pct: u128,
    payout_cap: u128,
    low_payout_fee: u128,
    low_payout_below: u128,
    payout_min: u128,
    usdc_payout_fee: u128,
}
impl Fees {
    // The naira deposit charge, taken before conversion.
    fn deposit(&self, ngn: u128) -> u128 {
        (ngn * self.deposit_pct / 100_000_000).min(self.deposit_cap)
    }
    // The bank payout charge for a recipient amount.
    fn payout(&self, ngn: u128) -> u128 {
        if ngn < self.low_payout_below {
            self.low_payout_fee
        } else {
            (ngn * self.payout_pct / 100_000_000).min(self.payout_cap)
        }
    }
}

// A failed Daya call: the HTTP status (0 when Daya never answered), its code and message.
struct Failure {
    status: u16,
    code: String,
    message: String,
}
impl Failure {
    fn api(self) -> ApiError {
        let code = self.code.to_ascii_lowercase();
        if code == "not_configured" {
            return not_ready();
        }
        match self.status {
            // Daya pauses an app it hasn't switched on yet (APP_PAUSED).
            _ if code.contains("paused") => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Bank transfers are paused right now. Try again later.".into(),
            ),
            0 | 500.. => busy(),
            401 => broken(),
            403 if code.contains("scope") => broken(),
            403 => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Bank transfers are paused right now. Try again later.".into(),
            ),
            409 => (StatusCode::CONFLICT, self.message),
            _ => (StatusCode::BAD_REQUEST, self.message),
        }
    }
}
fn busy() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Bank transfers are busy right now. Try again in a minute.".into(),
    )
}
fn broken() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Bank transfers aren't working right now. Try again later.".into(),
    )
}
fn not_ready() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Bank transfers aren't available yet.".into(),
    )
}
fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("daya: {error}");
    busy()
}
fn bad(message: &str) -> ApiError {
    (StatusCode::BAD_REQUEST, message.into())
}

impl DayaState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let secret = |var: &str| {
            env::var(var)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let mut state = Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("atlas-engine")
                .build()?,
            key: secret("DAYA_API_KEY"),
            webhook_secret: secret("DAYA_WEBHOOK_SECRET"),
            postgres: None,
            ramps: Default::default(),
            customers: Default::default(),
            banks: Default::default(),
            names: Default::default(),
            rates: Default::default(),
            fees: Default::default(),
            polled: Default::default(),
            updates: Default::default(),
            recipients: Default::default(),
        };
        if let Ok(url) = env::var("DATABASE_URL") {
            let (pg, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("daya database connection ended");
                }
            });
            pg.batch_execute(
                "CREATE TABLE IF NOT EXISTS atlas_daya_ramps (funding_account TEXT PRIMARY KEY,
                    receipt TEXT NOT NULL, owner TEXT NOT NULL, payload TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS atlas_daya_ramps_receipt ON atlas_daya_ramps(receipt);
                CREATE TABLE IF NOT EXISTS atlas_bank_recipients (owner TEXT NOT NULL,
                    bank_code TEXT NOT NULL, account_number TEXT NOT NULL, payload TEXT NOT NULL,
                    PRIMARY KEY (owner, bank_code, account_number))",
            )
            .await?;
            state.postgres = Some(Arc::new(pg));
        }
        Ok(state)
    }

    fn update_lock(&self, account: &str) -> Result<Arc<tokio::sync::Mutex<()>>, ApiError> {
        let mut held = self.updates.lock().map_err(internal)?;
        held.retain(|_, lock| lock.strong_count() > 0);
        let lock = held
            .get(account)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        held.insert(account.into(), Arc::downgrade(&lock));
        Ok(lock)
    }
    fn start_poll(&self, account: &str, focused: bool) -> Result<Option<PollGuard>, ApiError> {
        let interval = Duration::from_secs(if focused { 3 } else { 20 });
        let mut held = self.polled.lock().map_err(internal)?;
        held.retain(|_, poll| poll.busy || poll.at.elapsed() < Duration::from_secs(30));
        if held
            .get(account)
            .is_some_and(|poll| poll.busy || poll.at.elapsed() < interval)
        {
            return Ok(None);
        }
        held.insert(
            account.into(),
            Poll {
                at: Instant::now(),
                busy: true,
            },
        );
        Ok(Some(PollGuard {
            polled: self.polled.clone(),
            account: account.into(),
        }))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
        body: Option<&Value>,
        idempotency: Option<&str>,
    ) -> Result<Value, Failure> {
        let Some(key) = self.key.as_deref() else {
            return Err(Failure {
                status: 503,
                code: "not_configured".into(),
                message: "DAYA_API_KEY is not set".into(),
            });
        };
        // Logged by path only: a query can hold a customer's email.
        let path = url.path().to_owned();
        let mut request = self.http.request(method, url).header("X-Api-Key", key);
        if let Some(key) = idempotency {
            request = request.header("X-Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let failure = match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                if (200..300).contains(&status) {
                    return Ok(body);
                }
                let error = if body["error"].is_object() {
                    &body["error"]
                } else {
                    &body
                };
                Failure {
                    status,
                    code: error["code"].as_str().unwrap_or("").into(),
                    message: error["message"]
                        .as_str()
                        .unwrap_or("The bank transfer was refused")
                        .into(),
                }
            }
            Err(error) => Failure {
                status: 0,
                code: "network".into(),
                message: error.without_url().to_string(),
            },
        };
        eprintln!(
            "daya {path}: {} {} {}",
            failure.status, failure.code, failure.message
        );
        Err(failure)
    }
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, Failure> {
        self.call(reqwest::Method::GET, url(path, query), None, None)
            .await
    }

    // Supported Nigerian banks, by name, shared for six hours.
    pub(super) async fn banks(&self) -> Result<Arc<Vec<Value>>, ApiError> {
        if let Some((at, banks)) = self.banks.lock().map_err(internal)?.clone() {
            if at.elapsed() < Duration::from_secs(6 * 3600) {
                return Ok(banks);
            }
        }
        let body = self.get("/v1/banks", &[]).await.map_err(Failure::api)?;
        let mut banks: Vec<Value> = data(&body)
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| Some(json!({"code": b["code"].as_str()?, "name": b["name"].as_str()?})))
            .collect();
        if banks.is_empty() {
            return Err(busy());
        }
        // Daya lists no logos; nigerianbanks.xyz (free, no key) has them, matched by code then name.
        let logos = self.bank_logos().await;
        for bank in &mut banks {
            let code = bank["code"].as_str().unwrap_or("").to_owned();
            let name = bank_key(bank["name"].as_str().unwrap_or(""));
            if let Some(logo) = logo_for(&logos, &code, &name) {
                bank["logo"] = json!(logo);
            }
        }
        banks.sort_by_key(|b| b["name"].as_str().unwrap_or("").to_ascii_lowercase());
        let banks = Arc::new(banks);
        *self.banks.lock().map_err(internal)? = Some((Instant::now(), banks.clone()));
        Ok(banks)
    }

    // (code, name key, logo URL) for Nigerian banks; empty when the list can't be read.
    async fn bank_logos(&self) -> Vec<(String, String, String)> {
        let list: Option<Vec<Value>> = async {
            self.http
                .get("https://nigerianbanks.xyz")
                .timeout(Duration::from_secs(6))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()
        }
        .await;
        list.into_iter()
            .flatten()
            .filter_map(|b| {
                let logo = b["logo"].as_str().filter(|l| l.starts_with("https://"))?;
                Some((
                    b["code"].as_str().unwrap_or("").to_owned(),
                    bank_key(b["name"].as_str().unwrap_or("")),
                    logo.to_owned(),
                ))
            })
            .collect()
    }

    // The account holder's name, or None when the bank has no such account. Shared for ten minutes.
    pub(super) async fn account_name(
        &self,
        bank_code: &str,
        account_number: &str,
    ) -> Result<Option<String>, ApiError> {
        if bank_code.is_empty()
            || bank_code.len() > 12
            || !bank_code.bytes().all(|b| b.is_ascii_alphanumeric())
            || account_number.len() != 10
            || !account_number.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(bad("invalid Nigerian bank details"));
        }
        let key = format!("{bank_code}:{account_number}");
        if let Some((at, name)) = self.names.lock().map_err(internal)?.get(&key) {
            if at.elapsed() < Duration::from_secs(600) {
                return Ok(name.clone());
            }
        }
        let body = json!({"account_number": account_number, "bank_code": bank_code});
        let name = match self
            .call(
                reqwest::Method::POST,
                url("/v1/banks/resolve", &[]),
                Some(&body),
                None,
            )
            .await
        {
            Ok(found) => data(&found)["account_name"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
            // "Could not resolve account number", "Invalid bank code": no such account.
            Err(f) if f.status == 400 || f.status == 404 || f.status == 422 => None,
            Err(f) => return Err(f.api()),
        };
        self.names
            .lock()
            .map_err(internal)?
            .insert(key, (Instant::now(), name.clone()));
        Ok(name)
    }

    // Daya's firm NGN/USDC rate for a side (BUY: naira in, SELL: naira out), shared for a minute.
    async fn rate(&self, side: &'static str) -> Result<Rate, ApiError> {
        if let Some(rate) = self.rates.lock().map_err(internal)?.get(side) {
            if rate.fetched.elapsed() < Duration::from_secs(60)
                && rate.expires_ms > now_ms() + RATE_MARGIN_MS
            {
                return Ok(rate.clone());
            }
        }
        let body = self
            .get(
                "/v1/rates",
                &[("from", "NGN"), ("to", "USDC"), ("side", side)],
            )
            .await
            .map_err(Failure::api)?;
        let v = data(&body);
        let rate = Rate {
            id: v["rate_id"].as_str().unwrap_or("").into(),
            ngn_per_usdc: micros_of(&v["rate"]).unwrap_or(0),
            expires_ms: v["expires_at"].as_str().and_then(parse_time).unwrap_or(0),
            min_ngn: micros_of(&v["min_deposit_ngn"]).unwrap_or(0),
            fetched: Instant::now(),
        };
        if rate.id.is_empty() || rate.ngn_per_usdc == 0 || rate.expires_ms <= now_ms() + 60_000 {
            eprintln!("daya /v1/rates: unusable {side} rate");
            return Err(busy());
        }
        self.rates
            .lock()
            .map_err(internal)?
            .insert(side, rate.clone());
        Ok(rate)
    }

    // The current fee schedule, shared for an hour; the last good one stands while Daya is down.
    async fn fees(&self) -> Result<Fees, ApiError> {
        let held = *self.fees.lock().map_err(internal)?;
        if let Some((at, fees)) = held {
            if at.elapsed() < Duration::from_secs(3600) {
                return Ok(fees);
            }
        }
        let body = match self.get("/v1/fees", &[]).await {
            Ok(body) => body,
            Err(f) => return held.map(|(_, fees)| fees).ok_or_else(|| f.api()),
        };
        let v = data(&body);
        let ngn = &v["ngn"];
        let fees = (|| {
            Some(Fees {
                deposit_pct: micros_of(&ngn["deposit_fee_percent"])?,
                deposit_cap: micros_of(&ngn["deposit_fee_cap"]["amount"])?,
                payout_pct: micros_of(&ngn["transfer_fee_percent"])?,
                payout_cap: micros_of(&ngn["transfer_fee_cap"]["amount"])?,
                low_payout_fee: micros_of(&ngn["low_value_transfer_fee"]["amount"])?,
                low_payout_below: micros_of(&ngn["low_value_transfer_threshold"]["amount"])?,
                payout_min: micros_of(&ngn["minimum_transfer_amount"]["amount"])?,
                usdc_payout_fee: micros_of(&v["crypto_withdrawals"]["default_fee"]["amount"])?,
            })
        })()
        .ok_or_else(|| internal("unreadable fee schedule"))?;
        *self.fees.lock().map_err(internal)? = Some((Instant::now(), fees));
        Ok(fees)
    }

    // The user's Daya customer, found by email or made once. Atlas signs in with Google or email,
    // so every user has one.
    async fn customer(&self, user: &app_balance::VerifiedWallets) -> Result<String, ApiError> {
        let email = user
            .email
            .as_deref()
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| e.contains('@'))
            .ok_or((
                StatusCode::CONFLICT,
                "Bank transfers need an email on your account.".into(),
            ))?;
        if let Some((held_email, id)) = self.customers.lock().map_err(internal)?.get(&user.user_id)
        {
            if held_email == &email {
                return Ok(id.clone());
            }
        }
        let find = || async {
            let found = self
                .get("/v1/customers", &[("email", email.as_str())])
                .await
                .map_err(Failure::api)?;
            Ok::<_, ApiError>(
                data(&found)
                    .as_array()
                    .and_then(|list| list.iter().find(|c| customer_matches(c, &email)))
                    .and_then(|c| c["id"].as_str())
                    .map(str::to_owned),
            )
        };
        let id = match find().await? {
            Some(id) => id,
            None => {
                let (first, last) = names(user.name.as_deref());
                let mut body = json!({"email": email});
                if let Some(first) = first {
                    body["first_name"] = json!(first);
                }
                if let Some(last) = last {
                    body["last_name"] = json!(last);
                }
                match self
                    .call(
                        reqwest::Method::POST,
                        url("/v1/customers", &[]),
                        Some(&body),
                        None,
                    )
                    .await
                {
                    Ok(made) => {
                        let made = data(&made);
                        if !customer_matches(made, &email) {
                            return Err(internal("created customer email did not match its owner"));
                        }
                        made["id"].as_str().map(str::to_owned)
                    }
                    // Made by a request that raced this one.
                    Err(f) if f.status == 409 => find().await?,
                    Err(f) => return Err(f.api()),
                }
                .ok_or_else(|| internal("customer without an id"))?
            }
        };
        self.customers
            .lock()
            .map_err(internal)?
            .insert(user.user_id.clone(), (email, id.clone()));
        Ok(id)
    }

    // Opens a funding account and waits briefly for its payment details (a bank account or a USDC
    // address). The idempotency key makes a retried request return the same account.
    async fn open_account(
        &self,
        body: &Value,
        idempotency: &str,
        rail: &str,
    ) -> Result<(Value, Value), ApiError> {
        let started = Instant::now();
        let made = self
            .call(
                reqwest::Method::POST,
                url("/v1/funding-accounts", &[]),
                Some(body),
                Some(idempotency),
            )
            .await
            .map_err(Failure::api)?;
        let mut account = data(&made).clone();
        let id = account["id"]
            .as_str()
            .ok_or_else(|| internal("funding account without an id"))?
            .to_owned();
        for attempt in 0..8 {
            if let Some(details) = ready_instruction(&account, rail) {
                // How long the user waited on Daya for payment details after confirming.
                eprintln!(
                    "daya funding account {id}: {rail} ready in {} ms ({attempt} re-checks)",
                    started.elapsed().as_millis()
                );
                return Ok((account.clone(), details.clone()));
            }
            if matches!(account["status"].as_str(), Some("FAILED" | "DISABLED")) {
                break;
            }
            if attempt > 0 || account["instructions"].is_null() {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            account = match self.get(&format!("/v1/funding-accounts/{id}"), &[]).await {
                Ok(fresh) => data(&fresh).clone(),
                Err(_) => continue,
            };
        }
        let note = account["instructions"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|i| i["provider_availability"]["message"].as_str());
        eprintln!(
            "daya funding account {id}: no payment details ({:?})",
            account["status"]
        );
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            note.unwrap_or("The account couldn't be opened just now. Try again in a minute.")
                .into(),
        ))
    }

    // The bank, holder name and label for a bank withdrawal; None when there is no such account.
    pub(super) async fn payout_to(
        &self,
        bank_code: &str,
        account_number: &str,
    ) -> Result<Payout, ApiError> {
        if self.key.is_none() {
            return Err(not_ready());
        }
        let (name, banks) =
            tokio::try_join!(self.account_name(bank_code, account_number), self.banks())?;
        let name = name.ok_or((StatusCode::NOT_FOUND, "no such bank account".into()))?;
        let bank_name = banks
            .iter()
            .find(|b| b["code"] == bank_code)
            .and_then(|b| b["name"].as_str())
            .unwrap_or("Bank")
            .to_owned();
        Ok(Payout {
            label: format!("{bank_name} · {account_number} · {name}"),
            bank_code: bank_code.into(),
            account_number: account_number.into(),
            account_name: name,
            bank_name,
            ngn_per_usdc: 0,
        })
    }

    // What a bank payout of `ngn` costs: (USDC to send, Daya's payout fee), the fee in naira micros.
    // The fee goes on top, so the bank gets exactly `ngn` at Daya's current rate.
    pub(super) async fn payout_cost(
        &self,
        payout: &mut Payout,
        ngn: u128,
    ) -> Result<(u128, u128), ApiError> {
        let (rate, fees) = tokio::try_join!(self.rate("SELL"), self.fees())?;
        payout.ngn_per_usdc = rate.ngn_per_usdc;
        let least = fees.payout_min.max(rate.min_ngn);
        if ngn < least {
            return Err(bad(&format!(
                "The smallest bank withdrawal is {}",
                markets::say_micros(least.div_ceil(1_000_000) * 1_000_000, "NGN")
            )));
        }
        let fee = fees.payout(ngn);
        let usdc = payout_usdc(ngn + fee, rate.ngn_per_usdc);
        Ok((usdc, fee))
    }

    // Opens the one-time USDC address that pays the bank, on the chain the cash is sent from.
    // Refused when Daya's rate dropped at all since the quote, so the bank gets exactly what it showed.
    pub(super) async fn open_payout(
        &self,
        user: &app_balance::VerifiedWallets,
        payout: &Payout,
        chain: &str,
        idempotency: &str,
    ) -> Result<(String, String, u64), ApiError> {
        let (rate, customer) = tokio::try_join!(self.rate("SELL"), self.customer(user))?;
        if rate.ngn_per_usdc < payout.ngn_per_usdc {
            return Err((
                StatusCode::CONFLICT,
                "The naira rate just changed. Check the new amount and try again.".into(),
            ));
        }
        let body = json!({
            "type": "TEMPORARY",
            "rail": "CRYPTO_ADDRESS",
            "customer": {"customer_id": customer},
            "asset": "USDC",
            "chain": chain,
            "settlement_destination": {
                "type": "NGN_PAYOUT",
                "rate_id": rate.id,
                "destination_bank": {"account_number": payout.account_number, "bank_code": payout.bank_code},
            },
        });
        let (account, details) = self
            .open_account(&body, idempotency, "CRYPTO_ADDRESS")
            .await?;
        let address = details["address"].as_str().unwrap_or("");
        let bank = &account["settlement_destination"]["destination_bank"];
        let sound = details["chain"].as_str().is_none_or(|c| c == chain)
            && account["asset"].as_str().is_none_or(|a| a == "USDC")
            && account["customer_id"].as_str() == Some(customer.as_str())
            && bank["account_number"]
                .as_str()
                .is_none_or(|n| n == payout.account_number)
            && match chain {
                "SOLANA" => markets::looks_like_mint(address),
                _ => markets::looks_like_evm_address(address),
            };
        if !sound {
            eprintln!(
                "daya funding account {}: payout details don't match the request",
                account["id"]
            );
            return Err(broken());
        }
        let expires = account["expires_at"]
            .as_str()
            .and_then(parse_time)
            .unwrap_or(rate.expires_ms);
        Ok((
            account["id"].as_str().unwrap_or("").into(),
            address.into(),
            expires,
        ))
    }

    // Remembers a cash-out so Daya's payout updates reach its receipt.
    pub(super) async fn record_payout(
        &self,
        owner: &str,
        funding_account: &str,
        receipt: &str,
        usdc_units: u128,
        expires_ms: u64,
    ) -> Result<(), ApiError> {
        self.keep(&Ramp {
            funding_account: funding_account.into(),
            owner: owner.into(),
            kind: "offramp".into(),
            receipt: receipt.into(),
            created_ms: now_ms(),
            expires_ms,
            status: "waiting".into(),
            message: None,
            usdc_units: Some(usdc_units.to_string()),
            paid_ngn: None,
            tx: None,
            deposit_id: None,
            provider_updated_ms: 0,
            account: Value::Null,
        })
        .await
    }

    // The banks this account number could be at, with the holder's name at each where it exists.
    // A number's last digit is a check (NUBAN) against the bank's code, so it fits only a few of the
    // popular banks; OPay, PalmPay and Moniepoint also use phone numbers as account numbers. Every
    // candidate is confirmed with Daya, so only real accounts come back.
    pub(super) async fn guess_banks(&self, account_number: &str) -> Result<Vec<Value>, ApiError> {
        if account_number.len() != 10 || !account_number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad("invalid Nigerian bank details"));
        }
        let banks = self.banks().await?;
        let mut candidates: Vec<&Value> = Vec::new();
        if phone_like(account_number) {
            candidates.extend(banks.iter().filter(|b| {
                let key = bank_key(b["name"].as_str().unwrap_or(""));
                PHONE_BANKS.iter().any(|p| key.split(' ').any(|w| w == *p))
            }));
        }
        candidates.extend(banks.iter().filter(|b| {
            let key = bank_key(b["name"].as_str().unwrap_or(""));
            POPULAR_BANKS.iter().any(|p| key.contains(p))
                && institution(b["code"].as_str().unwrap_or(""))
                    .is_some_and(|code| nuban_fits(&code, account_number))
        }));
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|b| seen.insert(b["code"].as_str().unwrap_or("").to_owned()));
        candidates.truncate(8);
        let mut lookups = tokio::task::JoinSet::new();
        for (rank, bank) in candidates.into_iter().enumerate() {
            let (daya, bank, number) = (self.clone(), bank.clone(), account_number.to_owned());
            lookups.spawn(async move {
                let code = bank["code"].as_str().unwrap_or("").to_owned();
                let name = daya.account_name(&code, &number).await.ok().flatten();
                (rank, bank, name)
            });
        }
        let mut found = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(done) = lookups.join_next().await {
                if let Ok((rank, mut bank, Some(name))) = done {
                    bank["accountName"] = json!(name);
                    found.push((rank, bank));
                }
            }
        })
        .await;
        found.sort_by_key(|(rank, _)| *rank);
        Ok(found.into_iter().map(|(_, bank)| bank).collect())
    }

    // The user's bank recipients, most recently paid first (favorites never fall off).
    pub(super) async fn recipients(&self, owner: &str) -> Result<Vec<Value>, ApiError> {
        let mut list: Vec<Recipient> = if let Some(pg) = &self.postgres {
            pg.query(
                "SELECT payload FROM atlas_bank_recipients WHERE owner=$1",
                &[&owner],
            )
            .await
            .map_err(internal)?
            .iter()
            .filter_map(|row| serde_json::from_str(row.get::<_, &str>(0)).ok())
            .collect()
        } else {
            self.recipients
                .lock()
                .map_err(internal)?
                .iter()
                .filter(|((o, _, _), _)| o == owner)
                .map(|(_, r)| r.clone())
                .collect()
        };
        list.sort_by(|a, b| b.last_used_at_unix_ms.cmp(&a.last_used_at_unix_ms));
        list.retain(|r| r.favorite || r.last_used_at_unix_ms > 0);
        list.truncate(50);
        let logos = self.banks().await.unwrap_or_default();
        Ok(list
            .into_iter()
            .map(|r| {
                let mut value = json!(r);
                value["logo"] = logos
                    .iter()
                    .find(|b| b["code"] == r.bank_code)
                    .map(|b| b["logo"].clone())
                    .unwrap_or(Value::Null);
                value
            })
            .collect())
    }
    async fn recipient(
        &self,
        owner: &str,
        bank_code: &str,
        account_number: &str,
    ) -> Result<Option<Recipient>, ApiError> {
        if let Some(pg) = &self.postgres {
            return pg
                .query_opt(
                    "SELECT payload FROM atlas_bank_recipients WHERE owner=$1 AND bank_code=$2 AND account_number=$3",
                    &[&owner, &bank_code, &account_number],
                )
                .await
                .map_err(internal)?
                .map(|row| serde_json::from_str(row.get::<_, &str>(0)).map_err(internal))
                .transpose();
        }
        Ok(self
            .recipients
            .lock()
            .map_err(internal)?
            .get(&(owner.into(), bank_code.into(), account_number.into()))
            .cloned())
    }
    async fn keep_recipient(&self, owner: &str, r: &Recipient) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_bank_recipients(owner,bank_code,account_number,payload) VALUES($1,$2,$3,$4)
                ON CONFLICT(owner,bank_code,account_number) DO UPDATE SET payload=$4",
                &[&owner, &r.bank_code, &r.account_number, &serde_json::to_string(r).map_err(internal)?],
            )
            .await
            .map_err(internal)?;
        } else {
            self.recipients.lock().map_err(internal)?.insert(
                (owner.into(), r.bank_code.clone(), r.account_number.clone()),
                r.clone(),
            );
        }
        Ok(())
    }
    // A bank account was just paid: it goes to the top of recents (a favorite stays one).
    pub(super) async fn paid(&self, owner: &str, payout: &Payout) -> Result<(), ApiError> {
        let favorite = self
            .recipient(owner, &payout.bank_code, &payout.account_number)
            .await?
            .is_some_and(|r| r.favorite);
        self.keep_recipient(
            owner,
            &Recipient {
                bank_code: payout.bank_code.clone(),
                bank_name: payout.bank_name.clone(),
                account_number: payout.account_number.clone(),
                account_name: payout.account_name.clone(),
                favorite,
                last_used_at_unix_ms: now_ms(),
            },
        )
        .await
    }
    // Saves or unsaves a bank account as a favorite (checked with the bank first if it's new).
    async fn set_favorite(
        &self,
        owner: &str,
        bank_code: &str,
        account_number: &str,
        favorite: bool,
    ) -> Result<(), ApiError> {
        let mut r = match self.recipient(owner, bank_code, account_number).await? {
            Some(r) => r,
            None => {
                let payout = self.payout_to(bank_code, account_number).await?;
                Recipient {
                    bank_code: payout.bank_code,
                    bank_name: payout.bank_name,
                    account_number: payout.account_number,
                    account_name: payout.account_name,
                    favorite,
                    last_used_at_unix_ms: 0,
                }
            }
        };
        r.favorite = favorite;
        self.keep_recipient(owner, &r).await
    }

    async fn keep(&self, ramp: &Ramp) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_daya_ramps(funding_account,receipt,owner,payload) VALUES($1,$2,$3,$4)
                ON CONFLICT(funding_account) DO UPDATE SET payload=$4 WHERE atlas_daya_ramps.owner=$3",
                &[
                    &ramp.funding_account,
                    &ramp.receipt,
                    &ramp.owner,
                    &serde_json::to_string(ramp).map_err(internal)?,
                ],
            )
            .await
            .map_err(internal)?;
        } else {
            self.ramps
                .lock()
                .map_err(internal)?
                .insert(ramp.funding_account.clone(), ramp.clone());
        }
        Ok(())
    }
    async fn ramp(&self, column: &str, value: &str) -> Result<Option<Ramp>, ApiError> {
        if let Some(pg) = &self.postgres {
            let sql = if column == "receipt" {
                "SELECT payload FROM atlas_daya_ramps WHERE receipt=$1"
            } else {
                "SELECT payload FROM atlas_daya_ramps WHERE funding_account=$1"
            };
            return pg
                .query_opt(sql, &[&value])
                .await
                .map_err(internal)?
                .map(|row| serde_json::from_str(row.get::<_, &str>(0)).map_err(internal))
                .transpose();
        }
        Ok(self
            .ramps
            .lock()
            .map_err(internal)?
            .values()
            .find(|r| {
                if column == "receipt" {
                    r.receipt == value
                } else {
                    r.funding_account == value
                }
            })
            .cloned())
    }
}

// A bank withdrawal's destination, and the rate its quote showed.
#[derive(Clone)]
pub(super) struct Payout {
    pub(super) label: String,
    bank_code: String,
    account_number: String,
    account_name: String,
    bank_name: String,
    ngn_per_usdc: u128,
}
// Webhooks and polls share a lock so an older observation cannot race a completion.
async fn apply(state: &AppState, deposit: &Value) -> Result<(), ApiError> {
    if let Some(ramp) = apply_deposit(&state.daya, &state.history, deposit).await? {
        emails::ramp_changed(state, &ramp.owner, &ramp.receipt, &ramp.kind, &ramp.status);
    }
    Ok(())
}
async fn apply_deposit(
    daya: &DayaState,
    history: &transactions::HistoryStore,
    deposit: &Value,
) -> Result<Option<Ramp>, ApiError> {
    let Some(fa) = deposit["funding_account_id"].as_str() else {
        return Ok(None);
    };
    let lock = daya.update_lock(fa)?;
    let _guard = lock.lock().await;
    let Some(mut ramp) = daya.ramp("funding_account", fa).await? else {
        return Ok(None);
    };
    let previous = ramp.status.clone();
    if !merge_deposit(&mut ramp, deposit) {
        // If saving the ramp succeeded but its receipt failed, a duplicate repairs that receipt.
        mirror_receipt(history, &ramp).await?;
        return Ok(None);
    }
    // Keep the provider observation before acknowledging it; a retry also repairs its receipt.
    daya.keep(&ramp).await?;
    mirror_receipt(history, &ramp).await?;
    if previous != ramp.status {
        eprintln!(
            "daya receipt {}: {} -> {} at {} ({} ms since account opened, provider update {})",
            ramp.receipt,
            previous,
            ramp.status,
            now_ms(),
            now_ms().saturating_sub(ramp.created_ms),
            ramp.provider_updated_ms
        );
    }
    Ok(Some(ramp))
}
fn merge_deposit(ramp: &mut Ramp, deposit: &Value) -> bool {
    let status = deposit["status"]
        .as_str()
        .unwrap_or("")
        .to_ascii_uppercase();
    let settlement = deposit["settlement_status"]
        .as_str()
        .unwrap_or("")
        .to_ascii_uppercase();
    let status = if status == "REVERSED" || settlement == "REVERSED" {
        "REVERSED"
    } else if status == "FAILED" || settlement == "FAILED" {
        "FAILED"
    } else if matches!(status.as_str(), "REQUIRES_REVIEW" | "FLAGGED")
        || matches!(settlement.as_str(), "REQUIRES_REVIEW" | "FLAGGED")
    {
        "REQUIRES_REVIEW"
    } else if matches!(status.as_str(), "COMPLETED" | "SETTLED") {
        // A received crypto deposit is not proof that its bank settlement has finished.
        if settlement.is_empty() || matches!(settlement.as_str(), "COMPLETED" | "SETTLED") {
            "COMPLETED"
        } else {
            "PROCESSING"
        }
    } else {
        status.as_str()
    };
    let (next, message) = match status {
        "PENDING" | "RECEIVED" => ("received", None),
        "PROCESSING" => ("processing", None),
        "COMPLETED" | "SETTLED" => ("completed", None),
        "REQUIRES_REVIEW" | "FLAGGED" => (
            "review",
            Some(match deposit["flag_code"].as_str() {
                Some("late_deposit") => "The money arrived after the account expired, so it's being reviewed. Contact support if it isn't sorted within a day.",
                _ => "This payment is being checked. It usually clears; contact support if it takes more than a day.",
            }),
        ),
        "FAILED" => (
            "failed",
            Some(if ramp.kind == "onramp" {
                "The transfer didn't go through. If money left your bank, it comes back to you."
            } else {
                "The bank payout failed. Contact support with this receipt."
            }),
        ),
        "REVERSED" => ("failed", Some("This payment was reversed. Contact support.")),
        _ => return false,
    };
    let updated = deposit["updated_at"]
        .as_str()
        .and_then(parse_time)
        .unwrap_or(0);
    if updated > 0 && updated < ramp.provider_updated_ms {
        return false;
    }
    let newer = updated > ramp.provider_updated_ms;
    // A completed payment can only be superseded by an actual reversal. Failed payments stay failed.
    if ramp.status == "failed" || (ramp.status == "completed" && status != "REVERSED") {
        return false;
    }
    // Missing/equal provider timestamps cannot move an in-flight payment backwards.
    let rank = |status: &str| match status {
        "received" => 1,
        "processing" => 2,
        "review" => 3,
        "completed" | "failed" => 4,
        _ => 0,
    };
    if !newer && rank(next) < rank(&ramp.status) {
        return false;
    }
    if let Some(id) = deposit["id"].as_str().filter(|id| !id.is_empty()) {
        if ramp.deposit_id.as_deref().is_some_and(|known| known != id) {
            return false;
        }
        ramp.deposit_id = Some(id.into());
    }
    ramp.provider_updated_ms = ramp.provider_updated_ms.max(updated);
    ramp.status = next.into();
    ramp.message = message.map(str::to_owned);
    if let Some(tx) = deposit["tx_hash"].as_str().filter(|t| !t.is_empty()) {
        ramp.tx = Some(tx.into());
    }
    let customer_amount = &deposit["customer_amount"];
    if next == "completed" {
        if ramp.kind == "onramp" {
            let landed = if customer_amount["currency"] == "USDC" {
                micros_of(&customer_amount["amount"])
            } else if deposit["settled_currency"] == "USDC" {
                micros_of(&deposit["settled_amount"])
            } else {
                None
            };
            if let Some(units) = landed {
                ramp.usdc_units = Some(units.to_string());
            }
        } else if customer_amount["currency"] == "NGN" {
            ramp.paid_ngn = micros_of(&customer_amount["amount"]).map(|n| n.to_string());
        }
    }
    true
}

// Mirrors a ramp's state on its history receipt.
async fn update_receipt(state: &AppState, ramp: &Ramp) -> Result<(), ApiError> {
    mirror_receipt(&state.history, ramp).await?;
    emails::ramp_changed(state, &ramp.owner, &ramp.receipt, &ramp.kind, &ramp.status);
    Ok(())
}
async fn mirror_receipt(history: &transactions::HistoryStore, ramp: &Ramp) -> Result<(), ApiError> {
    let Some(mut receipt) = history.find(&ramp.owner, &ramp.receipt).await? else {
        return Ok(());
    };
    if ramp.kind == "onramp" {
        let (st, stage) = match ramp.status.as_str() {
            "completed" => ("filled", "settle"),
            "failed" | "expired" => ("failed", "settle"),
            "waiting" => ("pending", "waiting"),
            _ => ("pending", "settle"),
        };
        receipt.set_state(st, stage, ramp.message.clone());
        receipt.set_line(
            "Status",
            match ramp.status.as_str() {
                "waiting" => "Waiting for your transfer",
                "received" => "Naira received",
                "processing" => "Sending USDC to your wallet",
                "review" => "Being checked",
                "completed" => "Added to your balance",
                "expired" => "Expired",
                _ => "Failed",
            }
            .into(),
        );
        if let Some(units) = ramp.usdc_units.as_deref().and_then(|u| u.parse().ok()) {
            receipt.set_usdc(units);
        }
    } else {
        let line = match ramp.status.as_str() {
            "waiting" => "Waiting for your USDC".into(),
            "received" | "processing" => "Paying your bank".into(),
            "review" => "Being checked".into(),
            "completed" => match ramp.paid_ngn.as_deref().and_then(|n| n.parse().ok()) {
                Some(ngn) => format!("Paid {}", markets::say_micros(ngn, "NGN")),
                None => "Paid".into(),
            },
            _ => "Failed — contact support".into(),
        };
        receipt.set_line("Bank payout", line);
        // The provider's deposit/payout verdict is stronger than a delayed source intent.
        // Waiting preserves an unsigned draft or a source submission error.
        match ramp.status.as_str() {
            "completed" => receipt.set_state("filled", "settle", None),
            "failed" | "expired" => receipt.set_state("failed", "settle", ramp.message.clone()),
            "received" | "processing" | "review" => {
                receipt.set_state("pending", "settle", ramp.message.clone())
            }
            _ => {}
        }
    }
    if let Some(tx) = &ramp.tx {
        receipt.add_tx(tx.clone());
    }
    history.put(&receipt).await?;
    Ok(())
}

// A visible receipt checks at most every three seconds; background history every twenty.
// Concurrent readers share one provider read, and a known deposit is retrieved by its ID.
pub(super) async fn observe(state: &AppState, owner: &str, receipt: &str) -> Result<(), ApiError> {
    observe_inner(state, owner, receipt, false).await
}
pub(super) async fn observe_focused(
    state: &AppState,
    owner: &str,
    receipt: &str,
) -> Result<(), ApiError> {
    observe_inner(state, owner, receipt, true).await
}
async fn observe_inner(
    state: &AppState,
    owner: &str,
    receipt: &str,
    focused: bool,
) -> Result<(), ApiError> {
    let Some(mut ramp) = state.daya.ramp("receipt", receipt).await? else {
        return Ok(());
    };
    if ramp.owner != owner || matches!(ramp.status.as_str(), "completed" | "failed") {
        return Ok(());
    }
    let Some(_poll) = state.daya.start_poll(&ramp.funding_account, focused)? else {
        return Ok(());
    };
    if let Some(id) = &ramp.deposit_id {
        let fresh = state
            .daya
            .get(&format!("/v1/deposits/{id}"), &[])
            .await
            .map_err(Failure::api)?;
        let deposit = data(&fresh);
        if deposit["funding_account_id"].as_str() == Some(ramp.funding_account.as_str()) {
            return apply(state, deposit).await;
        }
        return Err(internal("deposit did not match its funding account"));
    }
    let kind = if ramp.kind == "onramp" {
        "NGN_DEPOSIT"
    } else {
        "CRYPTO_DEPOSIT"
    };
    let since = format_time(ramp.created_ms.saturating_sub(5 * 60_000));
    let mut scanned_all = false;
    for page in 1..=10 {
        let page_text = page.to_string();
        let found = state
            .daya
            .get(
                "/v1/deposits",
                &[
                    ("type", kind),
                    ("from", since.as_str()),
                    ("limit", "200"),
                    ("page", page_text.as_str()),
                ],
            )
            .await
            .map_err(Failure::api)?;
        let rows = data(&found)
            .as_array()
            .ok_or_else(|| internal("unreadable deposit list"))?;
        if let Some(deposit) = rows
            .iter()
            .filter(|d| d["funding_account_id"].as_str() == Some(ramp.funding_account.as_str()))
            .max_by_key(|d| d["updated_at"].as_str().and_then(parse_time).unwrap_or(0))
        {
            return apply(state, deposit).await;
        }
        let pages = found["total_pages"].as_u64();
        if pages.is_some_and(|last| page >= last) || (pages.is_none() && rows.len() < 200) {
            scanned_all = true;
            break;
        }
    }
    // Nothing arrived and the account has lapsed (a late transfer is reviewed by Daya, not lost).
    if scanned_all && ramp.kind == "onramp" && now_ms() > ramp.expires_ms + 10 * 60_000 {
        let lock = state.daya.update_lock(&ramp.funding_account)?;
        let _guard = lock.lock().await;
        ramp = state
            .daya
            .ramp("funding_account", &ramp.funding_account)
            .await?
            .unwrap_or(ramp);
        if ramp.status != "waiting" {
            return Ok(());
        }
        ramp.status = "expired".into();
        ramp.message = Some("No transfer arrived before this account expired. If you sent money after that, contact support.".into());
        update_receipt(state, &ramp).await?;
        state.daya.keep(&ramp).await?;
    }
    Ok(())
}

// POST /v1/daya/webhook: acknowledge only after the signed observation is saved.
pub(super) async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(secret) = state.daya.webhook_secret.clone() else {
        eprintln!("daya webhook: DAYA_WEBHOOK_SECRET is not set");
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let signature = headers
        .get("x-daya-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !signed(&secret, &body, signature) {
        eprintln!("daya webhook: bad signature");
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let name = event["event"].as_str().unwrap_or("");
    let result = if name.starts_with("deposit.") {
        let mut deposit = event["data"].clone();
        if deposit["updated_at"].as_str().is_none() {
            deposit["updated_at"] = event["timestamp"].clone();
        }
        apply(&state, &deposit).await
    } else if name == "funding_account.failed" || name == "funding_account.disabled" {
        account_closed(&state, &event["data"]).await
    } else {
        Ok(())
    };
    match result {
        Ok(()) => StatusCode::OK,
        Err((_, error)) => {
            eprintln!("daya webhook {name}: {error}");
            // Daya retries non-2xx delivery. Never acknowledge a failed database update.
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

// A naira account that closed before any money came in.
async fn account_closed(state: &AppState, account: &Value) -> Result<(), ApiError> {
    let Some(id) = account["id"].as_str() else {
        return Ok(());
    };
    let lock = state.daya.update_lock(id)?;
    let _guard = lock.lock().await;
    let Some(mut ramp) = state.daya.ramp("funding_account", id).await? else {
        return Ok(());
    };
    if ramp.kind != "onramp" || ramp.status != "waiting" {
        return Ok(());
    }
    ramp.status = "expired".into();
    ramp.message = Some("This account closed before a transfer arrived.".into());
    update_receipt(state, &ramp).await?;
    state.daya.keep(&ramp).await
}

#[derive(Deserialize)]
pub(super) struct OnrampBody {
    // Whole naira: to pay in, or (with `receive`) to land in the balance.
    amount: String,
    // The amount is what should land: the fees go on top of it, into what's transferred. Older apps
    // leave it out and get the old meaning (what they transfer, fees taken out of it).
    #[serde(default)]
    receive: bool,
    // The currency to show what lands in.
    currency: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct OnrampQuery {
    currency: Option<String>,
}

// What paying in `amount` naira gets: (naira paid, Daya's deposit fee, USDC that lands, the rate).
// The USDC (units) that paying `pay` naira micros lands: the deposit fee comes off the naira and
// the rest converts at Daya's rate. Daya's crypto-withdrawal fee is for withdrawals from a merchant
// balance; a naira deposit settled on-chain carries only its deposit fee.
fn lands(pay: u128, rate: &Rate, fees: &Fees) -> u128 {
    (pay - fees.deposit(pay)) * 1_000_000 / rate.ngn_per_usdc
}

// The whole naira to transfer so that `target` naira (micros, at the balance's own naira rate
// `ngn_fx`) lands: Daya's conversion and the deposit fee go on top.
fn pay_for(target: u128, rate: &Rate, fees: &Fees, ngn_fx: u128) -> u128 {
    let usdc = (target * 1_000_000).div_ceil(ngn_fx.max(1));
    let after_fee = (usdc * rate.ngn_per_usdc).div_ceil(1_000_000);
    // The deposit fee is a share up to a cap: whichever needs less.
    let by_share = if fees.deposit_pct < 100_000_000 {
        (after_fee * 100_000_000).div_ceil(100_000_000 - fees.deposit_pct)
    } else {
        u128::MAX
    };
    let mut pay = by_share
        .min(after_fee + fees.deposit_cap)
        .div_ceil(1_000_000)
        * 1_000_000;
    // Checked with the very arithmetic the account uses; a naira more if rounding fell short.
    for _ in 0..5 {
        if lands(pay, rate, fees) >= usdc {
            break;
        }
        pay += 1_000_000;
    }
    pay
}

async fn onramp_amounts(
    daya: &DayaState,
    amount: &str,
    receive: bool,
) -> Result<(u128, u128, u128, Rate, Fees), ApiError> {
    let naira: u128 = amount
        .trim()
        .parse()
        .map_err(|_| bad("Enter a whole naira amount"))?;
    let entered = naira
        .checked_mul(1_000_000)
        .ok_or_else(|| bad("amount too large"))?;
    if entered > 1_000_000_000_000_000 {
        return Err(bad("amount too large"));
    }
    let (rate, fees) = tokio::try_join!(daya.rate("BUY"), daya.fees())?;
    let pay = if receive {
        pay_for(entered, &rate, &fees, app_balance::fx_rate("NGN").await?)
    } else {
        entered
    };
    let fee = fees.deposit(pay);
    let usdc = lands(pay, &rate, &fees);
    if pay < rate.min_ngn || usdc < markets::MIN_USDC {
        return Err(bad(&format!(
            "The smallest bank transfer is {}",
            markets::say_micros(rate.min_ngn.div_ceil(1_000_000) * 1_000_000, "NGN")
        )));
    }
    if usdc > markets::MAX_USDC {
        return Err(bad(
            "That's more than one bank transfer can add. Split it into smaller transfers.",
        ));
    }
    Ok((pay, fee, usdc, rate, fees))
}

// POST /v1/onramp/bank/quote → what a naira bank transfer would add, before any account opens.
pub(super) async fn onramp_quote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OnrampBody>,
) -> Result<Json<Value>, ApiError> {
    app_balance::signed_in(&state, &headers).await?;
    if state.daya.key.is_none() {
        return Err(not_ready());
    }
    let currency = body.currency.as_deref().unwrap_or("NGN");
    markets::checked_currency(currency)?;
    let ((pay, fee, usdc, rate, _), fx) = tokio::try_join!(
        onramp_amounts(&state.daya, &body.amount, body.receive),
        app_balance::fx_rate(currency)
    )?;
    Ok(Json(json!({
        "pay": naira(pay),
        "fee": naira(fee),
        // Kept for older apps: delivery to the wallet costs nothing on top of the deposit fee.
        "networkFee": markets::say_money(0, currency, fx),
        "receive": {"amount": markets::format_units(usdc * fx / 1_000_000, 6), "currency": currency},
        "rate": format!("{} per $1", markets::say_micros(rate.ngn_per_usdc, "NGN")),
    })))
}

// POST /v1/onramp/bank → a one-time Nigerian bank account for this amount, paying USDC to the
// user's Base wallet at a locked rate.
pub(super) async fn onramp_open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OnrampBody>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if state.daya.key.is_none() {
        return Err(not_ready());
    }
    let currency = body.currency.as_deref().unwrap_or("NGN").to_owned();
    markets::checked_currency(&currency)?;
    let wallet = user
        .evm_wallet
        .clone()
        .filter(|w| markets::looks_like_evm_address(w))
        .ok_or((
            StatusCode::CONFLICT,
            "Your wallet isn't ready yet. Try again in a moment.".into(),
        ))?;
    let ((pay, fee, usdc, rate, _), customer) = tokio::try_join!(
        onramp_amounts(&state.daya, &body.amount, body.receive),
        state.daya.customer(&user)
    )?;
    if rate.expires_ms < now_ms() + RATE_MARGIN_MS {
        return Err(busy());
    }
    let request = json!({
        "type": "TEMPORARY",
        "rail": "NGN_VIRTUAL_ACCOUNT",
        "customer": {"customer_id": customer},
        "currency": "NGN",
        "amount": pay / 1_000_000,
        "settlement_destination": {
            "type": "ONCHAIN",
            "rate_id": rate.id,
            "destination_asset": "USDC",
            "destination_chain": ONRAMP_CHAIN,
            "destination_address": wallet,
        },
    });
    let idempotency = format!(
        "atlas-onramp-{:x}-{:x}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let (account, details) = state
        .daya
        .open_account(&request, &idempotency, "NGN_VIRTUAL_ACCOUNT")
        .await?;
    // The USDC must go to this user's wallet, on Base, as USDC; anything else is never shown.
    let to = &account["settlement_destination"];
    let exact = account["amount"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| account["amount"].as_number().map(|n| n.to_string()));
    let id = account["id"].as_str().unwrap_or("").to_owned();
    let sound = to["destination_address"]
        .as_str()
        .is_some_and(|a| a.eq_ignore_ascii_case(&wallet))
        && to["destination_chain"] == ONRAMP_CHAIN
        && to["destination_asset"] == "USDC"
        && account["customer_id"].as_str() == Some(customer.as_str())
        && !id.is_empty()
        && exact
            .as_deref()
            .and_then(|a| markets::parse_micros(a).ok())
            .is_some_and(|a| a >= pay);
    let Some(exact) = exact.filter(|_| sound) else {
        eprintln!("daya funding account {id}: naira account details don't match the request");
        return Err(broken());
    };
    let account_view = json!({
        "bankName": details["bank_name"],
        "accountNumber": details["account_number"],
        "accountName": details["account_name"],
        "amount": exact,
    });
    let expires = account["expires_at"]
        .as_str()
        .and_then(parse_time)
        .unwrap_or(rate.expires_ms)
        .min(rate.expires_ms);
    let receipt_id = format!("onramp-{id}");
    let mut receipt = transactions::Receipt::ramp(
        &user.user_id,
        &receipt_id,
        "onramp",
        vec![
            json!({"label":"You pay","value":format!("₦{exact}")}),
            json!({"label":"To","value":format!("{} · {}", details["bank_name"].as_str().unwrap_or(""), details["account_number"].as_str().unwrap_or(""))}),
            json!({"label":"Fee","value":markets::say_micros(fee, "NGN")}),
            json!({"label":"Rate","value":format!("{} per $1", markets::say_micros(rate.ngn_per_usdc, "NGN"))}),
        ],
        usdc,
    );
    receipt.set_line("Status", "Waiting for your transfer".into());
    state.history.put(&receipt).await?;
    let ramp = Ramp {
        funding_account: id.clone(),
        owner: user.user_id.clone(),
        kind: "onramp".into(),
        receipt: receipt_id,
        created_ms: now_ms(),
        expires_ms: expires,
        status: "waiting".into(),
        message: None,
        usdc_units: Some(usdc.to_string()),
        paid_ngn: None,
        tx: None,
        deposit_id: None,
        provider_updated_ms: 0,
        account: account_view,
    };
    state.daya.keep(&ramp).await?;
    Ok(Json(onramp_view(
        &ramp,
        &currency,
        app_balance::fx_rate(&currency).await?,
    )))
}

// GET /v1/onramp/bank/{id} → that account's details and progress.
pub(super) async fn onramp_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<OnrampQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let currency = q.currency.as_deref().unwrap_or("NGN");
    markets::checked_currency(currency)?;
    let mine = |r: &Ramp| r.owner == user.user_id && r.kind == "onramp";
    let ramp = state
        .daya
        .ramp("funding_account", &id)
        .await?
        .filter(mine)
        .ok_or((StatusCode::NOT_FOUND, "no such bank transfer".into()))?;
    if !ramp.done() {
        // A missed webhook only delays this answer, never fails it.
        let _ = tokio::time::timeout(
            Duration::from_secs(8),
            observe(&state, &user.user_id, &ramp.receipt),
        )
        .await;
    }
    let ramp = state
        .daya
        .ramp("funding_account", &id)
        .await?
        .filter(mine)
        .unwrap_or(ramp);
    Ok(Json(onramp_view(
        &ramp,
        currency,
        app_balance::fx_rate(currency).await?,
    )))
}

fn onramp_view(ramp: &Ramp, currency: &str, fx: u128) -> Value {
    let usdc = ramp
        .usdc_units
        .as_deref()
        .and_then(|u| u.parse::<u128>().ok())
        .unwrap_or(0);
    json!({
        "id": ramp.funding_account,
        "bankName": ramp.account["bankName"],
        "accountNumber": ramp.account["accountNumber"],
        "accountName": ramp.account["accountName"],
        "pay": {"amount": ramp.account["amount"], "currency": "NGN"},
        "receive": {"amount": markets::format_units(usdc * fx / 1_000_000, 6), "currency": currency},
        "expiresAtUnixMs": ramp.expires_ms,
        "state": ramp.status,
        "message": ramp.message,
        "txId": ramp.tx,
    })
}

// Checks Daya with no money involved (key, rates, fees, banks) at start and every ten minutes,
// keeping rates and the bank list warm; /health shows whether it worked.
pub(super) fn keep_checked(state: AppState) {
    if state.daya.key.is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            let daya = &state.daya;
            let result = tokio::try_join!(
                daya.rate("SELL"),
                daya.rate("BUY"),
                daya.fees(),
                daya.banks()
            );
            let check = match result {
                // Daya's public fee schedule too, so what the app charges can be checked against it.
                Ok((_, _, fees, _)) => json!({"ok": true, "at": now_ms(), "fees": {
                    "ngnDepositPercent": markets::format_units(fees.deposit_pct, 6),
                    "ngnDepositCap": markets::format_units(fees.deposit_cap, 6),
                    "usdcPayoutUsd": markets::format_units(fees.usdc_payout_fee, 6),
                    "ngnPayoutPercent": markets::format_units(fees.payout_pct, 6),
                    "ngnPayoutCap": markets::format_units(fees.payout_cap, 6),
                }}),
                Err((_, error)) => json!({"ok": false, "at": now_ms(), "error": error}),
            };
            if let Ok(mut held) = CHECK.lock() {
                *held = check;
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}
pub(super) fn health() -> Value {
    let set = |var: &str| env::var(var).is_ok_and(|v| !v.trim().is_empty());
    json!({
        "key": set("DAYA_API_KEY"),
        "webhookSecret": set("DAYA_WEBHOOK_SECRET"),
        "check": CHECK.lock().map(|c| c.clone()).unwrap_or(Value::Null),
    })
}

// A bank name for matching across lists: "Access Bank Plc" and "ACCESS BANK" both → "access".
fn bank_key(name: &str) -> String {
    name.to_ascii_lowercase()
        .replace("microfinance", "mfb")
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| {
            !w.is_empty()
                && !matches!(
                    *w,
                    "bank" | "plc" | "limited" | "ltd" | "nigeria" | "of" | "the"
                )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
// A bank's logo: by code, then the same name, then the shortest listed name holding every word of
// it ("Opay" → "OPay Digital Services Limited (OPay)").
fn logo_for(logos: &[(String, String, String)], code: &str, name: &str) -> Option<String> {
    if let Some((_, _, logo)) = logos.iter().find(|(c, _, _)| c == code) {
        return Some(logo.clone());
    }
    if name.is_empty() {
        return None;
    }
    if let Some((_, _, logo)) = logos.iter().find(|(_, n, _)| n == name) {
        return Some(logo.clone());
    }
    let words: Vec<&str> = name.split(' ').collect();
    logos
        .iter()
        .filter(|(_, n, _)| {
            let theirs: Vec<&str> = n.split(' ').collect();
            words.iter().all(|w| theirs.contains(w))
        })
        .min_by_key(|(_, n, _)| n.len())
        .map(|(_, _, logo)| logo.clone())
}
// The popular banks a number is checked against (by name), and those whose account numbers can be
// phone numbers.
const POPULAR_BANKS: &[&str] = &[
    "access",
    "guaranty",
    "zenith",
    "united for africa",
    "first",
    "fidelity",
    "union",
    "sterling",
    "stanbic",
    "wema",
    "first city monument",
    "fcmb",
    "ecobank",
    "polaris",
    "keystone",
    "providus",
    "kuda",
    "moniepoint",
    "opay",
    "palmpay",
];
const PHONE_BANKS: &[&str] = &["opay", "palmpay", "moniepoint"];
// A Nigerian mobile number without its leading 0 (OPay and the like use it as the account number).
fn phone_like(account: &str) -> bool {
    ["70", "80", "81", "90", "91"]
        .iter()
        .any(|p| account.starts_with(p))
}
// The six-digit institution code NUBAN check digits use: a bank's three-digit code with 000 in
// front, a microfinance bank's five-digit code with a 9.
fn institution(code: &str) -> Option<String> {
    if !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match code.len() {
        3 => Some(format!("000{code}")),
        5 => Some(format!("9{code}")),
        6 => Some(code.to_owned()),
        _ => None,
    }
}
// NUBAN: weights 3, 7, 3 over the institution code and the first nine digits; the tenth digit is
// what brings the sum up to a multiple of ten.
fn nuban_fits(institution: &str, account: &str) -> bool {
    let digits: Vec<u32> = institution
        .chars()
        .chain(account.chars().take(9))
        .filter_map(|c| c.to_digit(10))
        .collect();
    let Some(check) = account.chars().nth(9).and_then(|c| c.to_digit(10)) else {
        return false;
    };
    if digits.len() != 15 {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .zip([3, 7, 3].iter().cycle())
        .map(|(d, w)| d * w)
        .sum();
    (10 - sum % 10) % 10 == check
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GuessBody {
    account_number: String,
}
// POST /v1/offramp/guess { accountNumber } → { banks: [{ code, name, logo, accountName }] }
pub(super) async fn guess(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<GuessBody>,
) -> Result<Json<Value>, ApiError> {
    app_balance::signed_in(&state, &headers).await?;
    Ok(Json(
        json!({"banks": state.daya.guess_banks(body.account_number.trim()).await?}),
    ))
}
// GET /v1/offramp/recipients → { recipients: [...] }, most recently paid first.
pub(super) async fn recipients(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    Ok(Json(
        json!({"recipients": state.daya.recipients(&user.user_id).await?}),
    ))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FavoriteBody {
    bank_code: String,
    account_number: String,
    favorite: bool,
}
// POST /v1/offramp/recipients { bankCode, accountNumber, favorite } → the updated list.
pub(super) async fn favorite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FavoriteBody>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    state
        .daya
        .set_favorite(
            &user.user_id,
            &body.bank_code,
            &body.account_number,
            body.favorite,
        )
        .await?;
    Ok(Json(
        json!({"recipients": state.daya.recipients(&user.user_id).await?}),
    ))
}

// USDC (micros) that converts to at least `ngn` naira micros at `ngn_per_usdc`, rounded up.
fn payout_usdc(ngn: u128, ngn_per_usdc: u128) -> u128 {
    (ngn * 1_000_000).div_ceil(ngn_per_usdc)
}
fn naira(micros: u128) -> Value {
    json!({"amount": format!("{}.{:02}", micros / 1_000_000, micros % 1_000_000 / 10_000), "currency": "NGN"})
}
fn url(path: &str, query: &[(&str, &str)]) -> reqwest::Url {
    let mut url = reqwest::Url::parse(DAYA_API)
        .expect("Daya's address is valid")
        .join(path)
        .expect("Daya paths are valid");
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    url
}
// Daya answers some routes bare and some inside {"data": …}.
fn data(body: &Value) -> &Value {
    match body.get("data") {
        Some(inner) if body.get("object").is_none() && body.get("id").is_none() => inner,
        _ => body,
    }
}
// The instruction that holds payment details, once Daya has provisioned it.
fn ready_instruction<'a>(account: &'a Value, rail: &str) -> Option<&'a Value> {
    account["instructions"].as_array()?.iter().find(|i| {
        i["type"] == rail
            && i["status"] == "ACTIVE"
            && if rail == "CRYPTO_ADDRESS" {
                i["address"].as_str().is_some_and(|a| !a.is_empty())
            } else {
                i["account_number"].as_str().is_some_and(|a| !a.is_empty())
                    && i["bank_name"].as_str().is_some()
            }
    })
}
// "Ada Lovelace" → ("Ada", "Lovelace"); a single word is a first name.
fn names(full: Option<&str>) -> (Option<String>, Option<String>) {
    let full = full.unwrap_or("").trim();
    let clip = |s: &str| s.chars().take(100).collect::<String>();
    match full.split_once(' ') {
        Some((first, last)) if !last.trim().is_empty() => {
            (Some(clip(first)), Some(clip(last.trim())))
        }
        _ if !full.is_empty() => (Some(clip(full)), None),
        _ => (None, None),
    }
}
// A decimal string or number ("1545.50", 1545.5) as micros; digits past the sixth are dropped.
fn micros_of(value: &Value) -> Option<u128> {
    let text = match value {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let frac: u128 = format!("{:0<6}", &frac[..frac.len().min(6)]).parse().ok()?;
    whole
        .parse::<u128>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(frac)
}
fn signed(secret: &str, body: &[u8], signature: &str) -> bool {
    let signature = signature.trim();
    let signature = signature.strip_prefix("sha256=").unwrap_or(signature);
    let Some(expected) = hex(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    // Constant-time comparison.
    mac.verify_slice(&expected).is_ok()
}
fn hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
// Days since 1970-01-01 for a calendar date, and back (Howard Hinnant's civil-date algorithms).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}
// "2026-01-14T15:35:00Z" (fractions and a +01:00 offset allowed) → unix milliseconds.
fn parse_time(text: &str) -> Option<u64> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut parts = date.splitn(3, '-');
    let (y, m, d): (i64, i64, i64) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let (clock, offset_min) = match rest.strip_suffix('Z') {
        Some(clock) => (clock, 0),
        None => {
            let at = rest.rfind(['+', '-'])?;
            let (clock, offset) = rest.split_at(at);
            let (h, mm) = offset[1..].split_once(':')?;
            let minutes = h.parse::<i64>().ok()? * 60 + mm.parse::<i64>().ok()?;
            (
                clock,
                if offset.starts_with('-') {
                    -minutes
                } else {
                    minutes
                },
            )
        }
    };
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut t = hms.splitn(3, ':');
    let (h, mi, s): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s - offset_min * 60;
    u64::try_from(secs * 1000 + ms).ok()
}
pub(super) fn format_time(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stored_receipt(history: &transactions::HistoryStore) -> Value {
        serde_json::to_value(history.find("alice", "bank-send").await.unwrap().unwrap()).unwrap()
    }
    fn memory_state() -> DayaState {
        DayaState {
            http: reqwest::Client::new(),
            key: None,
            webhook_secret: None,
            postgres: None,
            ramps: Default::default(),
            customers: Default::default(),
            banks: Default::default(),
            names: Default::default(),
            rates: Default::default(),
            fees: Default::default(),
            polled: Default::default(),
            updates: Default::default(),
            recipients: Default::default(),
        }
    }
    fn bank_ramp() -> Ramp {
        Ramp {
            funding_account: "fa".into(),
            owner: "alice".into(),
            kind: "offramp".into(),
            receipt: "bank-send".into(),
            created_ms: 0,
            expires_ms: u64::MAX,
            status: "waiting".into(),
            message: None,
            usdc_units: Some("1000000".into()),
            paid_ngn: None,
            tx: None,
            deposit_id: None,
            provider_updated_ms: 0,
            account: Value::Null,
        }
    }
    fn deposit(status: &str, second: u64) -> Value {
        json!({"id":"dep", "funding_account_id":"fa", "status":status,
            "updated_at":format!("2026-10-10T10:00:{second:02}Z"),
            "customer_amount":{"currency":"NGN","amount":"500"}})
    }
    async fn bank_fixture() -> (DayaState, transactions::HistoryStore) {
        let daya = memory_state();
        let history = transactions::HistoryStore::default();
        daya.keep(&bank_ramp()).await.unwrap();
        history.put(&transactions::Receipt::plan("alice", &json!({
            "intentId":"bank-send", "kind":"offramp",
            "summary":[{"label":"Bank gets","value":"₦500.00"},{"label":"Bank payout","value":"Waiting for your USDC"}]
        }))).await.unwrap();
        (daya, history)
    }

    #[tokio::test]
    async fn concurrent_poll_and_webhook_cannot_undo_a_paid_bank_receipt() {
        let (daya, history) = bank_fixture().await;
        let complete = deposit("COMPLETED", 20);
        let stale = deposit("RECEIVED", 5);
        let (a, b) = tokio::join!(
            apply_deposit(&daya, &history, &complete),
            apply_deposit(&daya, &history, &stale),
        );
        a.unwrap();
        b.unwrap();
        let ramp = daya.ramp("receipt", "bank-send").await.unwrap().unwrap();
        let receipt = history.find("alice", "bank-send").await.unwrap().unwrap();
        assert_eq!(ramp.status, "completed");
        assert_eq!(ramp.deposit_id.as_deref(), Some("dep"));
        assert_eq!(serde_json::to_value(&receipt).unwrap()["state"], "filled");
        assert!(serde_json::to_value(&receipt).unwrap()["summary"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["label"] == "Bank payout" && l["value"] == "Paid ₦500.00"));
        assert!(history.find("bob", "bank-send").await.unwrap().is_none());
        assert!(apply_deposit(&daya, &history, &deposit("FAILED", 10))
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            daya.ramp("receipt", "bank-send")
                .await
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        apply_deposit(&daya, &history, &deposit("REVERSED", 30))
            .await
            .unwrap();
        assert_eq!(stored_receipt(&history).await["state"], "failed");
    }

    #[tokio::test]
    async fn receiving_crypto_waits_for_settlement_and_clears_a_resolved_review() {
        let (daya, history) = bank_fixture().await;
        let mut incoming = deposit("COMPLETED", 10);
        incoming["settlement_status"] = json!("PROCESSING");
        apply_deposit(&daya, &history, &incoming).await.unwrap();
        assert_eq!(
            daya.ramp("receipt", "bank-send")
                .await
                .unwrap()
                .unwrap()
                .status,
            "processing"
        );
        assert_eq!(stored_receipt(&history).await["state"], "pending");
        incoming = deposit("REQUIRES_REVIEW", 20);
        apply_deposit(&daya, &history, &incoming).await.unwrap();
        assert!(!stored_receipt(&history).await["error"].is_null());
        apply_deposit(&daya, &history, &deposit("PROCESSING", 30))
            .await
            .unwrap();
        assert!(stored_receipt(&history).await["error"].is_null());
        let mut wrong = deposit("COMPLETED", 40);
        wrong["id"] = json!("another-deposit");
        assert!(apply_deposit(&daya, &history, &wrong)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            daya.ramp("receipt", "bank-send")
                .await
                .unwrap()
                .unwrap()
                .status,
            "processing"
        );
    }

    #[tokio::test]
    async fn a_duplicate_completion_repairs_a_receipt_if_a_previous_mirror_failed() {
        let (daya, history) = bank_fixture().await;
        let complete = deposit("COMPLETED", 20);
        apply_deposit(&daya, &history, &complete).await.unwrap();
        let mut receipt = history.find("alice", "bank-send").await.unwrap().unwrap();
        receipt.set_state("pending", "settle", None);
        receipt.set_line("Bank payout", "Waiting for your USDC".into());
        history.put(&receipt).await.unwrap();
        apply_deposit(&daya, &history, &complete).await.unwrap();
        assert_eq!(stored_receipt(&history).await["state"], "filled");
    }

    #[tokio::test]
    async fn a_waiting_draft_is_unsigned_and_late_funds_can_leave_expiry() {
        let (daya, history) = bank_fixture().await;
        mirror_receipt(&history, &bank_ramp()).await.unwrap();
        assert_eq!(stored_receipt(&history).await["stage"], "validate");
        let mut ramp = bank_ramp();
        ramp.kind = "onramp".into();
        ramp.status = "expired".into();
        daya.keep(&ramp).await.unwrap();
        apply_deposit(&daya, &history, &deposit("REQUIRES_REVIEW", 20))
            .await
            .unwrap();
        assert_eq!(
            daya.ramp("receipt", "bank-send")
                .await
                .unwrap()
                .unwrap()
                .status,
            "review"
        );
    }

    #[test]
    fn focused_poll_is_not_held_back_by_history_and_inflight_reads_do_not_overlap() {
        let daya = memory_state();
        let held = daya.start_poll("fa", false).unwrap().unwrap();
        assert!(daya.start_poll("fa", true).unwrap().is_none());
        daya.polled.lock().unwrap().get_mut("fa").unwrap().at =
            Instant::now() - Duration::from_secs(4);
        // Even after its rate limit, an unfinished provider request stays single-flight.
        assert!(daya.start_poll("fa", true).unwrap().is_none());
        drop(held);
        assert!(daya.start_poll("fa", false).unwrap().is_none());
        let focused = daya.start_poll("fa", true).unwrap().unwrap();
        assert!(daya.start_poll("fa", true).unwrap().is_none());
        drop(focused);
        assert!(!daya.polled.lock().unwrap()["fa"].busy);
    }

    #[test]
    fn customers_are_matched_to_the_exact_sign_in_email_not_the_first_list_item() {
        let first = json!({"id":"ivan", "email":"ivan@example.com"});
        let actual = json!({"id":"ada", "email":"ADA@example.com"});
        assert!(!customer_matches(&first, "ada@example.com"));
        assert!(customer_matches(&actual, "ada@example.com"));
        assert!(!customer_matches(&json!({"id":"ada"}), "ada@example.com"));
    }

    #[test]
    fn times_round_trip() {
        let at = parse_time("2026-01-14T15:35:00Z").unwrap();
        assert_eq!(at, 1_768_404_900_000);
        assert_eq!(format_time(at), "2026-01-14T15:35:00Z");
        assert_eq!(parse_time("2026-01-14T16:35:00.250+01:00"), Some(at + 250));
        assert_eq!(
            parse_time("2024-02-29T00:00:00Z")
                .map(format_time)
                .as_deref(),
            Some("2024-02-29T00:00:00Z")
        );
        assert_eq!(parse_time("soon"), None);
    }

    #[test]
    fn amounts_are_exact_decimals() {
        assert_eq!(micros_of(&json!("1545.50")), Some(1_545_500_000));
        assert_eq!(micros_of(&json!(1545.5)), Some(1_545_500_000));
        assert_eq!(micros_of(&json!("0.1000")), Some(100_000));
        assert_eq!(micros_of(&json!("100")), Some(100_000_000));
        assert_eq!(micros_of(&json!("-1")), None);
        assert_eq!(micros_of(&json!("1e3")), None);
    }

    #[test]
    fn adding_money_puts_the_fees_on_top() {
        let fees = Fees {
            deposit_pct: 1_000_000,
            deposit_cap: 100_000_000,
            payout_pct: 1_000_000,
            payout_cap: 100_000_000,
            low_payout_fee: 20_000_000,
            low_payout_below: 1_000_000_000,
            payout_min: 100_000_000,
            usdc_payout_fee: 200_000,
        };
        let rate = Rate {
            id: "r".into(),
            ngn_per_usdc: 1_560_000_000,
            expires_ms: 0,
            min_ngn: 1_000_000_000,
            fetched: Instant::now(),
        };
        // ₦5,500 to land at ₦1,530/$ (the balance's rate): $3.594772 at ₦1,560/$, plus 1%, in whole
        // naira: ₦5,665. Daya's $0.20 crypto-withdrawal fee is not a top-up charge.
        let fx = 1_530_000_000;
        let pay = pay_for(5_500_000_000, &rate, &fees, fx);
        assert_eq!(pay, 5_665_000_000);
        assert!(lands(pay, &rate, &fees) * fx / 1_000_000 >= 5_500_000_000);
        // And never more than a naira over what's needed.
        assert!(lands(pay - 1_000_000, &rate, &fees) * fx / 1_000_000 < 5_500_000_000);
        // Big amounts hit the ₦100 cap instead of 1%.
        let big = pay_for(500_000_000_000, &rate, &fees, fx);
        assert!(lands(big, &rate, &fees) * fx / 1_000_000 >= 500_000_000_000);
        assert_eq!(fees.deposit(big), 100_000_000);
        assert!(lands(big - 1_000_000, &rate, &fees) * fx / 1_000_000 < 500_000_000_000);
    }

    #[test]
    fn fees_follow_the_schedule() {
        let fees = Fees {
            deposit_pct: 1_000_000,
            deposit_cap: 100_000_000,
            payout_pct: 1_000_000,
            payout_cap: 100_000_000,
            low_payout_fee: 20_000_000,
            low_payout_below: 1_000_000_000,
            payout_min: 100_000_000,
            usdc_payout_fee: 100_000,
        };
        // ₦500 → ₦20 flat; ₦5,000 → ₦50; ₦20,000 → the ₦100 cap.
        assert_eq!(fees.payout(500_000_000), 20_000_000);
        assert_eq!(fees.payout(5_000_000_000), 50_000_000);
        assert_eq!(fees.payout(20_000_000_000), 100_000_000);
        assert_eq!(fees.deposit(5_000_000_000), 50_000_000);
        assert_eq!(fees.deposit(50_000_000_000), 100_000_000);
    }

    #[test]
    fn payouts_send_enough_for_the_exact_amount() {
        // ₦2,000 to the bank plus the ₦20 fee at ₦1,465.50 per USDC.
        let usdc = payout_usdc(2_020_000_000, 1_465_500_000);
        assert_eq!(usdc, 1_378_370);
        assert!(usdc * 1_465_500_000 / 1_000_000 >= 2_020_000_000);
        assert!((usdc - 1) * 1_465_500_000 / 1_000_000 < 2_020_000_000);
    }

    #[test]
    fn webhooks_need_the_right_signature() {
        // openssl dgst -sha256 -hmac secret over the body below.
        let body = br#"{"event":"deposit.completed"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let good: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(signed("secret", body, &good));
        assert!(signed("secret", body, &format!("sha256={good}")));
        assert!(!signed("other", body, &good));
        assert!(!signed("secret", br#"{"event":"deposit.failed"}"#, &good));
        assert!(!signed("secret", body, "not-hex"));
    }

    #[test]
    fn wrapped_and_bare_answers_read_the_same() {
        let bare = json!({"rate_id":"r1","rate":1500});
        assert_eq!(data(&bare)["rate_id"], "r1");
        let wrapped = json!({"data":[{"code":"044","name":"Access Bank"}]});
        assert_eq!(data(&wrapped)[0]["code"], "044");
        let account = json!({"object":"funding_account","id":"fa","data":null});
        assert_eq!(data(&account)["id"], "fa");
    }

    #[test]
    fn only_active_details_are_shown() {
        let pending = json!({"instructions":[{"type":"NGN_VIRTUAL_ACCOUNT","status":"PENDING"}]});
        assert!(ready_instruction(&pending, "NGN_VIRTUAL_ACCOUNT").is_none());
        let active = json!({"instructions":[{"type":"NGN_VIRTUAL_ACCOUNT","status":"ACTIVE","bank_name":"Wema Bank","account_number":"1234567890"}]});
        assert!(ready_instruction(&active, "NGN_VIRTUAL_ACCOUNT").is_some());
        let address = json!({"instructions":[{"type":"CRYPTO_ADDRESS","status":"ACTIVE","address":"0x742d35cc6634c0532925a3b844bc9e7595f2bd18"}]});
        assert!(ready_instruction(&address, "CRYPTO_ADDRESS").is_some());
        assert!(ready_instruction(&address, "NGN_VIRTUAL_ACCOUNT").is_none());
    }

    #[test]
    fn account_numbers_fit_only_their_banks() {
        // Moniepoint MFB (50515) accounts from a real transfer list.
        let moniepoint = institution("50515").unwrap();
        for account in ["5016107344", "8225107279", "5013020091"] {
            assert!(nuban_fits(&moniepoint, account));
        }
        assert!(!nuban_fits(&moniepoint, "5016107345"));
        assert_eq!(institution("044").as_deref(), Some("000044"));
        assert_eq!(institution("ABC"), None);
        assert!(phone_like("9033935622"));
        assert!(!phone_like("5016107344"));
    }

    #[test]
    fn logos_match_short_names() {
        let logos = vec![
            (
                "999992".into(),
                bank_key("OPay Digital Services Limited (OPay)"),
                "opay.png".into(),
            ),
            (
                "50515".into(),
                bank_key("Moniepoint MFB"),
                "moniepoint.png".into(),
            ),
            ("044".into(), bank_key("Access Bank"), "access.png".into()),
            (
                "063".into(),
                bank_key("Access Bank (Diamond)"),
                "diamond.png".into(),
            ),
        ];
        assert_eq!(
            logo_for(&logos, "100004", &bank_key("Opay")).as_deref(),
            Some("opay.png")
        );
        assert_eq!(
            logo_for(&logos, "090405", &bank_key("Moniepoint Microfinance Bank")).as_deref(),
            Some("moniepoint.png")
        );
        assert_eq!(
            logo_for(&logos, "000", &bank_key("Access Bank Plc")).as_deref(),
            Some("access.png")
        );
        assert_eq!(logo_for(&logos, "000", &bank_key("Unknown Bank")), None);
    }

    #[test]
    fn bank_names_match_across_lists() {
        assert_eq!(bank_key("Access Bank Plc"), bank_key("ACCESS BANK"));
        assert_eq!(bank_key("First Bank of Nigeria"), "first");
        assert_ne!(bank_key("Kuda Bank"), bank_key("Opay"));
    }

    #[test]
    fn names_split_for_daya() {
        assert_eq!(
            names(Some("Ada Lovelace")),
            (Some("Ada".into()), Some("Lovelace".into()))
        );
        assert_eq!(names(Some("Ada")), (Some("Ada".into()), None));
        assert_eq!(names(None), (None, None));
    }
}
