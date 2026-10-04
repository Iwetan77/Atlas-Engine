//! Atlas Links: money anyone can claim with a link. The sender's phone makes the link's secret (an
//! EVM key) and only its address reaches the engine, until someone claims. The sender's plan funds
//! that escrow with USDC on Base, plus a few cents so Relay's fee doesn't come out of the gift.
//! Whoever opens the link and signs in claims it: the bridge, given the secret they hold, signs one
//! authorization paying the escrow out through Relay to their Solana wallet, with no gas. The sender
//! can take it back the same way, and after 30 days only the sender can.
use super::*;
use serde_json::{json, Value};

pub(super) const LINK_DAYS: u64 = 30;
// Funded on top of the amount: Relay's payout fee (about 3 cents) and its 0.5% price room, so the
// friend gets at least the full amount.
pub(super) const CLAIM_FEE_UNITS: u128 = 60_000;
// A claim that was cut off halfway (the engine restarted) can be tried again after this.
const CLAIM_STALE_MS: u64 = 10 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Link {
    // The escrow's address (lowercase), which is also the link's id.
    pub(super) escrow: String,
    pub(super) owner: String,
    pub(super) sender_name: Option<String>,
    pub(super) sender_handle: Option<String>,
    // What the claimer gets; the escrow holds CLAIM_FEE_UNITS more.
    pub(super) amount_units: u128,
    pub(super) currency: String,
    pub(super) message: Option<String>,
    // open → claiming → claimed (or cancelled, when the sender took it back).
    pub(super) state: String,
    pub(super) created_ms: u64,
    pub(super) expires_ms: u64,
    #[serde(default)]
    pub(super) claim_started_ms: u64,
    // Relay's request paying it out, and who it went to.
    #[serde(default)]
    pub(super) request_id: Option<String>,
    #[serde(default)]
    pub(super) claimer: Option<String>,
}

#[derive(Clone)]
pub(super) struct LinkStore {
    postgres: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<String, Link>>>,
}

impl LinkStore {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("cash-link database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_cashlinks (
                    escrow TEXT PRIMARY KEY,
                    owner TEXT NOT NULL,
                    state TEXT NOT NULL,
                    payload TEXT NOT NULL
                )",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            postgres,
            memory: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(super) async fn get(&self, escrow: &str) -> Result<Option<Link>, ApiError> {
        if let Some(pg) = &self.postgres {
            let row = pg
                .query_opt(
                    "SELECT payload FROM atlas_cashlinks WHERE escrow=$1",
                    &[&escrow],
                )
                .await
                .map_err(internal)?;
            return row
                .map(|r| serde_json::from_str(r.get::<_, &str>("payload")).map_err(internal))
                .transpose();
        }
        Ok(self.memory.lock().map_err(internal)?.get(escrow).cloned())
    }

    // A new link; false if one already uses this escrow.
    pub(super) async fn insert(&self, link: &Link) -> Result<bool, ApiError> {
        let payload = serde_json::to_string(link).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            let added = pg
                .execute(
                    "INSERT INTO atlas_cashlinks (escrow, owner, state, payload) VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
                    &[&link.escrow, &link.owner, &link.state, &payload],
                )
                .await
                .map_err(internal)?;
            return Ok(added == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        if memory.contains_key(&link.escrow) {
            return Ok(false);
        }
        memory.insert(link.escrow.clone(), link.clone());
        Ok(true)
    }

    // Saves `link` only if it's still in state `from`: two claims racing, one wins.
    pub(super) async fn transition(&self, from: &str, link: &Link) -> Result<bool, ApiError> {
        let payload = serde_json::to_string(link).map_err(internal)?;
        if let Some(pg) = &self.postgres {
            let changed = pg
                .execute(
                    "UPDATE atlas_cashlinks SET state=$2, payload=$3 WHERE escrow=$1 AND state=$4",
                    &[&link.escrow, &link.state, &payload, &from],
                )
                .await
                .map_err(internal)?;
            return Ok(changed == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        match memory.get(&link.escrow) {
            Some(current) if current.state == from => {
                memory.insert(link.escrow.clone(), link.clone());
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// The link id in a URL, as the escrow address: lowercase, with 0x.
pub(super) fn escrow_id(raw: &str) -> Option<String> {
    let hex = raw.trim().trim_start_matches("0x").to_ascii_lowercase();
    (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("0x{hex}"))
}

fn conflict(message: &str) -> ApiError {
    (StatusCode::CONFLICT, message.into())
}

// What the claim page shows. Mid-claim reads as claimed so nobody tries twice.
fn public_state(link: &Link, now: u64) -> &'static str {
    match link.state.as_str() {
        "claimed" | "claiming" => "claimed",
        "cancelled" => "cancelled",
        _ if now >= link.expires_ms => "expired",
        _ => "open",
    }
}

pub(super) async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let escrow = escrow_id(&id).ok_or((StatusCode::NOT_FOUND, "no such link".into()))?;
    let link = state
        .links
        .get(&escrow)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "no such link".into()))?;
    let rate = app_balance::fx_rate(&link.currency).await?;
    // The amount in the sender's currency at today's rate (micros), to the cent.
    let micros = link.amount_units.saturating_mul(rate) / 1_000_000;
    Ok(Json(json!({
        "linkId":link.escrow,
        "amount":{"amount":format!("{}.{:02}", micros / 1_000_000, micros % 1_000_000 / 10_000),
            "currency":link.currency},
        "sender":{"displayName":link.sender_name,"handle":link.sender_handle},
        "message":link.message,
        "state":public_state(&link, now_ms()),
        "expiresAtUnixMs":link.expires_ms
    })))
}

#[derive(Deserialize)]
pub(super) struct ClaimBody {
    secret: String,
}

fn status_of(link: &Link, state: &str, error: Option<String>) -> markets::IntentStatus {
    markets::IntentStatus {
        intent_id: format!("cashlink-{}", link.escrow),
        stage: "settle".into(),
        state: state.into(),
        tx_ids: link.request_id.iter().cloned().collect(),
        error,
    }
}

// Claims a link: to the claimer's Solana wallet, or, for the sender, back to their own.
pub(super) async fn claim(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ClaimBody>,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    pin::require_action(
        &state,
        &headers,
        pin::Action::Cashlink {
            link_id: id.clone(),
            secret: body.secret.clone(),
        },
        true,
    )
    .await?;
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let to = user
        .solana_wallet
        .clone()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| conflict("Your account is still being set up. Try again in a moment."))?;
    let escrow = escrow_id(&id).ok_or((StatusCode::NOT_FOUND, "no such link".into()))?;
    let link = state
        .links
        .get(&escrow)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "no such link".into()))?;
    let now = now_ms();
    let sender = link.owner == user.user_id;
    let stale_claim = link.state == "claiming" && now > link.claim_started_ms + CLAIM_STALE_MS;
    if link.state != "open" && !stale_claim {
        return Err(conflict(if link.state == "cancelled" {
            "The sender took this link back."
        } else {
            "This link has already been claimed."
        }));
    }
    if now >= link.expires_ms && !sender {
        return Err(conflict(
            "This link expired. The sender can take the money back.",
        ));
    }
    let held = state
        .markets
        .base
        .balance_of(engine_execution::swaps::uniswap::BASE_USDC, &escrow)
        .await
        .map_err(internal)?;
    if held == 0 {
        return Err(conflict(
            "The money for this link is still on its way. Try again in a minute.",
        ));
    }
    let payout = state
        .relay_link
        .base_to_solana_all(&escrow, &to, held)
        .await
        .map_err(|_| conflict("Couldn't claim right now. Try again shortly."))?;
    // Only one claim gets past here.
    let mut claiming = link.clone();
    claiming.state = "claiming".into();
    claiming.claim_started_ms = now;
    if !state.links.transition(&link.state, &claiming).await? {
        return Err(conflict("This link is already being claimed."));
    }
    let undo = |reason: String| {
        let state = state.clone();
        let (claiming, link) = (claiming.clone(), link.clone());
        async move {
            eprintln!("link {}: claim stopped: {reason}", link.escrow);
            let mut open = link;
            open.state = "open".into();
            let _ = state.links.transition(&claiming.state, &open).await;
        }
    };
    let signature = match sign_payout(&state, &headers, &body.secret, &payout.typed_data).await {
        Ok(signature) => signature,
        Err(reason) => {
            undo(reason).await;
            return Err((
                StatusCode::FORBIDDEN,
                "This link is incomplete or wrong. Ask the sender to share it again.".into(),
            ));
        }
    };
    if let Err(error) = state
        .relay_link
        .submit(&payout.request_id, &payout.api, &signature)
        .await
    {
        undo(error.to_string()).await;
        return Err(conflict("Couldn't claim right now. Try again shortly."));
    }
    let mut done = claiming.clone();
    done.state = if sender { "cancelled" } else { "claimed" }.into();
    done.request_id = Some(payout.request_id);
    done.claimer = Some(user.user_id);
    state.links.transition("claiming", &done).await?;
    Ok(Json(status_of(&done, "pending", None)))
}

// The escrow's signature over Relay's payout authorization, made by the bridge from the link's
// secret (and only for a payout to Relay's pinned receiver).
async fn sign_payout(
    state: &AppState,
    headers: &HeaderMap,
    secret: &str,
    typed_data: &Value,
) -> Result<String, String> {
    let AuthMode::Privy { bridge_url, http } = &state.auth else {
        return Err("Privy signing unavailable".into());
    };
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or("Privy access token required")?;
    let response = http
        .post(format!("{bridge_url}/escrow/sign-authorization"))
        .json(&json!({"accessToken":token,"secret":secret,"typedData":typed_data}))
        .send()
        .await
        .map_err(|_| "Privy bridge unavailable".to_string())?;
    let ok = response.status().is_success();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !ok {
        return Err(body["error"].as_str().unwrap_or("not signed").into());
    }
    body["signature"]
        .as_str()
        .filter(|s| s.len() == 132 && s.starts_with("0x"))
        .map(str::to_string)
        .ok_or_else(|| "signature missing".into())
}

// GET /v1/intents/cashlink-{escrow}: Relay says when the payout has landed.
pub(super) async fn status(
    state: AppState,
    headers: HeaderMap,
    intent_id: String,
) -> Result<Json<markets::IntentStatus>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let escrow = intent_id
        .strip_prefix("cashlink-")
        .and_then(escrow_id)
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    let link = state
        .links
        .get(&escrow)
        .await?
        .filter(|l| l.claimer.as_deref() == Some(user.user_id.as_str()) || l.owner == user.user_id)
        .ok_or((StatusCode::NOT_FOUND, "intent not found".into()))?;
    let Some(request) = link.request_id.clone() else {
        return Ok(Json(status_of(&link, "pending", None)));
    };
    Ok(Json(match state.relay_link.state(&request).await {
        Ok(engine_execution::layerswap::SwapState::Completed) => status_of(&link, "filled", None),
        Ok(engine_execution::layerswap::SwapState::Failed(_)) => {
            // Relay returns the money to the escrow: the link can be claimed again.
            let mut open = link.clone();
            open.state = "open".into();
            open.request_id = None;
            open.claimer = None;
            let _ = state.links.transition(&link.state, &open).await;
            status_of(
                &link,
                "failed",
                Some(
                    "The claim didn't go through, so the money is still in the link. Try again."
                        .into(),
                ),
            )
        }
        _ => status_of(&link, "pending", None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(state: &str, expires_ms: u64) -> Link {
        Link {
            escrow: "0x1111111111111111111111111111111111111111".into(),
            owner: "did:privy:sender".into(),
            sender_name: Some("Ade".into()),
            sender_handle: Some("ade".into()),
            amount_units: 5_000_000,
            currency: "NGN".into(),
            message: None,
            state: state.into(),
            created_ms: 0,
            expires_ms,
            claim_started_ms: 0,
            request_id: None,
            claimer: None,
        }
    }

    #[test]
    fn link_ids_are_escrow_addresses() {
        assert_eq!(
            escrow_id("0xAbCdEf0123456789abcdef0123456789ABCDEF01").as_deref(),
            Some("0xabcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(
            escrow_id("abcdef0123456789abcdef0123456789abcdef01").as_deref(),
            Some("0xabcdef0123456789abcdef0123456789abcdef01")
        );
        assert_eq!(escrow_id("0x12"), None);
        assert_eq!(escrow_id("not-a-link"), None);
    }

    #[test]
    fn the_claim_page_sees_one_simple_state() {
        assert_eq!(public_state(&link("open", 10), 5), "open");
        assert_eq!(public_state(&link("open", 10), 10), "expired");
        assert_eq!(public_state(&link("claiming", 10), 5), "claimed");
        assert_eq!(public_state(&link("claimed", 10), 50), "claimed");
        assert_eq!(public_state(&link("cancelled", 10), 5), "cancelled");
    }

    #[tokio::test]
    async fn only_one_claim_moves_a_link() {
        let store = LinkStore {
            postgres: None,
            memory: Arc::new(Mutex::new(HashMap::new())),
        };
        let open = link("open", u64::MAX);
        assert!(store.insert(&open).await.unwrap());
        assert!(!store.insert(&open).await.unwrap());
        let mut claiming = open.clone();
        claiming.state = "claiming".into();
        assert!(store.transition("open", &claiming).await.unwrap());
        assert!(!store.transition("open", &claiming).await.unwrap());
        let back: Link = serde_json::from_str(&serde_json::to_string(&claiming).unwrap()).unwrap();
        assert_eq!(store.get(&open.escrow).await.unwrap(), Some(back));
    }
}
