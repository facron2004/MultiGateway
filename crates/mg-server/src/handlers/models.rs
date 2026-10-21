use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use mg_core::error::MgError;
use mg_protocols::openai::{ModelEntry, ModelList};
use mg_providers::{with_agent_metadata, Provider};
use parking_lot::RwLock;

use crate::handlers::management::ModelQuery;
use crate::state::AppState;

type CatalogResult = std::result::Result<Vec<ModelEntry>, MgError>;

/// Catalogs are near-static, but every `models()` call fans out to upstream HTTP
/// (Trae alone issues two calls with 20s + 8s timeouts) and the dashboard polls
/// this endpoint every 10s. A short TTL collapses six upstream round trips per
/// minute into one.
#[cfg(not(test))]
const MODEL_CACHE_TTL: Duration = Duration::from_secs(60);
/// Tests share one process-wide cache, so it is disabled to keep them independent.
#[cfg(test)]
const MODEL_CACHE_TTL: Duration = Duration::ZERO;

struct CachedCatalog {
    fetched_at: Instant,
    result: CatalogResult,
}

static MODEL_CACHE: LazyLock<RwLock<HashMap<String, CachedCatalog>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn cache_key(provider: &str, region: Option<&str>) -> String {
    match region {
        Some(region) => format!("{provider}:{region}"),
        None => provider.to_string(),
    }
}

async fn models_with_cache(
    provider_name: &str,
    provider: &Arc<dyn Provider>,
    region: Option<&str>,
) -> CatalogResult {
    let selected_region = region.map(str::to_string).or_else(|| provider.active_region());
    let selected_region = selected_region.as_deref();
    if MODEL_CACHE_TTL.is_zero() {
        return provider.models_for_region(selected_region).await;
    }

    let key = cache_key(provider_name, selected_region);

    if let Some(cached) = MODEL_CACHE.read().get(&key) {
        if cached.fetched_at.elapsed() < MODEL_CACHE_TTL {
            return cached.result.clone();
        }
    }

    let fetched = provider.models_for_region(selected_region).await;

    // Keep serving the last good catalog when an upstream call fails, so one
    // provider outage does not blank out the whole model list.
    let resolved = if fetched.is_err() {
        match MODEL_CACHE.read().get(&key) {
            Some(stale) if stale.result.is_ok() => stale.result.clone(),
            _ => fetched,
        }
    } else {
        fetched
    };

    MODEL_CACHE.write().insert(
        key,
        CachedCatalog {
            fetched_at: Instant::now(),
            result: resolved.clone(),
        },
    );

    resolved
}

pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ModelQuery>,
    headers: HeaderMap,
) -> Response {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();

    let token = crate::handlers::chat::bearer_token(auth_header);
    if !state.key_manager.is_authorized(token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(MgError::unauthorized("Invalid API key").to_openai_json()),
        )
            .into_response();
    }

    // Drop lock before .await
    let provider_list: Vec<(String, Arc<dyn Provider>)> = {
        let providers = state.providers.read();
        providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let mut all_models = Vec::new();
    let mut provider_errors = HashMap::new();

    let region = query.region.as_deref();
    let names: Vec<String> = provider_list.iter().map(|(name, _)| name.clone()).collect();
    // Providers are independent upstreams; fetching them concurrently turns the
    // sum of their latencies into the max, which dominates a cold cache.
    let catalogs = futures::future::join_all(
        provider_list
            .iter()
            .map(|(name, provider)| models_with_cache(name, provider, region)),
    )
    .await;

    for (name, catalog) in names.into_iter().zip(catalogs) {
        match catalog {
            Ok(models) => {
                for m in models {
                    let prefixed_id = format!("{}/{}", name, m.id);
                    if state.key_manager.check_model_allowed(token, &prefixed_id)
                        || state.key_manager.check_model_allowed(token, &m.id)
                    {
                        let mut prefixed = with_agent_metadata(m);
                        prefixed.id = prefixed_id;
                        prefixed.owned_by = name.clone();
                        all_models.push(prefixed);
                    }
                }
            }
            Err(e) => {
                provider_errors.insert(name, e.message());
            }
        }
    }

    all_models.sort_by(|left, right| left.id.cmp(&right.id));
    let response = ModelList {
        object: "list".to_string(),
        data: all_models,
        provider_errors: if !provider_errors.is_empty() {
            Some(provider_errors)
        } else {
            None
        },
    };

    Json(response).into_response()
}

pub async fn list_provider_models(
    Path(provider_name): Path<String>,
    Query(query): Query<ModelQuery>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();
    let token = crate::handlers::chat::bearer_token(auth_header);
    if !state.key_manager.is_authorized(token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(MgError::unauthorized("Invalid API key").to_openai_json()),
        )
            .into_response();
    }

    let provider = match state.get_provider(&provider_name) {
        Some(provider) => provider,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(MgError::not_found(format!("Provider '{}' not found", provider_name)).to_openai_json()),
            )
                .into_response();
        }
    };

    match models_with_cache(&provider_name, &provider, query.region.as_deref()).await {
        Ok(models) => {
            let data = models
                .into_iter()
                .filter(|model| {
                    let prefixed_id = format!("{}/{}", provider_name, model.id);
                    state.key_manager.check_model_allowed(token, &model.id)
                        || state.key_manager.check_model_allowed(token, &prefixed_id)
                })
                .map(with_agent_metadata)
                .collect();
            Json(ModelList {
                object: "list".to_string(),
                data,
                provider_errors: None,
            })
            .into_response()
        }
        Err(error) => (
            StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(error.to_openai_json()),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path as FsPath, PathBuf};
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::extract::{Path, Query, State};
    use axum::http::{header, HeaderMap, StatusCode};
    use futures::stream;
    use mg_core::account::AccountQuota;
    use mg_core::error::{MgError, Result};
    use mg_core::{Activity, Config, KeyManager};
    use mg_protocols::openai::{ChatCompletionChunk, ChatCompletionRequest, ModelEntry};
    use mg_providers::{BoxStream, Provider};
    use parking_lot::RwLock;
    use serde_json::{json, Value};

    use super::{list_models, list_provider_models};
    use crate::handlers::management::{provider_models, ModelQuery};
    use crate::state::AppState;

    struct MockProvider {
        id: &'static str,
        models: Vec<ModelEntry>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn name(&self) -> &'static str {
            self.id
        }

        fn capabilities(&self) -> Value {
            json!({})
        }

        async fn models(&self) -> Result<Vec<ModelEntry>> {
            Ok(self.models.clone())
        }

        async fn models_for_region(&self, region: Option<&str>) -> Result<Vec<ModelEntry>> {
            match region {
                Some(region) => Ok(vec![model(&format!("{region}-model"))]),
                None => self.models().await,
            }
        }

        async fn chat(
            &self,
            _req: ChatCompletionRequest,
            _session_id: Option<&str>,
        ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
            Ok((Box::pin(stream::empty()), String::new()))
        }

        async fn refresh_quota(&self, _account_id: &str) -> Result<AccountQuota> {
            Ok(AccountQuota {
                checked_at: 0.0,
                remaining: None,
                limit: None,
                reset_at: None,
                details: json!({}),
            })
        }

        async fn start_login(&self, _redirect_url: Option<&str>) -> Result<Value> {
            Ok(json!({"ok": true}))
        }

        async fn poll_login(&self, _login_id: &str) -> Result<Value> {
            Err(MgError::bad_request("unused"))
        }
    }

    fn model(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            object: "model".to_string(),
            owned_by: String::new(),
            created: None,
            permission: None,
            capabilities: None,
            credits: None,
            extra: serde_json::Map::new(),
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-rust-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(dir: &FsPath) -> AppState {
        let key_file = dir.join("api_keys.json");
        std::fs::write(
            key_file,
            serde_json::to_vec(&vec![json!({
                "id": "restricted",
                "name": "Restricted",
                "key": "test-key",
                "masked": "test********key",
                "realm": "",
                "models": ["trae/model-a"],
                "enabled": true,
                "created_at": "2026/10/20 00:00"
            })])
            .unwrap(),
        )
        .unwrap();

        let mut allowed_model = model("model-a");
        allowed_model
            .extra
            .insert("display_name".to_string(), json!("Model A"));
        allowed_model.extra.insert("multimodal".into(), json!(true));
        allowed_model.extra.insert("reasoning_effort_config".into(), json!({
            "support_thinking":true,"options":["light","high"],"default_level":"high"
        }));
        let trae: Arc<dyn Provider> = Arc::new(MockProvider {
            id: "trae",
            models: vec![allowed_model, model("model-b")],
        });
        let grok: Arc<dyn Provider> = Arc::new(MockProvider {
            id: "grok",
            models: vec![model("grok-a")],
        });
        let providers = HashMap::from([
            ("trae".to_string(), trae),
            ("grok".to_string(), grok),
        ]);

        AppState {
            config: Arc::new(Config {
                data_dir: dir.to_path_buf(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(dir)),
            key_manager: Arc::new(KeyManager::new(dir, None).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        }
    }

    async fn body(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn provider_model_route_returns_only_allowed_models_from_that_provider() {
        let dir = temp_dir();
        let state = state(&dir);
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )]);

        let response = list_provider_models(Path("trae".to_string()), Query(ModelQuery::default()), State(state.clone()), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = body(response).await;
        assert_eq!(data["data"].as_array().unwrap().len(), 1);
        assert_eq!(data["data"][0]["id"], "model-a");
        assert_eq!(data["data"][0]["display_name"], "Model A");
        assert_eq!(data["data"][0]["supports_reasoning"], true);
        assert_eq!(data["data"][0]["supports_vision"], true);
        assert_eq!(data["data"][0]["reasoning_efforts"], json!(["light", "high"]));

        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn combined_model_route_allows_provider_qualified_key_scopes() {
        let dir = temp_dir();
        let state = state(&dir);
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )]);

        let response = list_models(State(state.clone()), Query(ModelQuery::default()), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = body(response).await["data"].as_array().unwrap().clone();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "trae/model-a");
        assert_eq!(data[0]["display_name"], "Model A");
        assert_eq!(data[0]["capabilities"]["reasoning"], true);
        assert_eq!(data[0]["reasoning_default_effort"], "high");

        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn unrestricted_combined_catalog_only_advertises_prefixed_ids_and_capabilities() {
        let dir = temp_dir();
        let mut state = state(&dir);
        state.key_manager = Arc::new(KeyManager::new(&dir, Some("admin-key".into())).unwrap());
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer admin-key".parse().unwrap(),
        )]);
        let response = list_models(State(state.clone()), Query(ModelQuery::default()), headers).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = body(response).await;
        let models = data["data"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models.iter().map(|model| model["id"].as_str().unwrap()).collect::<Vec<_>>(),
            ["grok/grok-a", "trae/model-a", "trae/model-b"]);
        assert!(models.iter().all(|model| model["capabilities"].is_object()));
        assert!(models.iter().all(|model| model["supported_reasoning_efforts"].is_array()));
        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn management_model_route_passes_selected_region_to_provider() {
        let dir = temp_dir();
        let state = state(&dir);
        let response = provider_models(
            Path("trae".to_string()),
            Query(ModelQuery {
                region: Some("cn".to_string()),
            }),
            State(state.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let data = body(response).await;
        assert_eq!(data["data"][0]["id"], "cn-model");

        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }
}
