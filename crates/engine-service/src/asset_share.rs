use super::*;
use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub(super) struct ShareState {
    pg: Option<Arc<tokio_postgres::Client>>,
    memory: Arc<Mutex<HashMap<String, ShareLink>>>,
    details: Arc<Mutex<HashMap<String, (Instant, Value)>>>,
    images: Arc<Mutex<HashMap<String, (Instant, Vec<u8>)>>>,
    render_slots: Arc<tokio::sync::Semaphore>,
}
#[derive(Clone)]
struct ShareLink {
    code: String,
    asset_id: String,
    currency: String,
}
impl ShareState {
    pub(super) async fn new(
        pg: Option<Arc<tokio_postgres::Client>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(pg) = &pg {
            pg.batch_execute(
                "CREATE TABLE IF NOT EXISTS atlas_asset_shares(
                code TEXT PRIMARY KEY, asset_id TEXT NOT NULL, currency TEXT NOT NULL,
                UNIQUE(asset_id,currency));",
            )
            .await?;
        }
        Ok(Self {
            pg,
            memory: Arc::new(Mutex::new(HashMap::new())),
            details: Arc::new(Mutex::new(HashMap::new())),
            images: Arc::new(Mutex::new(HashMap::new())),
            render_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    async fn save(&self, asset_id: &str, currency: &str) -> Result<ShareLink, ApiError> {
        if let Some(pg) = &self.pg {
            if let Some(row) = pg
                .query_opt(
                    "SELECT code FROM atlas_asset_shares WHERE asset_id=$1 AND currency=$2",
                    &[&asset_id, &currency],
                )
                .await
                .map_err(unavailable)?
            {
                return Ok(ShareLink {
                    code: row.get(0),
                    asset_id: asset_id.into(),
                    currency: currency.into(),
                });
            }
            // Collision-safe inserts never change another asset's public alias.
            for attempt in 0..8 {
                let code = short_code(asset_id, currency, attempt);
                let row = pg
                    .query_opt(
                        "INSERT INTO atlas_asset_shares(code,asset_id,currency) VALUES($1,$2,$3)
                    ON CONFLICT DO NOTHING RETURNING code",
                        &[&code, &asset_id, &currency],
                    )
                    .await
                    .map_err(unavailable)?;
                if row.is_some() {
                    return Ok(ShareLink {
                        code,
                        asset_id: asset_id.into(),
                        currency: currency.into(),
                    });
                }
                if let Some(row) = pg
                    .query_opt(
                        "SELECT code FROM atlas_asset_shares WHERE asset_id=$1 AND currency=$2",
                        &[&asset_id, &currency],
                    )
                    .await
                    .map_err(unavailable)?
                {
                    return Ok(ShareLink {
                        code: row.get(0),
                        asset_id: asset_id.into(),
                        currency: currency.into(),
                    });
                }
            }
            return Err(unavailable("Alias collision"));
        }
        let mut links = self.memory.lock().map_err(unavailable)?;
        for attempt in 0..8 {
            let code = short_code(asset_id, currency, attempt);
            if let Some(found) = links.get(&code) {
                if found.asset_id == asset_id && found.currency == currency {
                    return Ok(found.clone());
                }
                continue;
            }
            if links.len() >= 4096 {
                return Err(unavailable("Share storage unavailable"));
            }
            let link = ShareLink {
                code: code.clone(),
                asset_id: asset_id.into(),
                currency: currency.into(),
            };
            links.insert(code, link.clone());
            return Ok(link);
        }
        Err(unavailable("Alias collision"))
    }
    async fn get(&self, code: &str) -> Result<ShareLink, ApiError> {
        if !valid_code(code) {
            return Err((StatusCode::NOT_FOUND, "Asset link not found.".into()));
        }
        if let Some(pg) = &self.pg {
            let row = pg
                .query_opt(
                    "SELECT asset_id,currency FROM atlas_asset_shares WHERE code=$1",
                    &[&code],
                )
                .await
                .map_err(unavailable)?
                .ok_or((StatusCode::NOT_FOUND, "Asset link not found.".into()))?;
            return Ok(ShareLink {
                code: code.into(),
                asset_id: row.get(0),
                currency: row.get(1),
            });
        }
        self.memory
            .lock()
            .map_err(unavailable)?
            .get(code)
            .cloned()
            .ok_or((StatusCode::NOT_FOUND, "Asset link not found.".into()))
    }
}
fn unavailable(_: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "This asset is temporarily unavailable. Try again shortly.".into(),
    )
}
fn currency_valid(currency: &str) -> bool {
    matches!(
        currency,
        "NGN" | "USD" | "EUR" | "GBP" | "ZAR" | "KES" | "GHS"
    )
}
fn valid_code(code: &str) -> bool {
    code.len() == 8
        && code
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
fn short_code(asset_id: &str, currency: &str, attempt: u8) -> String {
    let hash = Sha256::digest(format!("{currency}\0{asset_id}\0{attempt}").as_bytes());
    URL_SAFE_NO_PAD.encode(&hash[..6])
}
fn public_url(code: &str) -> String {
    format!("https://justatlas.xyz/a/{code}")
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Create {
    asset_id: String,
    currency: String,
}
#[derive(Deserialize)]
pub(super) struct Lookup {
    currency: Option<String>,
}
async fn detail(state: &AppState, link: &ShareLink, currency: &str) -> Result<Value, ApiError> {
    let key = format!("{}:{currency}", link.code);
    if let Some((at, row)) = state
        .asset_shares
        .details
        .lock()
        .map_err(unavailable)?
        .get(&key)
    {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(row.clone());
        }
    }
    let rate = app_balance::fx_rate(currency).await?;
    let mut row = tokio::time::timeout(
        Duration::from_secs(8),
        markets::asset_detail_value(state, &link.asset_id, currency, rate),
    )
    .await
    .map_err(unavailable)??;
    row["shareCode"] = json!(link.code);
    row["shareUrl"] = json!(public_url(&link.code));
    let mut cache = state.asset_shares.details.lock().map_err(unavailable)?;
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), row.clone()));
    Ok(row)
}
pub(super) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> Result<Json<Value>, ApiError> {
    app_balance::verified_wallets(&state, &headers).await?;
    if input.asset_id.is_empty()
        || input.asset_id.len() > 200
        || input.asset_id.chars().any(char::is_control)
        || !currency_valid(&input.currency)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a supported asset and currency.".into(),
        ));
    }
    // Asset identity and artwork are obtained from our providers, never a caption from the caller.
    let rate = app_balance::fx_rate(&input.currency).await?;
    tokio::time::timeout(
        Duration::from_secs(8),
        markets::asset_detail_value(&state, &input.asset_id, &input.currency, rate),
    )
    .await
    .map_err(unavailable)??;
    let link = state
        .asset_shares
        .save(&input.asset_id, &input.currency)
        .await?;
    Ok(Json(
        json!({"code":link.code,"url":public_url(&link.code),"imageUrl":format!("{}/card.png",public_url(&link.code))}),
    ))
}
pub(super) async fn lookup(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(q): Query<Lookup>,
) -> Result<Json<Value>, ApiError> {
    let link = state.asset_shares.get(&code).await?;
    let currency = q.currency.as_deref().unwrap_or(&link.currency);
    if !currency_valid(currency) {
        return Err((StatusCode::BAD_REQUEST, "Unsupported currency.".into()));
    }
    Ok(Json(detail(&state, &link, currency).await?))
}
pub(super) async fn short_share(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    let link = match state.asset_shares.get(&code).await {
        Ok(link) => link,
        Err(e) => return e.into_response(),
    };
    let root = env::var("ATLAS_WEB_DIR").unwrap_or_else(|_| "crates/engine-service/web".into());
    let path = std::path::PathBuf::from(root).join("a/[code].html");
    let html = match tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await {
        Ok(Ok(html)) => html,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let row = detail(&state, &link, &link.currency).await.ok();
    let symbol = row
        .as_ref()
        .and_then(|r| r["symbol"].as_str())
        .unwrap_or("this asset");
    let name = row
        .as_ref()
        .and_then(|r| r["name"].as_str())
        .unwrap_or("Stocks, memes and crypto");
    let title = format!("Trade {symbol} on Atlas");
    let description =
        format!("{name}. Your next move, on Atlas. Click the link to explore and trade.");
    let url = public_url(&code);
    let image = format!("{url}/card.png");
    let html = social_metadata(&html, &title, &description, &url);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        social_image_metadata(&html, &image),
    )
        .into_response()
}
pub(super) async fn share(State(state): State<AppState>, Path(asset_id): Path<String>) -> Response {
    let root = env::var("ATLAS_WEB_DIR").unwrap_or_else(|_| "crates/engine-service/web".into());
    let path = std::path::PathBuf::from(root).join("asset/[assetId].html");
    let Ok(Ok(html)) = tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let row = tokio::time::timeout(Duration::from_secs(5), async {
        let rate = app_balance::fx_rate("NGN").await?;
        markets::asset_detail_value(&state, &asset_id, "NGN", rate).await
    })
    .await
    .ok()
    .and_then(Result::ok);
    let title = row
        .as_ref()
        .and_then(|r| r["symbol"].as_str())
        .map(|s| format!("Trade {s} on Atlas"))
        .unwrap_or_else(|| "Explore this asset on Atlas".into());
    let description = row
        .as_ref()
        .and_then(|r| r["name"].as_str())
        .map(|s| format!("{s}. Your next move, on Atlas."))
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
fn allowed_icon_url(text: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(text).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return None;
    }
    let host = url.host_str()?;
    let approved = [
        "raw.githubusercontent.com",
        "cdn.jsdelivr.net",
        "assets.coingecko.com",
        "coin-images.coingecko.com",
        "static.jup.ag",
        "ipfs.io",
        "gateway.pinata.cloud",
        "arweave.net",
        "img-v1.raydium.io",
        "img.fotofolio.xyz",
        "token-icons.s3.amazonaws.com",
        "cryptologos.cc",
        "cdn.dexscreener.com",
    ];
    approved.contains(&host).then_some(url)
}
fn local_icon_data(symbol: &str) -> Option<String> {
    let bytes: &[u8] = match symbol.to_ascii_uppercase().as_str() {
        "SOL" | "WSOL" => include_bytes!("../assets/social/icons/sol.png"),
        "BTC" | "WBTC" => include_bytes!("../assets/social/icons/btc.png"),
        "ETH" | "WETH" => include_bytes!("../assets/social/icons/eth.png"),
        "NEAR" | "WNEAR" => include_bytes!("../assets/social/icons/near.png"),
        "USDC" => include_bytes!("../assets/social/icons/usdc.png"),
        "USDT" => include_bytes!("../assets/social/icons/usdt.png"),
        "SUI" => include_bytes!("../assets/social/icons/sui.png"),
        "AAPL" | "AAPLX" => include_bytes!("../assets/social/icons/aapl.png"),
        "NVDA" | "NVDAX" => include_bytes!("../assets/social/icons/nvda.png"),
        "TSLA" | "TSLAX" => include_bytes!("../assets/social/icons/tsla.png"),
        "BONK" => include_bytes!("../assets/social/icons/bonk.png"),
        "WIF" => include_bytes!("../assets/social/icons/wif.png"),
        "DOGE" => include_bytes!("../assets/social/icons/doge.png"),
        "XRP" => include_bytes!("../assets/social/icons/xrp.png"),
        "BNB" => include_bytes!("../assets/social/icons/bnb.png"),
        "TRX" => include_bytes!("../assets/social/icons/trx.png"),
        "BRETT" => include_bytes!("../assets/social/icons/brett.png"),
        _ => return None,
    };
    Some(format!("data:image/png;base64,{}", STANDARD.encode(bytes)))
}
async fn icon_data(row: &Value) -> Option<String> {
    let url = allowed_icon_url(row["iconUrl"].as_str()?)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let mut response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    if response.content_length().is_some_and(|n| n > 1_000_000) {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > 1_000_000 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let png = sanitize_icon(&bytes)?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}
fn sanitize_icon(bytes: &[u8]) -> Option<Vec<u8>> {
    if let Ok(mut reader) =
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()
    {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(2048);
        limits.max_image_height = Some(2048);
        limits.max_alloc = Some(16_000_000);
        reader.limits(limits);
        if let Ok(image) = reader.decode() {
            let image = image.resize(224, 224, image::imageops::FilterType::Triangle);
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).ok()?;
            return Some(png.into_inner());
        }
    }
    // Small vector logos are rasterized in isolation. No files, sub-images, fonts or filters load.
    if bytes.len() > 100_000 {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let lower = text.to_ascii_lowercase();
    let unsafe_tag = lower.split('<').skip(1).any(|fragment| {
        let tag = fragment
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or("");
        matches!(tag.rsplit(':').next(), Some("filter" | "script"))
    });
    if !lower.contains("<svg")
        || unsafe_tag
        || ["<!doctype", "<!entity"].iter().any(|p| lower.contains(p))
    {
        return None;
    }
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver.resolve_data = Box::new(|_, _, _| None);
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_str(text, &options).ok()?;
    let size = tree.size();
    if !size.width().is_finite()
        || !size.height().is_finite()
        || size.width() <= 0.0
        || size.height() <= 0.0
    {
        return None;
    }
    let scale = 224.0 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(224, 224)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().ok()
}
fn bounded_text(text: &str, max: usize) -> String {
    let mut text = text
        .chars()
        .filter(|c| !c.is_control())
        .take(max + 1)
        .collect::<String>();
    if text.chars().count() > max {
        text = text.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
    }
    escape(&text)
}
fn grouped_amount(value: &str) -> String {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let mut out = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}
fn currency_symbol(currency: &str) -> &str {
    match currency {
        "NGN" => "₦",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "KES" => "KSh ",
        "GHS" => "GH₵ ",
        "ZAR" => "R ",
        _ => currency,
    }
}
fn card_svg(row: &Value, code: &str, icon: Option<&str>) -> String {
    let symbol = bounded_text(row["symbol"].as_str().unwrap_or("ASSET"), 18);
    let name = bounded_text(
        row["name"].as_str().unwrap_or("Discover your next move"),
        38,
    );
    let amount = row["price"]["amount"].as_str().unwrap_or("—");
    let currency = row["price"]["currency"].as_str().unwrap_or("NGN");
    let n = amount
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n > 0.0);
    let value = match n {
        Some(n) if n < 0.01 => format!("{n:.6}"),
        Some(n) if n < 1.0 => format!("{n:.4}"),
        Some(n) => format!("{n:.2}"),
        None => "—".into(),
    };
    let price = bounded_text(
        &format!("{}{}", currency_symbol(currency), grouped_amount(&value)),
        30,
    );
    let headline_size = if symbol.chars().count() > 14 {
        48
    } else if symbol.chars().count() > 10 {
        56
    } else {
        66
    };
    let artwork=icon.map(|data|format!("<image x='76' y='128' width='108' height='108' href='{}' clip-path='url(#avatar)'/>",escape(data)))
        .unwrap_or_else(||format!("<text x='130' y='203' text-anchor='middle' font-size='45' font-weight='700' fill='white'>{}</text>",bounded_text(row["symbol"].as_str().unwrap_or("A"),3)));
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630">
    <defs><linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#20141F"/><stop offset="1" stop-color="#14151C"/></linearGradient><clipPath id="avatar"><circle cx="130" cy="182" r="54"/></clipPath></defs>
    <rect width="1200" height="630" rx="36" fill="url(#bg)"/>
    <circle cx="1170" cy="-40" r="430" fill="#FF2E7E" opacity=".09"/><circle cx="1120" cy="30" r="280" fill="none" stroke="#FF2E7E" stroke-width="2" opacity=".25"/>
    <g font-family="Inter">
      <rect x="72" y="56" width="94" height="34" rx="17" fill="#FF2E7E"/><text x="119" y="80" text-anchor="middle" fill="white" font-size="17" font-weight="700">ATLAS</text>
      <text x="188" y="81" fill="#B4B6C4" font-size="20">YOUR NEXT MOVE</text>
      <circle cx="130" cy="182" r="58" fill="#33313E"/>{artwork}
      <text x="210" y="176" fill="white" font-size="36" font-weight="700">{name}</text>
      <text x="212" y="217" fill="#B4B6C4" font-size="24">{symbol}</text>
      <text x="72" y="327" fill="white" font-size="{headline_size}" font-weight="700">Trade {symbol} on Atlas</text>
      <text x="76" y="391" fill="#FF8AB5" font-size="37" font-weight="700">{price}</text>
      <text x="76" y="435" fill="#B4B6C4" font-size="20">Price at time of preview · Stocks, memes and crypto</text>
      <path d="M72 485H1128" stroke="#3C3743" stroke-width="2"/>
      <text x="76" y="532" fill="#B4B6C4" font-size="21">Click the link below to explore and trade</text>
      <text x="76" y="573" fill="white" font-size="26" font-weight="700">justatlas.xyz/a/{code}</text>
      <text x="1115" y="571" text-anchor="end" fill="#FF2E7E" font-size="39" font-weight="700">atlas.</text>
    </g></svg>"##
    )
}
fn render_card(svg: &str) -> Result<Vec<u8>, ApiError> {
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../assets/social/Inter-Regular.ttf").to_vec());
    options
        .fontdb_mut()
        .load_font_data(include_bytes!("../assets/social/Inter-Bold.ttf").to_vec());
    // Only inline sanitized artwork is present; no remote URLs or paths are passed to the renderer.
    let tree = resvg::usvg::Tree::from_str(svg, &options).map_err(unavailable)?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(1200, 630).ok_or_else(|| unavailable("Image unavailable"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().map_err(unavailable)
}
pub(super) async fn card(State(state): State<AppState>, Path(code): Path<String>) -> Response {
    let link = match state.asset_shares.get(&code).await {
        Ok(link) => link,
        Err(e) => return e.into_response(),
    };
    let cached = state.asset_shares.images.lock().ok().and_then(|cache| {
        cache
            .get(&code)
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(300))
            .map(|(_, bytes)| bytes.clone())
    });
    if let Some(png) = cached {
        return png_response(png);
    }
    let Ok(Ok(_permit)) = tokio::time::timeout(
        Duration::from_secs(5),
        state.asset_shares.render_slots.acquire(),
    )
    .await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let row = match detail(&state, &link, &link.currency).await {
        Ok(row) => row,
        Err(e) => return e.into_response(),
    };
    let icon = icon_data(&row)
        .await
        .or_else(|| local_icon_data(row["symbol"].as_str().unwrap_or("")));
    let svg = card_svg(&row, &code, icon.as_deref());
    let png = match tokio::task::spawn_blocking(move || render_card(&svg)).await {
        Ok(Ok(png)) => png,
        Ok(Err(e)) => return e.into_response(),
        Err(e) => return unavailable(e).into_response(),
    };
    if let Ok(mut cache) = state.asset_shares.images.lock() {
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(code, (Instant::now(), png.clone()));
    }
    png_response(png)
}
fn png_response(png: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        png,
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
fn remove_meta(html: &str, keys: &[&str]) -> String {
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
    cleaned
}
pub(super) fn social_metadata(html: &str, title: &str, description: &str, url: &str) -> String {
    let mut cleaned = remove_meta(
        html,
        &[
            "og:title",
            "og:description",
            "og:url",
            "twitter:title",
            "twitter:description",
            "description",
        ],
    );
    while let Some(start) = cleaned.find("<title") {
        let Some(end) = cleaned[start..].find("</title>") else {
            break;
        };
        cleaned.replace_range(start..start + end + 8, "");
    }
    let meta=format!("<title>{t}</title><meta property=\"og:title\" content=\"{t}\"/><meta property=\"og:description\" content=\"{d}\"/><meta property=\"og:url\" content=\"{u}\"/><meta name=\"twitter:title\" content=\"{t}\"/><meta name=\"twitter:description\" content=\"{d}\"/><meta name=\"description\" content=\"{d}\"/>",t=escape(title),d=escape(description),u=escape(url));
    cleaned.replacen("</head>", &(meta + "</head>"), 1)
}
fn social_image_metadata(html: &str, image: &str) -> String {
    let cleaned = remove_meta(
        html,
        &[
            "og:image",
            "og:image:type",
            "og:image:width",
            "og:image:height",
            "twitter:image",
            "twitter:card",
        ],
    );
    let meta=format!("<meta property=\"og:image\" content=\"{i}\"/><meta property=\"og:image:type\" content=\"image/png\"/><meta property=\"og:image:width\" content=\"1200\"/><meta property=\"og:image:height\" content=\"630\"/><meta name=\"twitter:card\" content=\"summary_large_image\"/><meta name=\"twitter:image\" content=\"{i}\"/>",i=escape(image));
    cleaned.replacen("</head>", &(meta + "</head>"), 1)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_previews_replace_generic_metadata_and_escape_untrusted_names() {
        let html="<html><head><title data-rh=\"true\"></title><title>Atlas</title><meta property=\"og:title\" content=\"Atlas\"/><meta name=\"description\" content=\"old\"/><meta property=\"og:image\" content=\"old.png\"/></head><body>app</body></html>";
        let result = social_image_metadata(
            &social_metadata(
                html,
                "Coin\"><script>bad</script>",
                "price & details",
                "https://justatlas.xyz/a/abcdefgh",
            ),
            "https://justatlas.xyz/a/abcdefgh/card.png",
        );
        assert_eq!(result.matches("property=\"og:title\"").count(), 1);
        assert_eq!(result.matches("property=\"og:image\"").count(), 1);
        assert_eq!(result.matches("<title>").count(), 1);
        assert!(!result.contains("<script>"));
        assert!(result.contains("&lt;script&gt;"));
        assert!(result.contains("price &amp; details"));
        assert!(!result.contains("old.png"));
        assert!(result.contains("<body>app</body>"));
    }
    #[test]
    fn aliases_are_short_currency_specific_and_safe() {
        let a = short_code("So11111111111111111111111111111111111111112", "NGN", 0);
        assert_eq!(a.len(), 8);
        assert!(valid_code(&a));
        assert_eq!(
            a,
            short_code("So11111111111111111111111111111111111111112", "NGN", 0)
        );
        assert_ne!(
            a,
            short_code("So11111111111111111111111111111111111111112", "USD", 0)
        );
        assert_ne!(a, short_code("different", "NGN", 0));
        assert_ne!(
            a,
            short_code("So11111111111111111111111111111111111111112", "NGN", 1)
        );
        for bad in ["../x", "<script>", "a bcd123", "abcdefgh/", ""] {
            assert!(!valid_code(bad));
        }
    }
    #[tokio::test]
    async fn stored_aliases_resolve_and_collisions_never_remap_an_asset() {
        let store = ShareState::new(None).await.unwrap();
        let first = short_code("asset-one", "NGN", 0);
        store.memory.lock().unwrap().insert(
            first.clone(),
            ShareLink {
                code: first.clone(),
                asset_id: "asset-two".into(),
                currency: "NGN".into(),
            },
        );
        let link = store.save("asset-one", "NGN").await.unwrap();
        assert_ne!(link.code, first);
        assert_eq!(store.get(&first).await.unwrap().asset_id, "asset-two");
        assert_eq!(store.get(&link.code).await.unwrap().asset_id, "asset-one");
        assert_eq!(
            store.save("asset-one", "NGN").await.unwrap().code,
            link.code
        );
        assert!(store.get("../bad").await.is_err());
        assert!(store.get("00000000").await.is_err());
    }
    #[test]
    fn social_icon_fetches_allow_only_approved_https_hosts() {
        assert!(
            allowed_icon_url("https://raw.githubusercontent.com/org/repo/main/icon.png").is_some()
        );
        for bad in [
            "http://raw.githubusercontent.com/x",
            "https://127.0.0.1/x",
            "https://localhost/x",
            "https://10.0.0.1/x",
            "https://raw.githubusercontent.com.evil.test/x",
            "https://u:p@raw.githubusercontent.com/x",
            "https://raw.githubusercontent.com:8443/x",
            "file:///etc/passwd",
        ] {
            assert!(allowed_icon_url(bad).is_none(), "{bad}");
        }
    }
    #[test]
    fn vector_logos_rasterize_without_external_resources() {
        let png=sanitize_icon(br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><circle cx="32" cy="32" r="30" fill="#2775ca"/><image href="/etc/passwd" width="64" height="64"/></svg>"##).unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!((image.width(), image.height()), (224, 224));
        assert!(sanitize_icon(
            br##"<svg xmlns="http://www.w3.org/2000/svg"><filter id="x"/></svg>"##
        )
        .is_none());
        assert!(sanitize_icon(
            br##"<svg:svg xmlns:svg="http://www.w3.org/2000/svg"><svg:filter id="x"/></svg:svg>"##
        )
        .is_none());
        assert!(sanitize_icon(
            br##"<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///etc/passwd">]><svg/>"##
        )
        .is_none());
    }
    #[test]
    fn social_card_is_a_real_png_with_brand_asset_and_short_footer() {
        let row =
            json!({"symbol":"SOL","name":"Solana","price":{"amount":"144000","currency":"NGN"}});
        let icon = local_icon_data("SOL").unwrap();
        let svg = card_svg(&row, "AbCd1234", Some(&icon));
        assert!(svg.contains("Trade SOL on Atlas"));
        assert!(svg.contains("Solana"));
        assert!(svg.contains("justatlas.xyz/a/AbCd1234"));
        assert!(svg.contains("<image"));
        let png = render_card(&svg).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!((image.width(), image.height()), (1200, 630));
        std::fs::write("/tmp/atlas-asset-social-preview.png", png).unwrap();
    }
}
