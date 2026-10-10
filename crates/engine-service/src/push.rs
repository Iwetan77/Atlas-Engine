use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Device notifications are temporarily unavailable".into(),
    )
}
fn valid(kind: &str, v: &Value) -> bool {
    if kind == "expo" {
        return v["token"].as_str().is_some_and(|s| {
            s.len() < 200
                && (s.starts_with("ExponentPushToken[") || s.starts_with("ExpoPushToken["))
                && s.ends_with(']')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_[]".contains(c))
        });
    }
    if kind != "web" {
        return false;
    }
    let Some(endpoint) = v["endpoint"].as_str().filter(|s| s.len() <= 2048) else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    let allowed = matches!(
        host,
        "fcm.googleapis.com" | "updates.push.services.mozilla.com" | "web.push.apple.com"
    ) || host.ends_with(".notify.windows.com");
    allowed
        && url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && [("p256dh", 87), ("auth", 22)].iter().all(|(key, len)| {
            v["keys"][key].as_str().is_some_and(|s| {
                s.len() == *len
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
            })
        })
}
fn device_id(kind: &str, v: &Value) -> String {
    let endpoint = if kind == "web" {
        v["endpoint"].as_str()
    } else {
        v["token"].as_str()
    }
    .unwrap_or("");
    format!(
        "{:x}",
        Sha256::digest(format!("{kind}:{endpoint}").as_bytes())
    )
}
#[derive(Clone)]
pub(super) struct PushState {
    pg: Option<Arc<tokio_postgres::Client>>,
    keys: Arc<tokio::sync::Mutex<Option<Value>>>,
    http: reqwest::Client,
}
impl PushState {
    pub(super) async fn new(
        pg: Option<Arc<tokio_postgres::Client>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(pg) = &pg {
            pg.batch_execute("CREATE TABLE IF NOT EXISTS atlas_notification_devices(id TEXT PRIMARY KEY,owner TEXT NOT NULL,
                kind TEXT NOT NULL,payload TEXT NOT NULL,updated_at BIGINT NOT NULL);
                CREATE TABLE IF NOT EXISTS atlas_notification_outbox(id TEXT PRIMARY KEY,owner TEXT NOT NULL,
                event_id TEXT NOT NULL REFERENCES atlas_notifications(id) ON DELETE CASCADE,device_id TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,due BIGINT NOT NULL,receipt TEXT);
                CREATE TABLE IF NOT EXISTS atlas_notification_keys(id INTEGER PRIMARY KEY CHECK(id=1),payload TEXT NOT NULL);").await?;
        }
        Ok(Self {
            pg,
            keys: Arc::new(tokio::sync::Mutex::new(None)),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    fn database(&self) -> Result<&Arc<tokio_postgres::Client>, ApiError> {
        self.pg
            .as_ref()
            .ok_or_else(|| internal("Notification database unavailable"))
    }
    async fn transport(
        &self,
        state: &AppState,
        route: &str,
        payload: Value,
    ) -> Result<Value, ApiError> {
        let AuthMode::Privy { bridge_url, .. } = &state.auth else {
            return Err(internal("No transport"));
        };
        self.http
            .post(format!("{bridge_url}/internal/notifications/{route}"))
            .json(&payload)
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(internal)?
            .json()
            .await
            .map_err(internal)
    }
    async fn keys(&self, state: &AppState) -> Result<Value, ApiError> {
        let pg = self.database()?;
        let mut cache = self.keys.lock().await;
        if let Some(v) = &*cache {
            return Ok(v.clone());
        }
        if let Some(r) = pg
            .query_opt(
                "SELECT payload FROM atlas_notification_keys WHERE id=1",
                &[],
            )
            .await
            .map_err(internal)?
        {
            let v: Value = serde_json::from_str(r.get("payload")).map_err(internal)?;
            *cache = Some(v.clone());
            return Ok(v);
        }
        let generated = self.transport(state, "keys", json!({})).await?;
        if generated["publicKey"].as_str().is_none() || generated["privateKey"].as_str().is_none() {
            return Err(internal("Keys unavailable"));
        }
        pg.execute(
            "INSERT INTO atlas_notification_keys(id,payload) VALUES(1,$1) ON CONFLICT DO NOTHING",
            &[&generated.to_string()],
        )
        .await
        .map_err(internal)?;
        let r = pg
            .query_one(
                "SELECT payload FROM atlas_notification_keys WHERE id=1",
                &[],
            )
            .await
            .map_err(internal)?;
        let v: Value = serde_json::from_str(r.get("payload")).map_err(internal)?;
        *cache = Some(v.clone());
        Ok(v)
    }
    async fn remove(&self, owner: &str, id: &str) -> Result<(), ApiError> {
        self.database()?
            .execute(
                "DELETE FROM atlas_notification_devices WHERE id=$1 AND owner=$2",
                &[&id, &owner],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }
}
pub(super) async fn config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    let keys = state.push.keys(&state).await?;
    Ok(Json(json!({"publicKey":keys["publicKey"]})))
}
#[derive(Deserialize)]
pub(super) struct Registration {
    kind: String,
    subscription: Value,
}
pub(super) async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Registration>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    if !valid(&input.kind, &input.subscription) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid notification registration".into(),
        ));
    }
    let id = device_id(&input.kind, &input.subscription);
    let pg = state.push.database()?;
    let count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM atlas_notification_devices WHERE owner=$1 AND id<>$2",
            &[&owner, &id],
        )
        .await
        .map_err(internal)?
        .get(0);
    if count >= 20 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Remove an old notification device first".into(),
        ));
    }
    pg.execute("INSERT INTO atlas_notification_devices(id,owner,kind,payload,updated_at) VALUES($1,$2,$3,$4,$5)
        ON CONFLICT(id) DO UPDATE SET owner=EXCLUDED.owner,kind=EXCLUDED.kind,payload=EXCLUDED.payload,updated_at=EXCLUDED.updated_at",
        &[&id,&owner,&input.kind,&input.subscription.to_string(),&now()]).await.map_err(internal)?;
    Ok(Json(json!({"id":id})))
}
// A user-triggered test exercises the same durable outbox as real money updates.
pub(super) async fn test(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    let count: i64 = state
        .push
        .database()?
        .query_one(
            "SELECT COUNT(*) FROM atlas_notification_devices WHERE owner=$1",
            &[&owner],
        )
        .await
        .map_err(internal)?
        .get(0);
    if count == 0 {
        return Err((
            StatusCode::PRECONDITION_REQUIRED,
            "Enable notifications on this device first.".into(),
        ));
    }
    state
        .notifications
        .emit(
            &owner,
            &format!("push-test:{}", now() / 60_000),
            "Notifications are ready",
            "This is your Atlas device notification test.",
            "/notifications",
            false,
        )
        .await?;
    Ok(Json(json!({"queued": true})))
}

#[derive(Deserialize)]
pub(super) struct Unregister {
    id: String,
}
pub(super) async fn unregister(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Unregister>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    state.push.remove(&owner, &input.id).await?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Clone)]
struct Job {
    id: String,
    owner: String,
    device: String,
    notice: String,
    kind: String,
    payload: Value,
    attempts: i32,
    receipt: Option<String>,
}
async fn jobs(pg: &tokio_postgres::Client) -> Result<Vec<Job>, ApiError> {
    // Lease the batch long enough for twenty bounded transport requests; lock out other instances.
    let claimed=pg.query("UPDATE atlas_notification_outbox SET due=$1+600000,attempts=attempts+1 WHERE id IN(
        SELECT id FROM atlas_notification_outbox WHERE due<=$1 ORDER BY due FOR UPDATE SKIP LOCKED LIMIT 20)
        RETURNING id,owner,event_id,device_id,attempts,receipt",&[&now()]).await.map_err(internal)?;
    let mut jobs = vec![];
    for r in claimed {
        let jid: String = r.get("id");
        let owner: String = r.get("owner");
        let device: String = r.get("device_id");
        let found = pg
            .query_opt(
                "SELECT kind,payload FROM atlas_notification_devices WHERE id=$1 AND owner=$2",
                &[&device, &owner],
            )
            .await
            .map_err(internal)?;
        if let Some(d) = found {
            jobs.push(Job {
                id: jid,
                owner,
                device,
                notice: r.get("event_id"),
                kind: d.get("kind"),
                payload: serde_json::from_str(d.get("payload")).map_err(internal)?,
                attempts: r.get("attempts"),
                receipt: r.get("receipt"),
            });
        } else {
            pg.execute("DELETE FROM atlas_notification_outbox WHERE id=$1", &[&jid])
                .await
                .map_err(internal)?;
        }
    }
    Ok(jobs)
}
async fn deliver(state: &AppState, j: &Job) -> Result<(String, Option<String>), ApiError> {
    if j.kind == "web" {
        let keys = state.push.keys(state).await?;
        let v = state
            .push
            .transport(
                state,
                "send",
                json!({"subscription":j.payload,"keys":keys,"noticeId":j.notice}),
            )
            .await?;
        return Ok((v["status"].as_str().unwrap_or("retry").into(), None));
    }
    // Only a generic alert and opaque notification id leave Atlas. Details require an authenticated inbox read.
    let (route, payload) = if let Some(receipt) = &j.receipt {
        ("getReceipts", json!({"ids":[receipt]}))
    } else {
        (
            "send",
            json!({"to":j.payload["token"],"title":"Atlas","body":"You have a new Atlas money update.",
            "sound":"default","channelId":"money","data":{"id":j.notice,"url":"/notifications"},"ttl":3600}),
        )
    };
    let mut request = state
        .push
        .http
        .post(format!("https://exp.host/--/api/v2/push/{route}"))
        .json(&payload);
    if let Ok(token) = env::var("EXPO_PUSH_ACCESS_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response: Value = request
        .send()
        .await
        .map_err(internal)?
        .error_for_status()
        .map_err(internal)?
        .json()
        .await
        .map_err(internal)?;
    let ticket = if let Some(receipt) = &j.receipt {
        &response["data"][receipt]
    } else {
        &response["data"]
    };
    if ticket["status"] == "ok" {
        return Ok((
            "sent".into(),
            if j.receipt.is_none() {
                ticket["id"].as_str().map(str::to_string)
            } else {
                None
            },
        ));
    }
    if ticket["details"]["error"] == "DeviceNotRegistered" {
        return Ok(("expired".into(), None));
    }
    Ok(("retry".into(), j.receipt.clone()))
}
pub(super) fn keep_delivering(state: AppState) {
    if state.push.pg.is_none() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(pg) = &state.push.pg else { continue };
            let Ok(jobs) = jobs(pg).await else { continue };
            for job in jobs {
                let (status, receipt) = deliver(&state, &job)
                    .await
                    .unwrap_or_else(|_| ("retry".into(), job.receipt.clone()));
                if status == "expired" {
                    let _ = state.push.remove(&job.owner, &job.device).await;
                }
                if (status == "retry" || receipt.is_some()) && job.attempts < 6 {
                    let due = now()
                        + if receipt.is_some() {
                            900_000
                        } else {
                            (30_000_i64 * (1_i64 << job.attempts.min(5))).min(900_000)
                        };
                    let _ = pg
                        .execute(
                            "UPDATE atlas_notification_outbox SET due=$2,receipt=$3 WHERE id=$1",
                            &[&job.id, &due, &receipt],
                        )
                        .await;
                } else {
                    let _ = pg
                        .execute(
                            "DELETE FROM atlas_notification_outbox WHERE id=$1",
                            &[&job.id],
                        )
                        .await;
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registrations_only_use_real_push_services() {
        assert!(valid(
            "expo",
            &json!({"token":"ExponentPushToken[test_token]"})
        ));
        assert!(!valid("expo", &json!({"token":"not-a-token"})));
        let keys = json!({"p256dh":"A".repeat(87),"auth":"B".repeat(22)});
        for endpoint in [
            "https://fcm.googleapis.com/fcm/send/test",
            "https://web.push.apple.com/test",
        ] {
            assert!(valid("web", &json!({"endpoint":endpoint,"keys":keys})));
        }
        for endpoint in [
            "http://127.0.0.1/x",
            "https://evil.com/x",
            "https://fcm.googleapis.com.evil.com/x",
            "https://user@fcm.googleapis.com/x",
        ] {
            assert!(!valid("web", &json!({"endpoint":endpoint,"keys":keys})));
        }
    }
}
