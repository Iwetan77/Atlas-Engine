use super::*;
use serde_json::{json, Value};

pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    let mut rows = near_intents::pending_rows(&state, &headers, &user.user_id).await?;
    rows.extend(markets::pending_rows(&state, &user.user_id).await?);
    rows.extend(hl::pending_rows(&state, &user.user_id).await?);
    Ok(Json(json!({"intents":rows})))
}
pub(super) async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if id.starts_with("near-intent-") {
        return near_intents::resume(state, headers, id).await;
    }
    let next = markets::next_transactions(State(state), Path(id.clone()), headers)
        .await?
        .0;
    Ok(Json(
        json!({"intentId":id,"stage":"sign","kind":next["kind"].as_str().unwrap_or("buy"),
        "summary":[{"label":"Finish","value":"Use the funds already set aside"}],
        "transactions":next["transactions"],"expiresAtUnixMs":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()+120000}),
    ))
}
