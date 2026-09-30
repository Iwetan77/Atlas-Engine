//! Balance API with Privy identity checks. The Phase 2 test account remains
//! available only when the operator explicitly enables local demo mode.

mod app_balance;
mod earn;
mod markets;
mod near_intents;
mod perps;
mod positions;
mod relay;
mod social;

use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use engine_execution::{
    balance::{include_solana_and_outgoing, usdc_buckets, OutgoingGatewayMovement},
    funding::deposit::{DepositTarget, EvmDepositScanner},
    gateway::{GatewayClient, GatewayEnvironment, GatewaySource},
    solana::{SolanaAtaPreflight, SolanaNetwork},
};
use serde::{Deserialize, Serialize};
use tower_http::cors::{AllowOrigin, CorsLayer};

type ApiError = (StatusCode, String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AtlasNetwork {
    Mainnet,
    Testnet,
}
impl AtlasNetwork {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        match env::var("ATLAS_NETWORK")
            .unwrap_or_else(|_| "mainnet".into())
            .as_str()
        {
            "mainnet" => Ok(Self::Mainnet),
            "testnet" => Ok(Self::Testnet),
            _ => Err("ATLAS_NETWORK must be mainnet or testnet".into()),
        }
    }
}

#[derive(Clone)]
struct AppState {
    user_id: String,
    base_wallet: String,
    solana_owner: String,
    internal_token: String,
    movement_path: PathBuf,
    movement: Arc<Mutex<Option<MovementRecord>>>,
    scanner: EvmDepositScanner,
    gateway: GatewayClient,
    solana: SolanaAtaPreflight,
    solana_mainnet: SolanaAtaPreflight,
    auth: AuthMode,
    network: AtlasNetwork,
    markets: markets::MarketState,
    near: near_intents::NearState,
    paradex: engine_execution::perps::ParadexClient,
    paradex_tokens: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    perps_trade: perps::TradeState,
    perp_cache: perps::PerpCache,
    layerswap: engine_execution::layerswap::LayerswapClient,
    earn: earn::EarnState,
    social: social::SocialState,
    trades: positions::TradeBook,
}

#[derive(Clone)]
enum AuthMode {
    Privy {
        bridge_url: String,
        http: reqwest::Client,
    },
    LocalDemo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MovementRecord {
    amount_base_units: u128,
    gateway_before_base_units: u128,
    solana_before_base_units: u128,
    #[serde(default)]
    transfer_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BalanceResponse {
    user: String,
    asset: &'static str,
    wallet_base_units: String,
    gateway_confirmed_base_units: String,
    gateway_pending_deposit_base_units: String,
    solana_wallet_base_units: String,
    outgoing_in_flight_base_units: String,
    spendable_base_units: String,
    total_base_units: Option<String>,
    display_usd: Option<String>,
    integrity: &'static str,
    active_transfer_id: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let user_id = env::var("ATLAS_TEST_USER_ID").unwrap_or_default();
    let base_wallet = env::var("ATLAS_TEST_WALLET_ADDRESS").unwrap_or_default();
    let solana_owner = env::var("ATLAS_SOLANA_OWNER_ADDRESS").unwrap_or_default();
    let internal_token = env::var("ATLAS_INTERNAL_TOKEN").unwrap_or_default();
    let movement_path = PathBuf::from(
        env::var("ATLAS_MOVEMENT_LEDGER_PATH")
            .unwrap_or_else(|_| ".env.gateway-movement.json".into()),
    );
    let movement = if movement_path.exists() {
        Some(serde_json::from_slice(&std::fs::read(&movement_path)?)?)
    } else {
        None
    };
    let base_rpc =
        env::var("ATLAS_BASE_RPC_URL").unwrap_or_else(|_| "https://sepolia.base.org".into());
    let solana_rpc =
        env::var("ATLAS_SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".into());
    let relayer_path = env::var("ATLAS_SOLANA_RELAYER_KEYPAIR_PATH").unwrap_or_default();
    let auth = if env::var("ATLAS_DEMO_AUTH_BYPASS").as_deref() == Ok("1") {
        AuthMode::LocalDemo
    } else {
        let bridge_url = env::var("PRIVY_BRIDGE_URL")?;
        let parsed = reqwest::Url::parse(&bridge_url)?;
        if parsed.scheme() != "http"
            || parsed.host_str() != Some("127.0.0.1")
            || parsed.port().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err("PRIVY_BRIDGE_URL must be a bare loopback HTTP origin".into());
        }
        // Every signing and verification step goes through this bridge, and a stalled call used to
        // hold an app request open with no end. Bound each one so callers get an answer.
        AuthMode::Privy {
            bridge_url,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        }
    };
    let state = AppState {
        user_id,
        base_wallet,
        solana_owner,
        internal_token,
        movement_path,
        movement: Arc::new(Mutex::new(movement)),
        scanner: EvmDepositScanner::new(base_rpc.parse()?, 2)?,
        gateway: GatewayClient::new(GatewayEnvironment::Testnet)?,
        solana: SolanaAtaPreflight::new(SolanaNetwork::Devnet, solana_rpc, relayer_path)?,
        solana_mainnet: SolanaAtaPreflight::new(
            SolanaNetwork::Mainnet,
            env::var("ATLAS_SOLANA_MAINNET_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
            "",
        )?,
        auth,
        network: AtlasNetwork::from_env()?,
        markets: markets::MarketState::new()?.with_database().await?,
        near: near_intents::NearState::new().await?,
        paradex: engine_execution::perps::ParadexClient::new(
            &env::var("PARADEX_ENV").unwrap_or_else(|_| "prod".into()),
        )?,
        paradex_tokens: Arc::new(Mutex::new(HashMap::new())),
        perps_trade: perps::TradeState::new().await?,
        perp_cache: perps::PerpCache::default(),
        layerswap: engine_execution::layerswap::LayerswapClient::new()?,
        earn: earn::EarnState::default(),
        social: social::SocialState::new().await?,
        trades: positions::TradeBook::new().await?,
    };
    let bind: SocketAddr = env::var("ATLAS_BALANCE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()?;
    if matches!(state.auth, AuthMode::LocalDemo) && !bind.ip().is_loopback() {
        return Err("demo auth bypass requires a loopback bind address".into());
    }
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/v1/balance", get(app_balance::balance))
        .route("/v1/assets", get(markets::assets))
        .route("/v1/assets/{asset_id}/chart", get(markets::chart))
        .route(
            "/v1/perps/onboarding",
            get(perps::onboarding).post(perps::onboard),
        )
        .route("/v1/perps/markets", get(perps::markets))
        .route("/v1/perps/positions", get(perps::positions))
        .route("/v1/positions/spot", get(positions::spot))
        .route("/v1/perps/quotes", post(perps::quotes))
        .route(
            "/v1/perps/quotes/{quote_id}/execute",
            post(perps::execute_quote),
        )
        .route(
            "/v1/perps/positions/{position_id}/close-quote",
            post(perps::close_quote),
        )
        .route(
            "/v1/perps/close-quotes/{quote_id}/execute",
            post(perps::execute_close),
        )
        .route("/v1/me", get(social::me))
        .route("/v1/me/handle", post(social::set_handle))
        .route("/v1/me/avatar", post(social::set_avatar))
        .route("/v1/users/resolve", get(social::resolve_user))
        .route("/v1/offramp/banks", get(social::banks))
        .route("/v1/offramp/resolve", post(social::resolve_bank))
        .route("/v1/sends/quote", post(social::send_quote))
        .route(
            "/v1/sends/quote/{quote_id}/execute",
            post(social::execute_send),
        )
        .route("/v1/cashlinks/{link_id}", get(social::cashlink))
        .route("/v1/cashlinks/{link_id}/claim", post(social::claim))
        .route("/v1/quotes", post(markets::quote))
        .route(
            "/v1/quotes/{quote_id}/execute",
            post(markets::execute_quote),
        )
        .route("/v1/intents/{intent_id}/signed", post(markets::signed))
        .route("/v1/relay/evm", post(relay::evm))
        .route("/v1/earn/options", get(earn::options))
        .route("/v1/earn/positions", get(earn::positions))
        .route("/v1/earn/quotes", post(earn::quote))
        .route("/v1/earn/quotes/{quote_id}/execute", post(earn::execute))
        .route("/v1/intents/{intent_id}", get(markets::intent_status))
        .route("/balance/{user}", get(balance))
        .route("/balance/{user}/movement", post(register_movement))
        .with_state(state);
    if let Some(cors) = configured_cors()? {
        app = app.layer(cors);
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Atlas balance API listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// Public and secret-free: lets anyone check which commit Render is actually serving.
async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "commit": env::var("RENDER_GIT_COMMIT").unwrap_or_else(|_| "unknown".into()),
    }))
}

fn configured_cors() -> Result<Option<CorsLayer>, Box<dyn std::error::Error>> {
    let configured = env::var("ATLAS_ALLOWED_ORIGINS").unwrap_or_default();
    if configured.trim().is_empty() {
        return Ok(None);
    }
    let mut origins = Vec::new();
    for raw in configured.split(',') {
        let origin = raw.trim().trim_end_matches('/');
        let parsed = reqwest::Url::parse(origin)?;
        let is_local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1"));
        if (parsed.scheme() != "https" && !(is_local && parsed.scheme() == "http"))
            || parsed.host_str().is_none()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("ATLAS_ALLOWED_ORIGINS must contain exact HTTPS origins (HTTP is allowed only for localhost)".into());
        }
        origins.push(HeaderValue::from_str(origin)?);
    }
    Ok(Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    ))
}

async fn balance(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
) -> Result<Json<BalanceResponse>, ApiError> {
    authorize_balance(&state.auth, &headers, &user).await?;
    if user != state.user_id {
        return Err((StatusCode::NOT_FOUND, "unknown user".into()));
    }
    let target = DepositTarget {
        user_id: user.clone(),
        wallet_address: state.base_wallet.clone(),
        token_contract: "0x036CbD53842c5426634e7929541eC2318f3dCF7e".into(),
        chain_id: 84532,
    };
    let sources = [GatewaySource {
        depositor: state.base_wallet.clone(),
        domain: None,
    }];
    let (wallet, gateway, deposits, solana) = tokio::join!(
        state.scanner.wallet_token_balance(&target),
        state.gateway.balances(&sources),
        state.gateway.deposits(&sources),
        state.solana.owner_usdc_balance(&state.solana_owner),
    );
    let wallet = wallet.map_err(internal)?;
    let gateway = gateway.map_err(internal)?;
    let deposits = deposits.map_err(internal)?;
    let solana = solana.map_err(internal)?;
    let buckets =
        usdc_buckets(wallet, &state.base_wallet, &gateway, &deposits).map_err(internal)?;
    let movement = state.movement.lock().map_err(internal)?.clone();
    let outgoing = movement.as_ref().map(|m| OutgoingGatewayMovement {
        amount_base_units: m.amount_base_units,
        gateway_before_base_units: m.gateway_before_base_units,
        solana_before_base_units: m.solana_before_base_units,
    });
    let unified =
        include_solana_and_outgoing(&buckets, solana, outgoing.as_ref()).map_err(internal)?;
    let total = unified.total_accounted_base_units;
    Ok(Json(BalanceResponse {
        user,
        asset: "USDC",
        wallet_base_units: wallet.to_string(),
        gateway_confirmed_base_units: buckets.gateway_confirmed_base_units.to_string(),
        gateway_pending_deposit_base_units: buckets.gateway_pending_base_units.to_string(),
        solana_wallet_base_units: solana.to_string(),
        outgoing_in_flight_base_units: unified.outgoing_in_flight_base_units.to_string(),
        spendable_base_units: unified.spendable_base_units.to_string(),
        total_base_units: total.map(|v| v.to_string()),
        display_usd: total.map(usd),
        integrity: if total.is_some() {
            "accounted"
        } else {
            "inconsistent_snapshots"
        },
        active_transfer_id: movement.and_then(|m| m.transfer_id),
    }))
}

async fn authorize_balance(
    auth: &AuthMode,
    headers: &HeaderMap,
    user: &str,
) -> Result<(), ApiError> {
    let AuthMode::Privy { bridge_url, http } = auth else {
        return Ok(());
    };
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Privy access token required".into(),
        ))?;
    let response = http
        .post(format!("{bridge_url}/verify"))
        .json(&serde_json::json!({"accessToken": token}))
        .send()
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Privy verification unavailable".into(),
            )
        })?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err((
            StatusCode::UNAUTHORIZED,
            "invalid or expired Privy access token".into(),
        ));
    }
    let response = response.error_for_status().map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy verification unavailable".into(),
        )
    })?;
    let verified: serde_json::Value = response.json().await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Privy verification unavailable".into(),
        )
    })?;
    if verified["userId"].as_str() != Some(user) {
        return Err((
            StatusCode::FORBIDDEN,
            "balance belongs to another user".into(),
        ));
    }
    Ok(())
}

async fn register_movement(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    Json(record): Json<MovementRecord>,
) -> Result<StatusCode, ApiError> {
    if user != state.user_id {
        return Err((StatusCode::NOT_FOUND, "unknown user".into()));
    }
    if state.internal_token.is_empty()
        || headers
            .get("x-atlas-internal-token")
            .and_then(|v| v.to_str().ok())
            != Some(state.internal_token.as_str())
    {
        return Err((StatusCode::UNAUTHORIZED, "internal token required".into()));
    }
    if record.amount_base_units == 0 {
        return Err((StatusCode::BAD_REQUEST, "amount must be positive".into()));
    }
    let bytes = serde_json::to_vec(&record).map_err(internal)?;
    let temporary_path = state.movement_path.with_extension("tmp");
    std::fs::write(&temporary_path, bytes).map_err(internal)?;
    std::fs::rename(&temporary_path, &state.movement_path).map_err(internal)?;
    *state.movement.lock().map_err(internal)? = Some(record);
    Ok(StatusCode::NO_CONTENT)
}

// Display currencies Atlas prices in. FX comes from Frankfurter with Coinbase as the fallback.
pub(crate) const DISPLAY_CURRENCIES: [&str; 7] = ["USD", "NGN", "EUR", "GBP", "ZAR", "KES", "GHS"];

fn usd(base_units: u128) -> String {
    format!("{}.{:06}", base_units / 1_000_000, base_units % 1_000_000)
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn balance_auth_rejects_missing_token() {
        let auth = AuthMode::Privy {
            bridge_url: "http://127.0.0.1:3101".into(),
            http: reqwest::Client::new(),
        };
        let error = authorize_balance(&auth, &HeaderMap::new(), "did:privy:alice")
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn balance_auth_rejects_another_verified_user() {
        let bridge = Router::new().route(
            "/verify",
            post(|| async { Json(serde_json::json!({"userId":"did:privy:alice"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });
        let auth = AuthMode::Privy {
            bridge_url: format!("http://{address}"),
            http: reqwest::Client::new(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer token".parse().unwrap(),
        );
        let error = authorize_balance(&auth, &headers, "did:privy:bob")
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        authorize_balance(&auth, &headers, "did:privy:alice")
            .await
            .unwrap();
    }
}
