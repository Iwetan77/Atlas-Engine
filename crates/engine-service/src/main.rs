//! Local Phase 2 balance API. The demo user and internal movement recorder are
//! configured by the host; the app's production authentication is a later gate.

use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
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

type ApiError = (StatusCode, String);

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
    let user_id = env::var("ATLAS_TEST_USER_ID")?;
    let base_wallet = env::var("ATLAS_TEST_WALLET_ADDRESS")?;
    let solana_owner = env::var("ATLAS_SOLANA_OWNER_ADDRESS")?;
    let internal_token = env::var("ATLAS_INTERNAL_TOKEN")?;
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
    let relayer_path = env::var("ATLAS_SOLANA_RELAYER_KEYPAIR_PATH")?;
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
    };
    let app = Router::new()
        .route("/balance/{user}", get(balance))
        .route("/balance/{user}/movement", post(register_movement))
        .with_state(state);
    let bind: SocketAddr = env::var("ATLAS_BALANCE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("Atlas balance API listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn balance(
    State(state): State<AppState>,
    Path(user): Path<String>,
) -> Result<Json<BalanceResponse>, ApiError> {
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

async fn register_movement(
    State(state): State<AppState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    Json(record): Json<MovementRecord>,
) -> Result<StatusCode, ApiError> {
    if user != state.user_id {
        return Err((StatusCode::NOT_FOUND, "unknown user".into()));
    }
    if headers
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

fn usd(base_units: u128) -> String {
    format!("{}.{:06}", base_units / 1_000_000, base_units % 1_000_000)
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, error.to_string())
}
