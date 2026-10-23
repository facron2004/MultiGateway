use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use mg_core::error::MgError;
use serde::Deserialize;

use crate::state::AppState;

fn daily_usage_fields(usage: &mg_core::DaySummary) -> (Option<u64>, bool) {
    let tokens = if usage.metered > 0 || usage.requests == 0 {
        Some(usage.tokens)
    } else {
        None
    };
    (tokens, usage.requests == usage.metered)
}

pub async fn gateway_status(State(state): State<AppState>) -> Response {
    let providers = state.providers.read();
    let mut status_map = serde_json::Map::new();

    for (name, provider) in providers.iter() {
        status_map.insert(
            name.clone(),
            serde_json::json!({
                "running": true,
                "label": provider.name(),
                "capabilities": provider.capabilities()
            }),
        );
    }

    Json(serde_json::Value::Object(status_map)).into_response()
}

pub async fn usage_board(State(state): State<AppState>) -> Response {
    let mut account_names = HashMap::new();
    for (provider_name, pool) in state.pools.iter() {
        for acc in pool.list() {
            let name = acc.display_name().to_string();
            if !name.is_empty() {
                account_names.insert((provider_name.clone(), acc.id.clone()), name);
            }
        }
    }
    let board = state.activity.board_with_names(&account_names);
    Json(board).into_response()
}

pub async fn provider_snapshot(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    let pool = state.pool(&provider_name).unwrap_or_else(|| {
        std::sync::Arc::new(mg_core::AccountPool::new(mg_core::resolve_pool_dir(
            &state.config.data_dir,
            &provider_name,
        )))
    });
    let raw_accounts = pool.list();

    let default_reg = if provider_name == "workbuddy"
        || provider_name == "trae"
        || provider_name == "qoder"
        || provider_name == "minimax"
    {
        "cn"
    } else {
        "intl"
    };

    let selected_region = provider.active_region();
    let mut current_region = selected_region
        .clone()
        .unwrap_or_else(|| default_reg.to_string());
    if selected_region.is_none() {
        let mut tally: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for acc in &raw_accounts {
            if !acc.region.is_empty() {
                *tally.entry(acc.region.as_str()).or_default() += 1;
            }
        }
        if let Some((region, _)) = tally.iter().max_by_key(|(_, count)| *count) {
            current_region = (*region).to_string();
        }
    }

    let total_accounts = raw_accounts.len();
    let mut enabled_count = 0;
    let mut ready_count = 0;
    let mut region_total = 0;
    let daily_usage = state.activity.account_usage(&provider_name);
    let now = mg_core::now_secs();

    let accounts: Vec<serde_json::Value> = raw_accounts
        .into_iter()
        .map(|acc| {
            let usage = daily_usage.get(&acc.id).cloned().unwrap_or_default();
            let (daily_tokens, daily_tokens_complete) = daily_usage_fields(&usage);
            let account_region = match provider_name.as_str() {
                "qoder" if mg_core::account::regions_match(&acc.region, "cn") => "cn",
                "qoder" => "intl",
                "workbuddy" if mg_core::account::regions_match(&acc.region, "cn") => "cn",
                "workbuddy" => "intl",
                "trae" if mg_core::account::regions_match(&acc.region, "cn") => "cn",
                "trae" => "intl",
                "minimax" if mg_core::account::regions_match(&acc.region, "cn") => "cn",
                "minimax" => "global",
                _ => acc.region.as_str(),
            };
            let is_cur = account_region == current_region;
            if is_cur {
                region_total += 1;
                if acc.enabled {
                    enabled_count += 1;
                    if acc.is_ready() {
                        ready_count += 1;
                    }
                }
            }

            let mut checkin_info = acc.checkin_info();
            let checkin_supported = provider
                .capabilities()
                .get("checkin")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && (provider_name != "workbuddy"
                    || (account_region == "cn"
                        && acc.organization_id.is_none()
                        && acc.extra.get("enterprise_id").is_none()));
            if let Some(obj) = checkin_info.as_object_mut() {
                obj.insert(
                    "supported".to_string(),
                    serde_json::Value::Bool(checkin_supported),
                );
            }

            serde_json::json!({
                "id": acc.id,
                "name": acc.display_name(),
                "email": acc.email,
                "region": account_region,
                "enabled": acc.enabled,
                "ready": acc.is_ready() && pool.cooldown_until(&acc.id) <= now,
                "cooldown_until": pool.cooldown_until(&acc.id),
                "remaining": acc.remaining(),
                "expires_at": acc.expires_at,
                "error": acc.error,
                "credential_invalid": acc.credential_invalid,
                "quota": acc.quota,
                "daily_tokens": daily_tokens,
                "daily_tokens_complete": daily_tokens_complete,
                "weight": acc.weight,
                "checkin": checkin_info,
            })
        })
        .collect();

    let mut capabilities = provider.capabilities();
    if provider_name == "workbuddy" && current_region != "cn" {
        if let Some(values) = capabilities.as_object_mut() {
            values.insert("checkin".to_string(), serde_json::Value::Bool(false));
        }
    }

    let snapshot = serde_json::json!({
        "provider": provider_name,
        "label": provider.name(),
        "region": current_region,
        "gateway": {
            "host": state.config.host,
            "port": state.config.port,
            "running": true,
            "error": ""
        },
        "capabilities": capabilities,
        "pool": {
            "total": total_accounts,
            "enabled": enabled_count,
            "ready": ready_count,
            "region_total": region_total
        },
        "accounts": accounts,
        "error": "",
        "activity": state.activity.view(&provider_name)
    });

    Json(snapshot).into_response()
}

pub async fn provider_models(
    Path(provider_name): Path<String>,
    Query(query): Query<ModelQuery>,
    State(state): State<AppState>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.models_for_region(query.region.as_deref()).await {
        Ok(models) => Json(serde_json::json!({
            "provider": provider_name,
            // Same envelope as /v1/models, so the dashboard shows the vision,
            // limit and thinking fields the inference API publishes.
            "data": models
                .into_iter()
                .map(|model| mg_providers::with_provider_metadata(
                    model,
                    mg_providers::ProviderDefaults::for_provider(&provider_name),
                ))
                .collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginPayload {
    #[allow(dead_code)]
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub redirect_url: Option<String>,
}

pub async fn provider_login(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<LoginPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider
        .start_login_region(payload.region.as_deref(), payload.redirect_url.as_deref())
        .await
    {
        Ok(res) => Json(res).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct PollPayload {
    pub login_id: String,
}

pub async fn provider_poll(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<PollPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.poll_login(&payload.login_id).await {
        Ok(res) => Json(res).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AccountActionPayload {
    pub id: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn provider_enable_account(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<AccountActionPayload>,
) -> Response {
    let pool = state.pool(&provider_name).unwrap_or_else(|| {
        std::sync::Arc::new(mg_core::AccountPool::new(mg_core::resolve_pool_dir(
            &state.config.data_dir,
            &provider_name,
        )))
    });

    if let Some(en) = payload.enabled {
        return match pool.update(&payload.id, |acc| acc.enabled = en) {
            Ok(Some(_)) => Json(serde_json::json!({ "ok": true })).into_response(),
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(MgError::not_found("Account not found").to_openai_json()),
            )
                .into_response(),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error.to_openai_json()),
            )
                .into_response(),
        };
    }

    (
        StatusCode::NOT_FOUND,
        Json(MgError::not_found("Account not found").to_openai_json()),
    )
        .into_response()
}

pub async fn provider_delete_account(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<AccountActionPayload>,
) -> Response {
    let pool = state.pool(&provider_name).unwrap_or_else(|| {
        std::sync::Arc::new(mg_core::AccountPool::new(mg_core::resolve_pool_dir(
            &state.config.data_dir,
            &provider_name,
        )))
    });

    match pool.delete(&payload.id) {
        Ok(_) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(e.to_openai_json())).into_response(),
    }
}

pub async fn provider_credits(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<AccountActionPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.refresh_quota(&payload.id).await {
        Ok(quota) => Json(serde_json::json!({ "ok": true, "quota": quota })).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

pub async fn provider_credentials_action(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<AccountActionPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.refresh_credentials(&payload.id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

pub async fn provider_checkin_action(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<AccountActionPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.checkin(&payload.id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct BatchPayload {
    pub operation: String,
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

pub async fn provider_batch(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<BatchPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    let pool = state.pool(&provider_name).unwrap_or_else(|| {
        std::sync::Arc::new(mg_core::AccountPool::new(mg_core::resolve_pool_dir(
            &state.config.data_dir,
            &provider_name,
        )))
    });
    let all_accounts = pool.list();

    let target_ids: std::collections::HashSet<String> = if let Some(ref ids) = payload.ids {
        ids.iter().cloned().collect()
    } else {
        all_accounts.iter().map(|a| a.id.clone()).collect()
    };

    let mut results = Vec::new();
    let mut succeeded = 0;
    let mut failed = 0;
    let mut skipped = 0;

    for acc in all_accounts {
        if !target_ids.contains(&acc.id) {
            continue;
        }

        let id = acc.id.clone();
        let name = acc.display_name().to_string();

        match payload.operation.as_str() {
            "enable" => {
                match pool.update(&id, |updated| updated.enabled = true) {
                    Ok(Some(_)) => {
                        succeeded += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": true,
                            "message": "账号已启用"
                        }));
                    }
                    Ok(None) => {
                        failed += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": false,
                            "message": "账号已不存在"
                        }));
                    }
                    Err(_) => {
                        failed += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": false,
                            "message": "保存失败"
                        }));
                    }
                }
            }
            "disable" => {
                match pool.update(&id, |updated| updated.enabled = false) {
                    Ok(Some(_)) => {
                        succeeded += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": true,
                            "message": "账号已停用"
                        }));
                    }
                    Ok(None) => {
                        failed += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": false,
                            "message": "账号已不存在"
                        }));
                    }
                    Err(_) => {
                        failed += 1;
                        results.push(serde_json::json!({
                            "id": id,
                            "name": name,
                            "ok": false,
                            "message": "保存失败"
                        }));
                    }
                }
            }
            "credits" => match provider.refresh_quota(&id).await {
                Ok(quota) => {
                    succeeded += 1;
                    let msg = if let Some(rem) = quota.remaining {
                        format!("积分/额度已更新: {:.2}", rem)
                    } else {
                        "积分/额度已更新".to_string()
                    };
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": true,
                        "message": msg
                    }));
                }
                Err(e) => {
                    failed += 1;
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": false,
                        "message": e.message()
                    }));
                }
            },
            "credentials" => match provider.refresh_credentials(&id).await {
                Ok(val) => {
                    let is_ok = val.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                    let msg = val
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("凭证刷新完成");
                    if is_ok {
                        succeeded += 1;
                    } else {
                        failed += 1;
                    }
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": is_ok,
                        "message": msg
                    }));
                }
                Err(e) => {
                    failed += 1;
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": false,
                        "message": e.message()
                    }));
                }
            },
            "checkin" => match provider.checkin(&id).await {
                Ok(val) => {
                    let is_ok = val.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                    let msg = val
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("签到完成");
                    if is_ok {
                        succeeded += 1;
                    } else {
                        failed += 1;
                    }
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": is_ok,
                        "message": msg
                    }));
                }
                Err(e) => {
                    failed += 1;
                    results.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "ok": false,
                        "message": e.message()
                    }));
                }
            },
            other => {
                skipped += 1;
                results.push(serde_json::json!({
                    "id": id,
                    "name": name,
                    "ok": false,
                    "skipped": true,
                    "message": format!("跳过不支持的操作: {}", other)
                }));
            }
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "results": results,
        "succeeded": succeeded,
        "failed": failed,
        "skipped": skipped
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RegionPayload {
    pub region: String,
}

pub async fn provider_region(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<RegionPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(provider) => provider,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };
    if provider
        .capabilities()
        .get("regions")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                MgError::bad_request(format!(
                    "Provider '{}' does not support region switching",
                    provider_name
                ))
                .to_openai_json(),
            ),
        )
            .into_response();
    }

    match provider.set_region(&payload.region).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => (
            StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(error.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ImportPayload {
    pub data: serde_json::Value,
}

pub async fn provider_import(
    Path(provider_name): Path<String>,
    State(state): State<AppState>,
    Json(payload): Json<ImportPayload>,
) -> Response {
    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    MgError::not_found(format!("Provider '{}' not found", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    match provider.import_accounts(payload.data).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
            Json(e.to_openai_json()),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ModelQuery {
    pub region: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ConfigurePayload {
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub host: Option<String>,
}

pub async fn gateway_configure(Json(_payload): Json<ConfigurePayload>) -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

pub async fn fallback_404() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": {
                "message": "Endpoint not found",
                "type": "not_found_error",
                "code": 404
            }
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyPayload {
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn api_key_generate(
    State(state): State<AppState>,
    Json(payload): Json<CreateKeyPayload>,
) -> Response {
    let name = payload.name.unwrap_or_else(|| "API Key".to_string());
    match state.key_manager.generate(&name) {
        Ok(entry) => Json(serde_json::json!({ "ok": true, "key": entry })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":{"message":format!("API Key 保存失败：{error}")}})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DeleteKeyPayload {
    pub id: String,
}

pub async fn api_key_delete(
    State(state): State<AppState>,
    Json(payload): Json<DeleteKeyPayload>,
) -> Response {
    match state.key_manager.delete(&payload.id) {
        Ok(ok) => Json(serde_json::json!({ "ok": ok })).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":{"message":format!("API Key 保存失败：{error}")}})),
        )
            .into_response(),
    }
}

pub async fn api_key_list(State(state): State<AppState>) -> Response {
    let list = state.key_manager.list();
    Json(serde_json::json!({ "keys": list })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct RevealQuery {
    pub id: String,
}

pub async fn settings_view(State(state): State<AppState>) -> Response {
    let list = state.key_manager.list();
    let desktop = match crate::desktop::load(&state.config.data_dir) {
        Ok(settings) => settings,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": {"message": error.to_string()}})),
            )
                .into_response()
        }
    };
    Json(serde_json::json!({
        "api_keys": list,
        "api_keys_active": !list.is_empty(),
        "auth_required": state.key_manager.auth_required(),
        "data_dir": std::path::absolute(&state.config.data_dir)
            .unwrap_or_else(|_| state.config.data_dir.clone()).to_string_lossy(),
        "key_backup_exists": state.config.data_dir.join("api_keys.json.bak").is_file(),
        "desktop": desktop,
        "desktop_supported": cfg!(any(windows, target_os = "macos")),
        "appearance": crate::appearance::load(&state.config.data_dir)
    }))
    .into_response()
}

fn remote_settings_origin(headers: &axum::http::HeaderMap, port: u16) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let allowed = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];
    !allowed.iter().any(|value| value == origin)
}

pub async fn desktop_settings_save(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(settings): Json<crate::desktop::DesktopSettings>,
) -> Response {
    if remote_settings_origin(&headers, state.config.port) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":{"message":"请在本机网关窗口修改桌面设置"}})),
        )
            .into_response();
    }
    match crate::desktop::save(&state.config, &settings) {
        Ok(()) => Json(serde_json::json!({"ok":true,"desktop":settings})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":{"message":error.to_string()}})),
        )
            .into_response(),
    }
}

pub async fn appearance_settings_save(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(settings): Json<crate::appearance::AppearanceSettings>,
) -> Response {
    if remote_settings_origin(&headers, state.config.port) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":{"message":"请在本机网关窗口修改外观设置"}})),
        )
            .into_response();
    }
    let settings = match crate::appearance::normalize(settings) {
        Ok(settings) => settings,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":{"message":message}})),
            )
                .into_response()
        }
    };
    match crate::appearance::save(&state.config.data_dir, &settings) {
        Ok(appearance) => Json(serde_json::json!({"ok":true,"appearance":appearance})).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":{"message":error.to_string()}})),
        )
            .into_response(),
    }
}

pub async fn settings_reveal(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<RevealQuery>,
) -> Response {
    if let Some(plain) = state.key_manager.reveal(&query.id) {
        Json(serde_json::json!({ "key": plain })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(MgError::not_found("Key not found").to_openai_json()),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::daily_usage_fields;
    use mg_core::DaySummary;

    #[test]
    fn daily_account_tokens_are_zero_when_unused_and_incomplete_when_unmetered() {
        assert_eq!(daily_usage_fields(&DaySummary::default()), (Some(0), true));
        assert_eq!(
            daily_usage_fields(&DaySummary {
                requests: 1,
                failed: 0,
                tokens: 0,
                metered: 0,
            }),
            (None, false)
        );
        assert_eq!(
            daily_usage_fields(&DaySummary {
                requests: 2,
                failed: 0,
                tokens: 42,
                metered: 1,
            }),
            (Some(42), false)
        );
    }
}
