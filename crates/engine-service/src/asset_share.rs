use super::*;
use axum::response::{IntoResponse, Response};

pub(super) async fn share(State(state): State<AppState>, Path(asset_id): Path<String>) -> Response {
    let root = env::var("ATLAS_WEB_DIR").unwrap_or_else(|_| "crates/engine-service/web".into());
    let path = std::path::PathBuf::from(root).join("asset/[assetId].html");
    let Ok(Ok(html)) = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let detail = async {
        let rate = app_balance::fx_rate("NGN").await?;
        markets::asset_detail_value(&state, &asset_id, "NGN", rate).await
    };
    let row = match tokio::time::timeout(Duration::from_secs(5), detail).await {
        Ok(Ok(row)) => Some(row),
        _ => None,
    };
    let title = row
        .as_ref()
        .and_then(|r| r["name"].as_str())
        .map(|name| format!("{name} on Atlas"))
        .unwrap_or_else(|| "Explore this asset on Atlas".into());
    let description = row.as_ref().and_then(|r| r["name"].as_str().zip(r["price"]["amount"].as_str()))
        .map(|(name, price)| format!("{name} is trading on Atlas for ₦{price}. One balance for stocks, memes and crypto."))
        .unwrap_or_else(|| "Explore stocks, memes and crypto with one Atlas balance.".into());
    let mut url = reqwest::Url::parse("https://justatlas.xyz/asset/").expect("public URL");
    url.path_segments_mut()
        .expect("public path")
        .pop_if_empty()
        .push(&asset_id);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        social_metadata(&html, &title, &description, url.as_str()),
    )
        .into_response()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub(super) fn social_metadata(html: &str, title: &str, description: &str, url: &str) -> String {
    // Replace existing metadata: social crawlers commonly choose the first duplicate.
    let keys = [
        "og:title",
        "og:description",
        "og:url",
        "twitter:title",
        "twitter:description",
        "description",
    ];
    let mut cleaned = String::new();
    for (i, fragment) in html.split("<meta").enumerate() {
        if i == 0 {
            cleaned.push_str(fragment);
            continue;
        }
        if let Some(end) = fragment.find('>') {
            let tag = &fragment[..=end];
            if keys.iter().any(|key| {
                tag.contains(&format!("property=\"{key}\""))
                    || tag.contains(&format!("name=\"{key}\""))
            }) {
                cleaned.push_str(&fragment[end + 1..]);
                continue;
            }
        }
        cleaned.push_str("<meta");
        cleaned.push_str(fragment);
    }
    // Expo can emit an empty React title before the document title. Keep one authoritative title.
    while let Some(start) = cleaned.find("<title") {
        let Some(end) = cleaned[start..].find("</title>") else {
            break;
        };
        cleaned.replace_range(start..start + end + 8, "");
    }
    let meta = format!("<title>{t}</title><meta property=\"og:title\" content=\"{t}\"/><meta property=\"og:description\" content=\"{d}\"/><meta property=\"og:url\" content=\"{u}\"/><meta name=\"twitter:title\" content=\"{t}\"/><meta name=\"twitter:description\" content=\"{d}\"/><meta name=\"description\" content=\"{d}\"/>",
        t=escape(title),d=escape(description),u=escape(url));
    cleaned.replacen("</head>", &(meta + "</head>"), 1)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_previews_replace_generic_metadata_and_escape_untrusted_names() {
        let html = "<html><head><title data-rh=\"true\"></title><title>Atlas</title><meta property=\"og:title\" content=\"Atlas\"/><meta name=\"description\" content=\"old\"/></head><body>app</body></html>";
        let result = social_metadata(
            html,
            "Coin\"><script>bad</script>",
            "price & details",
            "https://justatlas.xyz/asset/coin",
        );
        assert_eq!(result.matches("property=\"og:title\"").count(), 1);
        assert_eq!(result.matches("<title>").count(), 1);
        assert!(!result.contains("data-rh"));
        assert!(!result.contains("<script>"));
        assert!(result.contains("&lt;script&gt;"));
        assert!(result.contains("price &amp; details"));
        assert!(result.contains("<body>app</body>"));
    }
}
