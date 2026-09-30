//! Card top-ups through Circle's Onramp Kit: a short-lived hosted widget that delivers USDC straight
//! to the user's own Base wallet, which is their Atlas balance. Needs CIRCLE_ONRAMP_API_KEY on the
//! server (CIRCLE_ONRAMP_ENV=sandbox for Circle's test widget; production otherwise), and
//! CIRCLE_ONRAMP_REFERRER_DOMAIN (the domain registered in the Circle Console) for cards.
use super::*;
use engine_execution::funding::circle::{CircleEnvironment, CircleOnrampClient, CreateSession};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

// How long the app may treat a widget URL as fresh; it opens it straight away.
const SESSION_MS: u128 = 10 * 60_000;

pub(super) fn client_from_env() -> Result<Option<CircleOnrampClient>, Box<dyn std::error::Error>> {
    let Some(key) = env::var("CIRCLE_ONRAMP_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        return Ok(None);
    };
    let environment = match env::var("CIRCLE_ONRAMP_ENV").as_deref() {
        Ok("sandbox") => CircleEnvironment::Sandbox,
        Ok("production") | Err(_) => CircleEnvironment::Production,
        Ok(other) => {
            return Err(
                format!("CIRCLE_ONRAMP_ENV must be sandbox or production, not {other}").into(),
            )
        }
    };
    Ok(Some(CircleOnrampClient::new(key, environment)?))
}

#[derive(Deserialize)]
pub(super) struct SessionRequest {
    chain: Option<String>,
}

pub(super) async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SessionRequest>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let client = state.onramp.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Card top-ups aren't switched on yet".into(),
    ))?;
    // The balance lives on Base first; card money lands there.
    if req.chain.as_deref().is_some_and(|c| c != "base") {
        return Err((StatusCode::BAD_REQUEST, "card top-ups land on Base".into()));
    }
    let wallet = user.evm_wallet.filter(|w| !w.is_empty()).ok_or((
        StatusCode::CONFLICT,
        "Your wallet is still being set up. Try again in a moment.".into(),
    ))?;
    // The widget URL carries a session token: never logged, never cached.
    let referrer = env::var("CIRCLE_ONRAMP_REFERRER_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty());
    let session = client
        .create_session(CreateSession {
            app_user_id: &user.user_id,
            destination_address: &wallet,
            destination_chain: "BASE",
            referrer_domain: referrer.as_deref(),
        })
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Card top-up isn't available right now ({e})"),
            )
        })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(internal)?
        .as_millis();
    Ok(Json(
        json!({"widgetUrl": session.widget_url, "expiresAtUnixMs": now + SESSION_MS}),
    ))
}
