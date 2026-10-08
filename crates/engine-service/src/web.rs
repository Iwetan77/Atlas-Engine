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
// Only exported page names can be opened directly; API paths and arbitrary files are not pages.
pub(super) async fn page(uri: Uri) -> Response {
    match page_file(uri.path()) {
        Some(relative) => file(relative).await,
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn page_file(path: &str) -> Option<&'static str> {
    let path = path.trim_end_matches('/');
    match path {
        "" => Some("index.html"),
        "/sign-in" => Some("sign-in.html"),
        "/sign-in-email" => Some("sign-in-email.html"),
        "/install" => Some("install.html"),
        "/trade" => Some("trade.html"),
        "/perps" => Some("perps.html"),
        "/send" => Some("send.html"),
        "/more" => Some("more.html"),
        "/transactions" => Some("transactions.html"),
        "/notifications" => Some("notifications.html"),
        "/earn" => Some("earn.html"),
        "/predictions" => Some("predictions.html"),
        "/predictions/cash" => Some("predictions/cash.html"),
        "/deposit" => Some("deposit.html"),
        "/add-bank" => Some("add-bank.html"),
        "/profile" => Some("profile.html"),
        "/payment-pin" => Some("payment-pin.html"),
        "/handle" => Some("handle.html"),
        "/browse" => Some("browse.html"),
        "/send/friend" => Some("send/friend.html"),
        "/send/bank" => Some("send/bank.html"),
        "/send/link" => Some("send/link.html"),
        "/send/wallet" => Some("send/wallet.html"),
        _ => {
            let parts: Vec<_> = path.strip_prefix('/')?.split('/').collect();
            match parts.as_slice() {
                ["trade", id] if valid_id(id) => Some("trade/[assetId].html"),
                ["perps", "close", id] if valid_id(id) => Some("perps/close/[positionId].html"),
                ["predictions", id] if valid_id(id) => Some("predictions/[marketId].html"),
                ["perps", id] if valid_id(id) => Some("perps/[marketId].html"),
                ["transaction", id] if valid_id(id) => Some("transaction/[id].html"),
                ["mini", id] if valid_id(id) => Some("mini/[appId].html"),
                ["claim", id] if valid_id(id) => Some("claim/[linkId].html"),
                _ => None,
            }
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".."
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
        "webmanifest" => "application/manifest+json",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_links_resolve_only_exported_pages() {
        assert_eq!(page_file("/payment-pin"), Some("payment-pin.html"));
        assert_eq!(page_file("/payment-pin/"), Some("payment-pin.html"));
        assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("web/payment-pin.html")
            .is_file());
        assert_eq!(page_file("/notifications"), Some("notifications.html"));
        assert_eq!(page_file("/notifications/"), Some("notifications.html"));
        for name in ["notifications.html", "atlas-push-sw.js"] {
            assert!(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("web")
                .join(name)
                .is_file());
        }
        assert_eq!(page_file("/install"), Some("install.html"));
        assert_eq!(page_file("/install/"), Some("install.html"));
        assert_eq!(page_file("/send/wallet"), Some("send/wallet.html"));
        assert_eq!(page_file("/send/wallet/"), Some("send/wallet.html"));
        assert_eq!(page_file("/sign-in-email"), Some("sign-in-email.html"));
        assert_eq!(
            page_file("/trade/near%3Acoin"),
            Some("trade/[assetId].html")
        );
        assert_eq!(
            page_file("/perps/close/123"),
            Some("perps/close/[positionId].html")
        );
        assert_eq!(page_file("/transaction/123"), Some("transaction/[id].html"));
        assert_eq!(page_file("/claim/123"), Some("claim/[linkId].html"));
        for path in [
            "/v1/me",
            "/v1/intents/123",
            "/.env",
            "/Cargo.toml",
            "/unknown",
            "/trade/../.env",
            "/trade/..",
            "/trade//bad",
            "/perps/close/../key",
        ] {
            assert_eq!(page_file(path), None, "{path}");
        }
    }
}

// The newest Android release, so an installed app can offer the update (the APK has no
// over-the-air updates). Read from the repo's latest full GitHub release, kept for half an hour;
// ATLAS_ANDROID_MIN_VERSION (optional) is the oldest app that still works with this engine.
const ANDROID_DOWNLOAD: &str =
    "https://github.com/Iwetan77/Atlas/releases/latest/download/atlas.apk";
static ANDROID_LATEST: std::sync::LazyLock<Mutex<Option<(Instant, String)>>> =
    std::sync::LazyLock::new(Default::default);

async fn latest_android() -> Option<String> {
    let held = ANDROID_LATEST.lock().ok()?.clone();
    if let Some((at, version)) = &held {
        if at.elapsed() < Duration::from_secs(30 * 60) {
            return Some(version.clone());
        }
    }
    // The release page's redirect names the newest tag; unlike GitHub's API it isn't limited per
    // address, which matters on shared hosting.
    let fetched: Option<String> = async {
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(8))
            .build()
            .ok()?
            .get("https://github.com/Iwetan77/Atlas/releases/latest")
            .header("User-Agent", "atlas-engine")
            .send()
            .await
            .ok()?;
        let location = response.headers().get("location")?.to_str().ok()?;
        let tag = location
            .strip_prefix("https://github.com/Iwetan77/Atlas/releases/tag/")?
            .trim_start_matches('v');
        is_version(tag).then(|| tag.to_owned())
    }
    .await;
    match fetched {
        Some(version) => {
            if let Ok(mut latest) = ANDROID_LATEST.lock() {
                *latest = Some((Instant::now(), version.clone()));
            }
            Some(version)
        }
        None => held.map(|(_, version)| version),
    }
}

// "1.0.11", or "1.0.1-beta.2": digits and dots, then an optional pre-release tag.
fn is_version(text: &str) -> bool {
    let core = text.split('-').next().unwrap_or("");
    !core.is_empty()
        && core
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && text.len() <= 32
}

// GET /v1/app/android → { latestVersion, minimumVersion, downloadUrl }. Public: it only says
// which app version is newest.
pub(super) async fn android_release() -> Json<serde_json::Value> {
    let minimum = env::var("ATLAS_ANDROID_MIN_VERSION")
        .ok()
        .filter(|v| is_version(v));
    Json(serde_json::json!({
        "latestVersion": latest_android().await,
        "minimumVersion": minimum,
        "downloadUrl": ANDROID_DOWNLOAD,
    }))
}

#[cfg(test)]
mod release_tests {
    use super::*;
    #[test]
    fn only_plain_versions_are_offered() {
        assert!(is_version("1.0.11"));
        assert!(is_version("1.0.1-beta.2"));
        assert!(!is_version(""));
        assert!(!is_version("1..2"));
        assert!(!is_version("latest"));
        assert!(!is_version("<script>"));
    }
}
