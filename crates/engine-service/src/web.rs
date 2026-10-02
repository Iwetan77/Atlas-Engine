//! The Atlas web app's static build (`npx expo export -p web` in the app repo, copied to `web/`),
//! served by the engine so Atlas Links open in any browser: the claim page signs the friend in
//! and claims, with no separate web hosting. Only files inside the build are ever read.
use super::*;
use axum::{
    http::Uri,
    response::{IntoResponse, Response},
};
use std::path::{Component, PathBuf};

fn root() -> PathBuf {
    env::var("ATLAS_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("crates/engine-service/web"))
}

// GET /claim/{id}: the claim page (the link's id and secret are read by the page itself).
pub(super) async fn claim() -> Response {
    file("claim/[linkId].html").await
}
// GET /: the web app's home, where "Open Atlas" lands after a claim.
pub(super) async fn index() -> Response {
    file("index.html").await
}
// GET /_expo/…, /assets/…, /favicon.ico: the build's scripts, fonts and images.
pub(super) async fn asset(uri: Uri) -> Response {
    file(uri.path().trim_start_matches('/')).await
}

async fn file(relative: &str) -> Response {
    // Plain names only: no "..", no absolute paths.
    let path = PathBuf::from(relative);
    if relative.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let full = root().join(&path);
    let Ok(Ok(body)) = tokio::task::spawn_blocking(move || std::fs::read(full)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let kind = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    // Built files carry a content hash in their names and never change; pages always revalidate.
    let cache = if relative.starts_with("_expo/") || relative.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}
