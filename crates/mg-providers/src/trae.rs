use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mg_core::account::{regions_match, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_core::AccountPool;
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, ChatUsage, ChunkChoice, ChunkDelta,
    ModelEntry,
};
use parking_lot::RwLock;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::catalog::{fallback_models, representative_account};
use crate::traits::{BoxStream, Provider};

const KNOWN_TRAE_RATES: &[(&str, f64)] = &[
    ("doubao-seed-2.1-pro", 0.8),
    ("doubao-seed-2.1-turbo", 0.2),
    ("doubao-seed-evolving", 0.8),
    ("doubao-seed-code", 0.06),
    ("doubao-seed-2.0-code", 0.2),
    ("seed-code-pro-0430", 0.8),
    ("glm-5.2", 0.78),
    ("glm-5.3", 0.78),
    ("glm-5.3-flash", 0.06),
    ("glm-5.3-flashx", 0.31),
    ("glm-5", 0.78),
    ("glm-5-turbo", 0.31),
    ("kimi-k2.6", 0.75),
    ("kimi-k2.7-code", 0.83),
    ("kimi-k3", 1.83),
    ("mimo-v2.6-flash", 0.06),
    ("mimo-v2.6-pro", 0.15),
    ("step-5-preview", 0.48),
    ("deepseek-v4.1-flash", 0.12),
    ("deepseek-v4-flash-official", 0.2),
    ("deepseek-v4-flash", 0.2),
    ("deepseek-v4-pro-official", 0.72),
    ("deepseek-v4-pro", 0.72),
    ("minimax-m3", 0.26),
    ("qwen3.8-max", 1.5),
    ("qwen-3.7-plus", 0.25),
];

fn known_trae_rate(name: &str) -> Option<f64> {
    let normalized = name.trim().to_ascii_lowercase();
    let alias = normalized.strip_suffix("-official").unwrap_or(&normalized);
    KNOWN_TRAE_RATES
        .iter()
        .find(|(model, _)| *model == normalized || *model == alias)
        .map(|(_, rate)| *rate)
}

fn numeric_rate(value: &serde_json::Value) -> Option<f64> {
    let rate = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    rate.is_finite().then_some(rate)
}

fn trae_features(model: &serde_json::Value) -> Option<serde_json::Value> {
    match model.get("features")? {
        serde_json::Value::Object(features) => Some(serde_json::Value::Object(features.clone())),
        serde_json::Value::String(raw) => serde_json::from_str(raw).ok(),
        _ => None,
    }
}

fn trae_feature_rate(model: &serde_json::Value) -> Option<f64> {
    let features = trae_features(model)?;
    features
        .pointer("/consumption_rate/data/rate")
        .or_else(|| features.pointer("/consumption_rate/rate"))
        .and_then(numeric_rate)
}

fn trae_price_factor(name: &str, model: &serde_json::Value) -> Option<f64> {
    model
        .get("price_factor")
        .and_then(numeric_rate)
        .or_else(|| model.get("credits").and_then(serde_json::Value::as_f64))
        .or_else(|| trae_feature_rate(model))
        .or_else(|| {
            model
                .pointer("/display_config/display_name")
                .or_else(|| model.get("display_name"))
                .and_then(serde_json::Value::as_str)
                .and_then(known_trae_rate)
        })
        .or_else(|| known_trae_rate(name))
}

fn positive_context_window(value: &serde_json::Value) -> Option<u64> {
    let context = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .or_else(|| {
            value
                .as_array()
                .and_then(|values| values.iter().find_map(positive_context_window))
        })?;
    (context > 0).then_some(context)
}

fn trae_context_window(model: &serde_json::Value) -> Option<u64> {
    let features = trae_features(model);
    let feature_dev = features
        .as_ref()
        .and_then(|features| features.pointer("/context_windows/data/dev_context"));
    let context = [
        model.pointer("/context_window_tokens/dev"),
        model.pointer("/context_window_size/default"),
        feature_dev,
        model.get("context_window"),
        model.get("context"),
    ]
    .into_iter()
    .flatten()
    .find_map(positive_context_window);
    context
}

fn normalize_trae_model_metadata(
    name: &str,
    model: &serde_json::Value,
    extra: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let price_factor = trae_price_factor(name, model);
    extra.remove("credits");
    extra.remove("price_factor");
    if let Some(rate) = price_factor {
        extra.insert("price_factor".to_string(), serde_json::json!(rate));
    }

    if let Some(context) = trae_context_window(model) {
        extra.insert("context_window".to_string(), serde_json::json!(context));
        extra.insert("context".to_string(), serde_json::json!(context));
    }

    let max_mode = model.get("max_mode").and_then(serde_json::Value::as_bool) == Some(true)
        || model
            .pointer("/display_config/max_mode")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    let features = trae_features(model);
    let feature_max = features
        .as_ref()
        .and_then(|features| features.pointer("/context_windows/data/max_context"));
    let max_context = model
        .get("max_context")
        .and_then(positive_context_window)
        .or_else(|| {
            model
                .pointer("/context_window_tokens/max")
                .and_then(positive_context_window)
        })
        .or_else(|| {
            model
                .pointer("/context_window_size/max")
                .and_then(positive_context_window)
        })
        .or_else(|| feature_max.and_then(positive_context_window))
        .or_else(|| max_mode.then_some(1_000_000));
    if let Some(max_context) =
        max_context.filter(|max| *max > trae_context_window(model).unwrap_or(0))
    {
        extra.insert("max_context".to_string(), serde_json::json!(max_context));
        extra.insert("max_mode".to_string(), serde_json::Value::Bool(true));
    } else {
        extra.remove("max_context");
        extra.insert("max_mode".to_string(), serde_json::Value::Bool(false));
    }

    price_factor.map(|rate| serde_json::json!(rate))
}

fn trae_fallback_models() -> Vec<ModelEntry> {
    fallback_models(
        &[
            "claude-3-5-sonnet",
            "claude-3-7-sonnet",
            "gpt-4o",
            "gpt-4o-mini",
            "deepseek-v3",
            "deepseek-r1",
            "qwen3.8-max",
        ],
        "trae",
    )
    .into_iter()
    .map(|mut model| {
        model.credits = known_trae_rate(&model.id).map(|rate| serde_json::json!(rate));
        if let Some(rate) = model.credits.as_ref() {
            model.extra.insert("price_factor".to_string(), rate.clone());
        }
        if model.id == "qwen3.8-max" {
            model
                .extra
                .insert("display_name".to_string(), serde_json::json!("Qwen3.8-Max"));
            model
                .extra
                .insert("context_window".to_string(), serde_json::json!(200000));
            model
                .extra
                .insert("context".to_string(), serde_json::json!(200000));
            model
                .extra
                .insert("max_context".to_string(), serde_json::json!(1000000));
            model
                .extra
                .insert("max_mode".to_string(), serde_json::Value::Bool(true));
        }
        model
    })
    .collect()
}

#[derive(Clone)]
struct TraeSurface {
    remote_base: String,
    origin: String,
    model_host: String,
    default_language: &'static str,
    default_user_region: &'static str,
    package_type: &'static str,
    ug_host: &'static str,
    pay_host: &'static str,
    pay_version: &'static str,
}

struct TraeSurfaceEnv {
    web_base: Option<String>,
    web_origin: Option<String>,
    model_host: Option<String>,
}

fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn trae_surface(region: &str) -> TraeSurface {
    let intl = !regions_match(region, "cn");
    let prefix = if intl { "TRAE_INTL" } else { "TRAE" };
    trae_surface_with(
        region,
        TraeSurfaceEnv {
            web_base: env_override(&format!("{prefix}_WEB_BASE_URL")),
            web_origin: env_override(&format!("{prefix}_WEB_ORIGIN")),
            model_host: env_override(&format!("{prefix}_MODEL_API_HOST")),
        },
    )
}

fn trae_surface_with(region: &str, env: TraeSurfaceEnv) -> TraeSurface {
    let intl = !regions_match(region, "cn");
    let remote_fallback = if intl {
        "https://core-normal.trae.ai/api/remote/v1"
    } else {
        "https://trae-api-cn.mchost.guru/api/remote/v1"
    };
    let model_fallback = if intl {
        "https://a0ai-api-sg.byteintlapi.com"
    } else {
        "https://trae-api-cn.mchost.guru"
    };
    let remote_base =
        normalize_trae_remote_base(env.web_base.unwrap_or_else(|| remote_fallback.to_string()));
    let model_host = normalize_trae_model_host(
        env.model_host.unwrap_or_else(|| model_fallback.to_string()),
        intl,
    );
    let origin = env.web_origin.unwrap_or_else(|| {
        if intl {
            "https://work.trae.ai".to_string()
        } else if remote_base.contains("trae-api-cn")
            || remote_base.contains("mchost.guru")
            || remote_base.contains("core-normal.trae.cn")
        {
            "https://solo.trae.cn".to_string()
        } else {
            "https://solo.trae.ai".to_string()
        }
    });
    if intl {
        TraeSurface {
            remote_base,
            origin,
            model_host,
            default_language: "en",
            default_user_region: "SG",
            package_type: "stable_oversea",
            ug_host: "https://api.marscode.com",
            pay_host: "https://grow-normal.trae.ai",
            pay_version: "v1",
        }
    } else {
        TraeSurface {
            remote_base,
            origin,
            model_host,
            default_language: "zh-CN",
            default_user_region: "CN",
            package_type: "stable_cn",
            ug_host: "https://api.trae.cn",
            pay_host: "https://api.trae.cn",
            pay_version: "v2",
        }
    }
}

fn normalize_trae_remote_base(configured: String) -> String {
    let base = configured.trim_end_matches('/').to_string();
    let known_trae_host = [".trae.cn", ".trae.ai", "mchost.guru"]
        .iter()
        .any(|host| base.to_ascii_lowercase().contains(host));
    if known_trae_host && !base.contains("/api/remote/") {
        format!("{base}/api/remote/v1")
    } else {
        base
    }
}

fn normalize_trae_model_host(configured: String, intl: bool) -> String {
    let mut base = configured.trim_end_matches('/').to_string();
    let lowered = base.to_ascii_lowercase();
    if !intl
        && ["api.trae.cn", "api.trae.com.cn", "oauth"]
            .iter()
            .any(|host| lowered.contains(host))
        && !lowered.contains("mchost.guru")
    {
        base = "https://trae-api-cn.mchost.guru".to_string();
    }
    if let Some(index) = base.find("/api/remote/") {
        base.truncate(index);
    }
    base
}

fn parse_trae_solo_catalog(payload: &serde_json::Value) -> Vec<ModelEntry> {
    let models = payload
        .get("config_info_list")
        .or_else(|| payload.pointer("/data/config_info_list"))
        .and_then(serde_json::Value::as_array);
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for model in models.into_iter().flatten() {
        let usage = model
            .get("usage")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        let has_custom_models = model.get("custom_models").is_some_and(|value| match value {
            serde_json::Value::Null => false,
            serde_json::Value::Bool(value) => *value,
            serde_json::Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
            serde_json::Value::String(value) => !value.is_empty(),
            serde_json::Value::Array(value) => !value.is_empty(),
            serde_json::Value::Object(value) => !value.is_empty(),
        });
        if (!usage.is_empty() && !usage.eq_ignore_ascii_case("chat_completion"))
            || has_custom_models
            || model
                .get("is_invisible_to_user")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            continue;
        }
        let Some(name) = model
            .get("config_name")
            .or_else(|| model.get("name"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let config_name = name.trim();
        let id = config_name.to_ascii_lowercase();
        if id.is_empty() || !seen.insert(id.to_ascii_lowercase()) {
            continue;
        }
        let mut extra = model.as_object().cloned().unwrap_or_default();
        extra.insert("config_name".to_string(), serde_json::json!(config_name));
        extra
            .entry("model_name".to_string())
            .or_insert_with(|| serde_json::json!(config_name));
        let display_name = model
            .get("display_name")
            .or_else(|| model.pointer("/display_config/display_name"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(config_name);
        extra.insert("display_name".to_string(), serde_json::json!(display_name));
        let credits = normalize_trae_model_metadata(config_name, model, &mut extra);
        rows.push(ModelEntry {
            id,
            object: "model".to_string(),
            owned_by: "trae".to_string(),
            created: None,
            permission: None,
            capabilities: None,
            credits,
            extra,
        });
    }
    rows
}

fn is_hidden_trae_model(name: &str, model: &serde_json::Value) -> bool {
    let lowered = name.trim().to_ascii_lowercase();
    if lowered.is_empty()
        || matches!(lowered.as_str(), "auto" | "work" | "aquila" | "sagitta")
        || lowered.contains("subagent")
        || lowered.contains("sub_agent")
        || lowered.ends_with("_agent")
        || model
            .pointer("/display_config/is_invisible_to_user")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return true;
    }
    let display_name = model
        .pointer("/display_config/display_name")
        .or_else(|| model.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    matches!(display_name, "" | "-")
        && model
            .get("config_source")
            .and_then(serde_json::Value::as_i64)
            != Some(1)
}

fn push_trae_model(
    model: &serde_json::Value,
    seen: &mut HashSet<String>,
    rows: &mut Vec<ModelEntry>,
) {
    let Some(name) = model.get("name").and_then(serde_json::Value::as_str) else {
        return;
    };
    if is_hidden_trae_model(name, model) {
        return;
    }
    let config_name = name.trim();
    let id = config_name.to_ascii_lowercase();
    if !seen.insert(id.to_ascii_lowercase()) {
        return;
    }
    let mut extra = model.as_object().cloned().unwrap_or_default();
    for key in [
        "id",
        "object",
        "owned_by",
        "created",
        "permission",
        "capabilities",
    ] {
        extra.remove(key);
    }
    if let Some(display_name) = model
        .pointer("/display_config/display_name")
        .or_else(|| model.get("display_name"))
        .or_else(|| model.get("name"))
        .and_then(serde_json::Value::as_str)
    {
        extra.insert("display_name".to_string(), serde_json::json!(display_name));
    }
    extra.insert("config_name".to_string(), serde_json::json!(config_name));
    let credits = normalize_trae_model_metadata(config_name, model, &mut extra);
    rows.push(ModelEntry {
        id,
        object: "model".to_string(),
        owned_by: "trae".to_string(),
        created: None,
        permission: None,
        capabilities: model.get("capabilities").cloned(),
        credits,
        extra,
    });
}

fn parse_trae_catalog(payload: &serde_json::Value) -> Vec<ModelEntry> {
    let data = payload.get("data").unwrap_or(payload);
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let owned_models = data.get("models").and_then(serde_json::Value::as_array);
    let Some(items) = data
        .as_array()
        .or_else(|| data.get("list").and_then(serde_json::Value::as_array))
        .or(owned_models)
    else {
        return rows;
    };
    let mut items: Vec<_> = items.iter().collect();
    // Work and Design may precede Agent and repeat its models without Max metadata.
    items.sort_by_key(|item| {
        match item
            .get("function")
            .or_else(|| item.get("agent_type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "solo_agent_remote" => 0,
            "solo_agent_lite" => 1,
            _ => 2,
        }
    });
    for item in items {
        if let Some(models) = item.get("models").and_then(serde_json::Value::as_array) {
            for model in models {
                push_trae_model(model, &mut seen, &mut rows);
            }
        } else if item.get("name").is_some() {
            push_trae_model(item, &mut seen, &mut rows);
        }
    }
    rows
}

fn trae_model_config_names(models: &[ModelEntry]) -> HashMap<String, String> {
    let mut config_names = HashMap::new();
    for model in models {
        let config_name = model
            .extra
            .get("config_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&model.id)
            .to_string();
        config_names.insert(model.id.to_ascii_lowercase(), config_name.clone());
        config_names.insert(config_name.to_ascii_lowercase(), config_name.clone());
        if let Some(display_name) = model
            .extra
            .get("display_name")
            .and_then(serde_json::Value::as_str)
        {
            config_names.insert(display_name.to_ascii_lowercase(), config_name);
        }
    }
    config_names
}

fn canonical_trae_model_name(model: &str, config_names: &HashMap<String, String>) -> String {
    config_names
        .get(&model.to_ascii_lowercase())
        .cloned()
        .unwrap_or_else(|| model.to_string())
}

fn trae_provider_value<'a>(account: &'a mg_core::Account, keys: &[&str]) -> Option<&'a str> {
    let specific = account
        .provider_specific
        .as_ref()
        .filter(|value| !value.is_null())
        .or_else(|| account.extra.get("provider_specific"))
        .or_else(|| account.extra.get("providerSpecificData"));
    specific.and_then(|value| {
        keys.iter().find_map(|key| {
            value
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    })
}

fn trae_account_value(account: &mg_core::Account, keys: &[&str], fallback: &str) -> String {
    trae_provider_value(account, keys)
        .unwrap_or(fallback)
        .to_string()
}

fn trae_refresh_host(account: &mg_core::Account) -> String {
    let configured = trae_provider_value(account, &["host", "apiHost", "api_host"])
        .or_else(|| {
            account
                .extra
                .get("host")
                .and_then(serde_json::Value::as_str)
        })
        .map(str::trim)
        .filter(|host| host.starts_with("http"));
    if let Some(host) = configured {
        return host.trim_end_matches('/').to_string();
    }
    if regions_match(&account.region, "cn") {
        "https://api.trae.com.cn".to_string()
    } else {
        "https://api.trae.ai".to_string()
    }
}

fn trae_refresh_client_id(account: &mg_core::Account) -> String {
    trae_account_value(
        account,
        &["client_id", "clientId", "clientID", "ClientID"],
        "ono9krqynydwx5",
    )
}

fn trae_refresh_user_id(account: &mg_core::Account) -> String {
    trae_provider_value(account, &["bizUserId", "user_id", "userId", "userUniqueId"])
        .unwrap_or(&account.id)
        .to_string()
}

fn trae_jwt_expiry(token: &str) -> Option<f64> {
    let body = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(body))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("exp")
        .and_then(serde_json::Value::as_f64)
        .filter(|expiry| *expiry > 0.0)
}

fn trae_expire_at(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return Some(if number > 1e11 {
            number / 1000.0
        } else {
            number
        });
    }
    let text = value.as_str()?.trim();
    if let Ok(number) = text.parse::<f64>() {
        return Some(if number > 1e11 {
            number / 1000.0
        } else {
            number
        });
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|timestamp| timestamp.timestamp() as f64)
}

fn trae_token_needs_refresh(account: &mg_core::Account) -> bool {
    account.access_token.is_empty()
        || (account.expires_at > 0.0
            && account.expires_at <= mg_core::now_secs() + 300.0)
}

fn trae_web_headers(account: &mg_core::Account, surface: &TraeSurface) -> Vec<(String, String)> {
    let language = trae_account_value(account, &["appLanguage"], surface.default_language);
    let user_region = trae_account_value(
        account,
        &["userRegion", "user_region"],
        surface.default_user_region,
    );
    [
        (
            "Authorization",
            format!("Cloud-IDE-JWT {}", account.access_token),
        ),
        ("Content-Type", "application/json".to_string()),
        ("Accept", "application/json, text/event-stream, */*".to_string()),
        ("X-Trae-Client-Type", "web".to_string()),
        ("X-Preferenced-Language", language),
        ("x-user-region", user_region),
        ("Origin", surface.origin.clone()),
        (
            "Referer",
            format!("{}/", surface.origin.trim_end_matches('/')),
        ),
        (
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36".to_string(),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

fn apply_headers(
    mut request: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request
}

fn trae_text(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        part.get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| {
                        part.get("content")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn trae_history_blocks(messages: &[ChatMessage]) -> Vec<String> {
    let mut blocks = Vec::new();
    for message in messages {
        let text = trae_text(&message.content);
        match message.role.as_str() {
            "system" => blocks.push(format!("[System]\n{text}")),
            "assistant" => {
                let mut block = text;
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        if !block.is_empty() {
                            block.push_str("\n\n");
                        }
                        block.push_str(&format!(
                            "[Tool Call: {} {}]\nArguments: {}",
                            call.id, call.function.name, call.function.arguments
                        ));
                    }
                }
                if !block.is_empty() {
                    blocks.push(format!("[Assistant]\n{block}"));
                }
            }
            "tool" => {
                let identifier = message.tool_call_id.as_deref().unwrap_or("unknown");
                let name = message.name.as_deref().unwrap_or("tool");
                blocks.push(format!(
                    "[Client Tool Result: {identifier} {name} status=succeeded]\n{text}"
                ));
            }
            _ => {
                if !text.is_empty() {
                    blocks.push(text);
                }
            }
        }
    }
    blocks
}

fn trae_query(messages: &[ChatMessage]) -> String {
    serde_json::json!([{
        "type": "text",
        "data": {"content": trae_history_blocks(messages).join("\n\n")}
    }])
    .to_string()
}

fn trae_content(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    trae_history_blocks(messages)
        .into_iter()
        .map(|block| serde_json::json!({"type": "text", "data": {"content": block}}))
        .collect()
}

fn trae_resolve_mode(model: &str) -> (&'static str, &'static str, String, &'static str) {
    let normalized = model.trim().to_ascii_lowercase();
    if matches!(normalized.as_str(), "work" | "auto-work" | "solo-work") {
        return ("work", "auto", String::new(), "solo_work_remote");
    }
    if normalized.is_empty() || normalized == "auto" {
        return ("code", "auto", String::new(), "solo_agent_remote");
    }
    ("code", "manual", model.to_string(), "solo_agent_remote")
}

fn trae_common_params(account: &mg_core::Account, surface: &TraeSurface, mode: &str) -> String {
    let language = if surface
        .default_language
        .to_ascii_lowercase()
        .starts_with("zh")
    {
        "zh-cn"
    } else {
        "en"
    };
    serde_json::json!({
        "language": language,
        "app_language": trae_account_value(account, &["appLanguage"], surface.default_language),
        "quality": "stable",
        "app_version": trae_account_value(account, &["appVersion"], "1.0.0.1229"),
        "web_id": trae_account_value(account, &["webId"], &account.id),
        "user_identity": trae_account_value(account, &["userIdentity"], "Free"),
        "is_freshman": "0",
        "biz_user_id": trae_account_value(account, &["bizUserId"], &account.id),
        "user_unique_id": trae_account_value(account, &["userUniqueId"], &account.id),
        "scope": trae_account_value(account, &["scope"], "solo"),
        "tenant": trae_account_value(account, &["tenant"], "marscode"),
        "region": trae_account_value(account, &["region"], surface.default_user_region),
        "aiRegion": trae_account_value(account, &["aiRegion", "region"], surface.default_user_region),
        "is_privacy_mode": 0,
        "privacy_mode": "off",
        "solo_chat_mode": mode
    })
    .to_string()
}

fn trae_session_body(
    messages: &[ChatMessage],
    account: &mg_core::Account,
    surface: &TraeSurface,
    mode: &str,
    strategy: &str,
    model_name: &str,
    agent_type: &str,
    custom_model: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut initial = serde_json::json!({
        "chat_session_id": "",
        "content": trae_content(messages),
        "query": trae_query(messages),
        "model_name": model_name,
        "agent_type": agent_type,
        "model_selection_strategy": strategy,
        "common_params": trae_common_params(account, surface, mode)
    });
    if let Some(custom_model) = custom_model {
        initial["custom_model"] = custom_model;
    }
    serde_json::json!({
        "mode": mode,
        "environment_id": "default",
        "initial_message": initial,
        "env": "remote",
        "auto_create_project": false,
        "origin": "web"
    })
}

fn trae_custom_model(payload: &serde_json::Value, model_name: &str) -> Option<serde_json::Value> {
    let wanted = model_name.trim().to_ascii_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let mut matches = Vec::new();
    let data = payload.get("data").unwrap_or(payload);
    let groups = data
        .as_array()
        .or_else(|| data.get("list").and_then(serde_json::Value::as_array))
        .or_else(|| data.get("models").and_then(serde_json::Value::as_array));
    let Some(groups) = groups else {
        return None;
    };
    for group in groups {
        if let Some(models) = group.get("models").and_then(serde_json::Value::as_array) {
            matches.extend(models.iter().filter(|model| model.get("name").is_some()));
        } else if group.get("name").is_some() {
            matches.push(group);
        }
    }
    let raw = matches.into_iter().find(|model| {
        let name = model
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let display = model
            .get("display_name")
            .or_else(|| model.get("display_model_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        name.eq_ignore_ascii_case(model_name)
            || display.eq_ignore_ascii_case(model_name)
            || name.to_ascii_lowercase() == wanted
    })?;
    let mut config = raw.clone();
    if let Some(features) = config.get("features").and_then(serde_json::Value::as_str) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(features) {
            config["features"] = parsed;
        }
    }
    let name = config
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(model_name)
        .to_string();
    if config
        .get("config_name")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        config["config_name"] = serde_json::json!(name);
    }
    if config
        .get("model_name")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        config["model_name"] = serde_json::json!(name);
    }
    config["use_remote_service"] = serde_json::Value::Bool(
        !config
            .get("client_connect")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    );
    Some(config)
}

fn trae_visible_text(data: &serde_json::Value) -> String {
    let nested = ["message", "agent_message", "assistant_message"]
        .iter()
        .find_map(|key| data.get(*key).filter(|value| value.is_object()))
        .unwrap_or(data);
    match nested.get("content") {
        Some(serde_json::Value::String(text)) => return text.clone(),
        Some(serde_json::Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    let kind = part
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if matches!(kind, "reasoning" | "thinking" | "reasoning_text") {
                        return None;
                    }
                    part.get("text")
                        .or_else(|| part.get("content"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<String>();
            if !text.is_empty() {
                return text;
            }
        }
        _ => {}
    }
    for key in ["text", "response", "answer", "output"] {
        if let Some(text) = nested.get(key).and_then(serde_json::Value::as_str) {
            return text.to_string();
        }
    }
    String::new()
}

fn trae_apply_snapshot(visible: &mut String, next: &str) -> String {
    if next.is_empty() || next == visible.as_str() || visible.starts_with(next) {
        return String::new();
    }
    if let Some(delta) = next.strip_prefix(visible.as_str()) {
        *visible = next.to_string();
        return delta.to_string();
    }
    visible.push_str(next);
    next.to_string()
}

fn trae_usage(data: &serde_json::Value) -> Option<ChatUsage> {
    let usage = data.get("usage").unwrap_or(data);
    let number = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|key| {
                usage
                    .get(*key)
                    .and_then(serde_json::Value::as_u64)
                    .or_else(|| {
                        usage
                            .get(*key)
                            .and_then(serde_json::Value::as_f64)
                            .map(|value| value as u64)
                    })
            })
            .unwrap_or(0)
    };
    let prompt = number(&["prompt_tokens", "input_tokens"]);
    let completion = number(&["completion_tokens", "output_tokens"]);
    let total = number(&["total_tokens"]).max(prompt + completion);
    let credits = usage
        .get("credits_consumed")
        .or_else(|| usage.get("credit"))
        .or_else(|| usage.get("credits"))
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    (prompt > 0 || completion > 0 || total > 0).then_some(ChatUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_tokens_details: None,
        credits,
    })
}

struct TraeTranslator {
    id: String,
    model: String,
    started: bool,
    visible: String,
    reasoning: String,
    usage: Option<ChatUsage>,
    finished: bool,
}

impl TraeTranslator {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-trae-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            visible: String::new(),
            reasoning: String::new(),
            usage: None,
            finished: false,
        }
    }

    fn chunk(
        &self,
        delta: ChunkDelta,
        finish_reason: Option<String>,
        usage: Option<ChatUsage>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: mg_core::now_secs() as u64,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason,
            }],
            usage,
        }
    }

    fn push(&mut self, event: &str, data: &str) -> Vec<Result<ChatCompletionChunk>> {
        let event = event.trim().to_ascii_lowercase();
        if matches!(event.as_str(), "heartbeat" | "keepalive" | "ping") {
            return Vec::new();
        }
        let payload =
            serde_json::from_str::<serde_json::Value>(data).unwrap_or(serde_json::json!({}));
        if event == "error" {
            self.finished = true;
            let message = payload
                .get("message")
                .or_else(|| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Trae upstream error");
            return vec![Err(MgError::provider(
                502,
                format!("Trae request failed: {message}"),
                "upstream_error",
            ))];
        }
        if event == "token_usage" {
            self.usage = trae_usage(&payload).or(self.usage.clone());
            return Vec::new();
        }
        if event == "done" {
            self.finished = true;
            return vec![Ok(self.chunk(
                ChunkDelta::default(),
                Some(
                    payload
                        .get("finish_reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("stop")
                        .to_string(),
                ),
                self.usage.clone(),
            ))];
        }
        let reasoning = if event == "plan_item" {
            payload
                .get("reasoning_content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        let text = if event == "plan_item" {
            let thought = payload
                .get("thought")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if !thought.is_empty() {
                thought.to_string()
            } else {
                trae_visible_text(&serde_json::json!({
                    "content": payload.get("content").cloned().unwrap_or(serde_json::Value::Null)
                }))
            }
        } else if matches!(
            event.as_str(),
            "message" | "assistant_message" | "response" | "text" | "output"
        ) {
            trae_visible_text(&payload)
        } else {
            String::new()
        };
        let reasoning_delta = trae_apply_snapshot(&mut self.reasoning, &reasoning);
        let delta = trae_apply_snapshot(&mut self.visible, &text);
        if reasoning_delta.is_empty() && delta.is_empty() {
            return Vec::new();
        }
        let mut chunks = Vec::new();
        if !self.started {
            self.started = true;
            chunks.push(Ok(self.chunk(
                ChunkDelta {
                    role: Some("assistant".to_string()),
                    ..ChunkDelta::default()
                },
                None,
                None,
            )));
        }
        chunks.push(Ok(self.chunk(
            ChunkDelta {
                content: (!delta.is_empty()).then_some(delta),
                reasoning_content: (!reasoning_delta.is_empty()).then_some(reasoning_delta),
                ..ChunkDelta::default()
            },
            None,
            None,
        )));
        chunks
    }

    fn finish(&mut self) -> Option<ChatCompletionChunk> {
        if self.finished {
            return None;
        }
        self.finished = true;
        if !self.started && self.visible.is_empty() {
            return None;
        }
        Some(self.chunk(
            ChunkDelta::default(),
            Some("stop".to_string()),
            self.usage.clone(),
        ))
    }
}

async fn fetch_trae_custom_model(
    client: &Client,
    account: &mg_core::Account,
    surface: &TraeSurface,
    model_name: &str,
) -> Option<serde_json::Value> {
    let url = format!(
        "{}/models?functions=solo_agent_remote%2Csolo_work_remote%2Csolo_design_remote&show_custom_model=true",
        surface.remote_base
    );
    let response = apply_headers(
        client.get(url).timeout(Duration::from_secs(20)),
        &trae_web_headers(account, surface),
    )
    .send()
    .await
    .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload = response.json::<serde_json::Value>().await.ok()?;
    trae_custom_model(&payload, model_name)
}

async fn stop_trae_session(
    client: &Client,
    surface: &TraeSurface,
    headers: &[(String, String)],
    session_id: &str,
    message_id: &str,
) {
    if session_id.is_empty() || message_id.is_empty() {
        return;
    }
    let url = format!("{}/chat_sessions/{session_id}/stop", surface.remote_base);
    let _ = apply_headers(client.post(url).timeout(Duration::from_secs(10)), headers)
        .json(&serde_json::json!({
            "chat_session_id": session_id,
            "user_message_id": message_id
        }))
        .send()
        .await;
}

fn trae_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn parse_trae_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {
    if payload
        .get("code")
        .is_some_and(|code| code.as_i64() != Some(0))
    {
        return Err(MgError::provider(
            502,
            format!(
                "Trae credits query failed: {}",
                payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream business error")
            ),
            "upstream_error",
        ));
    }
    let packs = payload
        .get("user_entitlement_pack_list")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            MgError::provider(
                502,
                "Trae returned no entitlement packs",
                "invalid_response",
            )
        })?;

    let mut total_limit = 0.0;
    let mut used = 0.0;
    let mut unlimited = false;
    for pack in packs {
        let Some(limit) = pack
            .pointer("/entitlement_base_info/quota/credits_limit")
            .and_then(trae_number)
        else {
            continue;
        };
        let amount = pack
            .pointer("/usage/credits_amount")
            .and_then(trae_number)
            .unwrap_or(0.0);
        if limit == -1.0 {
            unlimited = true;
        } else if limit >= 0.0 {
            total_limit += limit;
        }
        if amount >= 0.0 {
            used += amount;
        }
    }
    if total_limit == 0.0 && used == 0.0 && !unlimited && packs.is_empty() {
        return Err(MgError::provider(
            502,
            "Trae returned an empty entitlement list",
            "invalid_response",
        ));
    }

    let remaining = (!unlimited).then_some((total_limit - used).max(0.0));
    let bucket_remaining = remaining
        .map(|value| serde_json::json!(value))
        .unwrap_or_else(|| serde_json::json!("unlimited"));
    let bucket_limit = if unlimited {
        serde_json::json!("unlimited")
    } else {
        serde_json::json!(total_limit)
    };
    Ok(AccountQuota {
        checked_at,
        remaining,
        limit: (!unlimited).then_some(total_limit),
        reset_at: None,
        details: serde_json::json!({
            "label": "账号积分",
            "unit": "积分",
            "unlimited": unlimited,
            "buckets": [{"label": "账号积分", "remaining": bucket_remaining, "limit": bucket_limit, "used": used, "unit": "积分"}]
        }),
    })
}

fn trae_device_id(account: &mg_core::Account) -> String {
    let token_identity = account
        .access_token
        .split('.')
        .nth(1)
        .and_then(|body| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(body)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(body))
                .ok()
        })
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|claims| {
            claims
                .pointer("/data/id")
                .or_else(|| claims.get("user_id"))
                .or_else(|| claims.get("userId"))
                .or_else(|| claims.get("sub"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    let mut identity = account
        .extra
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or(token_identity)
        .unwrap_or_else(|| account.id.clone());
    let generation = account
        .checkin
        .as_ref()
        .and_then(|checkin| checkin.get("device_generation"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if generation > 0 {
        identity = format!("{identity}#gen{generation}");
    }
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let numeric_id = digest.bytes().fold(0_u64, |value, byte| {
        let nibble = (byte as char).to_digit(16).unwrap_or(0) as u64;
        (value * 16 + nibble) % 10_000_000_000_000_000
    });
    format!("{numeric_id:016}")
}

pub struct TraeProvider {
    /// Client for short non-streaming calls (model catalog fetches). Keeps a
    /// total timeout so a trickling response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
    model_config_names: RwLock<HashMap<String, String>>,
}

impl TraeProvider {
    pub fn new(pool: Arc<AccountPool>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        let model_config_names = trae_model_config_names(&trae_fallback_models());

        Self {
            client,
            stream_client,
            pool,
            model_config_names: RwLock::new(model_config_names),
        }
    }

    pub fn pool(&self) -> &Arc<AccountPool> {
        &self.pool
    }

    async fn post_trae_ug(
        &self,
        account: &mg_core::Account,
        path: &str,
    ) -> Result<serde_json::Value> {
        let surface = trae_surface(&account.region);
        let response = self
            .client
            .post(format!("{}{path}", surface.ug_host))
            .timeout(Duration::from_secs(30))
            .header(
                "Authorization",
                format!("Cloud-IDE-JWT {}", account.access_token),
            )
            .header("Content-Type", "application/json")
            .header("x-device-id", trae_device_id(account))
            .header("x-device-brand", "ASUS TUF Gaming A15 FA507RM_FA507RM")
            .header("x-device-type", "windows")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // The claim endpoint reports an already-claimed day as HTTP 400
            // with a business message; treat it as success so batch results do
            // not show a false failure.
            if crate::is_already_checked_in_message(&text) {
                return Ok(serde_json::json!({
                    "code": 0,
                    "checked_in": true,
                    "message": "今日已签到"
                }));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae check-in failed: {}",
                    crate::upstream_error_body(status.as_u16(), &text)
                ),
                "upstream_error",
            ));
        }
        serde_json::from_str(&text)
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))
    }

    async fn ensure_fresh_account(&self, account: mg_core::Account) -> Result<mg_core::Account> {
        if !trae_token_needs_refresh(&account) || account.refresh_token.is_empty() {
            return Ok(account);
        }
        let lock = self.pool.refresh_lock(&account.id);
        let _guard = lock.lock().await;
        let account = self.pool.get(&account.id).unwrap_or(account);
        if !trae_token_needs_refresh(&account) {
            return Ok(account);
        }
        match self.refresh_credentials(&account.id).await {
            Ok(_) => self
                .pool
                .get(&account.id)
                .ok_or_else(|| MgError::not_found("Account not found")),
            Err(error) => {
                self.pool.set_error(&account.id, &error.message());
                Err(error)
            }
        }
    }
}

fn save_trae_checkin(
    pool: &AccountPool,
    account: mg_core::Account,
    checked_in: bool,
    credits: Option<serde_json::Value>,
) -> Result<()> {
    let mut patch = serde_json::json!({
        "checked_in": checked_in,
        "today_checked_in": checked_in,
        "updated_at": mg_core::now_secs()
    });
    if let Some(credits) = credits {
        patch["credits"] = credits;
    }
    save_trae_checkin_patch(pool, account, patch)
}

fn save_trae_checkin_patch(
    pool: &AccountPool,
    account: mg_core::Account,
    patch: serde_json::Value,
) -> Result<()> {
    pool.update(&account.id, |current| {
        let mut checkin = current
            .checkin
            .clone()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if let Some(patch) = patch.as_object() {
            for (key, value) in patch {
                checkin.insert(key.clone(), value.clone());
            }
        }
        current.checkin = Some(serde_json::Value::Object(checkin));
    })?;
    Ok(())
}

#[async_trait]
impl Provider for TraeProvider {
    fn id(&self) -> &'static str {
        "trae"
    }

    fn name(&self) -> &'static str {
        "Trae"
    }

    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({
            "login": false,
            "credentials": true,
            "credits": true,
            "checkin": true,
            "regions": true,
            "import": true
        })
    }

    fn active_region(&self) -> Option<String> {
        let region = self.pool.active_region();
        Some(
            if regions_match(&region, "cn") {
                "cn"
            } else {
                "intl"
            }
            .to_string(),
        )
    }

    async fn set_region(&self, region: &str) -> Result<serde_json::Value> {
        let region = match region.trim().to_ascii_lowercase().as_str() {
            "cn" | "china" => "cn",
            "intl" | "global" => "intl",
            _ => return Err(MgError::bad_request("Unsupported Trae region")),
        };
        self.pool.set_active_region(region)?;
        Ok(serde_json::json!({ "ok": true, "region": region }))
    }

    async fn models(&self) -> Result<Vec<ModelEntry>> {
        self.models_for_region(None).await
    }

    async fn models_for_region(&self, region: Option<&str>) -> Result<Vec<ModelEntry>> {
        let fallback = trae_fallback_models();
        let active_region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let selected_region = region
            .map(|region| {
                if regions_match(region, "cn") {
                    "cn"
                } else {
                    "intl"
                }
            })
            .unwrap_or(&active_region);
        let Some(account) = representative_account(&self.pool, Some(selected_region)) else {
            return Ok(fallback);
        };
        let account = match self.ensure_fresh_account(account).await {
            Ok(account) => account,
            Err(error) => {
                tracing::warn!(provider = "trae", %error, "model catalog skipped: credential refresh failed");
                return Ok(fallback);
            }
        };
        let surface = trae_surface(selected_region);
        let provider_specific = account
            .provider_specific
            .as_ref()
            .filter(|value| !value.is_null())
            .or_else(|| account.extra.get("provider_specific"))
            .or_else(|| account.extra.get("providerSpecificData"));
        let app_language = provider_specific
            .and_then(|value| value.get("appLanguage"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(surface.default_language)
            .to_string();
        let stored_user_region = provider_specific.and_then(|value| {
            value
                .get("userRegion")
                .or_else(|| value.get("user_region"))
                .or_else(|| value.get("region"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        });
        let user_region = stored_user_region
            .filter(|value| {
                regions_match(selected_region, "cn") || !value.eq_ignore_ascii_case("cn")
            })
            .unwrap_or(surface.default_user_region)
            .to_string();
        let url = format!(
            "{}/models?functions=solo_agent_remote%2Csolo_work_remote%2Csolo_design_remote&show_custom_model=true",
            surface.remote_base
        );
        let solo_url = format!("{}/api/ide/v1/get_detail_param", surface.model_host);
        let device_id = uuid::Uuid::new_v4().simple().to_string();
        let machine_id = format!("{:x}", Sha256::digest(account.id.as_bytes()));
        let user_id = account
            .extra
            .get("user_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&account.id);
        let solo_body = serde_json::json!({
            "function": "solo_work_lite",
            "config_names": null,
            "need_prompt": false,
            "current_config_info": null,
            "poly_prompt": true,
            "mode_type": null,
            "agent_type": null
        });
        // The two catalog calls are independent (the solo call never reads the
        // remote-catalog response), so run them concurrently.
        let (catalog_result, solo_response) = futures::join!(
            self.client
                .get(url)
                .timeout(Duration::from_secs(20))
                .header("Authorization", format!("Cloud-IDE-JWT {}", account.access_token))
                .header("X-Trae-Client-Type", "web")
                .header("X-Preferenced-Language", app_language)
                .header("x-user-region", user_region)
                .header("Origin", &surface.origin)
                .header("Referer", format!("{}/", surface.origin.trim_end_matches('/')))
                .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36")
                .send(),
            self.client
                .post(solo_url)
                .timeout(Duration::from_secs(8))
                .header("Authorization", format!("Cloud-IDE-JWT {}", account.access_token))
                .header("X-Cloudide-Token", &account.access_token)
                .header("x-uid", user_id)
                .header("x-app-id", "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8")
                .header("x-device-id", &device_id)
                .header("x-machine-id", &machine_id)
                .header("x-request-id", uuid::Uuid::new_v4().to_string())
                .header("x-ide-version", "0.1.52")
                .header("x-ide-version-code", "20260811")
                .header("x-ide-version-type", "stable")
                .header("x-device-cpu", "AMD")
                .header("x-device-brand", "83DG")
                .header("x-device-type", "windows")
                .header("x-device-platform", "windows")
                .header("x-ide-token", &account.access_token)
                .header("x-os-version", "Windows 11 Pro")
                .header("x-system-type", "Windows")
                .header("x-app-version", "default")
                .header("x-app-version-code", "20260811")
                .header("x-version-code", "20260811")
                .header("package-type", surface.package_type)
                .header("x-lscbd-aid", "787976")
                .header("x-lscbd-platform", "windows")
                .header("x-ss-dp", "787976")
                .header("x-plugin-channel", "icube-ai")
                .header("app-version", "0.1.52")
                .header("request-traffic-type", "prod")
                .header("Origin", &surface.origin)
                .header("Referer", format!("{}/", surface.origin.trim_end_matches('/')))
                .header("User-Agent", "Trae/0.1.52")
                .json(&solo_body)
                .send()
        );
        let response = match catalog_result {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::warn!(
                    provider = "trae",
                    region = selected_region,
                    status = %response.status(),
                    "Trae model catalog request failed"
                );
                return Ok(fallback);
            }
            Err(error) => {
                tracing::warn!(
                    provider = "trae",
                    region = selected_region,
                    error = %error,
                    "Trae model catalog request failed"
                );
                return Ok(fallback);
            }
        };
        let data = match response.json::<serde_json::Value>().await {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(provider = "trae", region = selected_region, error = %error, "Trae model catalog was not JSON");
                return Ok(fallback);
            }
        };
        let mut models = parse_trae_catalog(&data);
        if models.is_empty() {
            models = parse_trae_solo_catalog(&data);
        }
        if let Ok(response) = solo_response {
            if response.status().is_success() {
                if let Ok(payload) = response.json::<serde_json::Value>().await {
                    let solo_models = parse_trae_solo_catalog(&payload);
                    let mut seen: HashSet<String> = models
                        .iter()
                        .map(|model| model.id.to_ascii_lowercase())
                        .collect();
                    models.extend(
                        solo_models
                            .into_iter()
                            .filter(|model| seen.insert(model.id.to_ascii_lowercase())),
                    );
                }
            }
        }
        let models = if models.is_empty() { fallback } else { models };
        *self.model_config_names.write() = trae_model_config_names(&models);
        Ok(models)
    }

    async fn chat(
        &self,
        mut req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
        let region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let account = self
            .pool
            .choose_in_region(session_id, None, Some(&region))?;
        let account = self.ensure_fresh_account(account).await?;
        if account.access_token.is_empty() {
            return Err(MgError::unauthorized("Trae account has no token"));
        }
        let requested_model = req.model.clone();
        req.model = canonical_trae_model_name(&requested_model, &self.model_config_names.read());
        let account_id = account.id.clone();
        let surface = trae_surface(&account.region);
        let headers = trae_web_headers(&account, &surface);
        let (mode, strategy, model_name, agent_type) = trae_resolve_mode(&req.model);
        let custom_model = if strategy == "auto" {
            None
        } else {
            fetch_trae_custom_model(&self.client, &account, &surface, &model_name).await
        };
        let body = trae_session_body(
            &req.messages,
            &account,
            &surface,
            mode,
            strategy,
            &model_name,
            agent_type,
            custom_model,
        );

        info!(
            provider = "trae",
            model = %requested_model,
            account = %account_id,
            stream = req.stream,
            "Forwarding chat completion to Trae"
        );

        let created = apply_headers(
            self.stream_client
                .post(format!("{}/chat_sessions", surface.remote_base))
                .timeout(Duration::from_secs(60)),
            &headers,
        )
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            MgError::provider(
                502,
                format!("Trae connection failed: {error}"),
                "network_error",
            )
        })?;
        let status = created.status();
        if !status.is_success() {
            let err_text = created.text().await.unwrap_or_default();
            if crate::is_account_level_failure(status.as_u16()) {
                self.pool
                    .mark_cooldown(&account_id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(&account_id, &format!("upstream HTTP {}", status.as_u16()));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae request failed: {}",
                    crate::upstream_error_body(status.as_u16(), &err_text)
                ),
                "upstream_error",
            ));
        }
        let created = created
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        if created
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|code| code != 0)
            && created.get("data").is_none()
        {
            return Err(MgError::provider(
                502,
                format!("Trae request failed: {created}"),
                "upstream_error",
            ));
        }
        let payload = created.get("data").unwrap_or(&created);
        let remote_session = payload
            .get("chat_session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let message_id = payload
            .get("message_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if remote_session.is_empty() || message_id.is_empty() {
            return Err(MgError::provider(
                502,
                format!("Trae session response missing ids: {created}"),
                "invalid_response",
            ));
        }

        let events_url = format!(
            "{}/chat_sessions/{remote_session}/events?reply_to_message_id={message_id}",
            surface.remote_base
        );
        let events = apply_headers(self.stream_client.get(events_url), &headers)
            .send()
            .await
            .map_err(|error| {
                MgError::provider(
                    502,
                    format!("Trae connection failed: {error}"),
                    "network_error",
                )
            })?;
        let status = events.status();
        if !status.is_success() {
            let err_text = events.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae request failed: {}",
                    crate::upstream_error_body(status.as_u16(), &err_text)
                ),
                "upstream_error",
            ));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let client = self.stream_client.clone();
        let stop_surface = surface.clone();
        let stop_headers = headers.clone();
        let public_model = requested_model.clone();
        tokio::spawn(async move {
            let mut translator = TraeTranslator::new(&public_model);
            let mut incoming = events.bytes_stream().eventsource();
            while let Some(item) = incoming.next().await {
                match item {
                    Ok(event) => {
                        for chunk in translator.push(&event.event, &event.data) {
                            if tx.send(chunk).await.is_err() {
                                stop_trae_session(
                                    &client,
                                    &stop_surface,
                                    &stop_headers,
                                    &remote_session,
                                    &message_id,
                                )
                                .await;
                                return;
                            }
                        }
                        if translator.finished {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx
                            .send(Err(MgError::provider(
                                502,
                                format!("Stream read error: {error}"),
                                "upstream_error",
                            )))
                            .await;
                        break;
                    }
                }
            }
            if let Some(chunk) = translator.finish() {
                let _ = tx.send(Ok(chunk)).await;
            }
            stop_trae_session(
                &client,
                &stop_surface,
                &stop_headers,
                &remote_session,
                &message_id,
            )
            .await;
        });

        Ok((
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            account_id,
        ))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let account = self.ensure_fresh_account(account).await?;

        let surface = trae_surface(&account.region);
        let response = self
            .client
            .post(format!(
                "{}/trae/api/{}/pay/ide_user_ent_usage",
                surface.pay_host, surface.pay_version
            ))
            .header(
                "Authorization",
                format!("Cloud-IDE-JWT {}", account.access_token),
            )
            .header("Content-Type", "application/json")
            .header("x-device-id", trae_device_id(&account))
            .header("x-device-brand", "ASUS TUF Gaming A15 FA507RM_FA507RM")
            .header("x-device-type", "windows")
            .json(&serde_json::json!({"require_usage": true, "req_source": 1}))
            .send()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae credits query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = parse_trae_quota(&payload, mg_core::now_secs())?;
        self.pool.update_quota(account_id, quota.clone())?;
        Ok(quota)
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        if account.refresh_token.is_empty() {
            if !account.access_token.is_empty()
                && account.expires_at > mg_core::now_secs() + 300.0
            {
                return Ok(serde_json::json!({"ok": true, "message": "当前凭证仍有效"}));
            }
            return Err(MgError::unauthorized(
                "Trae account has no refresh token; sign in again",
            ));
        }

        let host = trae_refresh_host(&account);
        let payload = serde_json::json!({
            "ClientID": trae_refresh_client_id(&account),
            "RefreshToken": account.refresh_token.clone(),
            "ClientSecret": "-",
            "UserID": trae_refresh_user_id(&account),
        });
        let response = self
            .client
            .post(format!(
                "{host}/cloudide/api/v3/trae/oauth/ExchangeToken"
            ))
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|error| {
                MgError::provider(
                    502,
                    format!("Trae credential refresh failed: {error}"),
                    "network_error",
                )
            })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let body = serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default();
        if !status.is_success() {
            let message = body
                .pointer("/ResponseMetadata/Error/Message")
                .or_else(|| body.get("message"))
                .or_else(|| body.get("msg"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("upstream rejected the refresh token");
            return Err(MgError::provider(
                status.as_u16(),
                format!("Trae credential refresh failed: {message}"),
                "upstream_error",
            ));
        }
        let result = body
            .get("Result")
            .or_else(|| body.get("result"))
            .unwrap_or(&body);
        let access_token = result
            .get("Token")
            .or_else(|| result.get("token"))
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                MgError::provider(
                    502,
                    "Trae refresh response has no token",
                    "invalid_response",
                )
            })?;
        let access_token = access_token.to_string();
        let refresh_token = result
            .get("RefreshToken")
            .or_else(|| result.get("refreshToken"))
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        let expires_at = trae_expire_at(
            result
                .get("TokenExpireAt")
                .or_else(|| result.get("tokenExpireAt")),
        )
        .or_else(|| trae_jwt_expiry(&access_token));
        let pool = Arc::clone(&self.pool);
        let account_id = account.id.clone();
        tokio::task::spawn_blocking(move || {
            pool.update(&account_id, |current| {
                current.access_token = access_token;
                if let Some(refresh_token) = refresh_token {
                    current.refresh_token = refresh_token;
                }
                if let Some(expires_at) = expires_at {
                    current.expires_at = expires_at;
                }
                current.credential_invalid = false;
                current.error.clear();
            })
        })
        .await
        .map_err(|error| MgError::Io(error.to_string()))??;
        Ok(serde_json::json!({"ok": true, "message": "Trae 凭证已刷新"}))
    }

    async fn checkin(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let account = self.ensure_fresh_account(account).await?;
        if account.access_token.is_empty() {
            return Err(MgError::unauthorized("Trae account has no token"));
        }

        let status = self
            .post_trae_ug(&account, "/trae/api/v2/ug/checkin_credits/status")
            .await?;
        if status
            .get("checked_in")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            save_trae_checkin(&self.pool, account, true, status.get("credits").cloned())?;
            return Ok(serde_json::json!({
                "ok": true,
                "already": true,
                "message": "今日已签到"
            }));
        }
        if status.get("enable").and_then(serde_json::Value::as_bool) == Some(false) {
            return Ok(serde_json::json!({
                "ok": false,
                "message": "官方签到活动未开放"
            }));
        }

        let mut claim = self
            .post_trae_ug(&account, "/trae/api/v2/ug/checkin_credits/claim")
            .await?;
        if claim.get("code").and_then(serde_json::Value::as_i64) == Some(9074) {
            let mut rotated = self.pool.get(account_id).unwrap_or_else(|| account.clone());
            let generation = rotated
                .checkin
                .as_ref()
                .and_then(|checkin| checkin.get("device_generation"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                + 1;
            save_trae_checkin_patch(
                &self.pool,
                rotated.clone(),
                serde_json::json!({"device_generation": generation}),
            )?;
            rotated = self.pool.get(account_id).unwrap_or(rotated);
            claim = self
                .post_trae_ug(&rotated, "/trae/api/v2/ug/checkin_credits/claim")
                .await?;
        }

        let code = claim.get("code").and_then(serde_json::Value::as_i64);
        if code == Some(0)
            || claim.get("checked_in").and_then(serde_json::Value::as_bool) == Some(true)
        {
            let latest = self.pool.get(account_id).unwrap_or(account);
            save_trae_checkin(&self.pool, latest, true, claim.get("credits").cloned())?;
            let bonus = claim
                .get("credits")
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && *value > 0.0);
            let message = match bonus {
                Some(bonus) => format!("签到成功 +{bonus}"),
                None => "签到成功".to_string(),
            };
            return Ok(serde_json::json!({"ok": true, "message": message}));
        }

        let upstream = claim
            .get("message")
            .or_else(|| claim.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("upstream business error");
        Ok(serde_json::json!({
            "ok": false,
            "message": match code {
                Some(code) => format!("签到失败 [{code}]: {upstream}"),
                None => format!("签到失败: {upstream}"),
            }
        }))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "status": "manual",
            "message": "Trae 账号在本地配置文件读取"
        }))
    }

    async fn poll_login(&self, _login_id: &str) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "status": "pending"
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_trae_model_name, parse_trae_catalog, parse_trae_quota, parse_trae_solo_catalog,
        trae_custom_model, trae_fallback_models, trae_model_config_names, trae_surface_with,
        trae_token_needs_refresh, TraeProvider, TraeSurfaceEnv, TraeTranslator,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn refreshes_trae_tokens_before_they_expire_but_not_long_lived_ones() {
        let future = mg_core::now_secs() + 3600.0;
        let soon = mg_core::now_secs() + 60.0;
        let account = |token: &str, expiry: f64| -> mg_core::Account {
            serde_json::from_value(json!({
                "id": "trae-user",
                "access_token": token,
                "expires_at": expiry
            }))
            .unwrap()
        };
        assert!(!trae_token_needs_refresh(&account("token", future)));
        assert!(trae_token_needs_refresh(&account("token", soon)));
        assert!(trae_token_needs_refresh(&account("", 0.0)));
        // Unknown expiry is not treated as expired, otherwise every request
        // would refresh.
        assert!(!trae_token_needs_refresh(&account("token", 0.0)));
    }

    #[tokio::test]
    async fn expired_trae_accounts_refresh_and_persist_before_use() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let expires_at = (mg_core::now_secs() + 7200.0) as u64;
            let body = format!(
                "{{\"Result\":{{\"Token\":\"fresh-token\",\"RefreshToken\":\"fresh-refresh\",\"TokenExpireAt\":{expires_at}}}}}"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let dir = std::env::temp_dir().join(format!("mg-trae-refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = Arc::new(mg_core::AccountPool::new(&dir));
        let account: mg_core::Account = serde_json::from_value(json!({
            "id": "trae-user",
            "region": "cn",
            "access_token": "stale-token",
            "refresh_token": "stale-refresh",
            "expires_at": 1.0,
            "provider_specific": {"host": format!("http://{address}")}
        }))
        .unwrap();
        pool.save(account.clone()).unwrap();

        let provider = TraeProvider::new(pool.clone());
        let refreshed = provider.ensure_fresh_account(account).await.unwrap();
        assert_eq!(refreshed.access_token, "fresh-token");
        assert_eq!(refreshed.refresh_token, "fresh-refresh");
        assert!(refreshed.expires_at > mg_core::now_secs());
        assert_eq!(
            pool.get("trae-user").unwrap().access_token,
            "fresh-token"
        );
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_events_emit_only_the_new_snapshot_text() {
        let mut translator = TraeTranslator::new("doubao-seed-2.1-turbo");
        let first = translator.push("plan_item", r#"{"reasoning_content":"The"}"#);
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[1].as_ref().unwrap().choices[0]
                .delta
                .reasoning_content
                .as_deref(),
            Some("The")
        );
        let second = translator.push(
            "plan_item",
            r#"{"thought":"pong","reasoning_content":"The user"}"#,
        );
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].as_ref().unwrap().choices[0]
                .delta
                .content
                .as_deref(),
            Some("pong")
        );
        assert_eq!(
            second[0].as_ref().unwrap().choices[0]
                .delta
                .reasoning_content
                .as_deref(),
            Some(" user")
        );
        assert!(translator.push("heartbeat", "").is_empty());
        let config = trae_custom_model(
            &json!({"data":{"list":[{"function":"solo_agent_remote","models":[
                {"name":"doubao-seed-2.1-turbo","display_name":"Turbo"}
            ]}]}}),
            "doubao-seed-2.1-turbo",
        )
        .unwrap();
        assert_eq!(config["config_name"], "doubao-seed-2.1-turbo");
        assert_eq!(config["use_remote_service"], true);
    }

    #[test]
    fn totals_trae_credit_entitlements_and_detects_unlimited_packs() {
        let quota = parse_trae_quota(
            &json!({"code":0,"user_entitlement_pack_list":[
                {"entitlement_base_info":{"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"27.5"}},
                {"entitlement_base_info":{"quota":{}},"usage":{"credits_amount":0}}
            ]}),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(72.5));
        assert_eq!(quota.limit, Some(100.0));
        let unlimited = parse_trae_quota(
            &json!({"code":0,"user_entitlement_pack_list":[
                {"entitlement_base_info":{"quota":{"credits_limit":-1}},"usage":{"credits_amount":0}}
            ]}),
            12.0,
        )
        .unwrap();
        assert_eq!(unlimited.remaining, None);
        assert_eq!(unlimited.details["unlimited"], true);
        assert!(parse_trae_quota(&json!({"code":900,"message":"expired"}), 12.0).is_err());
    }

    #[test]
    fn parses_remote_model_groups_filters_hidden_entries_and_keeps_metadata() {
        let payload = json!({
            "data": {
                "list": [
                    {"function": "solo_agent_remote", "models": [
                        {"name": "Claude-V4", "config_source": 1, "display_config": {"display_name": "Claude 4"}, "context_window": 200000},
                        {"name": "auto", "config_source": 1},
                        {"name": "agent_subagent", "config_source": 1}
                    ]},
                    {"function": "solo_work_remote", "models": [
                        {"name": "claude-v4", "config_source": 1},
                        {"name": "qwen-coder", "config_source": 1, "display_name": "Qwen Coder"}
                    ]}
                ]
            }
        });

        let models = parse_trae_catalog(&payload);
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["claude-v4", "qwen-coder"]
        );
        assert_eq!(models[0].extra["display_name"], "Claude 4");
        assert_eq!(models[0].extra["config_name"], "Claude-V4");
        let config_names = trae_model_config_names(&models);
        assert_eq!(
            canonical_trae_model_name("claude-v4", &config_names),
            "Claude-V4"
        );
        assert_eq!(
            canonical_trae_model_name("Claude 4", &config_names),
            "Claude-V4"
        );
        let fallback_names = trae_model_config_names(&trae_fallback_models());
        assert_eq!(
            canonical_trae_model_name("Qwen3.8-Max", &fallback_names),
            "qwen3.8-max"
        );
    }

    #[test]
    fn parses_flat_trae_model_response() {
        let models = parse_trae_catalog(&json!({
            "data": [{"name": "GPT-5", "config_source": 1}]
        }));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5");
    }

    #[test]
    fn agent_max_context_is_preserved_regardless_of_catalog_group_order() {
        let work_model = json!({
            "name": "glm-5.2",
            "config_source": 1,
            "display_name": "GLM-5.2",
            "max_mode": false,
            "context_window_tokens": {"dev": 200000, "max": 0},
            "features": {"consumption_rate": {"data": {"rate": 0.78}}}
        });
        let mut agent_model = work_model.clone();
        agent_model["max_mode"] = json!(true);
        agent_model["context_window_tokens"]["max"] = json!(1000000);
        let groups = [
            json!({"function": "solo_design_remote", "models": [work_model.clone()]}),
            json!({"function": "solo_work_remote", "models": [work_model]}),
            json!({"function": "solo_agent_remote", "models": [agent_model]}),
        ];

        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let models = parse_trae_catalog(&json!({
                "data": {"list": order.map(|index| groups[index].clone())}
            }));

            assert_eq!(models.len(), 1);
            assert_eq!(models[0].id, "glm-5.2");
            assert_eq!(models[0].extra["context_window"], json!(200000));
            assert_eq!(
                models[0].extra["max_mode"],
                json!(true),
                "group order {order:?}"
            );
            assert_eq!(models[0].extra["max_context"], json!(1000000));
            assert_eq!(models[0].credits, Some(json!(0.78)));
        }
    }

    #[test]
    fn restores_trae_credit_factors_and_max_context_metadata() {
        let models = parse_trae_catalog(&json!({
            "data": {"list": [{"models": [
                {
                    "name": "Qwen3.8-Max",
                    "config_source": 1,
                    "features": "{\"consumption_rate\":{\"data\":{\"rate\":1.25}}}",
                    "context_window_tokens": {"dev": 128000, "max": 1000000}
                },
                {
                    "name": "glm-5.2",
                    "config_source": 1,
                    "display_config": {"display_name": "GLM-5.2"}
                }
            ]}]}
        }));

        let max_model = models
            .iter()
            .find(|model| model.id == "qwen3.8-max")
            .unwrap();
        assert_eq!(max_model.credits, Some(json!(1.25)));
        assert_eq!(max_model.extra["price_factor"], json!(1.25));
        assert_eq!(max_model.extra["context_window"], json!(128000));
        assert_eq!(max_model.extra["context"], json!(128000));
        assert_eq!(max_model.extra["max_context"], json!(1000000));
        assert_eq!(max_model.extra["max_mode"], json!(true));

        let rated_model = models.iter().find(|model| model.id == "glm-5.2").unwrap();
        assert_eq!(rated_model.credits, Some(json!(0.78)));
        assert_eq!(rated_model.extra["price_factor"], json!(0.78));
    }

    #[test]
    fn reads_max_mode_from_display_config_without_explicit_max_tokens() {
        let models = parse_trae_catalog(&json!({
            "data": {"list": [{"models": [{
                "name": "Qwen3.8-Max",
                "config_source": 1,
                "display_config": {"display_name": "Qwen3.8-Max", "max_mode": true},
                "context_window_tokens": {"dev": 128000}
            }]}]}
        }));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].extra["context_window"], json!(128000));
        assert_eq!(models[0].extra["max_context"], json!(1000000));
        assert_eq!(models[0].extra["max_mode"], json!(true));
    }

    #[test]
    fn reads_max_context_from_context_window_size_and_feature_metadata() {
        let models = parse_trae_catalog(&json!({
            "data": {"list": [{"models": [
                {
                    "name": "Seed-Max",
                    "config_source": 1,
                    "context_window_size": {"default": 233000, "max": [1000000]}
                },
                {
                    "name": "Feature-Max",
                    "config_source": 1,
                    "features": "{\"context_windows\":{\"data\":{\"dev_context\":200000,\"max_context\":1000000}}}"
                }
            ]}]}
        }));

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].extra["context_window"], json!(233000));
        assert_eq!(models[0].extra["max_context"], json!(1000000));
        assert_eq!(models[0].extra["max_mode"], json!(true));
        assert_eq!(models[1].extra["context_window"], json!(200000));
        assert_eq!(models[1].extra["max_context"], json!(1000000));
        assert_eq!(models[1].extra["max_mode"], json!(true));
    }

    #[test]
    fn parses_solo_catalog_with_public_ids_and_exact_config_names() {
        let models = parse_trae_solo_catalog(&json!({
            "data": {"config_info_list": [
                {
                    "config_name": "Doubao-Seed-Code",
                    "usage": "CHAT_COMPLETION",
                    "display_name": "Seed Code",
                    "custom_models": [],
                    "features": {"consumption_rate": {"rate": 0.12}},
                    "context_window_tokens": {"dev": 128000, "max": 1000000}
                },
                {"config_name": "custom_model_1m", "usage": "completion", "display_name": "Internal"},
                {"config_name": "hidden-model", "usage": "chat_completion", "is_invisible_to_user": true}
            ]}
        }));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "doubao-seed-code");
        assert_eq!(models[0].extra["config_name"], "Doubao-Seed-Code");
        assert_eq!(models[0].extra["display_name"], "Seed Code");
        assert_eq!(models[0].credits, Some(json!(0.12)));
        assert_eq!(models[0].extra["max_context"], json!(1000000));
        assert_eq!(models[0].extra["max_mode"], json!(true));
    }

    #[test]
    fn fallback_trae_catalog_includes_qwen_max_with_its_credit_factor() {
        let models = trae_fallback_models();
        let max_model = models
            .iter()
            .find(|model| model.id == "qwen3.8-max")
            .unwrap();

        assert_eq!(max_model.credits, Some(json!(1.5)));
        assert_eq!(max_model.extra["price_factor"], json!(1.5));
        assert_eq!(max_model.extra["display_name"], json!("Qwen3.8-Max"));
        assert_eq!(max_model.extra["context_window"], json!(200000));
        assert_eq!(max_model.extra["max_context"], json!(1000000));
        assert_eq!(max_model.extra["max_mode"], json!(true));
    }

    #[test]
    fn international_catalog_uses_the_sg_hosts_and_credit_endpoints() {
        let empty = TraeSurfaceEnv {
            web_base: None,
            web_origin: None,
            model_host: None,
        };
        let intl = trae_surface_with("intl", empty);
        assert_eq!(
            intl.remote_base,
            "https://core-normal.trae.ai/api/remote/v1"
        );
        assert_eq!(intl.origin, "https://work.trae.ai");
        assert_eq!(intl.model_host, "https://a0ai-api-sg.byteintlapi.com");
        assert_eq!(intl.default_user_region, "SG");
        assert_eq!(intl.default_language, "en");
        assert_eq!(intl.ug_host, "https://api.marscode.com");
        assert_eq!(intl.pay_host, "https://grow-normal.trae.ai");
        assert_eq!(intl.pay_version, "v1");

        let cn = trae_surface_with(
            "cn",
            TraeSurfaceEnv {
                web_base: None,
                web_origin: None,
                model_host: None,
            },
        );
        assert_eq!(
            cn.remote_base,
            "https://trae-api-cn.mchost.guru/api/remote/v1"
        );
        assert_eq!(cn.origin, "https://solo.trae.cn");
        assert_eq!(cn.default_user_region, "CN");
        assert_eq!(cn.ug_host, "https://api.trae.cn");
        assert_eq!(cn.pay_version, "v2");
    }
}
