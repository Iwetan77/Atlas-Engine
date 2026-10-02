use super::*;
use serde_json::{json, Value};

pub(super) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let user = app_balance::verified_wallets(&state, &headers).await?;
    // Finish is only for money waiting outside cash: coins a paid Sui or NEAR buy already received,
    // and a closed position's cash still in perps. A Base or Solana buy that stopped after moving its
    // cash isn't listed: that cash is already in the balance, and finishing an old buy later would
    // spend it at a price nobody agreed to.
    let mut rows = near_intents::pending_rows(&state, &headers, &user.user_id).await?;
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
    if !id.starts_with("hl-") {
        return Err((
            StatusCode::CONFLICT,
            "This buy stopped earlier and won't be finished now. Its cash is in your balance."
                .into(),
        ));
    }
    let next = hl::resume_close(state, id.clone(), headers).await?.0;
    Ok(Json(
        json!({"intentId":id,"stage":"sign","kind":next["kind"].as_str().unwrap_or("buy"),
        "summary":[{"label":"Finish","value":"Use the funds already set aside"}],
        "transactions":next["transactions"],"expiresAtUnixMs":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()+120000}),
    ))
}
