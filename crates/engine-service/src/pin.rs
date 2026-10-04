//! A PIN authorizes one saved action, including its later signing steps, never a whole session.
use super::*;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

const GRANT_MS: i64 = 20 * 60_000;
const MAX_TRIES: i32 = 5;
const HEADER: &str = "x-atlas-pin-authorization";

#[derive(Clone)]
pub(super) struct PinState {
    pg: Option<Arc<AsyncMutex<tokio_postgres::Client>>>,
    pepper: Option<Arc<Vec<u8>>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum Action {
    Intent {
        #[serde(rename = "intentId")]
        intent_id: String,
    },
    Tpsl {
        #[serde(rename = "positionId")]
        position_id: String,
        #[serde(rename = "takeProfitPct")]
        take_profit_pct: Option<f64>,
        #[serde(rename = "stopLossPct")]
        stop_loss_pct: Option<f64>,
    },
    Cashlink {
        #[serde(rename = "linkId")]
        link_id: String,
        secret: String,
    },
    Wallet {
        origin: String,
        request: Value,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SetPin {
    pin: String,
    confirmation: String,
    current_pin: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Authorize {
    pin: String,
    action: Action,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consume {
    authorization: String,
    action: Action,
}

impl PinState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Domain-separated pre-hashing keeps four-digit PINs safe if only the database leaks.
        // A dedicated stable pepper may be set before the first PIN; otherwise use the existing
        // server credential. Changing either requires account recovery, never silently resetting PINs.
        let pepper = env::var("ATLAS_PIN_PEPPER")
            .ok()
            .or_else(|| env::var("PRIVY_APP_SECRET").ok())
            .filter(|p| p.len() >= 24)
            .map(|p| Arc::new(p.into_bytes()));
        let pg = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("transaction PIN database connection ended");
                }
            });
            client.batch_execute(
                "CREATE TABLE IF NOT EXISTS atlas_transaction_pins (
                    user_id TEXT PRIMARY KEY, pin_hash TEXT NOT NULL,
                    failures INTEGER NOT NULL DEFAULT 0 CHECK(failures >= 0 AND failures <= 5),
                    lock_level INTEGER NOT NULL DEFAULT 0, locked_until_ms BIGINT NOT NULL DEFAULT 0);
                 CREATE TABLE IF NOT EXISTS atlas_pin_authorizations (
                    token_hash BYTEA PRIMARY KEY, user_id TEXT NOT NULL,
                    scope TEXT NOT NULL, expires_ms BIGINT NOT NULL, consumed BOOLEAN NOT NULL DEFAULT FALSE);
                 CREATE INDEX IF NOT EXISTS atlas_pin_authorizations_owner ON atlas_pin_authorizations(user_id,scope);
                 CREATE TABLE IF NOT EXISTS atlas_pin_plans (
                    intent_id TEXT PRIMARY KEY,user_id TEXT NOT NULL,expires_ms BIGINT NOT NULL);"
            ).await?;
            Some(Arc::new(AsyncMutex::new(client)))
        } else {
            None
        };
        Ok(Self { pg, pepper })
    }
    pub(super) fn configured(&self) -> bool {
        self.pg.is_some() && self.pepper.is_some()
    }
    fn storage(&self) -> Result<&Arc<AsyncMutex<tokio_postgres::Client>>, ApiError> {
        if self.pepper.is_none() {
            return Err(unavailable());
        }
        self.pg.as_ref().ok_or_else(unavailable)
    }
    async fn set_for(&self, user: &str, body: SetPin) -> Result<Json<Value>, ApiError> {
        check_new_pin(&body.pin, &body.confirmation)?;
        let pg = self.storage()?;
        let pepper = self.pepper.clone().ok_or_else(unavailable)?;
        let mut client = pg.lock().await;
        let tx = client.transaction().await.map_err(db_error)?;
        // Serialize creation too: two devices cannot overwrite the first PIN.
        tx.query_one(
            "SELECT pg_advisory_xact_lock(hashtextextended($1,481719))",
            &[&user],
        )
        .await
        .map_err(db_error)?;
        let row = tx.query_opt("SELECT pin_hash,failures,lock_level,locked_until_ms FROM atlas_transaction_pins WHERE user_id=$1 FOR UPDATE", &[&user]).await.map_err(db_error)?;
        if let Some(row) = &row {
            let current = body.current_pin.ok_or((
                StatusCode::CONFLICT,
                "Your PIN is already set. Enter the current PIN to change it.".into(),
            ))?;
            if let Err(error) = verify_attempt(&self.pepper, row, current).await {
                save_failure(&tx, &user, row, &error).await?;
                tx.commit().await.map_err(db_error)?;
                return Err(error);
            }
        } else if body.current_pin.is_some() {
            return Err(setup_required());
        }
        let hash = tokio::task::spawn_blocking(move || hash_pin(&pepper, &body.pin))
            .await
            .map_err(db_error)??;
        tx.execute("INSERT INTO atlas_transaction_pins(user_id,pin_hash) VALUES($1,$2) ON CONFLICT(user_id) DO UPDATE SET pin_hash=EXCLUDED.pin_hash,failures=0,lock_level=0,locked_until_ms=0", &[&user,&hash]).await.map_err(db_error)?;
        tx.execute(
            "DELETE FROM atlas_pin_authorizations WHERE user_id=$1",
            &[&user],
        )
        .await
        .map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok(Json(json!({"configured":true,"lockedUntilUnixMs":null})))
    }
    async fn approve(&self, user: &str, pin: String, action: &Action) -> Result<String, ApiError> {
        let pg = self.storage()?;
        let scope = scope(action)?;
        // The row lock serializes guesses across processes, not just this instance.
        let mut client = pg.lock().await;
        let tx = client.transaction().await.map_err(db_error)?;
        let row = tx.query_opt("SELECT pin_hash,failures,lock_level,locked_until_ms FROM atlas_transaction_pins WHERE user_id=$1 FOR UPDATE", &[&user])
            .await.map_err(db_error)?.ok_or_else(setup_required)?;
        let result = verify_attempt(&self.pepper, &row, pin).await;
        if let Err(error) = result {
            save_failure(&tx, user, &row, &error).await?;
            tx.commit().await.map_err(db_error)?;
            return Err(error);
        }
        let mut random = [0_u8; 32];
        OsRng.fill_bytes(&mut random);
        let token = URL_SAFE_NO_PAD.encode(random);
        let token_hash = Sha256::digest(token.as_bytes()).to_vec();
        let expires = now() + GRANT_MS;
        tx.execute("UPDATE atlas_transaction_pins SET failures=0,lock_level=0,locked_until_ms=0 WHERE user_id=$1", &[&user]).await.map_err(db_error)?;
        tx.execute(
            "DELETE FROM atlas_pin_authorizations WHERE expires_ms <= $1",
            &[&now()],
        )
        .await
        .map_err(db_error)?;
        tx.execute("INSERT INTO atlas_pin_authorizations(token_hash,user_id,scope,expires_ms) VALUES($1,$2,$3,$4)",
            &[&token_hash,&user,&scope,&expires]).await.map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        Ok(token)
    }
    async fn require(
        &self,
        user: &str,
        token: &str,
        action: &Action,
        consume: bool,
    ) -> Result<(), ApiError> {
        let hash = token_hash(token)?;
        let scope = scope(action)?;
        let client = self.storage()?.lock().await;
        let at = now();
        let accepted = if consume {
            client.execute("UPDATE atlas_pin_authorizations SET consumed=TRUE WHERE token_hash=$1 AND user_id=$2 AND scope=$3 AND expires_ms>$4 AND NOT consumed",
                &[&hash,&user,&scope,&at]).await.map_err(db_error)? == 1
        } else {
            client.query_opt("SELECT 1 FROM atlas_pin_authorizations WHERE token_hash=$1 AND user_id=$2 AND scope=$3 AND expires_ms>$4 AND NOT consumed",
                &[&hash,&user,&scope,&at]).await.map_err(db_error)?.is_some()
        };
        if !accepted {
            return Err(authorization_required());
        }
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Your payment PIN could not be checked. Try again shortly.".into(),
    )
}
fn db_error(_: impl std::fmt::Display) -> ApiError {
    unavailable()
}
fn setup_required() -> ApiError {
    (
        StatusCode::PRECONDITION_REQUIRED,
        "Set up your four-digit payment PIN in Atlas first.".into(),
    )
}
fn authorization_required() -> ApiError {
    (
        StatusCode::PRECONDITION_REQUIRED,
        "Enter your payment PIN for this action. If you do not see a PIN prompt, update Atlas."
            .into(),
    )
}
fn valid_pin(pin: &str) -> bool {
    pin.len() == 4 && pin.bytes().all(|b| b.is_ascii_digit())
}
fn check_new_pin(pin: &str, confirmation: &str) -> Result<(), ApiError> {
    if !valid_pin(pin) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Your PIN must be exactly four digits.".into(),
        ));
    }
    if pin != confirmation {
        return Err((
            StatusCode::BAD_REQUEST,
            "Those PINs do not match. Try again.".into(),
        ));
    }
    Ok(())
}
fn token_hash(token: &str) -> Result<Vec<u8>, ApiError> {
    if token.len() != 43 || URL_SAFE_NO_PAD.decode(token).is_err() {
        return Err(authorization_required());
    }
    Ok(Sha256::digest(token.as_bytes()).to_vec())
}
fn scope(action: &Action) -> Result<String, ApiError> {
    let id_ok = |s: &str| !s.is_empty() && s.len() <= 160 && !s.chars().any(char::is_control);
    match action {
        Action::Intent { intent_id } if id_ok(intent_id) => {
            return Ok(format!("intent:{intent_id}"))
        }
        Action::Tpsl {
            position_id,
            take_profit_pct,
            stop_loss_pct,
        } if id_ok(position_id)
            && [take_profit_pct, stop_loss_pct]
                .iter()
                .all(|v| v.is_none_or(|v| v.is_finite() && v > 0.0 && v <= 10000.0)) => {}
        Action::Cashlink { link_id, secret }
            if id_ok(link_id) && !secret.is_empty() && secret.len() <= 2048 => {}
        Action::Wallet { origin, request }
            if reqwest::Url::parse(origin)
                .is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some())
                && matches!(
                    request["method"].as_str(),
                    Some(
                        "personal_sign"
                            | "eth_signTypedData_v4"
                            | "eth_sendTransaction"
                            | "solana:signMessage"
                            | "solana:signTransaction"
                            | "solana:signAndSendTransaction"
                    )
                ) => {}
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "This payment approval is incomplete.".into(),
            ))
        }
    }
    let bytes = serde_json::to_vec(action).map_err(db_error)?;
    if bytes.len() > 64 * 1024 {
        return Err((
            StatusCode::BAD_REQUEST,
            "This payment approval is too large.".into(),
        ));
    }
    Ok(format!(
        "action:{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
    ))
}
fn lock_error(until: i64) -> ApiError {
    let minutes = ((until - now()).max(1) + 59_999) / 60_000;
    (
        StatusCode::TOO_MANY_REQUESTS,
        format!(
            "Too many wrong PINs. Try again in {minutes} minute{}.",
            if minutes == 1 { "" } else { "s" }
        ),
    )
}
fn failed_attempt(failures: i32, level: i32, at: i64) -> (i32, i32, i64) {
    let failures = failures + 1;
    if failures < MAX_TRIES {
        (failures, level, 0)
    } else {
        let level = (level + 1).min(3);
        let duration = match level {
            1 => 15 * 60_000,
            2 => 60 * 60_000,
            _ => 24 * 60 * 60_000,
        };
        (0, level, at + duration)
    }
}
async fn verify_attempt(
    pepper: &Option<Arc<Vec<u8>>>,
    row: &tokio_postgres::Row,
    pin: String,
) -> Result<(), ApiError> {
    let until: i64 = row.get(3);
    if until > now() {
        return Err(lock_error(until));
    }
    let hash: String = row.get(0);
    let pepper = pepper.clone().ok_or_else(unavailable)?;
    let correct = tokio::task::spawn_blocking(move || verify_hash(&pepper, &pin, &hash))
        .await
        .map_err(db_error)??;
    if correct {
        return Ok(());
    }
    let (tries, _, until) = failed_attempt(row.get(1), row.get(2), now());
    if until > 0 {
        return Err(lock_error(until));
    }
    Err((
        StatusCode::FORBIDDEN,
        format!(
            "Incorrect PIN. {} attempt{} left.",
            MAX_TRIES - tries,
            if MAX_TRIES - tries == 1 { "" } else { "s" }
        ),
    ))
}
async fn save_failure(
    tx: &tokio_postgres::Transaction<'_>,
    user: &str,
    row: &tokio_postgres::Row,
    error: &ApiError,
) -> Result<(), ApiError> {
    let locked: i64 = row.get(3);
    if locked > now()
        || !matches!(
            error.0,
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        )
    {
        return Ok(());
    }
    let (failures, level, until) = failed_attempt(row.get(1), row.get(2), now());
    tx.execute("UPDATE atlas_transaction_pins SET failures=$2,lock_level=$3,locked_until_ms=$4 WHERE user_id=$1",
        &[&user,&failures,&level,&until]).await.map_err(db_error)?;
    Ok(())
}
fn secret(pepper: &[u8], pin: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).expect("HMAC accepts arbitrary key sizes");
    mac.update(b"atlas.transaction-pin.v1\0");
    mac.update(pin.as_bytes());
    mac.finalize().into_bytes().to_vec()
}
fn hash_pin(pepper: &[u8], pin: &str) -> Result<String, ApiError> {
    Argon2::default()
        .hash_password(&secret(pepper, pin), &SaltString::generate(&mut OsRng))
        .map(|h| h.to_string())
        .map_err(db_error)
}
fn verify_hash(pepper: &[u8], pin: &str, hash: &str) -> Result<bool, ApiError> {
    let hash = PasswordHash::new(hash).map_err(db_error)?;
    Ok(valid_pin(pin)
        && Argon2::default()
            .verify_password(&secret(pepper, pin), &hash)
            .is_ok())
}

// Older apps send a transaction before reporting /signed. Refuse their plans up front, so
// rolling out PIN enforcement can never strand an old client's buy after it pays.
pub(super) async fn plan_gate(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    let path = request.uri().path();
    if request.method() != Method::POST || !plan_route(path) {
        return Ok(next.run(request).await);
    }
    if request
        .headers()
        .get("x-atlas-payment-pin")
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return Err((
            StatusCode::PRECONDITION_REQUIRED,
            "Update Atlas to set up your payment PIN before moving money.".into(),
        ));
    }
    let user = app_balance::verified_wallets(&state, request.headers()).await?;
    {
        let client = state.pin.storage()?.lock().await;
        if client
            .query_opt(
                "SELECT 1 FROM atlas_transaction_pins WHERE user_id=$1",
                &[&user.user_id],
            )
            .await
            .map_err(db_error)?
            .is_none()
        {
            return Err(setup_required());
        }
    }
    let response = next.run(request).await;
    if !response.status().is_success() {
        return Ok(response);
    }
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024)
        .await
        .map_err(db_error)?;
    let plan: Value = serde_json::from_slice(&bytes).map_err(db_error)?;
    let id = plan["intentId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(unavailable)?;
    let expires = plan["expiresAtUnixMs"].as_i64().ok_or_else(unavailable)?;
    state.pin.storage()?.lock().await.execute(
        "INSERT INTO atlas_pin_plans(intent_id,user_id,expires_ms) VALUES($1,$2,$3)
         ON CONFLICT(intent_id) DO UPDATE SET expires_ms=EXCLUDED.expires_ms WHERE atlas_pin_plans.user_id=EXCLUDED.user_id",
        &[&id,&user.user_id,&expires]).await.map_err(db_error)?;
    Ok(axum::response::Response::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
}

fn plan_route(path: &str) -> bool {
    path.starts_with("/v1/") && (path.ends_with("/execute") || path.ends_with("/resume"))
}

pub(super) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let client = state.pin.storage()?.lock().await;
    let row = client
        .query_opt(
            "SELECT locked_until_ms FROM atlas_transaction_pins WHERE user_id=$1",
            &[&user.user_id],
        )
        .await
        .map_err(db_error)?;
    Ok(Json(
        json!({"configured":row.is_some(),"lockedUntilUnixMs":row.map(|r|r.get::<_,i64>(0)).filter(|v|*v>now())}),
    ))
}
pub(super) async fn set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetPin>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    check_new_pin(&body.pin, &body.confirmation)?;
    state.pin.set_for(&user.user_id, body).await
}

pub(super) async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Authorize>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if let Action::Intent { intent_id } = &body.action {
        let client = state.pin.storage()?.lock().await;
        let row = client
            .query_opt(
                "SELECT expires_ms FROM atlas_pin_plans WHERE intent_id=$1 AND user_id=$2",
                &[&intent_id, &user.user_id],
            )
            .await
            .map_err(db_error)?
            .ok_or((
                StatusCode::PRECONDITION_REQUIRED,
                "Open this payment's review again before entering your PIN.".into(),
            ))?;
        if row.get::<_, i64>(0) <= now() {
            return Err((
                StatusCode::CONFLICT,
                "This quote expired. Go back and try again.".into(),
            ));
        }
        drop(client);

        if state
            .history
            .find(&user.user_id, intent_id)
            .await?
            .is_none()
        {
            return Err((
                StatusCode::NOT_FOUND,
                "This payment could not be found. Request a fresh quote.".into(),
            ));
        }
    }
    let token = state
        .pin
        .approve(&user.user_id, body.pin, &body.action)
        .await?;
    Ok(Json(
        json!({"authorization":token,"expiresAtUnixMs":now()+GRANT_MS}),
    ))
}
pub(super) async fn consume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Consume>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if !matches!(body.action, Action::Wallet { .. }) {
        return Err((
            StatusCode::BAD_REQUEST,
            "This approval is used by its own payment.".into(),
        ));
    }
    state
        .pin
        .require(&user.user_id, &body.authorization, &body.action, true)
        .await?;
    Ok(Json(json!({"approved":true})))
}
pub(super) async fn require_action(
    state: &AppState,
    headers: &HeaderMap,
    action: Action,
    consume: bool,
) -> Result<(), ApiError> {
    let user = app_balance::verified_wallets(state, headers).await?;
    let token = headers
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(authorization_required)?;
    state
        .pin
        .require(&user.user_id, token, &action, consume)
        .await
}
pub(super) async fn require_intent(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<(), ApiError> {
    require_action(
        state,
        headers,
        Action::Intent {
            intent_id: id.into(),
        },
        false,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_clients_are_stopped_before_any_money_plan_is_returned() {
        for path in [
            "/v1/quotes/q/execute",
            "/v1/sends/quote/q/execute",
            "/v1/perps/quotes/q/execute",
            "/v1/perps/close-quotes/q/execute",
            "/v1/earn/quotes/q/execute",
            "/v1/withdrawals/quote/q/execute",
            "/v1/predictions/quotes/q/execute",
            "/v1/intents/i/resume",
        ] {
            assert!(plan_route(path));
        }
        for path in [
            "/health",
            "/v1/balance",
            "/v1/me/pin",
            "/v1/deposit/quote",
            "/v1/daya/webhook",
        ] {
            assert!(!plan_route(path));
        }
    }
    #[test]
    fn pin_is_exactly_four_ascii_digits_and_confirmation_matches() {
        for p in ["", "123", "12345", "１２３４", "12 4", "12a4"] {
            assert!(!valid_pin(p));
        }
        assert!(valid_pin("0007"));
        assert!(check_new_pin("1234", "1234").is_ok());
        assert!(check_new_pin("1234", "4321").is_err());
    }
    #[test]
    fn salted_peppered_hashes_are_not_portable_or_plaintext() {
        let first = hash_pin(b"one secret server pepper", "0729").unwrap();
        let second = hash_pin(b"one secret server pepper", "0729").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("$argon2id$"));
        assert!(verify_hash(b"one secret server pepper", "0729", &first).unwrap());
        assert!(!verify_hash(b"another secret pepper", "0729", &first).unwrap());
        assert!(!verify_hash(b"one secret server pepper", "0728", &first).unwrap());
    }
    #[test]
    fn five_failures_lock_then_locks_escalate_and_do_not_expire_on_restart() {
        let at = 10000;
        assert_eq!(failed_attempt(3, 0, at), (4, 0, 0));
        assert_eq!(failed_attempt(4, 0, at), (0, 1, at + 15 * 60_000));
        assert_eq!(failed_attempt(4, 1, at), (0, 2, at + 60 * 60_000));
        assert_eq!(failed_attempt(4, 2, at), (0, 3, at + 24 * 60 * 60_000));
        assert_eq!(failed_attempt(4, 3, at), (0, 3, at + 24 * 60 * 60_000));
    }
    #[test]
    fn authorization_is_bound_to_one_action_and_every_direct_field() {
        let intent = Action::Intent {
            intent_id: "buy-one".into(),
        };
        assert_ne!(
            scope(&intent).unwrap(),
            scope(&Action::Intent {
                intent_id: "buy-two".into()
            })
            .unwrap()
        );
        let original = Action::Tpsl {
            position_id: "BTC".into(),
            take_profit_pct: Some(20.0),
            stop_loss_pct: Some(10.0),
        };
        assert_ne!(
            scope(&original).unwrap(),
            scope(&Action::Tpsl {
                position_id: "BTC".into(),
                take_profit_pct: Some(21.0),
                stop_loss_pct: Some(10.0)
            })
            .unwrap()
        );
        let first = Action::Cashlink {
            link_id: "abc".into(),
            secret: "secret-one".into(),
        };
        assert_ne!(
            scope(&first).unwrap(),
            scope(&Action::Cashlink {
                link_id: "abc".into(),
                secret: "secret-two".into()
            })
            .unwrap()
        );
        assert!(token_hash("1234").is_err());
    }
    #[tokio::test]
    #[ignore = "requires the isolated local atlas_pin_test database, never production"]
    async fn postgres_pins_survive_restarts_and_concurrent_guesses_cannot_bypass_locks() {
        let url = env::var("ATLAS_PIN_TEST_DATABASE_URL").expect("local PIN database URL");
        let u = reqwest::Url::parse(&url).unwrap();
        assert_eq!(u.host_str(), Some("127.0.0.1"));
        assert_eq!(u.path(), "/atlas_pin_test");
        env::set_var("DATABASE_URL", &url);
        env::set_var(
            "ATLAS_PIN_PEPPER",
            "a test-only pepper that is not a production credential",
        );
        let state = PinState::new().await.unwrap();
        let user = format!("pin-test-{}", now());
        let input = |pin: &str, current: Option<&str>| SetPin {
            pin: pin.into(),
            confirmation: pin.into(),
            current_pin: current.map(str::to_string),
        };
        assert_eq!(
            state.set_for(&user, input("0729", None)).await.unwrap().0["configured"],
            true
        );
        assert_eq!(
            state
                .set_for(&user, input("0000", None))
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let action = Action::Intent {
            intent_id: "purchase-one".into(),
        };
        let grant = state.approve(&user, "0729".into(), &action).await.unwrap();
        state.require(&user, &grant, &action, false).await.unwrap();
        // Repeated reports stay limited to this one payment, including later signing steps.
        state.require(&user, &grant, &action, false).await.unwrap();
        assert!(state
            .require("another-user", &grant, &action, false)
            .await
            .is_err());
        assert!(state
            .require(
                &user,
                &grant,
                &Action::Intent {
                    intent_id: "purchase-two".into()
                },
                false
            )
            .await
            .is_err());
        let direct = Action::Tpsl {
            position_id: "BTC".into(),
            take_profit_pct: Some(10.0),
            stop_loss_pct: Some(5.0),
        };
        let once = state.approve(&user, "0729".into(), &direct).await.unwrap();
        state.require(&user, &once, &direct, true).await.unwrap();
        assert!(state.require(&user, &once, &direct, true).await.is_err());
        let _ = state
            .set_for(&user, input("9821", Some("0729")))
            .await
            .unwrap();
        assert!(state.require(&user, &grant, &action, false).await.is_err());
        let second = PinState::new().await.unwrap();
        let expired = second.approve(&user, "9821".into(), &action).await.unwrap();
        second
            .storage()
            .unwrap()
            .lock()
            .await
            .execute(
                "UPDATE atlas_pin_authorizations SET expires_ms=0 WHERE user_id=$1",
                &[&user],
            )
            .await
            .unwrap();
        assert!(state
            .require(&user, &expired, &action, false)
            .await
            .is_err());
        let mut tasks = Vec::new();
        for i in 0..12 {
            let state = if i % 2 == 0 {
                state.clone()
            } else {
                second.clone()
            };
            let user = user.clone();
            let action = action.clone();
            tasks.push(tokio::spawn(async move {
                state
                    .approve(&user, "1111".into(), &action)
                    .await
                    .unwrap_err()
                    .0
            }));
        }
        let mut wrong = 0;
        let mut locked = 0;
        for t in tasks {
            match t.await.unwrap() {
                StatusCode::FORBIDDEN => wrong += 1,
                StatusCode::TOO_MANY_REQUESTS => locked += 1,
                other => panic!("{other}"),
            }
        }
        assert_eq!(wrong, 4);
        assert_eq!(locked, 8);
        let restarted = PinState::new().await.unwrap();
        assert_eq!(
            restarted
                .approve(&user, "9821".into(), &action)
                .await
                .unwrap_err()
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            restarted
                .set_for(&user, input("2345", Some("9821")))
                .await
                .unwrap_err()
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        let pg = restarted.storage().unwrap().lock().await;
        let row=pg.query_one("SELECT failures,lock_level,locked_until_ms FROM atlas_transaction_pins WHERE user_id=$1",&[&user]).await.unwrap();
        assert_eq!(row.get::<_, i32>(0), 0);
        assert_eq!(row.get::<_, i32>(1), 1);
        assert!(row.get::<_, i64>(2) > now());
        pg.execute(
            "DELETE FROM atlas_pin_authorizations WHERE user_id=$1",
            &[&user],
        )
        .await
        .unwrap();
        pg.execute(
            "DELETE FROM atlas_transaction_pins WHERE user_id=$1",
            &[&user],
        )
        .await
        .unwrap();
        println!("PIN storage: create/change protected; wrong user/action/expiry/reuse rejected; 12 concurrent guesses across two clients yielded 4 incorrect + 8 locked; restart kept the lock; no money moved.");
    }
}
