use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/"]
pub struct Assets;

const CACHE_CONTROL: HeaderValue = HeaderValue::from_static("no-cache");

/// Extracts the client's `If-None-Match` value so unchanged assets can 304.
pub fn if_none_match(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
}

fn etag_of(content: &rust_embed::EmbeddedFile) -> String {
    format!("\"{}\"", STANDARD.encode(content.metadata.sha256_hash()))
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("\"\""))
}

pub async fn serve_asset(path: &str, if_none_match: Option<&str>) -> Response<Body> {
    let target = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    let Some(content) = Assets::get(target) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))
            .body(Body::from("Asset not found"))
            .unwrap();
    };

    let etag = etag_of(&content);

    // Embedded assets are content-addressed by their sha256, so a matching
    // If-None-Match means the client's copy is byte-identical.
    if if_none_match.is_some_and(|value| value == etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, header_value(&etag))
            .header(header::CACHE_CONTROL, CACHE_CONTROL)
            .body(Body::empty())
            .unwrap();
    }

    let mime = mime_guess::from_path(target).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(mime.as_ref())
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        )
        .header(header::ETAG, header_value(&etag))
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .body(Body::from(content.data))
        .unwrap()
}
