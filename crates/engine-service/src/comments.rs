//! Comments on Atlas Predictions markets, signed with the writer's @handle. Anyone signed in reads
//! them; only people with a handle write, at most one every 15 seconds and 30 an hour, and each can
//! delete their own. Links aren't allowed, so a comment can't send anyone off to a scam.
use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CHARS: usize = 280;
const PAGE: usize = 30;
const GAP_MS: u64 = 15_000;
const HOUR_MS: u64 = 60 * 60 * 1000;
const PER_HOUR: usize = 30;
static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Comment {
    id: String,
    market_id: String,
    user_id: String,
    // The writer's handle when they wrote it (handles are permanent).
    handle: String,
    body: String,
    created_ms: u64,
}

#[derive(Clone)]
pub(super) struct CommentStore {
    postgres: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<Vec<Comment>>>,
    // When each user last posted, within the past hour, for the pace limits.
    recent: Arc<Mutex<HashMap<String, Vec<u64>>>>,
}

impl CommentStore {
    pub(super) async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let postgres = if let Ok(url) = env::var("DATABASE_URL") {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("comments database connection ended: {error}");
                }
            });
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS atlas_market_comments (
                    id TEXT PRIMARY KEY,
                    market_id TEXT NOT NULL,
                    user_id TEXT NOT NULL,
                    handle TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_ms BIGINT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS atlas_market_comments_market
                    ON atlas_market_comments(market_id, created_ms DESC)",
                )
                .await?;
            Some(Arc::new(client))
        } else {
            None
        };
        Ok(Self {
            postgres,
            memory: Arc::new(Mutex::new(Vec::new())),
            recent: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    // A market's comments, newest first, from before `before` (ms) when paging back.
    async fn page(&self, market_id: &str, before: u64) -> Result<Vec<Comment>, ApiError> {
        if let Some(pg) = &self.postgres {
            let rows = pg
                .query(
                    "SELECT id, market_id, user_id, handle, body, created_ms FROM atlas_market_comments
                     WHERE market_id=$1 AND created_ms<$2 ORDER BY created_ms DESC, id DESC LIMIT $3",
                    &[
                        &market_id,
                        &i64::try_from(before).unwrap_or(i64::MAX),
                        &(PAGE as i64 + 1),
                    ],
                )
                .await
                .map_err(internal)?;
            return Ok(rows
                .into_iter()
                .map(|r| Comment {
                    id: r.get("id"),
                    market_id: r.get("market_id"),
                    user_id: r.get("user_id"),
                    handle: r.get("handle"),
                    body: r.get("body"),
                    created_ms: r.get::<_, i64>("created_ms").max(0) as u64,
                })
                .collect());
        }
        let mut found: Vec<Comment> = self
            .memory
            .lock()
            .map_err(internal)?
            .iter()
            .filter(|c| c.market_id == market_id && c.created_ms < before)
            .cloned()
            .collect();
        found.sort_by(|a, b| (b.created_ms, &b.id).cmp(&(a.created_ms, &a.id)));
        found.truncate(PAGE + 1);
        Ok(found)
    }

    async fn insert(&self, comment: &Comment) -> Result<(), ApiError> {
        if let Some(pg) = &self.postgres {
            pg.execute(
                "INSERT INTO atlas_market_comments (id,market_id,user_id,handle,body,created_ms) VALUES ($1,$2,$3,$4,$5,$6)",
                &[
                    &comment.id,
                    &comment.market_id,
                    &comment.user_id,
                    &comment.handle,
                    &comment.body,
                    &i64::try_from(comment.created_ms).map_err(internal)?,
                ],
            )
            .await
            .map_err(internal)?;
            return Ok(());
        }
        self.memory.lock().map_err(internal)?.push(comment.clone());
        Ok(())
    }

    // Removes a comment if it's the user's own; false when there's no such comment of theirs.
    async fn delete(&self, id: &str, user_id: &str) -> Result<bool, ApiError> {
        if let Some(pg) = &self.postgres {
            let removed = pg
                .execute(
                    "DELETE FROM atlas_market_comments WHERE id=$1 AND user_id=$2",
                    &[&id, &user_id],
                )
                .await
                .map_err(internal)?;
            return Ok(removed == 1);
        }
        let mut memory = self.memory.lock().map_err(internal)?;
        let before = memory.len();
        memory.retain(|c| !(c.id == id && c.user_id == user_id));
        Ok(memory.len() < before)
    }

    // Records a post now if the user is within the pace limits.
    fn pace(&self, user_id: &str, now: u64) -> Result<(), ApiError> {
        let mut recent = self.recent.lock().map_err(internal)?;
        let times = recent.entry(user_id.into()).or_default();
        times.retain(|t| now.saturating_sub(*t) < HOUR_MS);
        may_post(times, now).map_err(|m| (StatusCode::TOO_MANY_REQUESTS, m.into()))?;
        times.push(now);
        Ok(())
    }
}

// Whether someone who posted at `times` (within the past hour) may post again `now`.
fn may_post(times: &[u64], now: u64) -> Result<(), &'static str> {
    if times.iter().any(|t| now.saturating_sub(*t) < GAP_MS) {
        return Err("Slow down a little: one comment every 15 seconds.");
    }
    if times.len() >= PER_HOUR {
        return Err("That's a lot of comments for one hour. Try again later.");
    }
    Ok(())
}

// A comment as it's kept: one line of plain text, up to 280 characters, without links.
fn clean(body: &str) -> Result<String, &'static str> {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err("Write something first.");
    }
    if text.chars().count() > MAX_CHARS {
        return Err("Comments can be up to 280 characters.");
    }
    if text.chars().any(char::is_control) {
        return Err("That comment has characters Atlas can't show.");
    }
    let lower = text.to_lowercase();
    if ["http://", "https://", "www."]
        .iter()
        .any(|l| lower.contains(l))
    {
        return Err("Links aren't allowed in comments.");
    }
    Ok(text)
}

// Polymarket market ids are numbers.
fn market_id(id: &str) -> Result<&str, ApiError> {
    if id.is_empty() || id.len() > 15 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err((StatusCode::NOT_FOUND, "Market not found".into()));
    }
    Ok(id)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    eprintln!("comments: {error}");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Comments aren't available right now. Try again in a minute.".into(),
    )
}

fn view(c: &Comment, viewer: &str) -> Value {
    json!({"id":c.id,"handle":c.handle,"body":c.body,"createdAtUnixMs":c.created_ms,"mine":c.user_id == viewer})
}

#[derive(Deserialize)]
pub(super) struct Page {
    before: Option<u64>,
}

// GET /v1/predictions/markets/{id}/comments?before=: newest first, 30 at a time.
pub(super) async fn list(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::extract::Query(page): axum::extract::Query<Page>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let id = market_id(&id)?;
    let mut found = state
        .comments
        .page(id, page.before.unwrap_or(u64::MAX))
        .await?;
    let more = found.len() > PAGE;
    found.truncate(PAGE);
    let next = more.then(|| found.last().map(|c| c.created_ms)).flatten();
    Ok(Json(json!({
        "comments": found.iter().map(|c| view(c, &user.user_id)).collect::<Vec<_>>(),
        "nextBefore": next,
    })))
}

#[derive(Deserialize)]
pub(super) struct NewComment {
    body: String,
}

// POST /v1/predictions/markets/{id}/comments { body }: posts as the user's @handle.
pub(super) async fn post(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<NewComment>,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let id = market_id(&id)?;
    let body = clean(&req.body).map_err(|m| (StatusCode::BAD_REQUEST, m.to_string()))?;
    let handle = state
        .social
        .handle_of(&user.user_id)
        .await?
        .ok_or((StatusCode::CONFLICT, "Pick your @handle to comment.".into()))?;
    let created = now();
    state.comments.pace(&user.user_id, created)?;
    let comment = Comment {
        id: format!("c-{created}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        market_id: id.into(),
        user_id: user.user_id.clone(),
        handle,
        body,
        created_ms: created,
    };
    state.comments.insert(&comment).await?;
    Ok(Json(view(&comment, &user.user_id)))
}

// POST /v1/predictions/comments/{id}/delete: only your own.
pub(super) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    if id.len() > 64 || !state.comments.delete(&id, &user.user_id).await? {
        return Err((StatusCode::NOT_FOUND, "Comment not found".into()));
    }
    Ok(Json(json!({"deleted": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comment_is_one_short_line_without_links() {
        assert_eq!(
            clean("  Yes  will\n win \t easily "),
            Ok("Yes will win easily".into())
        );
        assert!(clean("   ").is_err());
        assert!(clean(&"a".repeat(281)).is_err());
        assert_eq!(clean(&"ñ".repeat(280)).map(|t| t.chars().count()), Ok(280));
        assert!(clean("claim free money at https://scam.example").is_err());
        assert!(clean("go to WWW.scam.example").is_err());
        assert!(clean("bad\u{0}byte").is_err());
    }

    #[test]
    fn posting_keeps_a_gentle_pace() {
        let now = 10 * HOUR_MS;
        assert!(may_post(&[], now).is_ok());
        assert!(may_post(&[now - 5_000], now).is_err());
        assert!(may_post(&[now - 20_000], now).is_ok());
        let busy: Vec<u64> = (0..PER_HOUR as u64).map(|i| now - 60_000 - i).collect();
        assert!(may_post(&busy, now).is_err());
    }

    #[test]
    fn only_market_numbers_are_markets() {
        assert!(market_id("2589812").is_ok());
        assert!(market_id("").is_err());
        assert!(market_id("12a").is_err());
        assert!(market_id("1234567890123456").is_err());
    }

    #[tokio::test]
    async fn comments_page_newest_first_and_only_their_writer_deletes_them() {
        let store = CommentStore {
            postgres: None,
            memory: Arc::default(),
            recent: Arc::default(),
        };
        for i in 0..(PAGE as u64 + 5) {
            let c = Comment {
                id: format!("c-{i}"),
                market_id: "1".into(),
                user_id: if i == 0 { "alice".into() } else { "bob".into() },
                handle: "x".into(),
                body: format!("comment {i}"),
                created_ms: 1_000 + i,
            };
            store.insert(&c).await.unwrap();
        }
        let first = store.page("1", u64::MAX).await.unwrap();
        assert_eq!(first.len(), PAGE + 1);
        assert_eq!(first[0].body, format!("comment {}", PAGE + 4));
        let older = store.page("1", first[PAGE - 1].created_ms).await.unwrap();
        assert_eq!(older.len(), 5);
        assert!(store.page("2", u64::MAX).await.unwrap().is_empty());
        assert!(!store.delete("c-0", "bob").await.unwrap());
        assert!(store.delete("c-0", "alice").await.unwrap());
        assert!(!store.delete("c-0", "alice").await.unwrap());
    }
}
