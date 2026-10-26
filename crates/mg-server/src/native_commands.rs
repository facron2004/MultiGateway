use axum::{
    body::to_bytes,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    appearance::AppearanceSettings, desktop::DesktopSettings, handlers::management as service,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct CommandError {
    status: u16,
    message: String,
}

type CommandResult = Result<Value, CommandError>;

// Both transports call the same Rust business functions. IPC never sends an HTTP request.
async fn result(response: Response) -> CommandResult {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|error| CommandError {
            status: 500,
            message: error.to_string(),
        })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| CommandError {
        status: 500,
        message: error.to_string(),
    })?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(CommandError {
            status: status.as_u16(),
            message: value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .or_else(|| value.get("error").and_then(Value::as_str))
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("Operation failed"))
                .to_string(),
        })
    }
}

#[tauri::command]
pub async fn gateway_health() -> Value {
    serde_json::json!({"ok": true, "application": "MultiGateway-Rust", "version": env!("CARGO_PKG_VERSION")})
}

#[tauri::command]
pub async fn gateway_status(state: tauri::State<'_, AppState>) -> CommandResult {
    result(service::gateway_status(State(state.inner().clone())).await).await
}

#[tauri::command]
pub async fn usage_board(state: tauri::State<'_, AppState>) -> CommandResult {
    result(service::usage_board(State(state.inner().clone())).await).await
}

#[tauri::command]
pub async fn provider_snapshot(
    state: tauri::State<'_, AppState>,
    provider_name: String,
) -> CommandResult {
    result(service::provider_snapshot(Path(provider_name), State(state.inner().clone())).await)
        .await
}

#[tauri::command]
pub async fn provider_models(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    region: Option<String>,
) -> CommandResult {
    result(
        service::provider_models(
            Path(provider_name),
            Query(service::ModelQuery { region }),
            State(state.inner().clone()),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_login(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::LoginPayload,
) -> CommandResult {
    result(
        service::provider_login(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_poll(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::PollPayload,
) -> CommandResult {
    result(
        service::provider_poll(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_enable_account(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::AccountActionPayload,
) -> CommandResult {
    result(
        service::provider_enable_account(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_delete_account(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::AccountActionPayload,
) -> CommandResult {
    result(
        service::provider_delete_account(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_credits(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::AccountActionPayload,
) -> CommandResult {
    result(
        service::provider_credits(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_credentials_action(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::AccountActionPayload,
) -> CommandResult {
    result(
        service::provider_credentials_action(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_checkin_action(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::AccountActionPayload,
) -> CommandResult {
    result(
        service::provider_checkin_action(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_batch(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::BatchPayload,
) -> CommandResult {
    result(
        service::provider_batch(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_region(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::RegionPayload,
) -> CommandResult {
    result(
        service::provider_region(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_settings_save(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::ProviderSettingsPayload,
) -> CommandResult {
    result(
        service::provider_settings_save(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn provider_import(
    state: tauri::State<'_, AppState>,
    provider_name: String,
    payload: service::ImportPayload,
) -> CommandResult {
    result(
        service::provider_import(
            Path(provider_name),
            State(state.inner().clone()),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn gateway_configure(payload: service::ConfigurePayload) -> CommandResult {
    result(service::gateway_configure(Json(payload)).await).await
}

#[tauri::command]
pub async fn settings_view(state: tauri::State<'_, AppState>) -> CommandResult {
    result(service::settings_view(State(state.inner().clone())).await).await
}

#[tauri::command]
pub async fn desktop_settings_save(
    state: tauri::State<'_, AppState>,
    payload: DesktopSettings,
) -> CommandResult {
    result(
        service::desktop_settings_save(
            State(state.inner().clone()),
            HeaderMap::new(),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn appearance_settings_save(
    state: tauri::State<'_, AppState>,
    payload: AppearanceSettings,
) -> CommandResult {
    result(
        service::appearance_settings_save(
            State(state.inner().clone()),
            HeaderMap::new(),
            Json(payload),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn settings_reveal(state: tauri::State<'_, AppState>, id: String) -> CommandResult {
    result(
        service::settings_reveal(
            State(state.inner().clone()),
            Query(service::RevealQuery { id }),
        )
        .await,
    )
    .await
}

#[tauri::command]
pub async fn api_key_generate(
    state: tauri::State<'_, AppState>,
    payload: service::CreateKeyPayload,
) -> CommandResult {
    result(service::api_key_generate(State(state.inner().clone()), Json(payload)).await).await
}

#[tauri::command]
pub async fn api_key_delete(
    state: tauri::State<'_, AppState>,
    payload: service::DeleteKeyPayload,
) -> CommandResult {
    result(service::api_key_delete(State(state.inner().clone()), Json(payload)).await).await
}

#[tauri::command]
pub async fn api_key_list(state: tauri::State<'_, AppState>) -> CommandResult {
    result(service::api_key_list(State(state.inner().clone())).await).await
}

#[cfg(test)]
mod tests {
    use super::result;
    use axum::{http::StatusCode, response::IntoResponse, Json};
    use serde_json::json;

    #[tokio::test]
    async fn command_errors_preserve_business_message_and_status() {
        let error = result(
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":{"message":"Account not found"}})),
            )
                .into_response(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, 404);
        assert_eq!(error.message, "Account not found");
        let value = json!({"accounts":[{"id":"example","enabled":false}],"pool":{"total":1}});
        assert_eq!(
            result(Json(value.clone()).into_response()).await.unwrap(),
            value
        );
    }
}
