//! Persistent one-shot price alerts. Only generic alerts leave Atlas through push.rs.
use super::*;
use axum::extract::Query;
use rand::RngCore;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone)]
pub(super) struct AlertState {
    pg: Option<Arc<AsyncMutex<tokio_postgres::Client>>>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Alert {
    id: String,
    asset_id: String,
    symbol: String,
    target_usd: String,
    direction: String,
    state: String,
    created_at: i64,
    triggered_at: Option<i64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Create {
    asset_id: String,
    target_usd: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListQuery {
    asset_id: Option<String>,
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn unavailable(_: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Price alerts are temporarily unavailable. Try again shortly.".into(),
    )
}
fn target(text: &str) -> Result<f64, ApiError> {
    text.parse::<f64>()
        .ok()
        .filter(|p| p.is_finite() && *p >= 1e-12 && *p <= 1e12)
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Enter a positive USD price.".into(),
        ))
}
fn hit(direction: &str, price: f64, target: f64) -> bool {
    price.is_finite()
        && price > 0.0
        && match direction {
            "above" => price >= target,
            "below" => price <= target,
            _ => false,
        }
}
fn item(r: &tokio_postgres::Row) -> Alert {
    Alert {
        id: r.get("id"),
        asset_id: r.get("asset_id"),
        symbol: r.get("symbol"),
        target_usd: r.get::<_, f64>("target_usd").to_string(),
        direction: r.get("direction"),
        state: r.get("state"),
        created_at: r.get("created_at"),
        triggered_at: r.get("triggered_at"),
    }
}
impl AlertState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let pg = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("Price alerts database connection ended");
                }
            });
            client.batch_execute("CREATE TABLE IF NOT EXISTS atlas_price_alerts(
                id TEXT PRIMARY KEY,owner TEXT NOT NULL,asset_id TEXT NOT NULL,symbol TEXT NOT NULL,
                target_usd DOUBLE PRECISION NOT NULL,direction TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'active',
                created_at BIGINT NOT NULL,checked_at BIGINT NOT NULL DEFAULT 0,triggered_at BIGINT,fired_price DOUBLE PRECISION);
                CREATE INDEX IF NOT EXISTS atlas_price_alerts_owner ON atlas_price_alerts(owner,created_at DESC);
                CREATE INDEX IF NOT EXISTS atlas_price_alerts_pending ON atlas_price_alerts(state,checked_at);").await?;
            Some(Arc::new(AsyncMutex::new(client)))
        } else {
            None
        };
        Ok(Self { pg })
    }
    fn database(&self) -> Result<&Arc<AsyncMutex<tokio_postgres::Client>>, ApiError> {
        self.pg
            .as_ref()
            .ok_or_else(|| unavailable("No price alert database"))
    }
}
pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let rows=state.price_alerts.database()?.lock().await.query(
        "SELECT * FROM atlas_price_alerts WHERE owner=$1 AND ($2::TEXT IS NULL OR asset_id=$2) AND state<>'cancelled' ORDER BY created_at DESC LIMIT 100",
        &[&owner,&q.asset_id]).await.map_err(unavailable)?;
    Ok(Json(
        json!({"alerts":rows.iter().map(item).collect::<Vec<_>>()}),
    ))
}
pub(super) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let threshold = target(&input.target_usd)?;
    if input.asset_id.is_empty()
        || input.asset_id.len() > 200
        || input.asset_id.chars().any(char::is_control)
    {
        return Err((StatusCode::BAD_REQUEST, "Choose an asset first.".into()));
    }
    let (symbol, current) = markets::alert_price(&state, &input.asset_id).await?;
    if !current.is_finite() || current <= 0.0 {
        return Err(unavailable("Price unavailable"));
    }
    let direction = if threshold >= current {
        "above"
    } else {
        "below"
    };
    let mut random = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let mut pg = state.price_alerts.database()?.lock().await;
    let tx = pg.transaction().await.map_err(unavailable)?;
    tx.query_one(
        "SELECT pg_advisory_xact_lock(hashtextextended($1,831451))",
        &[&owner],
    )
    .await
    .map_err(unavailable)?;
    if let Some(row)=tx.query_opt("SELECT * FROM atlas_price_alerts WHERE owner=$1 AND asset_id=$2 AND target_usd=$3 AND state='active'",
        &[&owner,&input.asset_id,&threshold]).await.map_err(unavailable)? {
        tx.commit().await.map_err(unavailable)?;return Ok(Json(json!({"alert":item(&row)})));
    }
    let count:i64=tx.query_one("SELECT COUNT(*) FROM atlas_price_alerts WHERE owner=$1 AND state IN ('active','firing')",&[&owner]).await.map_err(unavailable)?.get(0);
    if count >= 20 {
        return Err((
            StatusCode::BAD_REQUEST,
            "You can keep 20 active price alerts. Remove one before adding another.".into(),
        ));
    }
    let row=tx.query_one("INSERT INTO atlas_price_alerts(id,owner,asset_id,symbol,target_usd,direction,created_at) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING *",
        &[&id,&owner,&input.asset_id,&symbol,&threshold,&direction,&now()]).await.map_err(unavailable)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(Json(json!({"alert":item(&row)})))
}
pub(super) async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let affected=state.price_alerts.database()?.lock().await.execute(
        "UPDATE atlas_price_alerts SET state='cancelled' WHERE owner=$1 AND id=$2 AND state<>'firing'",&[&owner,&id]).await.map_err(unavailable)?;
    if affected == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            "This price alert is no longer active.".into(),
        ));
    }
    Ok(Json(json!({"ok":true})))
}
async fn notify(state: &AppState, row: &tokio_postgres::Row) -> Result<(), ApiError> {
    let owner: String = row.get("owner");
    let id: String = row.get("id");
    let symbol: String = row.get("symbol");
    let asset: String = row.get("asset_id");
    let price: f64 = row.get("fired_price");
    let threshold: f64 = row.get("target_usd");
    let direction: String = row.get("direction");
    // This writes details ONLY into Atlas's authenticated inbox database.
    // notifications::emit queues only an opaque ID; push::deliver and bridge/notifications.mjs
    // send the constant "You have a new Atlas money update." (privacy tests verify that payload).
    state
        .notifications
        .emit(
            &owner,
            &format!("price-alert:{id}"),
            &format!("{symbol} price alert"),
            &format!(
                "{symbol} is USD {price}. Your alert was USD {threshold} or {}.",
                if direction == "above" {
                    "higher"
                } else {
                    "lower"
                }
            ),
            &format!("/trade/{}", percent_id(&asset)),
            false,
        )
        .await?;
    state
        .price_alerts
        .database()?
        .lock()
        .await
        .execute(
            "UPDATE atlas_price_alerts SET state='triggered' WHERE id=$1 AND state='firing'",
            &[&id],
        )
        .await
        .map_err(unavailable)?;
    Ok(())
}
fn percent_id(id: &str) -> String {
    let mut url = reqwest::Url::parse("https://justatlas.xyz/trade/").expect("Atlas URL");
    url.path_segments_mut()
        .expect("Atlas path")
        .pop_if_empty()
        .push(id);
    url.path().trim_start_matches("/trade/").to_owned()
}
async fn check(state: &AppState) -> Result<(), ApiError> {
    let rows = state
        .price_alerts
        .database()?
        .lock()
        .await
        .query(
            "SELECT * FROM atlas_price_alerts WHERE state='firing' LIMIT 100",
            &[],
        )
        .await
        .map_err(unavailable)?;
    for row in rows {
        let _ = notify(state, &row).await;
    }
    let rows = state
        .price_alerts
        .database()?
        .lock()
        .await
        .query(
            "SELECT * FROM atlas_price_alerts WHERE state='active' ORDER BY checked_at LIMIT 100",
            &[],
        )
        .await
        .map_err(unavailable)?;
    let mut prices: HashMap<String, Option<f64>> = HashMap::new();
    let mut assets = rows
        .iter()
        .map(|r| r.get::<_, String>("asset_id"))
        .collect::<Vec<_>>();
    assets.sort();
    assets.dedup();
    // At most ten bounded market calls at once; one call per asset, not one per user's alert.
    for batch in assets.chunks(10) {
        let mut pending = tokio::task::JoinSet::new();
        for asset in batch {
            let state = state.clone();
            let asset = asset.clone();
            pending.spawn(async move {
                let price = tokio::time::timeout(
                    Duration::from_secs(8),
                    markets::alert_price(&state, &asset),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .map(|(_, p)| p);
                (asset, price)
            });
        }
        while let Some(result) = pending.join_next().await {
            if let Ok((asset, price)) = result {
                prices.insert(asset, price);
            }
        }
    }
    for row in rows {
        let asset: String = row.get("asset_id");
        let id: String = row.get("id");
        let price = prices.get(&asset).copied().flatten();
        state
            .price_alerts
            .database()?
            .lock()
            .await
            .execute(
                "UPDATE atlas_price_alerts SET checked_at=$2 WHERE id=$1 AND state='active'",
                &[&id, &now()],
            )
            .await
            .map_err(unavailable)?;
        let Some(price) = price else { continue };
        if !hit(
            &row.get::<_, String>("direction"),
            price,
            row.get("target_usd"),
        ) {
            continue;
        }
        let claimed=state.price_alerts.database()?.lock().await.query_opt(
            "UPDATE atlas_price_alerts SET state='firing',triggered_at=$2,fired_price=$3 WHERE id=$1 AND state='active' RETURNING *",&[&id,&now(),&price]).await.map_err(unavailable)?;
        if let Some(row) = claimed {
            let _ = notify(state, &row).await;
        }
    }
    Ok(())
}
pub(super) fn keep_watching(state: AppState) {
    if state.price_alerts.pg.is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            if check(&state).await.is_err() {
                eprintln!("Price alerts check unavailable; will retry");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thresholds_reject_invalid_numbers() {
        for input in ["0", "-1", "NaN", "inf", "1e30", "0.0000000000001", ""] {
            assert!(target(input).is_err(), "{input}");
        }
        assert_eq!(target("96").unwrap(), 96.0);
        assert!(target("0.000001").is_ok());
    }
    #[test]
    fn triggers_only_cross_selected_direction_with_valid_prices() {
        assert!(hit("above", 96.0, 96.0));
        assert!(hit("above", 97.0, 96.0));
        assert!(!hit("above", 95.0, 96.0));
        assert!(hit("below", 95.0, 96.0));
        assert!(!hit("below", 97.0, 96.0));
        for p in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert!(!hit("above", p, 96.0));
        }
        assert!(!hit("invalid", 97.0, 96.0));
    }
    #[test]
    fn notification_paths_escape_identifiers() {
        assert_eq!(percent_id("near:token"), "near:token");
        assert_eq!(percent_id("x/#?y"), "x%2F%23%3Fy");
    }
}
