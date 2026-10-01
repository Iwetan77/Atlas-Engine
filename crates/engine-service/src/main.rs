//! The Atlas Engine HTTP API. Every route checks the caller's Privy identity; a fixed local test
//! account is available only when the operator explicitly enables local demo mode.

mod app_balance;
mod cashlinks;
mod earn;
mod gasless;
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
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    routing::{get, post},
    Json, Router,
};
use engine_execution::solana::{SolanaAtaPreflight, SolanaNetwork};
use serde::{Deserialize, Serialize};
use tower_http::cors::{AllowOrigin, CorsLayer};

type ApiError = (StatusCode, String);

#[derive(Clone)]
struct AppState {
    user_id: String,
    base_wallet: String,
    solana_owner: String,
    solana_mainnet: SolanaAtaPreflight,
    auth: AuthMode,
    markets: markets::MarketState,
    near: near_intents::NearState,
    paradex: engine_execution::perps::ParadexClient,
    paradex_tokens: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    perps_trade: perps::TradeState,
    perp_cache: perps::PerpCache,
    layerswap: engine_execution::layerswap::LayerswapClient,
    relay_link: engine_execution::relay_link::RelayClient,
    cow: engine_execution::cow::CowClient,
    links: cashlinks::LinkStore,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let user_id = env::var("ATLAS_TEST_USER_ID").unwrap_or_default();
    let base_wallet = env::var("ATLAS_TEST_WALLET_ADDRESS").unwrap_or_default();
    let solana_owner = env::var("ATLAS_SOLANA_OWNER_ADDRESS").unwrap_or_default();
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
        solana_mainnet: SolanaAtaPreflight::new(
            SolanaNetwork::Mainnet,
            env_url(
                "ATLAS_SOLANA_MAINNET_RPC_URL",
                "https://api.mainnet-beta.solana.com",
            ),
            "",
        )?,
        auth,
        markets: markets::MarketState::new()?.with_database().await?,
        near: near_intents::NearState::new().await?,
        paradex: engine_execution::perps::ParadexClient::new(
            &env::var("PARADEX_ENV").unwrap_or_else(|_| "prod".into()),
        )?,
        paradex_tokens: Arc::new(Mutex::new(HashMap::new())),
        perps_trade: perps::TradeState::new().await?,
        perp_cache: perps::PerpCache::default(),
        layerswap: engine_execution::layerswap::LayerswapClient::new()?,
        relay_link: engine_execution::relay_link::RelayClient::new(env::var("RELAY_API_KEY").ok())?,
        cow: engine_execution::cow::CowClient::new()?,
        links: cashlinks::LinkStore::new().await?,
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
    perps::keep_warm(state.clone());
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
        .route("/v1/cashlinks/{link_id}", get(cashlinks::get))
        .route("/v1/cashlinks/{link_id}/claim", post(cashlinks::claim))
        .route("/v1/quotes", post(markets::quote))
        .route(
            "/v1/quotes/{quote_id}/execute",
            post(markets::execute_quote),
        )
        .route("/v1/intents/{intent_id}/signed", post(markets::signed))
        .route(
            "/v1/intents/{intent_id}/next",
            get(markets::next_transactions),
        )
        .route("/v1/relay/evm", post(relay::evm))
        .route("/v1/deposit/networks", get(near_intents::deposit_networks))
        .route("/v1/deposit/quote", post(near_intents::deposit_quote))
        .route("/v1/deposit/status", get(near_intents::deposit_status))
        .route("/v1/earn/options", get(earn::options))
        .route("/v1/earn/positions", get(earn::positions))
        .route("/v1/earn/quotes", post(earn::quote))
        .route("/v1/earn/quotes/{quote_id}/execute", post(earn::execute))
        .route("/v1/intents/{intent_id}", get(markets::intent_status))
        .with_state(state);
    if let Some(cors) = configured_cors()? {
        app = app.layer(cors);
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Atlas balance API listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// Public and secret-free: which commit Render is serving, and which RPC hosts it reads (hosts only;
// a key in the URL never shows).
async fn health() -> Json<serde_json::Value> {
    let host = |var: &str, default: &str| {
        reqwest::Url::parse(&env_url(var, default))
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    };
    Json(serde_json::json!({
        "ok": true,
        "commit": env::var("RENDER_GIT_COMMIT").unwrap_or_else(|_| "unknown".into()),
        "rpc": {
            "solana": host("ATLAS_SOLANA_MAINNET_RPC_URL", "https://api.mainnet-beta.solana.com"),
            "base": host("ATLAS_BASE_MAINNET_RPC_URL", "https://base-rpc.publicnode.com"),
        },
    }))
}

// An RPC URL from the environment, forgiving what a dashboard paste adds (spaces, quotes, the < >
// of a placeholder). Still not a URL: the public default, logged by name only (it may hold a key).
pub(crate) fn env_url(var: &str, default: &str) -> String {
    let Ok(raw) = env::var(var) else {
        return default.into();
    };
    clean_url(&raw).unwrap_or_else(|| {
        eprintln!("{var} is not a valid URL; using the public endpoint");
        default.into()
    })
}

fn clean_url(raw: &str) -> Option<String> {
    let value = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '<' | '>'))
        .trim();
    let url = reqwest::Url::parse(value).ok()?;
    (matches!(url.scheme(), "https" | "http") && url.host_str().is_some())
        .then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_rpc_urls_are_forgiven_or_refused() {
        let helius = "https://mainnet.helius-rpc.com/?api-key=abc";
        assert_eq!(clean_url(helius).as_deref(), Some(helius));
        assert_eq!(
            clean_url(&format!(" \"{helius}\"\n")).as_deref(),
            Some(helius)
        );
        assert_eq!(clean_url(&format!("<{helius}>")).as_deref(), Some(helius));
        assert_eq!(clean_url("mainnet.helius-rpc.com/?api-key=abc"), None);
        assert_eq!(clean_url("wss://mainnet.helius-rpc.com"), None);
        assert_eq!(clean_url(""), None);
    }
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

// Display currencies Atlas prices in. FX comes from Frankfurter with Coinbase as the fallback.
pub(crate) const DISPLAY_CURRENCIES: [&str; 7] = ["USD", "NGN", "EUR", "GBP", "ZAR", "KES", "GHS"];

fn usd(base_units: u128) -> String {
    format!("{}.{:06}", base_units / 1_000_000, base_units % 1_000_000)
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}
