use std::{collections::HashMap, sync::OnceLock, time::Instant};

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State as TauriState};

use crate::state::AppState;

/// Sidebar order, so the tray panel and the main window list providers alike.
const PROVIDER_ORDER: [&str; 7] = [
    "workbuddy",
    "trae",
    "minimax",
    "qoder",
    "antigravity",
    "copilot",
    "grok",
];

static STARTED: OnceLock<Instant> = OnceLock::new();

pub fn mark_started() {
    STARTED.get_or_init(Instant::now);
}

fn uptime_seconds() -> u64 {
    STARTED.get_or_init(Instant::now).elapsed().as_secs()
}

fn display_endpoint(host: &str, port: u16) -> String {
    let host = if matches!(host, "0.0.0.0" | "::" | "[::]") {
        "127.0.0.1"
    } else {
        host
    };
    if host.contains(':') && !host.starts_with('[') {
        format!(
            "http://[{}]:{}",
            host.trim_matches(|ch| ch == '[' || ch == ']'),
            port
        )
    } else {
        format!("http://{}:{}", host, port)
    }
}

pub fn summary(state: &AppState) -> Value {
    let days = state.activity.daily_totals(7);
    let today = days
        .last()
        .map(|(_, summary)| summary)
        .cloned()
        .unwrap_or_default();
    let active = state.activity.active_count();
    let keys = state.key_manager.list();
    let enabled_keys = keys.iter().filter(|key| key.enabled).count();

    let mut providers = Vec::new();
    let mut total_accounts = 0usize;
    let mut ready_accounts = 0usize;
    let mut account_labels: HashMap<(String, String), String> = HashMap::new();
    {
        let registered = state.providers.read();
        for name in PROVIDER_ORDER {
            let Some(provider) = registered.get(name) else {
                continue;
            };
            let accounts = state
                .pools
                .get(name)
                .map(|pool| pool.list())
                .unwrap_or_default();
            let total = accounts.len();
            let enabled = accounts.iter().filter(|account| account.enabled).count();
            let ready = accounts.iter().filter(|account| account.is_ready()).count();
            for account in &accounts {
                account_labels.insert(
                    (name.to_string(), account.id.clone()),
                    account.display_name().to_string(),
                );
            }
            total_accounts += total;
            ready_accounts += ready;
            let usage = state.activity.account_usage(name).into_values().fold(
                mg_core::DaySummary::default(),
                |mut total, entry| {
                    total.requests += entry.requests;
                    total.failed += entry.failed;
                    total.tokens += entry.tokens;
                    total.metered += entry.metered;
                    total
                },
            );
            providers.push(json!({
                "id": name,
                "label": provider.name(),
                "accounts": total,
                "enabled": enabled,
                "ready": ready,
                "requests": usage.requests,
                "failed": usage.failed,
                "tokens": usage.tokens,
            }));
        }
    }

    let recent = state
        .activity
        .recent(4)
        .into_iter()
        .map(|record| {
            let account = account_labels
                .get(&(record.provider.clone(), record.account.clone()))
                .cloned()
                .unwrap_or_default();
            json!({
                "provider": record.provider,
                "model": record.model,
                "status": record.status,
                "account": account,
                "tokens": record.input_tokens.unwrap_or(0) + record.output_tokens.unwrap_or(0),
                "time": record.timestamp,
                "error": record.error,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "endpoint": display_endpoint(&state.config.host, state.config.port),
        "host": state.config.host,
        "port": state.config.port,
        "uptime": uptime_seconds(),
        "active": active,
        "keys": enabled_keys,
        "auth_required": state.key_manager.auth_required(),
        "accounts": total_accounts,
        "ready_accounts": ready_accounts,
        "today": {
            "requests": today.requests,
            "failed": today.failed,
            "tokens": today.tokens,
            "metered": today.metered,
        },
        "days": days
            .into_iter()
            .map(|(date, summary)| json!({
                "date": date,
                "requests": summary.requests,
                "failed": summary.failed,
                "tokens": summary.tokens,
            }))
            .collect::<Vec<_>>(),
        "providers": providers,
        "recent": recent,
        "appearance": crate::appearance::load(&state.config.data_dir),
    })
}

pub fn tooltip(summary: &Value) -> String {
    let requests = summary
        .pointer("/today/requests")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let active = summary.get("active").and_then(Value::as_u64).unwrap_or(0);
    let ready = summary
        .get("ready_accounts")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = summary.get("accounts").and_then(Value::as_u64).unwrap_or(0);
    let english = summary
        .pointer("/appearance/language")
        .and_then(Value::as_str)
        == Some("en");
    if english {
        let mut text =
            format!("MultiGateway · {requests} requests today · {ready}/{total} accounts ready");
        if active > 0 {
            text.push_str(&format!(" · {active} in flight"));
        }
        text
    } else {
        let mut text = format!("MultiGateway · 今日 {requests} 次请求 · {ready}/{total} 账号就绪");
        if active > 0 {
            text.push_str(&format!(" · {active} 个进行中"));
        }
        text
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayAction {
    ShowMain,
    OpenLogs,
    OpenSettings,
    OpenProvider,
    Hide,
    Quit,
    OpenFolder,
}

pub fn hide_panel(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("tray") {
        let _ = window.hide();
    }
}

pub fn show_main(app: &AppHandle, hash: Option<&str>, provider: Option<&str>) {
    hide_panel(app);
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    if let Some(hash) = hash {
        if let Ok(literal) = serde_json::to_string(hash) {
            let _ = window.eval(format!("window.location.hash = {literal};"));
        }
    }
    if let Some(provider) = provider {
        if let Ok(selector) =
            serde_json::to_string(&format!(".provider[data-provider=\"{provider}\"]"))
        {
            let _ = window.eval(format!("document.querySelector({selector})?.click();"));
        }
    }
    let _ = window.set_focus();
}

#[tauri::command]
pub fn tray_summary(app: AppHandle, state: TauriState<'_, AppState>) -> Value {
    let value = summary(&state);
    #[cfg(any(windows, target_os = "macos"))]
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(tooltip(&value)));
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = &app;
    value
}

#[tauri::command]
pub async fn tray_action(
    app: AppHandle,
    state: TauriState<'_, AppState>,
    action: TrayAction,
    provider: Option<String>,
) -> Result<(), String> {
    match action {
        TrayAction::ShowMain => show_main(&app, None, None),
        TrayAction::OpenLogs => show_main(&app, Some("#/logs"), None),
        TrayAction::OpenSettings => show_main(&app, Some("#/settings/general"), None),
        TrayAction::OpenProvider => {
            let name = provider.ok_or_else(|| "Missing provider".to_string())?;
            if !state.providers.read().contains_key(&name) {
                return Err(format!("Unknown provider: {name}"));
            }
            show_main(&app, Some("#/accounts"), Some(&name));
        }
        TrayAction::Hide => hide_panel(&app),
        TrayAction::Quit => app.exit(0),
        TrayAction::OpenFolder => {
            open::that_detached(&state.config.data_dir).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub async fn http_summary(State(state): State<AppState>) -> Response {
    Json(summary(&state)).into_response()
}

#[cfg(test)]
mod tests {
    use super::display_endpoint;

    #[test]
    fn endpoint_stays_copyable_when_the_gateway_listens_on_every_interface() {
        assert_eq!(display_endpoint("127.0.0.1", 8791), "http://127.0.0.1:8791");
        assert_eq!(display_endpoint("0.0.0.0", 9000), "http://127.0.0.1:9000");
        assert_eq!(display_endpoint("::1", 9000), "http://[::1]:9000");
        assert_eq!(display_endpoint("[::]", 9000), "http://127.0.0.1:9000");
    }
}
