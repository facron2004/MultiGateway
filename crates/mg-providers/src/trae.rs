use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use mg_core::account::{regions_match, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_core::AccountPool;
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, ChatUsage, ChunkChoice, ChunkDelta,
    FunctionCallChunk, ModelEntry, PromptTokensDetails, ToolCallChunk,
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

const DEFAULT_TRAE_QUERY_MAX_CHARS: usize = 400_000;

fn trae_query_max_chars() -> usize {
    std::env::var("TRAE_REMOTE_QUERY_MAX_CHARS")
        .ok()
        .and_then(|val| val.trim().parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_TRAE_QUERY_MAX_CHARS)
}

fn is_continuation_turn(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "继续"
            | "继续执行"
            | "继续完成"
            | "继续处理"
            | "继续进行"
            | "接着做"
            | "接着执行"
            | "往下继续"
            | "下一步"
            | "continue"
            | "continue working"
            | "go on"
            | "proceed"
            | "keep going"
            | "resume"
    )
}

/// Truncate oldest non-system messages so the flattened query fits upstream.
/// Trae's remote session silently drops or cancels sessions when initial_message.query
/// exceeds roughly 480k-500k chars.
fn bound_trae_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let limit = trae_query_max_chars();
    if messages.is_empty() {
        return Vec::new();
    }

    let latest_user_idx = messages.iter().rposition(|m| m.role == "user");
    let needs_task_anchor = if let Some(idx) = latest_user_idx {
        let text = trae_text(&messages[idx].content);
        is_continuation_turn(&text)
    } else {
        false
    };

    let anchor_prompt = if needs_task_anchor {
        messages
            .iter()
            .take(latest_user_idx.unwrap())
            .filter(|m| m.role == "user")
            .filter_map(|m| {
                let text = trae_text(&m.content).trim().to_string();
                if !text.is_empty() && !is_continuation_turn(&text) {
                    Some(text)
                } else {
                    None
                }
            })
            .last()
    } else {
        None
    };

    let mut bounded: Vec<ChatMessage> = messages.to_vec();

    loop {
        let current_chars: usize = bounded
            .iter()
            .map(|m| {
                let mut len = trae_text(&m.content).len();
                if let Some(calls) = &m.tool_calls {
                    for c in calls {
                        len += c.id.len() + c.function.name.len() + c.function.arguments.len() + 30;
                    }
                }
                len
            })
            .sum();

        if current_chars <= limit {
            break;
        }

        let non_system_indices: Vec<usize> = bounded
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role != "system" && m.role != "developer")
            .map(|(i, _)| i)
            .collect();

        if non_system_indices.len() <= 1 {
            if let Some(&last_idx) = non_system_indices.first() {
                let text = trae_text(&bounded[last_idx].content);
                if text.len() > limit {
                    let head_len = limit.min(40_000);
                    let tail_len = limit.saturating_sub(head_len + 100);
                    let truncated = format!(
                        "{}\n\n...[history middle truncated for context window]...\n\n{}",
                        &text[..text
                            .char_indices()
                            .take(head_len)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0)],
                        &text[text
                            .char_indices()
                            .rev()
                            .take(tail_len)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(text.len())..]
                    );
                    bounded[last_idx].content = Some(serde_json::Value::String(truncated));
                }
            }
            break;
        }

        let head = non_system_indices[0];
        let mut drop_indices = std::collections::BTreeSet::new();
        drop_indices.insert(head);

        if bounded[head].role == "assistant" && bounded[head].tool_calls.is_some() {
            if head + 1 < bounded.len() && bounded[head + 1].role == "tool" {
                drop_indices.insert(head + 1);
            }
        }

        bounded = bounded
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !drop_indices.contains(i))
            .map(|(_, m)| m)
            .collect();
    }

    if let Some(anchor) = anchor_prompt {
        let has_anchor_already = bounded.iter().any(|m| {
            if m.role == "user" {
                let text = trae_text(&m.content);
                text.contains(&anchor)
            } else {
                false
            }
        });
        if !has_anchor_already {
            let max_anchor_len = 16_000;
            let anchor_text = if anchor.len() > max_anchor_len {
                format!(
                    "{}...",
                    &anchor[..anchor
                        .char_indices()
                        .take(max_anchor_len)
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(anchor.len())]
                )
            } else {
                anchor
            };
            let insert_pos = bounded
                .iter()
                .position(|m| m.role == "system" || m.role == "developer")
                .map(|p| p + 1)
                .unwrap_or(0);
            bounded.insert(
                insert_pos,
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(serde_json::Value::String(format!(
                        "[Task Anchor - User Request]\n{anchor_text}"
                    ))),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            );
        }
    }

    bounded
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
        "agent_id": agent_type,
        "agent_type": agent_type,
        "chat_session_id": "",
        "content": serde_json::json!([]),
        "query": trae_query(messages),
        "model_name": model_name,
        "model_selection_strategy": strategy,
        "common_params": trae_common_params(account, surface, mode)
    });
    if let Some(custom_model) = custom_model {
        let bound_model_name = custom_model
            .get("name")
            .or_else(|| custom_model.get("config_name"))
            .or_else(|| custom_model.get("model_name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(model_name);
        if strategy == "manual" {
            initial["model_name"] = serde_json::json!(bound_model_name);
            let config_source = custom_model
                .get("config_source")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(1);
            initial["model_config_source"] = serde_json::json!(config_source);
            let is_preset = custom_model
                .get("is_preset")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            initial["model_is_preset"] = serde_json::json!(is_preset);
            let provider = custom_model
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            initial["model_provider"] = serde_json::json!(provider);
        }
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

/// Normalize upstream event spellings (`PlanItem`, `ResponseDone`, ...) to the
/// snake_case names used by the translator.
fn normalize_trae_event_name(raw: &str) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut normalized = String::new();
    for (index, value) in chars.iter().enumerate() {
        if value.is_ascii_alphanumeric() {
            let previous = index.checked_sub(1).and_then(|index| chars.get(index));
            let next = chars.get(index + 1);
            if value.is_ascii_uppercase()
                && !normalized.is_empty()
                && (previous.is_some_and(|value| {
                    value.is_ascii_lowercase() || value.is_ascii_digit()
                }) || next.is_some_and(|value| value.is_ascii_lowercase()))
            {
                normalized.push('_');
            }
            normalized.push(value.to_ascii_lowercase());
        } else if !normalized.is_empty() && !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    let normalized = normalized.trim_matches('_').to_string();
    let compact: String = normalized.chars().filter(|value| *value != '_').collect();
    match compact.as_str() {
        "modelconfig" => "model_config",
        "planitem" => "plan_item",
        "requestwaitinqueue" => "request_wait_in_queue",
        "responsedone" | "streamdone" => "done",
        "tokenusage" => "token_usage",
        _ => normalized.as_str(),
    }
    .to_string()
}

fn trae_payload_event_name(payload: &serde_json::Value) -> String {
    ["event", "event_type", "eventType", "type"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_str))
        .map(normalize_trae_event_name)
        .unwrap_or_default()
}

fn trae_nested_string(value: &serde_json::Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty()).or_else(|| {
        ["content", "text", "summary", "value"]
            .iter()
            .find_map(|key| {
                value
                    .get(*key)
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
            })
    })
}

fn trae_payload_reasoning(data: &serde_json::Value) -> String {
    for key in [
        "reasoning_content",
        "reasoningContent",
        "reasoning",
        "thought",
        "thinking",
    ] {
        if let Some(text) = data.get(key).and_then(trae_nested_string) {
            return text.to_string();
        }
    }
    let Some(message) = data.get("message").filter(|value| value.is_object()) else {
        return String::new();
    };
    let nested = trae_payload_reasoning(message);
    if !nested.is_empty() {
        return nested;
    }
    let Some(parts) = message.get("content").and_then(serde_json::Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| {
            let kind = part
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !matches!(kind.as_str(), "reasoning" | "thinking" | "reasoning_text") {
                return None;
            }
            part.get("text")
                .or_else(|| part.get("content"))
                .or_else(|| part.get("value"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Visible assistant text carried by message-like events.
fn trae_message_text(data: &serde_json::Value) -> String {
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
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if matches!(kind.as_str(), "reasoning" | "thinking" | "reasoning_text") {
                        return None;
                    }
                    part.as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            part.get("text")
                                .or_else(|| part.get("content"))
                                .or_else(|| part.get("value"))
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
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
        if let Some(parts) = nested.get(key).and_then(serde_json::Value::as_array) {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.as_str().map(str::to_owned).or_else(|| {
                        part.get("text")
                            .or_else(|| part.get("content"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                })
                .collect::<String>();
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn trae_plan_has_split_reasoning(data: &serde_json::Value) -> bool {
    data.get("reasoning_content").is_some() || data.get("reasoningContent").is_some()
}

fn trae_plan_visible_text(data: &serde_json::Value) -> String {
    let content = trae_message_text(data);
    if !content.is_empty() {
        return content;
    }
    data.get("thought")
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Trae's agent reports its final answer through the internal `finish` tool.
/// Some turns never stream an answer plan item, so this summary is the only
/// visible output.
fn trae_finish_summary(data: &serde_json::Value) -> String {
    let Some(info) = data
        .get("tool_call_info")
        .or_else(|| data.get("toolCallInfo"))
        .filter(|value| value.is_object())
    else {
        return String::new();
    };
    if info.get("name").and_then(serde_json::Value::as_str) != Some("finish") {
        return String::new();
    }
    let params = info.get("params").or_else(|| info.get("paramsJson"));
    let params = match params {
        Some(serde_json::Value::String(raw)) => {
            serde_json::from_str::<serde_json::Value>(raw).unwrap_or(serde_json::Value::Null)
        }
        Some(value) => value.clone(),
        None => return String::new(),
    };
    params
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn trae_common_prefix_len(left: &str, right: &str) -> usize {
    let mut common = left
        .bytes()
        .zip(right.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while common > 0 && !left.is_char_boundary(common) {
        common -= 1;
    }
    common
}

fn trae_text_delta(previous: &str, current: &str) -> String {
    if current.is_empty() || current == previous {
        return String::new();
    }
    if let Some(delta) = current.strip_prefix(previous) {
        return delta.to_string();
    }
    if previous.starts_with(current) {
        return String::new();
    }
    let common = trae_common_prefix_len(previous, current);
    current.get(common..).unwrap_or_default().to_string()
}

/// Merge a cumulative snapshot into the previous value without replaying or
/// losing text when snapshots arrive out of order.
fn merge_trae_snapshot(current: &mut String, next: &str) {
    if next.is_empty() || next == current.as_str() {
        return;
    }
    if current.starts_with(next) {
        return;
    }
    if next.starts_with(current.as_str()) {
        *current = next.to_string();
        return;
    }
    if current.ends_with(next) && next.len() < current.len() {
        return;
    }
    let common = trae_common_prefix_len(current, next);
    let shorter = current.len().min(next.len());
    if common >= 4 || (shorter > 0 && common * 2 >= shorter) {
        *current = next.to_string();
    } else {
        current.push_str(next);
    }
}

fn trae_suffix_prefix_overlap(left: &str, right: &str, minimum: usize) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let limit = left.len().min(right.len());
    (minimum..=limit)
        .rev()
        .find(|size| left.ends_with(&right[..*size]))
        .unwrap_or(0)
}

fn merge_trae_text(plan: &str, message: &str) -> String {
    if plan.is_empty() {
        return message.to_string();
    }
    if message.is_empty() {
        return plan.to_string();
    }
    if plan == message || plan.starts_with(message) || plan.ends_with(message) {
        return plan.to_string();
    }
    if message.starts_with(plan) {
        return message.to_string();
    }
    let overlap = trae_suffix_prefix_overlap(plan, message, 4);
    // The overlap is computed on bytes; keep only a char-boundary suffix.
    if overlap > 0 && message.is_char_boundary(overlap) {
        return format!("{plan}{}", &message[overlap..]);
    }
    format!("{}\n\n{}", plan.trim_end(), message.trim_start())
}

fn merge_trae_summary(text: &str, summary: &str) -> String {
    if summary.is_empty() {
        return text.to_string();
    }
    if text.is_empty() {
        return summary.to_string();
    }
    let text_end = text.trim_end();
    let summary_end = summary.trim_end();
    if text_end.ends_with(summary_end) || text_end.starts_with(summary_end) {
        return text.to_string();
    }
    if summary.starts_with(text_end) {
        return summary.to_string();
    }
    format!("{text_end}\n\n{}", summary.trim_start())
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
    // Trae reports the prompt-cache split on its `token_usage` event. Dropping
    // it made every turn look like a cold 24k-token prompt, so the dashboard
    // showed "input only" and cached tokens were billed as fresh ones.
    let cache_keys = [
        "cache_read_input_tokens",
        "cached_tokens",
        "cache_read_tokens",
        "prompt_cache_hit_tokens",
    ];
    let cache_creation_keys = ["cache_creation_input_tokens", "cache_creation_tokens"];
    let cached = number(&cache_keys);
    let cache_reported = cache_keys
        .iter()
        .chain(cache_creation_keys.iter())
        .any(|key| usage.get(*key).is_some());
    let credits = usage
        .get("credits_consumed")
        .or_else(|| usage.get("credit"))
        .or_else(|| usage.get("credits"))
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    (prompt > 0 || completion > 0 || total > 0).then_some(ChatUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        prompt_tokens_details: cache_reported.then_some(PromptTokensDetails {
            cached_tokens: cached.min(prompt),
        }),
        credits,
    })
}

fn trae_snapshot_entry<'a>(
    entries: &'a mut Vec<(String, String)>,
    key: &str,
) -> &'a mut String {
    let index = if let Some(index) = entries.iter().position(|(id, _)| id == key) {
        index
    } else {
        entries.push((key.to_string(), String::new()));
        entries.len() - 1
    };
    &mut entries[index].1
}

fn trae_join_snapshots(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(_, text)| text.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

struct TraeTranslator {
    id: String,
    model: String,
    started: bool,
    visible: String,
    /// Cumulative visible snapshots keyed by plan-item id.
    plan_texts: Vec<(String, String)>,
    message_text: String,
    /// Final answer reported through the internal `finish` tool.
    finish_summary: String,
    /// Cumulative reasoning snapshots keyed by plan-item id.
    reasoning_texts: Vec<(String, String)>,
    emitted_reasoning: String,
    usage: Option<ChatUsage>,
    finish_reason: String,
    finished: bool,
    completed: bool,
}

impl TraeTranslator {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-trae-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            visible: String::new(),
            plan_texts: Vec::new(),
            message_text: String::new(),
            finish_summary: String::new(),
            reasoning_texts: Vec::new(),
            emitted_reasoning: String::new(),
            usage: None,
            finish_reason: "stop".to_string(),
            finished: false,
            completed: false,
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

    fn role_chunk(&self) -> ChatCompletionChunk {
        self.chunk(
            ChunkDelta {
                role: Some("assistant".to_string()),
                ..ChunkDelta::default()
            },
            None,
            None,
        )
    }

    fn refresh_visible(&mut self) -> String {
        let plan_text = self
            .plan_texts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>();
        let merged = merge_trae_summary(
            &merge_trae_text(&plan_text, &self.message_text),
            &self.finish_summary,
        );
        if merged.is_empty() {
            return String::new();
        }
        let delta = trae_text_delta(&self.visible, &merged);
        self.visible = merged;
        delta
    }

    fn refresh_reasoning(&mut self) -> String {
        let combined = trae_join_snapshots(&self.reasoning_texts);
        let delta = trae_text_delta(&self.emitted_reasoning, &combined);
        self.emitted_reasoning = combined;
        delta
    }

    fn push(&mut self, event: &str, data: &str) -> Vec<Result<ChatCompletionChunk>> {
        let mut event_name = normalize_trae_event_name(event);
        let payload = if data.trim() == "[DONE]" {
            serde_json::json!({})
        } else {
            serde_json::from_str::<serde_json::Value>(data).unwrap_or(serde_json::json!({}))
        };
        if event_name.is_empty() || event_name == "message" {
            let embedded = trae_payload_event_name(&payload);
            if !embedded.is_empty() && embedded != "message" {
                event_name = embedded;
            } else if event_name.is_empty() {
                event_name = "message".to_string();
            }
        }
        if matches!(event_name.as_str(), "heartbeat" | "keepalive" | "ping") {
            return Vec::new();
        }
        if event_name == "error" {
            self.finished = true;
            self.completed = true;
            let message = payload
                .get("message")
                .or_else(|| payload.get("error"))
                .and_then(|value| trae_nested_string(value))
                .unwrap_or("Trae upstream error");
            return vec![Err(MgError::provider(
                502,
                format!("Trae request failed: {message}"),
                "upstream_error",
            ))];
        }
        if matches!(
            event_name.as_str(),
            "status_change" | "status_changed" | "session_status"
        ) {
            let status = payload
                .get("status")
                .or_else(|| payload.get("new_status"))
                .or_else(|| payload.get("newStatus"))
                .and_then(serde_json::Value::as_i64);
            if let Some(status) = status {
                if status == 4 || status == 6 {
                    self.finished = true;
                    self.completed = true;
                    let msg = if status == 4 {
                        "Trae session was canceled upstream"
                    } else {
                        "Trae session failed upstream"
                    };
                    return vec![Err(MgError::provider(502, msg, "upstream_error"))];
                }
            }
        }
        if let Some(code) = payload.get("code").and_then(serde_json::Value::as_i64) {
            if code != 0 && code != 200 && payload.get("data").is_none() {
                let msg = payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream business error");
                self.finished = true;
                self.completed = true;
                return vec![Err(MgError::provider(
                    502,
                    format!("Trae error ({code}): {msg}"),
                    "upstream_error",
                ))];
            }
        }
        if event_name == "token_usage" {
            self.usage = trae_usage(&payload).or(self.usage.clone());
            return Vec::new();
        }
        if event_name == "done" || data.trim() == "[DONE]" {
            self.finished = true;
            if let Some(reason) = payload
                .get("finish_reason")
                .or_else(|| payload.get("stop_reason"))
                .or_else(|| payload.get("finishReason"))
                .or_else(|| payload.get("stopReason"))
                .and_then(serde_json::Value::as_str)
                .filter(|reason| !reason.is_empty())
            {
                self.finish_reason = reason.to_string();
            }
            return Vec::new();
        }

        let summary = trae_finish_summary(&payload);
        if !summary.is_empty() {
            self.finish_summary = summary;
        }
        let mut changed = false;
        match event_name.as_str() {
            "plan_item" => {
                let id = payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("plan");
                let reasoning = if trae_plan_has_split_reasoning(&payload) {
                    payload
                        .get("reasoning_content")
                        .or_else(|| payload.get("reasoningContent"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                } else {
                    trae_payload_reasoning(&payload)
                };
                let visible = trae_plan_visible_text(&payload);
                if !reasoning.is_empty() {
                    let snapshot = trae_snapshot_entry(&mut self.reasoning_texts, id);
                    merge_trae_snapshot(snapshot, &reasoning);
                    changed = true;
                }
                if !visible.is_empty() {
                    let snapshot = trae_snapshot_entry(&mut self.plan_texts, id);
                    merge_trae_snapshot(snapshot, &visible);
                    changed = true;
                }
            }
            "message" | "assistant_message" | "agent_message" | "response" | "text" | "output" => {
                let text = trae_message_text(&payload);
                let reasoning = trae_payload_reasoning(&payload);
                if !text.is_empty() {
                    merge_trae_snapshot(&mut self.message_text, &text);
                    changed = true;
                }
                if !reasoning.is_empty() {
                    let snapshot = trae_snapshot_entry(&mut self.reasoning_texts, "__message__");
                    merge_trae_snapshot(snapshot, &reasoning);
                    changed = true;
                }
            }
            _ => {}
        }
        if !changed && self.finish_summary.is_empty() {
            return Vec::new();
        }
        let reasoning_delta = self.refresh_reasoning();
        let delta = self.refresh_visible();
        if reasoning_delta.is_empty() && delta.is_empty() {
            return Vec::new();
        }
        let mut chunks = Vec::new();
        if !self.started {
            self.started = true;
            chunks.push(Ok(self.role_chunk()));
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

    fn finish(&mut self) -> Vec<Result<ChatCompletionChunk>> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        if self.visible.is_empty() {
            // Some turns only fill the reasoning channel. Prefer a bounded
            // trace over telling the client the turn completed with no output.
            let reasoning = trae_join_snapshots(&self.reasoning_texts);
            if reasoning.is_empty() {
                return vec![Err(MgError::provider(
                    502,
                    "Trae upstream returned an empty response",
                    "empty_response",
                ))];
            }
            self.visible = reasoning.clone();
            let mut chunks = Vec::new();
            if !self.started {
                self.started = true;
                chunks.push(Ok(self.role_chunk()));
            }
            chunks.push(Ok(self.chunk(
                ChunkDelta {
                    content: Some(reasoning),
                    ..ChunkDelta::default()
                },
                None,
                None,
            )));
            chunks.push(Ok(self.chunk(
                ChunkDelta::default(),
                Some(self.finish_reason.clone()),
                self.usage.clone(),
            )));
            return chunks;
        }
        vec![Ok(self.chunk(
            ChunkDelta::default(),
            Some(self.finish_reason.clone()),
            self.usage.clone(),
        ))]
    }
}

/// Trae upstream transports.
///
/// `remote` drives a full solo-agent session
/// (`/api/remote/v1/chat_sessions`): Trae injects its own ~24k-token agent
/// prompt, resolves built-in skills and boots a sandbox before the model is
/// called, so the first token only lands after that bootstrap. `solo` calls
/// the SOLO gateway directly (`/api/agent/v3/llm_utils_chat`), which invokes
/// the model without the agent layer and therefore reaches the first token in
/// roughly half the time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraeUpstream {
    Remote,
    Solo,
}

impl TraeUpstream {
    /// Stable name used by the desktop panel and the settings file.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Solo => "solo",
        }
    }

    /// Parse a mode name. Returns `None` for anything unknown so callers can
    /// reject the value instead of silently falling back.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "remote" | "agent" | "session" | "web" => Some(Self::Remote),
            "solo" | "direct" | "model" => Some(Self::Solo),
            _ => None,
        }
    }
}

/// SOLO gateway chat path on the model host.
const TRAE_SOLO_CHAT_PATH: &str = "/api/agent/v3/llm_utils_chat";

/// How many upstream calls one account may run at the same time.
///
/// The upstream enforces the same number for agent sessions and answers
/// `429 solo_agent_parallel_limit` past it.
const TRAE_WEB_PARALLEL_LIMIT: usize = 2;

/// How long a request waits for a free account slot before it is rejected.
/// Long enough to ride out one in-flight turn, short enough that a stuck
/// upstream does not hang the caller forever.
const TRAE_WEB_SLOT_WAIT_SECS: u64 = 90;

/// How many times a session creation is retried when the upstream reports that
/// another client holds the account's agent-session slots.
const TRAE_PARALLEL_LIMIT_RETRIES: usize = 4;

/// Pause between those retries.
const TRAE_PARALLEL_LIMIT_RETRY_SECS: u64 = 3;

/// Concurrent remote agent sessions allowed per account.
fn trae_web_parallel_limit() -> usize {
    env_override("TRAE_WEB_PARALLEL_LIMIT")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(TRAE_WEB_PARALLEL_LIMIT)
}

/// Whether an upstream error body reports the per-account agent-session
/// concurrency limit.
///
/// This is a traffic condition, not a broken account: benching the account for
/// it would take the whole pool offline for the length of the cooldown.
fn trae_is_parallel_limit(body: &str) -> bool {
    body.contains("solo_agent_parallel_limit")
}

/// Runtime override for the upstream transport.
///
/// The desktop panel writes it so operators can switch channels without
/// restarting the gateway. `None` means "no override" and lets the
/// `TRAE_UPSTREAM_MODE` environment variable decide.
static TRAE_UPSTREAM_OVERRIDE: RwLock<Option<TraeUpstream>> = RwLock::new(None);

/// Set the runtime upstream transport. `None` clears the override so the
/// environment variable takes over again.
pub fn set_trae_upstream_mode(mode: Option<TraeUpstream>) {
    *TRAE_UPSTREAM_OVERRIDE.write() = mode;
}

/// Upstream transport currently in effect.
///
/// The runtime override wins over the environment variable, which in turn wins
/// over the default (the fast SOLO route).
pub fn trae_upstream_mode() -> TraeUpstream {
    if let Some(mode) = *TRAE_UPSTREAM_OVERRIDE.read() {
        return mode;
    }
    match env_override("TRAE_UPSTREAM_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "remote" | "agent" | "session" | "web" => TraeUpstream::Remote,
        _ => TraeUpstream::Solo,
    }
}

/// Headers the SOLO gateway expects from a native Trae client.
fn trae_solo_headers(account: &mg_core::Account, surface: &TraeSurface) -> Vec<(String, String)> {
    let device_id = trae_device_id(account);
    let machine_id = format!("{:x}", Sha256::digest(account.id.as_bytes()));
    let user_id = trae_account_value(
        account,
        &["bizUserId", "user_id", "userId", "userUniqueId"],
        &account.id,
    );
    let token = account.access_token.clone();
    let headers = [
        ("Authorization", format!("Cloud-IDE-JWT {token}")),
        ("Content-Type", "application/json".to_string()),
        ("Accept", "text/event-stream".to_string()),
        ("X-Cloudide-Token", token.clone()),
        ("x-uid", user_id),
        ("x-app-id", "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8".to_string()),
        ("x-device-id", device_id),
        ("x-machine-id", machine_id),
        ("x-request-id", uuid::Uuid::new_v4().to_string()),
        ("x-device-cpu", "AMD".to_string()),
        ("x-device-brand", "83DG".to_string()),
        ("x-device-type", "windows".to_string()),
        ("x-device-platform", "windows".to_string()),
        ("x-ide-token", token),
        ("x-os-version", "Windows 11 Pro".to_string()),
        ("x-system-type", "Windows".to_string()),
        ("x-app-version", "default".to_string()),
        ("x-app-version-code", "20260811".to_string()),
        ("x-ide-version", "0.1.52".to_string()),
        ("x-ide-version-code", "20260811".to_string()),
        ("x-ide-version-type", "stable".to_string()),
        ("x-version-code", "20260811".to_string()),
        ("package-type", surface.package_type.to_string()),
        ("x-lscbd-aid", "787976".to_string()),
        ("x-lscbd-platform", "windows".to_string()),
        ("x-ss-dp", "787976".to_string()),
        ("x-plugin-channel", "icube-ai".to_string()),
        ("app-version", "0.1.52".to_string()),
        ("x-bridge-transport", "aha".to_string()),
        ("x-ahanet-timeout", "86400".to_string()),
        ("x-lgw-req-sdk-type", "3".to_string()),
        ("x-net-sdk-domain-dispatch", "1".to_string()),
        ("x-ttnet-bypass-decompression", "1".to_string()),
        ("x-ttnet-bypass-cookie", "0".to_string()),
        ("request-traffic-type", "prod".to_string()),
        ("User-Agent", "Trae/0.1.52".to_string()),
        (
            "Referer",
            format!("{}{TRAE_SOLO_CHAT_PATH}", surface.model_host),
        ),
    ];
    headers
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

/// Convert chat messages into the SOLO gateway's native content blocks.
fn trae_solo_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut native = Vec::new();
    for message in messages {
        let text = trae_text(&message.content);
        let (role, content) = match message.role.as_str() {
            "system" | "developer" => ("system", text),
            "assistant" => {
                let mut content = text;
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        if !content.is_empty() {
                            content.push_str("\n\n");
                        }
                        content.push_str(&format!(
                            "[Tool Call: {} {}]\nArguments: {}",
                            call.id, call.function.name, call.function.arguments
                        ));
                    }
                }
                ("assistant", content)
            }
            "tool" => (
                "user",
                format!(
                    "[Client Tool Result: {} {}]\n{}",
                    message.tool_call_id.as_deref().unwrap_or("unknown"),
                    message.name.as_deref().unwrap_or("tool"),
                    text
                ),
            ),
            _ => ("user", text),
        };
        if content.trim().is_empty() {
            continue;
        }
        native.push(serde_json::json!({
            "role": role,
            "content": [{"type": "text", "text": content}]
        }));
    }
    native
}

/// Convert OpenAI tool definitions to the SOLO gateway's native shape.
///
/// The upstream expects `function.parameters` as a JSON *string*; every other
/// field keeps the caller's schema untouched.
fn trae_solo_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let mut function = match object.get("function").and_then(serde_json::Value::as_object) {
            Some(function) => function.clone(),
            None => {
                // Accept the compact function shape some adapters send.
                let mut function = serde_json::Map::new();
                for key in ["name", "description", "parameters", "strict"] {
                    if let Some(value) = object.get(key) {
                        function.insert(key.to_string(), value.clone());
                    }
                }
                function
            }
        };
        if function
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            continue;
        }
        let parameters = function.get("parameters").cloned().unwrap_or_else(|| {
            function
                .get("input_schema")
                .or_else(|| function.get("inputSchema"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}))
        });
        let parameters = match parameters {
            serde_json::Value::String(text) => text,
            other => serde_json::to_string(&other).unwrap_or_else(|_| "{}".to_string()),
        };
        function.insert("parameters".to_string(), serde_json::Value::String(parameters));
        function.remove("input_schema");
        function.remove("inputSchema");
        let mut converted = object.clone();
        converted.insert("function".to_string(), serde_json::Value::Object(function));
        converted.insert(
            "type".to_string(),
            serde_json::Value::String(
                object
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("function")
                    .to_string(),
            ),
        );
        result.push(serde_json::Value::Object(converted));
    }
    result
}

/// Normalize the caller's tool choice into the SOLO gateway's spelling.
fn trae_solo_tool_choice(choice: &serde_json::Value) -> Option<String> {
    if let Some(text) = choice.as_str() {
        let lowered = text.trim().to_ascii_lowercase();
        return match lowered.as_str() {
            "none" | "auto" | "required" => Some(lowered),
            _ => None,
        };
    }
    match choice.get("type").and_then(serde_json::Value::as_str)? {
        "none" | "auto" | "required" => choice
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        "function" => choice
            .pointer("/function/name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Build one SOLO gateway chat request.
fn trae_solo_body(req: &ChatCompletionRequest, model_name: &str) -> serde_json::Value {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut body = serde_json::json!({
        "messages": trae_solo_messages(&req.messages),
        "config_name": model_name,
        "model": model_name,
        "function": "solo_work_lite",
        "stream": true,
        "request_id": session_id,
        "session_id": session_id,
    });
    if let Some(max_tokens) = req.max_completion_tokens.or(req.max_tokens) {
        let ceiling = crate::model_limits::clamp_output_tokens(max_tokens, model_name);
        body["max_tokens"] = serde_json::json!(ceiling);
    }
    if let Some(tools) = req.tools.as_deref() {
        let native = trae_solo_tools(tools);
        if !native.is_empty() {
            body["tools"] = serde_json::Value::Array(native);
            if let Some(choice) = req.tool_choice.as_ref().and_then(trae_solo_tool_choice) {
                if choice != "none" {
                    body["tool_choice"] = serde_json::Value::String(choice);
                }
            }
            if let Some(parallel) = req.parallel_tool_calls {
                body["parallel_tool_calls"] = serde_json::Value::Bool(parallel);
            }
        }
    }
    body
}

/// Read tool calls carried by a SOLO `output` event.
///
/// The upstream uses the native `function_call` envelope and streams one call
/// in pieces: the first event carries the id and the name, a later event
/// carries the arguments. Both share the upstream `index`, and empty fields
/// are dropped so the caller's accumulator keeps what it already has.
fn trae_solo_tool_calls(payload: &serde_json::Value) -> Option<Vec<ToolCallChunk>> {
    let calls = payload.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }
    Some(
        calls
            .iter()
            .enumerate()
            .map(|(position, call)| {
                let index = call
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as u32)
                    .unwrap_or(position as u32);
                let name = call
                    .pointer("/function_call/name")
                    .or_else(|| call.pointer("/function/name"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let arguments = call
                    .pointer("/function_call/arguments")
                    .or_else(|| call.pointer("/function/arguments"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                ToolCallChunk {
                    index,
                    id: call
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    tool_type: call
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    function: Some(FunctionCallChunk { name, arguments }),
                }
            })
            .collect(),
    )
}

/// Translates the SOLO gateway's event stream into OpenAI chunks.
///
/// Unlike the remote agent stream, SOLO emits plain deltas, so no cumulative
/// snapshot merging is required.
struct TraeSoloTranslator {
    id: String,
    model: String,
    started: bool,
    usage: Option<ChatUsage>,
    finish_reason: String,
    finished: bool,
    completed: bool,
    text: String,
    reasoning: String,
}

impl TraeSoloTranslator {
    fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-trae-{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            usage: None,
            finish_reason: "stop".to_string(),
            finished: false,
            completed: false,
            text: String::new(),
            reasoning: String::new(),
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

    fn role_chunk(&self) -> ChatCompletionChunk {
        self.chunk(
            ChunkDelta {
                role: Some("assistant".to_string()),
                ..ChunkDelta::default()
            },
            None,
            None,
        )
    }

    fn push(&mut self, event: &str, data: &str) -> Vec<Result<ChatCompletionChunk>> {
        let mut chunks = Vec::new();
        let event_name = event.trim().to_ascii_lowercase();
        if data.trim() == "[DONE]" {
            self.finished = true;
            return chunks;
        }
        let payload =
            serde_json::from_str::<serde_json::Value>(data).unwrap_or(serde_json::Value::Null);
        match event_name.as_str() {
            "output" => {
                let text = payload
                    .get("response")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let reasoning = payload
                    .get("reasoning_content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let tool_calls = trae_solo_tool_calls(&payload);
                if text.is_empty() && reasoning.is_empty() && tool_calls.is_none() {
                    return chunks;
                }
                if !self.started {
                    self.started = true;
                    chunks.push(Ok(self.role_chunk()));
                }
                self.text.push_str(text);
                self.reasoning.push_str(reasoning);
                chunks.push(Ok(self.chunk(
                    ChunkDelta {
                        content: (!text.is_empty()).then(|| text.to_string()),
                        reasoning_content: (!reasoning.is_empty()).then(|| reasoning.to_string()),
                        tool_calls,
                        ..ChunkDelta::default()
                    },
                    None,
                    None,
                )));
            }
            "token_usage" => {
                self.usage = trae_usage(&payload).or(self.usage.clone());
            }
            "done" => {
                self.finished = true;
                if let Some(reason) = payload
                    .get("finish_reason")
                    .and_then(serde_json::Value::as_str)
                    .filter(|reason| !reason.is_empty())
                {
                    self.finish_reason = reason.to_string();
                }
            }
            "error" => {
                self.finished = true;
                let code = payload
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let message = payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream error");
                chunks.push(Err(MgError::provider(
                    502,
                    format!("Trae error ({code}): {message}"),
                    "upstream_error",
                )));
            }
            _ => {}
        }
        chunks
    }

    fn finish(&mut self) -> Vec<Result<ChatCompletionChunk>> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let mut chunks = Vec::new();
        if self.text.trim().is_empty() && !self.reasoning.trim().is_empty() {
            // The turn only produced reasoning; surface it instead of handing
            // the caller an empty answer.
            let reasoning = self.reasoning.clone();
            if !self.started {
                self.started = true;
                chunks.push(Ok(self.role_chunk()));
            }
            chunks.push(Ok(self.chunk(
                ChunkDelta {
                    content: Some(reasoning),
                    ..ChunkDelta::default()
                },
                None,
                None,
            )));
        }
        chunks.push(Ok(self.chunk(
            ChunkDelta::default(),
            Some(self.finish_reason.clone()),
            self.usage.clone(),
        )));
        chunks
    }
}

/// How long a fetched remote model config stays usable. The remote catalog
/// only changes when Trae ships new models, so a short TTL is enough to keep
/// the per-request upstream round trip off the critical path.
const TRAE_MODEL_CONFIG_TTL_SECS: f64 = 900.0;

impl TraeProvider {
    /// Resolve one remote model config, reusing a recent catalog lookup.
    ///
    /// The upstream catalog is stable for hours, but every chat request used
    /// to fetch it again, which added a full upstream round trip (and an extra
    /// upstream call) before the turn could even start.
    async fn fetch_trae_custom_model(
        &self,
        account: &mg_core::Account,
        surface: &TraeSurface,
        model_name: &str,
    ) -> Option<serde_json::Value> {
        let key = format!(
            "{}\u{1f}{}",
            account.id,
            model_name.trim().to_ascii_lowercase()
        );
        let now = mg_core::now_secs();
        {
            let cache = self.model_configs.read();
            if let Some((config, fetched_at)) = cache.get(&key) {
                if now - *fetched_at < TRAE_MODEL_CONFIG_TTL_SECS {
                    return Some(config.clone());
                }
            }
        }
        let url = format!(
            "{}/models?functions=solo_agent_remote%2Csolo_work_remote%2Csolo_design_remote&show_custom_model=true",
            surface.remote_base
        );
        let response = apply_headers(
            self.client.get(url).timeout(Duration::from_secs(20)),
            &trae_web_headers(account, surface),
        )
        .send()
        .await
        .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let payload = response.json::<serde_json::Value>().await.ok()?;
        let config = trae_custom_model(&payload, model_name)?;
        self.model_configs
            .write()
            .insert(key, (config.clone(), now));
        Some(config)
    }
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

/// Stop the upstream session in the background.
///
/// The stop releases the account's agent-session slot upstream, so it must
/// happen - but waiting for the round trip would hold back a response the
/// client is already reading.
fn stop_trae_session_in_background(
    client: Client,
    surface: TraeSurface,
    headers: Vec<(String, String)>,
    session_id: String,
    message_id: String,
) {
    tokio::spawn(async move {
        stop_trae_session(&client, &surface, &headers, &session_id, &message_id).await;
    });
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

/// Describe a failed upstream stream in terms an operator can act on.
///
/// `reqwest` reports every incomplete response body the same way — "error
/// decoding response body" — no matter whether the body was cut short by a
/// timeout, a connection reset, or a truncated chunked frame. On its own that
/// message says nothing about the request: it does not say whether the
/// upstream ever answered, how much of the answer arrived, or how long the
/// gateway waited for it.
///
/// `chunks` separates "the upstream never sent anything" from "the upstream
/// stopped mid-answer", and the transport's timeout flag names the actual
/// cause instead of leaving it to guesswork.
fn trae_stream_failure(
    route: &str,
    error: &EventStreamError<reqwest::Error>,
    elapsed: Duration,
    chunks: usize,
) -> String {
    let (reason, detail) = match error {
        EventStreamError::Transport(transport) => {
            let reason = if transport.is_timeout() {
                "the upstream stopped sending data"
            } else if transport.is_connect() {
                "the upstream connection failed"
            } else {
                "the upstream closed the connection before the answer was complete"
            };
            (reason, transport.to_string())
        }
        EventStreamError::Utf8(_) => (
            "the upstream sent a response that is not valid UTF-8",
            error.to_string(),
        ),
        EventStreamError::Parser(_) => (
            "the upstream sent a malformed event stream",
            error.to_string(),
        ),
    };
    format!(
        "Trae {route} stream failed after {chunks} chunk(s) in {:.1}s: {reason} ({detail})",
        elapsed.as_secs_f64()
    )
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
    /// Remote model configs keyed by `<account id>\u{1f}<model>`. The catalog
    /// changes rarely, and re-fetching it for every chat request put a full
    /// upstream round trip in front of every turn.
    model_configs: RwLock<HashMap<String, (serde_json::Value, f64)>>,
    /// Per-account gate for the upstream calls.
    ///
    /// The upstream allows only two concurrent agent sessions per account and
    /// answers `429 solo_agent_parallel_limit` beyond that. Without a local
    /// gate the gateway pushes every request upstream at once: the extras are
    /// rejected outright, or they queue inside the upstream, which shows up as
    /// a 15-55 second first-token delay.
    ///
    /// Both routes draw on this one gate. The direct SOLO route needs it too:
    /// a request that starts while the upstream is still draining agent
    /// sessions waits there for the better part of a minute.
    ///
    /// The permit is held for the whole lifetime of the upstream call, so the
    /// gateway never exceeds the upstream's own limit.
    web_slots: RwLock<HashMap<String, Arc<tokio::sync::Semaphore>>>,
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
            model_configs: RwLock::new(HashMap::new()),
            web_slots: RwLock::new(HashMap::new()),
        }
    }

    /// Take one of the account's remote agent-session slots.
    ///
    /// Waiting is the point: the upstream rejects the extra requests instead of
    /// serving them, so a caller that arrives too early is better off waiting
    /// for a slot than failing.
    async fn acquire_web_slot(
        &self,
        account_id: &str,
    ) -> Result<tokio::sync::OwnedSemaphorePermit> {
        let semaphore = {
            let mut slots = self.web_slots.write();
            slots
                .entry(account_id.to_string())
                .or_insert_with(|| {
                    Arc::new(tokio::sync::Semaphore::new(trae_web_parallel_limit()))
                })
                .clone()
        };
        match tokio::time::timeout(
            Duration::from_secs(TRAE_WEB_SLOT_WAIT_SECS),
            semaphore.acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(MgError::provider(
                503,
                "Trae 会话槽已关闭".to_string(),
                "unavailable",
            )),
            Err(_) => Err(MgError::provider(
                429,
                "Trae 会话并发已达上限，请稍后重试".to_string(),
                "too_many_requests",
            )),
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
        let account_id = account.id.clone();
        let lock = self.pool.refresh_lock(&account_id);
        {
            let _guard = lock.lock().await;
            let current = self.pool.get(&account_id).unwrap_or(account);
            if !trae_token_needs_refresh(&current) {
                return Ok(current);
            }
        }
        match self.refresh_credentials(&account_id).await {
            Ok(_) => self
                .pool
                .get(&account_id)
                .ok_or_else(|| MgError::not_found("Account not found")),
            Err(error) => {
                self.pool.set_error(&account_id, &error.message());
                Err(error)
            }
        }
    }

    /// Run one turn through the remote solo-agent session.
    async fn chat_remote(
        &self,
        req: &ChatCompletionRequest,
        account: &mg_core::Account,
        surface: &TraeSurface,
        account_id: &str,
        requested_model: &str,
        mode: &str,
        strategy: &str,
        model_name: &str,
        agent_type: &str,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)> {
        // One upstream agent session per slot. Holding the permit for the whole
        // session keeps the gateway inside the upstream's own concurrency limit.
        let web_slot = self.acquire_web_slot(account_id).await?;
        let headers = trae_web_headers(account, surface);
        let custom_model = if strategy == "auto" {
            None
        } else {
            self.fetch_trae_custom_model(account, surface, model_name)
                .await
        };
        let bounded_messages = bound_trae_messages(&req.messages);
        let body = trae_session_body(
            &bounded_messages,
            account,
            surface,
            mode,
            strategy,
            model_name,
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

        let mut attempt = 0usize;
        let created = loop {
            attempt += 1;
            let response = apply_headers(
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
            let status = response.status();
            if status.is_success() {
                break response;
            }
            let err_text = response.text().await.unwrap_or_default();
            // Someone else - another client of the same account - is holding the
            // agent-session slots. Wait for them instead of failing the turn.
            if status.as_u16() == 429
                && trae_is_parallel_limit(&err_text)
                && attempt < TRAE_PARALLEL_LIMIT_RETRIES
            {
                tokio::time::sleep(Duration::from_secs(TRAE_PARALLEL_LIMIT_RETRY_SECS)).await;
                continue;
            }
            // A concurrency rejection says the account is busy, not broken.
            // Benching it would take the whole pool offline for the length of
            // the cooldown while the other sessions finish on their own.
            if crate::is_account_level_failure(status.as_u16())
                && !trae_is_parallel_limit(&err_text)
            {
                self.pool
                    .mark_cooldown(account_id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(account_id, &format!("upstream HTTP {}", status.as_u16()));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae request failed: {}",
                    crate::upstream_error_body(status.as_u16(), &err_text)
                ),
                "upstream_error",
            ));
        };
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
        let public_model = requested_model.to_string();
        tokio::spawn(async move {
            // Holding the permit for the whole session keeps the gateway inside
            // the upstream's per-account agent-session limit.
            let _web_slot = web_slot;
            let mut translator = TraeTranslator::new(&public_model);
            let mut incoming = events.bytes_stream().eventsource();
            let started = Instant::now();
            let mut chunks = 0usize;
            while let Some(item) = incoming.next().await {
                match item {
                    Ok(event) => {
                        for chunk in translator.push(&event.event, &event.data) {
                            chunks += 1;
                            if tx.send(chunk).await.is_err() {
                                // The caller hung up: tell upstream to stop
                                // generating so the account's agent slot frees.
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
                                trae_stream_failure("agent session", &error, started.elapsed(), chunks),
                                "upstream_error",
                            )))
                            .await;
                        break;
                    }
                }
            }
            for chunk in translator.finish() {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
            // Tell the upstream to stop the session. It frees the account's
            // agent-session slot upstream, so it has to happen - but it must
            // not hold back the response the client is already reading, hence
            // the background task. A terminal `done` event means the turn is
            // over and the upstream answers the stop with 409, which is fine:
            // the slot is released when the upstream ends the turn.
            stop_trae_session_in_background(
                client.clone(),
                stop_surface.clone(),
                stop_headers.clone(),
                remote_session.clone(),
                message_id.clone(),
            );
        });

        Ok((
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            account_id.to_string(),
            Some("trae:agent-session".to_string()),
        ))
    }

    /// Run one turn through the SOLO gateway (`llm_utils_chat`).
    ///
    /// This is the direct model route: no solo-agent bootstrap, no injected
    /// agent prompt, so the first token arrives roughly twice as fast as the
    /// remote session route.
    async fn chat_solo(
        &self,
        req: &ChatCompletionRequest,
        account: &mg_core::Account,
        surface: &TraeSurface,
        model_name: &str,
        requested_model: &str,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)> {
        // The direct route shares the account's upstream budget with the agent
        // session route, so it takes a slot from the same gate.
        let web_slot = self.acquire_web_slot(&account.id).await?;
        let headers = trae_solo_headers(account, surface);
        let body = trae_solo_body(req, model_name);
        let url = format!("{}{TRAE_SOLO_CHAT_PATH}", surface.model_host);

        info!(
            provider = "trae",
            model = %requested_model,
            account = %account.id,
            upstream = "solo",
            stream = req.stream,
            "Forwarding chat completion to the Trae SOLO gateway"
        );

        // No total timeout on the streaming request: the upstream's turn can
        // legitimately run for minutes (the same one-line prompt measured
        // between 2.6s and 48s), and a total timeout aborts the response body
        // mid-answer. The client's idle `read_timeout` is the bound that
        // matters here - it only fires when the upstream goes quiet, and the
        // upstream sends progress pings while it thinks.
        let response = apply_headers(self.stream_client.post(url), &headers)
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
        let status = response.status();
        if !status.is_success() {
            let err_text = response.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Trae SOLO request failed: {}",
                    crate::upstream_error_body(status.as_u16(), &err_text)
                ),
                "upstream_error",
            ));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let public_model = requested_model.to_string();
        tokio::spawn(async move {
            // The direct route draws on the same account budget as an agent
            // session: a request that starts while the upstream is still
            // draining agent sessions waits there for a minute. Holding the
            // slot here keeps that wait local and bounded.
            let _web_slot = web_slot;
            let mut translator = TraeSoloTranslator::new(&public_model);
            let mut incoming = response.bytes_stream().eventsource();
            let started = Instant::now();
            let mut chunks = 0usize;
            while let Some(item) = incoming.next().await {
                match item {
                    Ok(event) => {
                        for chunk in translator.push(&event.event, &event.data) {
                            chunks += 1;
                            if tx.send(chunk).await.is_err() {
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
                                trae_stream_failure("SOLO", &error, started.elapsed(), chunks),
                                "upstream_error",
                            )))
                            .await;
                        break;
                    }
                }
            }
            for chunk in translator.finish() {
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
        });

        Ok((
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            account.id.clone(),
            Some("trae:direct-model".to_string()),
        ))
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
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)> {
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
        let (mode, strategy, model_name, agent_type) = trae_resolve_mode(&req.model);

        if trae_upstream_mode() == TraeUpstream::Solo {
            // The SOLO gateway takes the exact upstream config name, which the
            // remote catalog already carries. `auto` has no config to bind, so
            // fall back to a known preset.
            let solo_model = if model_name.trim().is_empty() {
                "glm-5.2".to_string()
            } else {
                model_name.clone()
            };
            match self
                .chat_solo(&req, &account, &surface, &solo_model, &requested_model)
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) => {
                    // The fallback is not a cosmetic detail: the agent session
                    // injects Trae's ~24k-token agent prompt, so a turn that
                    // silently switches to it reports a completely different
                    // prompt size and latency than the direct route. Say so.
                    tracing::warn!(
                        provider = "trae",
                        model = %requested_model,
                        %error,
                        "Trae SOLO upstream failed, falling back to the remote agent session"
                    );
                }
            }
        }

        self.chat_remote(
            &req,
            &account,
            &surface,
            &account_id,
            &requested_model,
            mode,
            strategy,
            &model_name,
            agent_type,
        )
        .await
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
        let lock = self.pool.refresh_lock(account_id);
        let _guard = lock.lock().await;
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
        bound_trae_messages, canonical_trae_model_name, parse_trae_catalog, parse_trae_quota,
        parse_trae_solo_catalog, trae_custom_model, trae_fallback_models, trae_is_parallel_limit,
        trae_model_config_names, trae_session_body, trae_solo_body, trae_solo_messages,
        trae_stream_failure, trae_surface, trae_surface_with, trae_token_needs_refresh,
        trae_upstream_mode, trae_usage, trae_web_parallel_limit, EventStreamError, TraeProvider,
        TraeSoloTranslator, TraeSurfaceEnv, TraeTranslator, TraeUpstream,
    };
    use mg_protocols::openai::{
        ChatCompletionRequest, ChatMessage, FunctionCall, ToolCall,
    };
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// `TRAE_REMOTE_QUERY_MAX_CHARS` is process-global, so tests that mutate it
    /// must not overlap; otherwise one test observes the other's limit.
    static QUERY_LIMIT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `TRAE_UPSTREAM_MODE` is process-global for the same reason.
    static UPSTREAM_MODE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `TRAE_WEB_PARALLEL_LIMIT` is process-global too.
    static PARALLEL_LIMIT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn translator_text(
        chunks: &[mg_core::Result<mg_protocols::openai::ChatCompletionChunk>],
    ) -> String {
        chunks
            .iter()
            .filter_map(|chunk| chunk.as_ref().ok())
            .flat_map(|chunk| chunk.choices.iter())
            .filter_map(|choice| choice.delta.content.clone())
            .collect()
    }

    fn translator_reasoning(
        chunks: &[mg_core::Result<mg_protocols::openai::ChatCompletionChunk>],
    ) -> String {
        chunks
            .iter()
            .filter_map(|chunk| chunk.as_ref().ok())
            .flat_map(|chunk| chunk.choices.iter())
            .filter_map(|choice| choice.delta.reasoning_content.clone())
            .collect()
    }

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
    fn finish_tool_summary_is_emitted_when_no_plan_item_carries_an_answer() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        let reasoning = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","reasoning_content":"The user wants pong"}"#,
        );
        assert_eq!(translator_reasoning(&reasoning), "The user wants pong");
        assert!(translator_text(&reasoning).is_empty());

        let finish = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","reasoning_content":"","tool_call_info":{"name":"finish","params":{"summary":"pong"}}}"#,
        );
        assert_eq!(translator_text(&finish), "pong");

        assert!(translator.push("done", "{}").is_empty());
        let terminal = translator.finish();
        assert_eq!(terminal.len(), 1);
        let chunk = terminal[0].as_ref().unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn finish_summary_only_turn_is_not_reported_as_empty() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        let chunks = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","tool_call_info":{"name":"finish","params":{"summary":"done"}}}"#,
        );
        assert_eq!(translator_text(&chunks), "done");

        translator.push("done", "{}");
        let terminal = translator.finish();
        assert_eq!(terminal.len(), 1);
        assert!(
            translator_text(&terminal).is_empty(),
            "finish summary must not be replayed"
        );
    }

    #[test]
    fn finish_summary_does_not_duplicate_streamed_answer() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        assert_eq!(
            translator_text(&translator.push("plan_item", r#"{"id":"pi_1","thought":"pong"}"#)),
            "pong"
        );
        let finish = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","tool_call_info":{"name":"finish","params":{"summary":"pong"}}}"#,
        );
        assert!(finish.is_empty());
        let extended = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","tool_call_info":{"name":"finish","params":{"summary":"pong!"}}}"#,
        );
        assert_eq!(translator_text(&extended), "!");
    }

    #[test]
    fn plan_item_prefers_explicit_content_over_thought() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        let chunks = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"narrating the answer","reasoning_content":"","content":"the answer"}"#,
        );
        assert_eq!(translator_text(&chunks), "the answer");
        assert!(translator_reasoning(&chunks).is_empty());
    }

    #[test]
    fn camel_case_events_and_embedded_payload_types_are_understood() {
        let mut prefixed = TraeTranslator::new("deepseek-v4.1-flash");
        assert_eq!(
            translator_text(
                &prefixed.push("PlanItem", r#"{"id":"pi_1","thought":"pong"}"#)
            ),
            "pong"
        );
        assert!(!prefixed.finished);
        prefixed.push("ResponseDone", "{}");
        assert!(prefixed.finished);

        let mut embedded = TraeTranslator::new("deepseek-v4.1-flash");
        assert_eq!(
            translator_text(&embedded.push(
                "message",
                r#"{"type":"plan_item","id":"pi_1","thought":"pong"}"#
            )),
            "pong"
        );
    }

    #[test]
    fn empty_upstream_turn_reports_an_error_instead_of_a_completed_empty_response() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        assert!(translator.push("metadata", "{}").is_empty());
        assert!(translator.push("done", "{}").is_empty());
        let chunks = translator.finish();
        assert_eq!(chunks.len(), 1);
        let error = chunks[0].as_ref().unwrap_err();
        assert!(error.message().contains("empty response"));
    }

    #[test]
    fn reasoning_only_turn_falls_back_to_content() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        let chunks = translator.push(
            "plan_item",
            r#"{"id":"pi_1","thought":"","reasoning_content":"only reasoning"}"#,
        );
        assert_eq!(translator_reasoning(&chunks), "only reasoning");
        translator.push("done", "{}");
        let terminal = translator.finish();
        assert_eq!(translator_text(&terminal), "only reasoning");
    }

    #[test]
    fn multiple_plan_items_are_joined_in_arrival_order() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        assert_eq!(
            translator_text(
                &translator.push("plan_item", r#"{"id":"pi_1","thought":"hello "}"#)
            ),
            "hello "
        );
        assert_eq!(
            translator_text(
                &translator.push("plan_item", r#"{"id":"pi_2","thought":"world"}"#)
            ),
            "world"
        );
        let grown = translator.push("plan_item", r#"{"id":"pi_1","thought":"hello there "}"#);
        assert_eq!(translator_text(&grown), "there world");
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

    #[test]
    fn trae_session_body_passes_empty_content_and_agent_id() {
        let account = serde_json::from_value::<mg_core::Account>(json!({
            "id": "2578818247695664",
            "access_token": "token",
            "region": "cn"
        }))
        .unwrap();
        let surface = trae_surface("cn");
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let custom_model = json!({
            "name": "deepseek-v4.1-flash",
            "config_source": 1,
            "is_preset": true,
            "provider": "deepseek"
        });
        let body = trae_session_body(
            &messages,
            &account,
            &surface,
            "code",
            "manual",
            "deepseek-v4.1-flash",
            "solo_agent_remote",
            Some(custom_model),
        );
        let initial = &body["initial_message"];
        assert_eq!(initial["content"], json!([]));
        assert_eq!(initial["agent_id"], "solo_agent_remote");
        assert_eq!(initial["agent_type"], "solo_agent_remote");
        assert_eq!(initial["model_name"], "deepseek-v4.1-flash");
        assert_eq!(initial["model_config_source"], 1);
        assert_eq!(initial["model_is_preset"], true);
        assert_eq!(initial["model_provider"], "deepseek");
    }

    #[test]
    fn bound_trae_messages_keeps_short_history_intact() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(json!("You are an AI assistant.")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("Hello!")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let bounded = bound_trae_messages(&messages);
        assert_eq!(bounded.len(), 2);
        assert_eq!(bounded[0].role, "system");
        assert_eq!(bounded[1].role, "user");
    }

    #[test]
    fn bound_trae_messages_drops_paired_assistant_tool_call_and_result() {
        let _env = QUERY_LIMIT_ENV
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::env::set_var("TRAE_REMOTE_QUERY_MAX_CHARS", "200");
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(json!("System prompt")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("Run tool A")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("Calling tool")),
                name: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "fetch".to_string(),
                        arguments: "{\"url\":\"http://test\"}".to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("Very long tool output that exceeds limit by far ...")),
                name: Some("fetch".to_string()),
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("Final turn question")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let bounded = bound_trae_messages(&messages);
        std::env::remove_var("TRAE_REMOTE_QUERY_MAX_CHARS");

        // The system prompt and final question must remain
        assert_eq!(bounded[0].role, "system");
        let last = bounded.last().unwrap();
        assert_eq!(last.role, "user");
        assert_eq!(last.content, Some(json!("Final turn question")));
        // The assistant call and tool result must not leave an orphaned half
        let has_call = bounded.iter().any(|m| m.tool_calls.is_some());
        let has_tool = bounded.iter().any(|m| m.role == "tool");
        assert_eq!(has_call, has_tool);
    }

    #[test]
    fn bound_trae_messages_injects_task_anchor_when_continuation_turn() {
        let _env = QUERY_LIMIT_ENV
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::env::set_var("TRAE_REMOTE_QUERY_MAX_CHARS", "100");
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(json!("System prompt")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("Implement feature XYZ for the project")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("Working on it... very very long intermediate output from the model that definitely takes up space in the context")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("继续")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let bounded = bound_trae_messages(&messages);
        std::env::remove_var("TRAE_REMOTE_QUERY_MAX_CHARS");

        // The anchor must be present
        let has_anchor = bounded.iter().any(|m| {
            if let Some(content) = m.content.as_ref().and_then(|c| c.as_str()) {
                content.contains("Task Anchor - User Request") && content.contains("Implement feature XYZ")
            } else {
                false
            }
        });
        assert!(has_anchor, "Task anchor must be preserved for continuation turn");
    }

    #[test]
    fn status_change_to_canceled_or_failed_reports_error() {
        let mut translator = TraeTranslator::new("deepseek-v4.1-flash");
        let chunks = translator.push("status_change", r#"{"status":4}"#);
        assert_eq!(chunks.len(), 1);
        let error = chunks[0].as_ref().unwrap_err();
        assert!(error.message().contains("canceled upstream"));

        let mut fail_translator = TraeTranslator::new("deepseek-v4.1-flash");
        let fail_chunks = fail_translator.push("status_changed", r#"{"new_status":6}"#);
        assert_eq!(fail_chunks.len(), 1);
        let fail_err = fail_chunks[0].as_ref().unwrap_err();
        assert!(fail_err.message().contains("failed upstream"));
    }

    #[test]
    fn trae_usage_reports_the_upstream_prompt_cache() {
        // Shape Trae's `token_usage` event sends on both transports.
        let usage = trae_usage(&json!({
            "name": "",
            "prompt_tokens": 24045,
            "completion_tokens": 43,
            "total_tokens": 24088,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 23936,
            "reasoning_tokens": 27
        }))
        .expect("usage must be reported");
        assert_eq!(usage.prompt_tokens, 24045);
        assert_eq!(usage.completion_tokens, 43);
        let details = usage
            .prompt_tokens_details
            .expect("cache split must reach the client");
        assert_eq!(details.cached_tokens, 23936);
    }

    #[test]
    fn trae_usage_without_cache_fields_keeps_details_absent() {
        let usage = trae_usage(&json!({
            "prompt_tokens": 16,
            "completion_tokens": 7,
            "total_tokens": 23
        }))
        .expect("usage must be reported");
        assert!(usage.prompt_tokens_details.is_none());
    }

    #[test]
    fn trae_usage_never_reports_more_cached_than_prompt_tokens() {
        let usage = trae_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 1,
            "cache_read_input_tokens": 999
        }))
        .expect("usage must be reported");
        let details = usage.prompt_tokens_details.expect("cache split present");
        assert_eq!(details.cached_tokens, 10);
    }

    #[test]
    fn trae_solo_body_uses_native_content_blocks() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(json!("system prompt")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let native = trae_solo_messages(&messages);
        assert_eq!(native.len(), 2);
        assert_eq!(native[0]["role"], "system");
        assert_eq!(native[0]["content"][0]["type"], "text");
        assert_eq!(native[0]["content"][0]["text"], "system prompt");
        assert_eq!(native[1]["role"], "user");

        let request = ChatCompletionRequest {
            model: "deepseek-v4.1-flash".to_string(),
            messages,
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(true),
            stop: None,
            max_tokens: Some(512),
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: Default::default(),
        };
        let body = trae_solo_body(&request, "deepseek-v4.1-flash");
        assert_eq!(body["config_name"], "deepseek-v4.1-flash");
        assert_eq!(body["model"], "deepseek-v4.1-flash");
        assert_eq!(body["function"], "solo_work_lite");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["request_id"], body["session_id"]);
        assert!(!body["request_id"].as_str().unwrap_or_default().is_empty());
    }

    #[test]
    fn trae_solo_body_forwards_native_tool_definitions() {
        let request = ChatCompletionRequest {
            model: "deepseek-v4.1-flash".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(json!("list files")),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(true),
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: Some(vec![json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            })]),
            tool_choice: Some(json!({"type": "function", "function": {"name": "read_file"}})),
            parallel_tool_calls: Some(false),
            extra: Default::default(),
        };
        let body = trae_solo_body(&request, "deepseek-v4.1-flash");
        let tools = body["tools"].as_array().expect("tools forwarded");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read_file");
        // The upstream wants the schema as a JSON string, not an object.
        assert!(tools[0]["function"]["parameters"].is_string());
        let schema: serde_json::Value =
            serde_json::from_str(tools[0]["function"]["parameters"].as_str().unwrap())
                .expect("schema stays valid json");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(body["tool_choice"], "read_file");
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn trae_solo_translator_streams_plain_deltas() {
        let mut translator = TraeSoloTranslator::new("deepseek-v4.1-flash");
        let chunks = translator.push("output", r#"{"response":"","reasoning_content":"think"}"#);
        assert_eq!(chunks.len(), 2, "role chunk plus the delta");
        let delta = &chunks[1].as_ref().unwrap().choices[0].delta;
        assert_eq!(delta.reasoning_content.as_deref(), Some("think"));
        assert!(delta.content.is_none());

        let chunks = translator.push("output", r#"{"response":"hello"}"#);
        let delta = &chunks[0].as_ref().unwrap().choices[0].delta;
        assert_eq!(delta.content.as_deref(), Some("hello"));

        translator.push(
            "token_usage",
            r#"{"prompt_tokens":24045,"completion_tokens":7,"cache_read_input_tokens":23936}"#,
        );
        translator.push("done", r#"{"finish_reason":"stop"}"#);
        let chunks = translator.finish();
        let last = chunks.last().unwrap().as_ref().unwrap();
        assert_eq!(last.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = last.usage.as_ref().expect("usage must be attached");
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .expect("cache split present")
                .cached_tokens,
            23936
        );
    }

    #[test]
    fn trae_solo_translator_merges_split_tool_calls() {
        // The upstream streams one call in two events: id+name first, then the
        // arguments, both under the same index.
        let mut translator = TraeSoloTranslator::new("deepseek-v4.1-flash");
        translator.push(
            "output",
            r#"{"response":"","reasoning_content":null,"tool_calls":[{"index":0,"id":"call_1","type":"function","function_call":{"name":"get_weather","arguments":""}}]}"#,
        );
        let chunks = translator.push(
            "output",
            r#"{"response":"","reasoning_content":null,"tool_calls":[{"index":0,"id":"","type":"","function_call":{"name":"","arguments":"{\"city\":\"beijing\"}"}}]}"#,
        );
        assert_eq!(chunks.len(), 1);
        let call = &chunks[0].as_ref().unwrap().choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.index, 0, "both fragments belong to the same call");
        assert_eq!(call.id.as_deref(), None, "an empty id must not overwrite the real one");
        let function = call.function.as_ref().unwrap();
        assert_eq!(function.name.as_deref(), None, "an empty name must not be resent");
        assert_eq!(function.arguments.as_deref(), Some("{\"city\":\"beijing\"}"));
    }

    #[test]
    fn trae_upstream_mode_follows_the_environment() {
        let _env = UPSTREAM_MODE_ENV
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // The fast SOLO route is the default.
        std::env::remove_var("TRAE_UPSTREAM_MODE");
        super::set_trae_upstream_mode(None);
        assert_eq!(trae_upstream_mode(), TraeUpstream::Solo);

        std::env::set_var("TRAE_UPSTREAM_MODE", "remote");
        assert_eq!(trae_upstream_mode(), TraeUpstream::Remote);

        std::env::set_var("TRAE_UPSTREAM_MODE", "AGENT");
        assert_eq!(trae_upstream_mode(), TraeUpstream::Remote);

        std::env::set_var("TRAE_UPSTREAM_MODE", "solo");
        assert_eq!(trae_upstream_mode(), TraeUpstream::Solo);

        std::env::remove_var("TRAE_UPSTREAM_MODE");
        super::set_trae_upstream_mode(None);
    }

    #[test]
    fn trae_upstream_mode_prefers_the_runtime_override() {
        let _env = UPSTREAM_MODE_ENV
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::env::remove_var("TRAE_UPSTREAM_MODE");
        super::set_trae_upstream_mode(None);

        // Without a panel choice the environment variable decides.
        std::env::set_var("TRAE_UPSTREAM_MODE", "remote");
        assert_eq!(trae_upstream_mode(), TraeUpstream::Remote);

        // A panel choice wins over the environment variable.
        super::set_trae_upstream_mode(Some(TraeUpstream::Solo));
        assert_eq!(trae_upstream_mode(), TraeUpstream::Solo);

        // Clearing it hands control back to the environment variable.
        super::set_trae_upstream_mode(None);
        assert_eq!(trae_upstream_mode(), TraeUpstream::Remote);

        std::env::remove_var("TRAE_UPSTREAM_MODE");
        super::set_trae_upstream_mode(None);
    }

    #[test]
    fn trae_upstream_names_round_trip_and_unknown_values_are_rejected() {
        assert_eq!(TraeUpstream::parse("solo"), Some(TraeUpstream::Solo));
        assert_eq!(TraeUpstream::parse(" REMOTE "), Some(TraeUpstream::Remote));
        assert_eq!(TraeUpstream::parse("agent"), Some(TraeUpstream::Remote));
        assert_eq!(TraeUpstream::parse("direct"), Some(TraeUpstream::Solo));
        assert_eq!(TraeUpstream::parse("turbo"), None);
        assert_eq!(TraeUpstream::parse(""), None);
        assert_eq!(TraeUpstream::Solo.as_str(), "solo");
        assert_eq!(TraeUpstream::Remote.as_str(), "remote");
    }

    #[test]
    fn the_web_parallel_limit_defaults_to_two_and_follows_the_environment() {
        let _env = PARALLEL_LIMIT_ENV
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        std::env::remove_var("TRAE_WEB_PARALLEL_LIMIT");
        assert_eq!(trae_web_parallel_limit(), 2);

        std::env::set_var("TRAE_WEB_PARALLEL_LIMIT", "4");
        assert_eq!(trae_web_parallel_limit(), 4);

        // A nonsensical value falls back to the upstream's own limit.
        std::env::set_var("TRAE_WEB_PARALLEL_LIMIT", "0");
        assert_eq!(trae_web_parallel_limit(), 2);
        std::env::set_var("TRAE_WEB_PARALLEL_LIMIT", "many");
        assert_eq!(trae_web_parallel_limit(), 2);

        std::env::remove_var("TRAE_WEB_PARALLEL_LIMIT");
    }

    #[test]
    fn the_parallel_limit_is_recognized_as_a_traffic_condition() {
        let body = r#"{"code":991502,"error":{"reason":"solo_agent_parallel_limit","limit":2,"running":2}}"#;
        assert!(trae_is_parallel_limit(body));
        // Other 429 reasons - a real quota limit - stay account level.
        assert!(!trae_is_parallel_limit(
            r#"{"code":6004,"msg":"您的使用量已超出频率限制"}"#
        ));
    }

    #[test]
    fn a_failed_stream_says_what_happened_and_not_just_that_the_body_broke() {
        // reqwest reports every incomplete body the same way, so the gateway
        // has to add the context itself: which route died, how much of the
        // answer made it through, and how long the gateway waited.
        let broken = EventStreamError::<reqwest::Error>::Utf8(
            String::from_utf8(vec![0xff, 0xfe]).expect_err("invalid utf-8"),
        );
        let message = trae_stream_failure(
            "SOLO",
            &broken,
            std::time::Duration::from_millis(3_400),
            12,
        );
        assert!(message.contains("SOLO"), "{}", message);
        assert!(message.contains("12 chunk(s)"), "{}", message);
        assert!(message.contains("3.4s"), "{}", message);
        assert!(
            message.contains("not valid UTF-8"),
            "the reason must name the actual fault: {}",
            message
        );
        assert!(
            !message.contains("error decoding response body"),
            "the old message said nothing about the cause: {}",
            message
        );
    }

    #[tokio::test]
    async fn the_web_slot_gate_keeps_an_account_inside_the_upstream_limit() {
        let dir = std::env::temp_dir().join(format!("mg-trae-slots-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let provider = TraeProvider::new(std::sync::Arc::new(mg_core::AccountPool::new(dir)));

        let first = provider
            .acquire_web_slot("account-1")
            .await
            .expect("first slot");
        let second = provider
            .acquire_web_slot("account-1")
            .await
            .expect("second slot");

        // The third caller has to wait: handing it a slot would push the
        // account past the upstream's own limit.
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            provider.acquire_web_slot("account-1"),
        )
        .await;
        assert!(
            waiting.is_err(),
            "the third caller must wait for a free slot instead of taking one"
        );

        // Another account is not affected by this account's slots.
        let other = provider
            .acquire_web_slot("account-2")
            .await
            .expect("other account has its own slots");
        drop(other);

        drop(first);
        let third = provider
            .acquire_web_slot("account-1")
            .await
            .expect("a released slot is reusable");
        drop(second);
        drop(third);
    }
}
