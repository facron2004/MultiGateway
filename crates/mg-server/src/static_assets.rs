use axum::{
    body::Body,
    extract::Extension,
    http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode, Uri},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

struct Asset {
    bytes: Bytes,
    etag: String,
}

pub struct UiAssets(HashMap<String, Asset>);

impl UiAssets {
    // Browser and headless modes reuse Tauri's bundle instead of embedding a second UI.
    pub fn new(context: &tauri::Context<tauri::Wry>) -> Self {
        let assets = context.assets();
        Self(
            assets
                .iter()
                .map(|(path, _)| {
                    let content = assets
                        .get(&path.as_ref().into())
                        .expect("embedded Tauri asset");
                    let etag = format!("\"{}\"", STANDARD.encode(Sha256::digest(content.as_ref())));
                    (
                        path.trim_start_matches('/').to_string(),
                        Asset {
                            bytes: Bytes::from(content.into_owned()),
                            etag,
                        },
                    )
                })
                .collect(),
        )
    }
}

const CACHE_CONTROL: HeaderValue = HeaderValue::from_static("no-cache");

pub fn if_none_match(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
}

pub async fn serve_asset(
    assets: &UiAssets,
    path: &str,
    if_none_match: Option<&str>,
) -> Response<Body> {
    let target = if path.is_empty() || path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    let Some(content) = assets.0.get(target) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("Asset not found"))
            .unwrap();
    };
    if if_none_match.is_some_and(|value| value == content.etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &content.etag)
            .header(header::CACHE_CONTROL, CACHE_CONTROL)
            .body(Body::empty())
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            mime_guess::from_path(target)
                .first_or_octet_stream()
                .as_ref(),
        )
        .header(header::ETAG, &content.etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .body(Body::from(content.bytes.clone()))
        .unwrap()
}

pub async fn fallback(
    Extension(assets): Extension<Arc<UiAssets>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response<Body> {
    if matches!(method, Method::GET | Method::HEAD)
        && assets.0.contains_key(uri.path().trim_start_matches('/'))
    {
        let mut response = serve_asset(&assets, uri.path(), if_none_match(&headers)).await;
        if method == Method::HEAD {
            *response.body_mut() = Body::empty();
        }
        response
    } else {
        crate::handlers::fallback_404().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_bundle_also_serves_browser_assets_and_conditional_requests() {
        let assets = UiAssets::new(&tauri::generate_context!());
        let response = serve_asset(&assets, "index.html", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("text/html"));
        let etag = response.headers()[header::ETAG].to_str().unwrap();
        assert_eq!(
            serve_asset(&assets, "index.html", Some(etag))
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            serve_asset(&assets, "app.js", None).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            serve_asset(&assets, "not-an-asset", None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}
