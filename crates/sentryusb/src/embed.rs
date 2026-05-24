use std::fmt::Write;

use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::{Embed, EmbeddedFile};

#[derive(Embed)]
#[folder = "static/"]
struct StaticFiles;

/// SPA fallback handler: serve static files or index.html for client-side
/// routing. Sets caching headers so repeat page loads don't re-download the
/// JS bundle from a car parked outside on flaky WiFi:
///
///   /assets/* — Vite content-hashes these (e.g. `index-CThdLhPi.js`); the
///   bytes are immutable, so cache them forever.
///
///   index.html and other entry files — `no-cache` so a soft reload picks
///   up a new bundle after an OTA update without a hard refresh.
///
/// If `build.sh` produced `.br` / `.gz` siblings for the asset, we serve
/// the pre-compressed bytes directly — no per-request compression CPU,
/// which matters on the Pi Zero 2W. Browsers that don't advertise br/gzip
/// support fall back to the raw bytes.
///
/// ETag is the first 16 bytes (hex) of the sha256 hash that rust-embed
/// pre-computes at compile time, suffixed with the encoding so a client
/// that downgrades from br→identity gets a fresh body instead of a stale
/// 304. If the client's `If-None-Match` matches, return 304 — the asset
/// isn't re-sent.
pub async fn spa_handler(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');

    if let Some((file, encoding)) = pick_encoding(path, &headers) {
        return serve_embedded(path, file, encoding, &headers);
    }

    if let Some(file) = StaticFiles::get(path) {
        return serve_embedded(path, file, None, &headers);
    }

    // SPA fallback. index.html is short and changes per release; let
    // tower-http's CompressionLayer handle its (small) gzip.
    match StaticFiles::get("index.html") {
        Some(file) => serve_embedded("index.html", file, None, &headers),
        None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

/// Returns (file, content-encoding) if the client accepts a pre-compressed
/// sibling we have on disk. Brotli first (better ratio), then gzip.
fn pick_encoding(path: &str, req_headers: &HeaderMap) -> Option<(EmbeddedFile, Option<&'static str>)> {
    let accept = req_headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if accept.contains("br") {
        if let Some(f) = StaticFiles::get(&format!("{path}.br")) {
            return Some((f, Some("br")));
        }
    }
    if accept.contains("gzip") {
        if let Some(f) = StaticFiles::get(&format!("{path}.gz")) {
            return Some((f, Some("gzip")));
        }
    }
    None
}

fn serve_embedded(
    path: &str,
    file: EmbeddedFile,
    encoding: Option<&'static str>,
    req_headers: &HeaderMap,
) -> Response {
    let etag = etag_for(&file, encoding);
    let cache_control = cache_control_for(path);

    if let Some(if_none_match) = req_headers.get(header::IF_NONE_MATCH) {
        if if_none_match.as_bytes() == etag.as_bytes() {
            let mut resp = Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::ETAG, &etag);
            if let Some(enc) = encoding {
                resp = resp.header(header::CONTENT_ENCODING, enc);
            }
            return resp.body(axum::body::Body::empty()).unwrap();
        }
    }

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::ETAG, &etag);
    if let Some(enc) = encoding {
        // Tell intermediaries the encoded body varies by Accept-Encoding
        // so a proxy doesn't hand the br bytes to a client that asked
        // for identity.
        resp = resp.header(header::CONTENT_ENCODING, enc);
        resp = resp.header(header::VARY, "Accept-Encoding");
    }
    resp.body(axum::body::Body::from(file.data)).unwrap()
}

fn etag_for(file: &EmbeddedFile, encoding: Option<&str>) -> String {
    let hash = file.metadata.sha256_hash();
    let mut s = String::with_capacity(40);
    s.push('"');
    for b in &hash[..16] {
        let _ = write!(s, "{:02x}", b);
    }
    if let Some(enc) = encoding {
        s.push('-');
        s.push_str(enc);
    }
    s.push('"');
    s
}

fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}
