use super::*;
use axum::extract::Query;
use rand::RngCore;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn id() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|v| format!("{v:02x}")).collect()
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Notifications are temporarily unavailable".into(),
    )
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Notice {
    id: String,
    title: String,
    body: String,
    url: String,
    created_at: i64,
    read: bool,
    warning: bool,
}
#[derive(Clone)]
pub(super) struct NotificationState {
    pub(super) pg: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<(String, String), Notice>>>,
}
impl NotificationState {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let pg = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if connection.await.is_err() {
                    eprintln!("Notification database connection ended");
                }
            });
            client.batch_execute("CREATE TABLE IF NOT EXISTS atlas_notifications(
                id TEXT PRIMARY KEY, owner TEXT NOT NULL, event_key TEXT NOT NULL, title TEXT NOT NULL,
                body TEXT NOT NULL, url TEXT NOT NULL, created_at BIGINT NOT NULL, read BOOLEAN NOT NULL DEFAULT FALSE,
                warning BOOLEAN NOT NULL DEFAULT FALSE, UNIQUE(owner,event_key));
                CREATE INDEX IF NOT EXISTS atlas_notifications_owner ON atlas_notifications(owner,created_at DESC);").await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            pg,
            memory: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    pub(super) async fn emit(
        &self,
        owner: &str,
        key: &str,
        title: &str,
        body: &str,
        url: &str,
        warning: bool,
    ) -> Result<(), ApiError> {
        let n = Notice {
            id: id(),
            title: title.chars().take(160).collect(),
            body: body.chars().take(700).collect(),
            url: safe_url(url),
            created_at: now(),
            read: false,
            warning,
        };
        if let Some(pg) = &self.pg {
            pg.execute("WITH event AS (
                INSERT INTO atlas_notifications(id,owner,event_key,title,body,url,created_at,warning)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(owner,event_key) DO NOTHING RETURNING id,owner)
                INSERT INTO atlas_notification_outbox(id,owner,event_id,device_id,due)
                SELECT event.id||':'||d.id,event.owner,event.id,d.id,$7 FROM event
                JOIN atlas_notification_devices d ON d.owner=event.owner",
                &[&n.id,&owner,&key,&n.title,&n.body,&n.url,&n.created_at,&warning]).await.map_err(internal)?;
        } else {
            self.memory
                .lock()
                .map_err(internal)?
                .entry((owner.to_string(), key.to_string()))
                .or_insert(n);
        }
        Ok(())
    }
}
fn safe_url(url: &str) -> String {
    let local = url.strip_prefix("https://justatlas.xyz").unwrap_or(url);
    if local.starts_with('/') && !local.starts_with("//") && !local.contains(['\\', '\r', '\n']) {
        local.to_owned()
    } else {
        "/notifications".into()
    }
}
fn notice(r: &tokio_postgres::Row) -> Notice {
    Notice {
        id: r.get("id"),
        title: r.get("title"),
        body: r.get("body"),
        url: r.get("url"),
        created_at: r.get("created_at"),
        read: r.get("read"),
        warning: r.get("warning"),
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListQuery {
    before: Option<i64>,
    before_id: Option<String>,
}
pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let owner = &user.user_id;
    let (items, unread) = if let Some(pg) = &state.notifications.pg {
        let rows=pg.query("SELECT * FROM atlas_notifications WHERE owner=$1 AND (created_at,id)<($2,$3) ORDER BY created_at DESC,id DESC LIMIT 40",
            &[&owner,&q.before.unwrap_or(i64::MAX),&q.before_id.as_deref().unwrap_or("~")]).await.map_err(internal)?;
        let unread: i64 = pg
            .query_one(
                "SELECT COUNT(*) FROM atlas_notifications WHERE owner=$1 AND NOT read",
                &[&owner],
            )
            .await
            .map_err(internal)?
            .get(0);
        (rows.iter().map(notice).collect::<Vec<_>>(), unread)
    } else {
        let m = state.notifications.memory.lock().map_err(internal)?;
        let mut items = m
            .iter()
            .filter(|((o, _), n)| {
                o == owner
                    && (n.created_at, n.id.as_str())
                        < (
                            q.before.unwrap_or(i64::MAX),
                            q.before_id.as_deref().unwrap_or("~"),
                        )
            })
            .map(|(_, n)| n.clone())
            .collect::<Vec<_>>();
        items.sort_by_key(|n| std::cmp::Reverse((n.created_at, n.id.clone())));
        items.truncate(40);
        let unread = m.iter().filter(|((o, _), n)| o == owner && !n.read).count() as i64;
        (items, unread)
    };
    Ok(Json(
        json!({"hasMore":items.len()==40,"items":items,"unread":unread}),
    ))
}
#[derive(Deserialize)]
pub(super) struct MarkRead {
    id: Option<String>,
}
pub(super) async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<MarkRead>,
) -> Result<Json<Value>, ApiError> {
    let owner = app_balance::verified_wallets(&state, &headers)
        .await?
        .user_id;
    if let Some(pg) = &state.notifications.pg {
        pg.execute("UPDATE atlas_notifications SET read=TRUE WHERE owner=$1 AND ($2::TEXT IS NULL OR id=$2)",&[&owner,&input.id]).await.map_err(internal)?;
    } else {
        for ((o, _), n) in state
            .notifications
            .memory
            .lock()
            .map_err(internal)?
            .iter_mut()
        {
            if *o == owner && input.id.as_ref().is_none_or(|i| *i == n.id) {
                n.read = true;
            }
        }
    }
    Ok(Json(json!({"ok":true})))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_links_stay_on_atlas() {
        assert_eq!(
            safe_url("https://justatlas.xyz/transaction/123"),
            "/transaction/123"
        );
        for url in [
            "https://evil.com/",
            "//evil.com",
            "/\\evil.com",
            "https://justatlas.xyz.evil.com/",
        ] {
            assert_eq!(safe_url(url), "/notifications");
        }
    }
    #[tokio::test]
    async fn money_events_are_once_per_user() {
        let s = NotificationState {
            pg: None,
            memory: Arc::new(Mutex::new(HashMap::new())),
        };
        s.emit("alice", "send:1", "Sent", "Complete", "/", false)
            .await
            .unwrap();
        s.emit("alice", "send:1", "Sent", "Complete", "/", false)
            .await
            .unwrap();
        s.emit("bob", "send:1", "Received", "Complete", "/", false)
            .await
            .unwrap();
        assert_eq!(s.memory.lock().unwrap().len(), 2);
    }
}
