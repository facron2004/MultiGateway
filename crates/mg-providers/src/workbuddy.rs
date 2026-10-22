use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use md5::{Digest, Md5};
use mg_core::account::AccountQuota;
use mg_core::error::{MgError, Result};
use mg_core::AccountPool;
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, ModelEntry, ToolCall,
};
use reqwest::Client;
use tracing::{info, warn};

use crate::catalog::{parse_model_entries, representative_account};
use crate::traits::{BoxStream, Provider};

/// Upstream risk-control code meaning "this request looked like it came from an
/// unapproved channel"; the same payload succeeds when resent, which is what the
/// upstream's own display message asks for ("Request blocked. Please send it again").
const WORKBUDDY_REQUEST_BLOCKED_CODE: i64 = 11128;
/// DeepSeek-family upstreams retire the conversation on this code
/// (`tool_call_sequence_broken`): every retry replays the same history.
const WORKBUDDY_TOOL_MISMATCH_CODE: i64 = 11148;
const WORKBUDDY_BLOCK_ATTEMPTS: u32 = 3;
const WORKBUDDY_TEXT_REWRITES: &[(&str, &str)] = &[
    (
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.",
        "You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant.",
    ),
    (
        "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
        "To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
    ),
    (
        "You are an AI agent powered by DeepSeek Harness",
        "You are an AI agent powered by the DeepSeek Harness runtime",
    ),
    (
        "You are interacting with the user through the DeepSeek Harness Web GUI",
        "You assist the user through the DeepSeek Harness Web GUI",
    ),
    ("11128", "11-128"),
];
const WORKBUDDY_TEXT_FEATURES: &[&str] = &[
    "x-anthropic-billing-header",
    "cc_entrypoint=",
    "You are Claude Code",
    "Main branch (",
    "You are a coding agent running in the Codex CLI",
    "You are an AI agent powered by DeepSeek Harness",
    "You are interacting with the user through the DeepSeek Harness Web GUI",
    "github.com/anthropics/",
    "11128",
];

const WORKBUDDY_INTL_FALLBACK: &[(&str, &str)] = &[
    ("deepseek-v4.1-flash", "Deepseek-V4.1-Flash"),
    ("gpt-6-astra", "GPT-6-Astra"),
    ("hy4-preview-f", "Hy4 preview"),
    ("hy4-preview", "Hy4 preview"),
    ("hy3", "Hy3"),
    ("gpt-5.6-sol", "GPT-5.6-Sol"),
    ("gpt-5.6-terra", "GPT-5.6-Terra"),
    ("gpt-5.6-luna", "GPT-5.6-Luna"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.4", "GPT-5.4"),
    ("grok-4.7", "Grok-4.7"),
    ("gpt-5.3-codex", "GPT-5.3-Codex"),
    ("gemini-3.5-flash", "Gemini-3.5-Flash"),
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.2", "GLM-5.2"),
    ("kimi-k3", "Kimi-K3"),
    ("kimi-k2.8-preview", "Kimi-K2.8-Preview"),
    ("kimi-k2.6", "Kimi-K2.6"),
];

const WORKBUDDY_CN_FALLBACK: &[(&str, &str)] = &[
    ("hy3", "Hy3"),
    ("hy3-x", "Hy3"),
    ("hy4-preview", "Hy4 preview"),
    ("hy4-preview-dev", "Hy4 preview"),
    ("hy4-preview-f", "Hy4 preview"),
    ("hy4-preview-x", "Hy4 preview"),
    ("minimax-m2.5", "MiniMax-M2.5"),
    ("glm-5v-turbo", "GLM-5v-Turbo"),
    ("glm-5.3", "GLM-5.3"),
    ("glm-5.3-flash", "GLM-5.3-Flash"),
    ("glm-5.2", "GLM-5.2"),
    ("glm-5.1", "GLM-5.1"),
    ("glm-5.0-turbo", "GLM-5.0-Turbo"),
    ("glm-4.6v", "GLM-4.6V"),
    ("kimi-k3-1", "Kimi-K3"),
    ("kimi-k2.8-preview", "Kimi-K2.8-Preview"),
    ("kimi-k2.7", "Kimi-K2.7-Code"),
    ("kimi-k2.6", "Kimi-K2.6"),
    ("kimi-k2.5", "Kimi-K2.5"),
    ("kimi-k2-thinking", "Kimi-K2-Thinking"),
    ("minimax-m3", "MiniMax-M3"),
    ("minimax-m2.7", "MiniMax-M2.7"),
    ("glm-4.6", "GLM-4.6"),
    ("deepseek-v4-flash", "Deepseek-V4-Flash"),
    ("deepseek-v4.1-flash", "Deepseek-V4.1-Flash"),
    ("deepseek-v4-pro", "Deepseek-V4-Pro"),
    ("deepseek-v3-2-volc", "DeepSeek-V3.2"),
    ("deepseek-v3-1-volc", "DeepSeek-V3-1-Terminus"),
    ("deepseek-v3-1-lkeap", "DeepSeek-V3-1"),
    ("deepseek-v3-1", "DeepSeek-V3.1"),
    ("deepseek-v3-0324-lkeap", "DeepSeek-V3-0324"),
    ("deepseek-r1-0528-lkeap", "DeepSeek-R1-0528"),
    ("kimi-k2-instruct-taiji", "Kimi-K2"),
    ("default-1.1", "Claude-3.7-Sonnet"),
    ("deepseek-r1-0528", "deepseek-r1"),
    ("deepseek-v3-0324", "deepseek-v3"),
    ("default-1.2", "Claude-4.0-Sonnet"),
    ("hunyuan-2.0-instruct", "Hunyuan-2.0-Instruct"),
    ("hunyuan-chat", "Hunyuan-Turbos"),
];

fn workbuddy_fallback_models(region: &str) -> Vec<ModelEntry> {
    let catalog = if region == "intl" {
        WORKBUDDY_INTL_FALLBACK
    } else {
        WORKBUDDY_CN_FALLBACK
    };
    catalog
        .iter()
        .map(|(id, display_name)| {
            let mut extra = serde_json::Map::new();
            extra.insert("display_name".to_string(), serde_json::json!(display_name));
            ModelEntry {
                id: (*id).to_string(),
                object: "model".to_string(),
                owned_by: "workbuddy".to_string(),
                created: None,
                permission: None,
                capabilities: None,
                credits: None,
                extra,
            }
        })
        .collect()
}

fn is_workbuddy_chat_model(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    !id.is_empty()
        && !matches!(
            id.as_str(),
            "lite"
                | "auto"
                | "default-model"
                | "fast-model"
                | "balanced-model"
                | "primary-model"
                | "deep-model"
        )
        && !id.starts_with("codewise-")
        && !id.starts_with("completion-")
        && !id.ends_with("-image-alpha")
        && !id.ends_with("-image-alpha-edit")
        && !id.ends_with("-taco-completion")
        && !id.ends_with("-sg")
        && !id.ends_with("-x")
}

pub struct WorkBuddyProvider {
    /// Client for short non-streaming calls (catalog fetch, quota summary,
    /// checkin, credential refresh). Keeps a total timeout so a trickling
    /// response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
    intl_model_path: Option<PathBuf>,
    cn_model_path: Option<PathBuf>,
    legacy_model_path: Option<PathBuf>,
}

impl WorkBuddyProvider {
    pub fn new(
        pool: Arc<AccountPool>,
        intl_model_path: Option<PathBuf>,
        cn_model_path: Option<PathBuf>,
        legacy_model_path: Option<PathBuf>,
    ) -> Self {
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

        Self {
            client,
            stream_client,
            pool,
            intl_model_path,
            cn_model_path,
            legacy_model_path,
        }
    }

    pub fn pool(&self) -> &Arc<AccountPool> {
        &self.pool
    }
}

fn read_product_catalog(path: &PathBuf) -> Option<Vec<ModelEntry>> {
    let content = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let models = parse_workbuddy_catalog(&value);
    (!models.is_empty()).then_some(models)
}

fn find_product_model_list(value: &serde_json::Value) -> Option<&serde_json::Value> {
    if let Some(models) = value.get("models").filter(|models| {
        models.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("id").and_then(serde_json::Value::as_str).is_some())
        })
    }) {
        return Some(models);
    }
    value
        .as_object()?
        .values()
        .find_map(find_product_model_list)
}

fn parse_workbuddy_catalog(value: &serde_json::Value) -> Vec<ModelEntry> {
    let mut cli_lists = Vec::<Vec<String>>::new();
    let mut other_lists = Vec::<Vec<String>>::new();
    let mut roots = vec![value];
    if let Some(data) = value.get("data").filter(|data| data.is_object()) {
        roots.push(data);
    }

    for root in roots {
        let Some(agents) = root.get("agents") else {
            continue;
        };
        let entries: Vec<(String, &serde_json::Value)> = match agents {
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(name, entry)| (name.clone(), entry))
                .collect(),
            serde_json::Value::Array(items) => items
                .iter()
                .map(|entry| {
                    (
                        entry
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry,
                    )
                })
                .collect(),
            _ => continue,
        };

        for (name, entry) in entries {
            let Some(models) = entry.get("models").and_then(serde_json::Value::as_array) else {
                continue;
            };
            let ids: Vec<String> = models
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect();
            if ids.is_empty() {
                continue;
            }
            if name.eq_ignore_ascii_case("cli") {
                cli_lists.push(ids);
            } else {
                other_lists.push(ids);
            }
        }
    }

    let mut ids = cli_lists
        .into_iter()
        .max_by_key(|ids| ids.len())
        .or_else(|| other_lists.into_iter().max_by_key(|ids| ids.len()))
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| is_workbuddy_chat_model(id) && seen.insert(id.clone()));

    if ids.is_empty() {
        return find_product_model_list(value)
            .map(|models| parse_model_entries(models, "workbuddy"))
            .unwrap_or_default()
            .into_iter()
            .filter(|model| is_workbuddy_chat_model(&model.id))
            .collect();
    }

    let metadata: HashMap<_, _> = parse_model_entries(value, "workbuddy")
        .into_iter()
        .map(|model| (model.id.clone(), model))
        .collect();
    ids.into_iter()
        .map(|id| {
            let mut model = metadata.get(&id).cloned().unwrap_or_else(|| ModelEntry {
                id,
                object: "model".to_string(),
                owned_by: "workbuddy".to_string(),
                created: None,
                permission: None,
                capabilities: None,
                credits: None,
                extra: serde_json::Map::new(),
            });
            let display_name = model
                .extra
                .get("display_name")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    model
                        .extra
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or(&model.id)
                .to_string();
            model
                .extra
                .insert("display_name".to_string(), serde_json::json!(display_name));
            model
        })
        .collect()
}

fn workbuddy_region(region: Option<&str>) -> Option<&'static str> {
    match region
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("cn" | "china") => Some("cn"),
        Some("intl" | "global") => Some("intl"),
        _ => None,
    }
}

fn workbuddy_number(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn workbuddy_enterprise_id(account: &mg_core::Account) -> Option<String> {
    account
        .organization_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            account
                .extra
                .get("enterprise_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            account
                .provider_specific
                .as_ref()?
                .get("enterprise_id")?
                .as_str()
        })
        .map(str::to_string)
        .or_else(|| {
            use base64::Engine;
            let token_body = account.access_token.split('.').nth(1)?;
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(token_body)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(token_body))
                .ok()?;
            let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
            claims
                .get("enterprise_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
}

fn workbuddy_token_expiry(token: &str) -> Option<f64> {
    use base64::Engine;
    let token_body = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token_body)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(token_body))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let expiry = claims.get("exp")?.as_f64()?;
    (expiry > 0.0).then_some(expiry)
}

fn parse_workbuddy_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {
    if payload.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        return Err(MgError::provider(
            502,
            format!(
                "WorkBuddy quota query failed: {}",
                payload
                    .get("msg")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream business error")
            ),
            "upstream_error",
        ));
    }
    let data = payload
        .get("data")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            MgError::provider(502, "WorkBuddy returned no quota data", "invalid_response")
        })?;
    let packages = data
        .get("Packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            MgError::provider(
                502,
                "WorkBuddy returned no package list",
                "invalid_response",
            )
        })?;
    let mut remaining = 0.0;
    let mut used = 0.0;
    let mut limit = 0.0;
    let mut buckets = Vec::new();
    for package in packages {
        let package_remaining = workbuddy_number(
            package
                .get("CycleRemainCapacity")
                .or_else(|| package.get("Remain")),
        );
        let package_used = workbuddy_number(
            package
                .get("CycleUsedCapacity")
                .or_else(|| package.get("Used")),
        );
        let package_limit = workbuddy_number(
            package
                .get("CycleTotalCapacity")
                .or_else(|| package.get("Size")),
        );
        if package_remaining.is_none() && package_used.is_none() && package_limit.is_none() {
            continue;
        }
        let package_remaining = package_remaining.unwrap_or(0.0);
        let package_used = package_used.unwrap_or(0.0);
        let package_limit = package_limit.unwrap_or(package_remaining + package_used);
        remaining += package_remaining;
        used += package_used;
        limit += package_limit;
        buckets.push(serde_json::json!({
            "label": package.get("PackageName").or_else(|| package.get("Name")).and_then(serde_json::Value::as_str).unwrap_or("额度包"),
            "remaining": package_remaining,
            "used": package_used,
            "limit": package_limit,
            "unit": "积分"
        }));
    }
    if buckets.is_empty() {
        return Err(MgError::provider(
            502,
            "WorkBuddy returned no parseable package quotas",
            "invalid_response",
        ));
    }
    Ok(AccountQuota {
        checked_at,
        remaining: Some(remaining),
        limit: Some(limit),
        reset_at: None,
        details: serde_json::json!({"label": "WorkBuddy 积分", "unit": "积分", "used": used, "buckets": buckets}),
    })
}

fn parse_workbuddy_enterprise_quota(
    payload: &serde_json::Value,
    checked_at: f64,
) -> Result<AccountQuota> {
    if payload.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        return Err(MgError::provider(
            502,
            format!(
                "WorkBuddy enterprise quota query failed: {}",
                payload
                    .get("msg")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("upstream business error")
            ),
            "upstream_error",
        ));
    }
    let data = payload
        .get("data")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            MgError::provider(
                502,
                "WorkBuddy returned no enterprise quota data",
                "invalid_response",
            )
        })?;
    let used = workbuddy_number(data.get("credit")).ok_or_else(|| {
        MgError::provider(
            502,
            "WorkBuddy enterprise response has no usage",
            "invalid_response",
        )
    })?;
    let limit = workbuddy_number(data.get("limitNum")).ok_or_else(|| {
        MgError::provider(
            502,
            "WorkBuddy enterprise response has no quota limit",
            "invalid_response",
        )
    })?;
    let remaining = (limit - used).max(0.0);
    let reset_at = data
        .get("cycleEndTime")
        .or_else(|| data.get("cycleResetTime"))
        .cloned();
    Ok(AccountQuota {
        checked_at,
        remaining: Some(remaining),
        limit: Some(limit),
        reset_at: None,
        details: serde_json::json!({
            "label": "企业周期额度",
            "unit": "积分",
            "used": used,
            "is_enterprise": true,
            "buckets": [{"label": "企业周期额度", "remaining": remaining, "used": used, "limit": limit, "unit": "积分", "reset_at": reset_at}]
        }),
    })
}

fn workbuddy_upstream_code(body: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("code")?
        .as_i64()
}

fn workbuddy_catalog_url(region: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    if region == "cn" {
        (
            "https://copilot.tencent.com/v3/config",
            "https://www.codebuddy.cn",
            "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1",
            "WorkBuddy/5.5.6",
        )
    } else {
        (
            "https://www.workbuddy.ai/v3/config",
            "https://www.workbuddy.ai",
            "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2",
            "WorkBuddy/5.5.2",
        )
    }
}

fn replace_ascii_case_insensitive(value: &str, needle: &str, replacement: &str) -> String {
    let lower_value = value.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(value.len());
    let mut copied_until = 0;
    for (start, _) in lower_value.match_indices(&lower_needle) {
        if start < copied_until {
            continue;
        }
        output.push_str(&value[copied_until..start]);
        output.push_str(replacement);
        copied_until = start + lower_needle.len();
    }
    output.push_str(&value[copied_until..]);
    output
}

fn has_workbuddy_text_fingerprint(value: &str) -> bool {
    if WORKBUDDY_TEXT_FEATURES
        .iter()
        .any(|feature| value.contains(feature))
    {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    lower.contains("x-anthropic-billing-header")
        || lower.contains("sisyphus-junior - focused executor from ohmyopencode")
}

fn remove_workbuddy_billing_header(value: &str) -> String {
    const NEEDLE: &str = "x-anthropic-billing-header:";
    let lower = value.to_ascii_lowercase();
    let mut output = String::with_capacity(value.len());
    let mut copied_until = 0;
    let mut search_from = 0;

    while let Some(relative_start) = lower[search_from..].find(NEEDLE) {
        let start = search_from + relative_start;
        output.push_str(&value[copied_until..start]);
        let mut end = start + NEEDLE.len();
        while let Some(ch) = value[end..].chars().next() {
            if matches!(ch, ';' | '\r' | '\n') {
                break;
            }
            end += ch.len_utf8();
        }
        if value[end..].starts_with(';') {
            end += 1;
        }
        while let Some(ch) = value[end..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            end += ch.len_utf8();
        }
        copied_until = end;
        search_from = end;
    }

    output.push_str(&value[copied_until..]);
    output
}

fn remove_workbuddy_cc_metadata(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut copied_until = 0;
    let mut search_from = 0;

    while let Some(relative_start) = lower[search_from..].find("cc_") {
        let start = search_from + relative_start;
        let has_word_boundary = value[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        if !has_word_boundary {
            search_from = start + "cc_".len();
            continue;
        }

        let mut equals = start + "cc_".len();
        while bytes
            .get(equals)
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        {
            equals += 1;
        }
        if equals == start + "cc_".len() || bytes.get(equals) != Some(&b'=') {
            search_from = start + "cc_".len();
            continue;
        }

        output.push_str(&value[copied_until..start]);
        let mut end = equals + 1;
        while let Some(ch) = value[end..].chars().next() {
            if matches!(ch, ';' | '\r' | '\n') {
                break;
            }
            end += ch.len_utf8();
        }
        if value[end..].starts_with(';') {
            end += 1;
        }
        while let Some(ch) = value[end..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            end += ch.len_utf8();
        }
        copied_until = end;
        search_from = end;
    }

    output.push_str(&value[copied_until..]);
    output
}

fn sanitize_workbuddy_text(value: &str) -> String {
    if !has_workbuddy_text_fingerprint(value) {
        return value.to_string();
    }

    let mut sanitized = replace_ascii_case_insensitive(
        value,
        "Sisyphus-Junior - Focused executor from OhMyOpenCode",
        "Sisyphus-Junior - Focused executor",
    );
    for (pattern, replacement) in WORKBUDDY_TEXT_REWRITES {
        sanitized = sanitized.replace(pattern, replacement);
    }
    sanitized = remove_workbuddy_billing_header(&sanitized);
    if sanitized.contains("cc_") {
        sanitized = remove_workbuddy_cc_metadata(&sanitized);
    }
    sanitized = replace_ascii_case_insensitive(
        &sanitized,
        "x-anthropic-billing-header",
        "x-anthropic-billing-hdr",
    );
    sanitized.trim().to_string()
}

fn sanitize_workbuddy_content(content: &mut serde_json::Value) {
    match content {
        serde_json::Value::String(text) => *text = sanitize_workbuddy_text(text),
        serde_json::Value::Array(parts) => {
            for part in parts {
                if part.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(serde_json::Value::String(text)) = part.get_mut("text") {
                    *text = sanitize_workbuddy_text(text);
                }
            }
        }
        _ => {}
    }
}

fn empty_message_content(content: &Option<serde_json::Value>) -> bool {
    match content {
        None => true,
        Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(text)) => text.is_empty(),
        Some(serde_json::Value::Array(parts)) => parts.is_empty(),
        Some(_) => false,
    }
}

fn append_unique_calls(dst: &mut Vec<ToolCall>, extra: Vec<ToolCall>) {
    let mut seen: HashSet<String> = dst
        .iter()
        .filter(|call| !call.id.is_empty())
        .map(|call| call.id.clone())
        .collect();
    for call in extra {
        if !call.id.is_empty() && !seen.insert(call.id.clone()) {
            continue;
        }
        dst.push(call);
    }
}

/// Merge the split tool-call batches DeepSeek rejects with code 11148.
///
/// The upstream requires one assistant message per batch, immediately followed
/// by exactly the matching `role:"tool"` results and nothing else. Agent
/// clients (Responses `function_call` items, in particular) replay a parallel
/// batch as several adjacent assistant messages, and Codex inserts notices
/// between the results. Both shapes are the same calls and the same results;
/// this only restores the adjacency the upstream checks.
fn repair_workbuddy_tool_pairing(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if !messages.iter().any(|message| {
        message.role == "tool"
            || message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty())
    }) {
        return messages;
    }
    let (messages, merged) = merge_adjacent_tool_calls(messages);
    let (messages, repacked) = repack_tool_result_blocks(messages);
    let (messages, cleaned) = cleanup_orphan_tool_calls(messages);
    if merged || repacked || cleaned {
        info!(
            provider = "workbuddy",
            merged, repacked, cleaned, "repaired tool-call pairing before forwarding to WorkBuddy"
        );
    }
    messages
}

fn merge_adjacent_tool_calls(messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, bool) {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut changed = false;
    for message in messages {
        if message.role == "assistant" {
            if let Some(prev) = out.last_mut() {
                if prev.role == "assistant" {
                    let prev_calls = prev
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty());
                    let this_calls = message
                        .tool_calls
                        .as_ref()
                        .is_some_and(|calls| !calls.is_empty());
                    // Trailing declaration with no content joins the open batch.
                    if this_calls && empty_message_content(&message.content) && prev_calls {
                        let extra = message.tool_calls.unwrap_or_default();
                        append_unique_calls(prev.tool_calls.as_mut().unwrap(), extra);
                        changed = true;
                        continue;
                    }
                    // Responses replay: assistant text, then the function_call items.
                    if this_calls && empty_message_content(&message.content) && !prev_calls {
                        prev.tool_calls = message.tool_calls;
                        changed = true;
                        continue;
                    }
                    // The same split batch in the other order: text after the calls.
                    if !this_calls
                        && prev_calls
                        && empty_message_content(&prev.content)
                        && matches!(&message.content, Some(serde_json::Value::String(text)) if !text.is_empty())
                    {
                        prev.content = message.content;
                        changed = true;
                        continue;
                    }
                }
            }
        }
        out.push(message);
    }
    (out, changed)
}

fn repack_tool_result_blocks(messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, bool) {
    let mut out = Vec::with_capacity(messages.len());
    let mut changed = false;
    let mut rest = messages.into_iter().peekable();
    while let Some(message) = rest.next() {
        let call_ids: Vec<String> = message
            .tool_calls
            .as_ref()
            .filter(|_| message.role == "assistant")
            .map(|calls| {
                calls
                    .iter()
                    .filter(|call| !call.id.is_empty())
                    .map(|call| call.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if call_ids.is_empty() {
            out.push(message);
            continue;
        }
        let want: HashSet<&str> = call_ids.iter().map(String::as_str).collect();
        // A result that arrived before its declaration still belongs to it.
        let mut early = Vec::new();
        while out.last().is_some_and(|prev| {
            prev.role == "tool"
                && prev
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| want.contains(id))
        }) {
            early.push(out.pop().unwrap());
            changed = true;
        }
        early.reverse();

        out.push(message);
        let mut results = early;
        let mut between = Vec::new();
        while let Some(next) = rest.peek() {
            if next.role == "assistant"
                && next
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
            {
                break;
            }
            if next.role == "tool" {
                let id = next.tool_call_id.as_deref().unwrap_or("");
                if !want.contains(id) {
                    break;
                }
                if !between.is_empty() {
                    changed = true;
                }
                results.push(rest.next().unwrap());
                continue;
            }
            between.push(rest.next().unwrap());
        }
        out.extend(results);
        out.extend(between);
    }
    (out, changed)
}

fn cleanup_orphan_tool_calls(messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, bool) {
    let mut call_ids = HashSet::new();
    let mut result_ids = HashSet::new();
    for message in &messages {
        if message.role == "tool" {
            if let Some(id) = message.tool_call_id.as_ref() {
                if !id.is_empty() {
                    result_ids.insert(id.clone());
                }
            }
        } else if message.role == "assistant" {
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    if !call.id.is_empty() {
                        call_ids.insert(call.id.clone());
                    }
                }
            }
        }
    }
    if call_ids.is_empty() && result_ids.is_empty() {
        return (messages, false);
    }
    let keep: HashSet<String> = call_ids.intersection(&result_ids).cloned().collect();
    let mut changed = false;
    let mut seen_results = HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    for mut message in messages {
        if message.role == "tool" {
            let id = message.tool_call_id.clone().unwrap_or_default();
            if !keep.contains(&id) || !seen_results.insert(id) {
                changed = true;
                continue;
            }
            out.push(message);
            continue;
        }
        if message.role == "assistant" {
            if let Some(calls) = message.tool_calls.take() {
                let original = calls.len();
                if original > 0 {
                    let mut seen = HashSet::new();
                    let kept: Vec<ToolCall> = calls
                        .into_iter()
                        .filter(|call| {
                            !call.id.is_empty()
                                && keep.contains(&call.id)
                                && seen.insert(call.id.clone())
                        })
                        .collect();
                    if kept.len() != original {
                        changed = true;
                    }
                    if kept.is_empty() {
                        if empty_message_content(&message.content) {
                            continue;
                        }
                    } else {
                        message.tool_calls = Some(kept);
                    }
                }
            }
        }
        out.push(message);
    }
    (out, changed)
}

fn tool_pairing_summary(messages: &[ChatMessage]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        if message.role == "assistant" {
            if let Some(calls) = &message.tool_calls {
                let ids: Vec<&str> = calls
                    .iter()
                    .filter(|call| !call.id.is_empty())
                    .map(|call| call.id.as_str())
                    .collect();
                if !ids.is_empty() {
                    parts.push(format!("assistant[{}]", ids.join(",")));
                    continue;
                }
            }
        }
        if message.role == "tool" {
            parts.push(format!(
                "tool:{}",
                message.tool_call_id.as_deref().unwrap_or("")
            ));
            continue;
        }
        parts.push(message.role.clone());
    }
    let summary = parts.join(" ");
    let max = 500;
    if summary.chars().count() <= max {
        summary
    } else {
        summary.chars().take(max).collect::<String>() + "…"
    }
}

fn normalize_workbuddy_request(req: &mut ChatCompletionRequest) {
    // WorkBuddy only serves SSE; the handlers collect it for JSON responses.
    req.stream = Some(true);
    for message in &mut req.messages {
        if message.role == "developer" {
            message.role = "system".to_string();
        }
        if let Some(content) = message.content.as_mut() {
            sanitize_workbuddy_content(content);
        }
        if let Some(tool_calls) = message.tool_calls.as_mut() {
            for tool_call in tool_calls {
                tool_call.function.arguments =
                    sanitize_workbuddy_text(&tool_call.function.arguments);
            }
        }
    }
    req.messages = repair_workbuddy_tool_pairing(std::mem::take(&mut req.messages));
}

fn workbuddy_fingerprint_id(uid: &str, salt: &str) -> String {
    let uid = if uid.is_empty() { "anonymous" } else { uid };
    let seed = format!("{salt}:{uid}");
    Md5::digest(seed.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn with_workbuddy_desktop_identity(
    request: reqwest::RequestBuilder,
    uid: &str,
    is_cn: bool,
) -> reqwest::RequestBuilder {
    let (domain, version, user_agent, language) = if is_cn {
        (
            "copilot.tencent.com",
            "5.5.6",
            "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1",
            "zh-CN",
        )
    } else {
        (
            "www.workbuddy.ai",
            "5.5.2",
            "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2",
            "en-US",
        )
    };

    request
        .header("X-Agent-Purpose", "conversation")
        .header("X-IDE-Name", "WorkBuddy")
        .header("X-IDE-Type", "WorkBuddy")
        .header("X-IDE-Version", version)
        .header("X-Product", "WorkBuddy")
        .header("X-Domain", domain)
        .header("User-Agent", user_agent)
        .header("X-User-Id", uid)
        .header("X-Request-ID", uuid::Uuid::new_v4().simple().to_string())
        .header("X-Machine-ID", workbuddy_fingerprint_id(uid, "machine"))
        .header("X-Session-ID", workbuddy_fingerprint_id(uid, "session"))
        .header("X-CodeBuddy-Request", "1")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Accept-Language", language)
}

#[async_trait]
impl Provider for WorkBuddyProvider {
    fn id(&self) -> &'static str {
        "workbuddy"
    }

    fn name(&self) -> &'static str {
        "WorkBuddy"
    }

    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({
            "credentials": true,
            "credits": true,
            "checkin": true,
            "regions": true,
            "import": false
        })
    }

    fn active_region(&self) -> Option<String> {
        let region = self.pool.active_region();
        Some(workbuddy_region(Some(&region)).unwrap_or("cn").to_string())
    }

    async fn set_region(&self, region: &str) -> Result<serde_json::Value> {
        let region = workbuddy_region(Some(region))
            .ok_or_else(|| MgError::bad_request("Unsupported WorkBuddy region"))?;
        self.pool.set_active_region(region)?;
        Ok(serde_json::json!({ "ok": true, "region": region }))
    }

    async fn models(&self) -> Result<Vec<ModelEntry>> {
        self.models_for_region(None).await
    }

    async fn models_for_region(&self, region: Option<&str>) -> Result<Vec<ModelEntry>> {
        let active_region = self.active_region();
        let selected_region = region
            .and_then(|value| workbuddy_region(Some(value)))
            .or_else(|| {
                active_region
                    .as_deref()
                    .and_then(|value| workbuddy_region(Some(value)))
            })
            .unwrap_or("cn");
        let account = representative_account(&self.pool, Some(selected_region));
        let fallback = workbuddy_fallback_models(selected_region);
        let mut models = Vec::new();
        if let Some(account) = account {
            let realm = selected_region;
            let (url, origin, chat_ua, billing_ua) = workbuddy_catalog_url(realm);
            for user_agent in [chat_ua, billing_ua] {
                let response = match self
                    .client
                    .get(url)
                    .timeout(Duration::from_secs(10))
                    .bearer_auth(&account.access_token)
                    .header("Accept", "application/json, text/plain, */*")
                    .header("User-Agent", user_agent)
                    .header("Origin", origin)
                    .header("Referer", format!("{}/", origin))
                    .header("X-User-Id", &account.id)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => response,
                    _ => continue,
                };
                let Ok(data) = response.json::<serde_json::Value>().await else {
                    continue;
                };
                models = parse_workbuddy_catalog(&data);
                if !models.is_empty() {
                    break;
                }
            }
        }
        if !models.is_empty() {
            return Ok(models);
        }

        let local_paths: Vec<Option<&PathBuf>> = match selected_region {
            "cn" => vec![self.cn_model_path.as_ref(), self.legacy_model_path.as_ref()],
            "intl" => vec![
                self.intl_model_path.as_ref(),
                self.legacy_model_path.as_ref(),
            ],
            _ => vec![
                self.intl_model_path.as_ref(),
                self.cn_model_path.as_ref(),
                self.legacy_model_path.as_ref(),
            ],
        };
        for path in local_paths.into_iter().flatten() {
            // Sync file IO must not block the async worker thread.
            let path = path.clone();
            let models = tokio::task::spawn_blocking(move || read_product_catalog(&path))
                .await
                .ok()
                .flatten();
            if let Some(models) = models {
                if !models.is_empty() {
                    return Ok(models);
                }
            }
        }
        Ok(fallback)
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
        let mut req = req;
        normalize_workbuddy_request(&mut req);

        let region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let account = self
            .pool
            .choose_in_region(session_id, None, Some(&region))?;

        let account_id = account.id.clone();
        let is_cn = mg_core::account::regions_match(&account.region, "cn");
        let url = if is_cn {
            "https://copilot.tencent.com/v2/chat/completions"
        } else {
            "https://www.workbuddy.ai/v2/chat/completions"
        };

        let origin = if is_cn {
            "https://www.codebuddy.cn"
        } else {
            "https://www.workbuddy.ai"
        };

        info!(
            provider = "workbuddy",
            model = %req.model,
            account = %account_id,
            stream = req.stream,
            "Forwarding chat completion to WorkBuddy"
        );

        let mut attempt: u32 = 0;

        let upstream_res = loop {
            attempt += 1;
            let request_builder = with_workbuddy_desktop_identity(
                self.stream_client
                    .post(url)
                    .header("Authorization", format!("Bearer {}", account.access_token))
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/plain, */*")
                    .header("Origin", origin)
                    .header("Referer", format!("{}/", origin)),
                &account.id,
                is_cn,
            )
            .json(&req);

            let response = request_builder.send().await.map_err(|e| {
                MgError::provider(
                    502,
                    format!("WorkBuddy connection failed: {}", e),
                    "network_error",
                )
            })?;

            let status = response.status();
            if status.is_success() {
                break response;
            }

            let err_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown upstream error".to_string());
            if workbuddy_upstream_code(&err_text) == Some(WORKBUDDY_REQUEST_BLOCKED_CODE)
                && attempt < WORKBUDDY_BLOCK_ATTEMPTS
            {
                warn!(
                    provider = "workbuddy",
                    model = %req.model,
                    account = %account_id,
                    attempt,
                    "WorkBuddy blocked the request as an unapproved channel; resending"
                );
                tokio::time::sleep(Duration::from_millis(300u64.saturating_mul(attempt as u64)))
                    .await;
                continue;
            }

            if workbuddy_upstream_code(&err_text) == Some(WORKBUDDY_TOOL_MISMATCH_CODE) {
                warn!(
                    provider = "workbuddy",
                    model = %req.model,
                    account = %account_id,
                    pairing = %tool_pairing_summary(&req.messages),
                    "WorkBuddy rejected tool-call pairing"
                );
            }

            if crate::is_account_level_failure(status.as_u16()) {
                self.pool
                    .mark_cooldown(&account_id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(&account_id, &format!("upstream HTTP {}", status.as_u16()));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "WorkBuddy request failed: {}",
                    crate::upstream_error_body(status.as_u16(), &err_text)
                ),
                "upstream_error",
            ));
        };

        let stream_req_id: Arc<str> =
            Arc::from(format!("chatcmpl-wb-{}", uuid::Uuid::new_v4().simple()).as_str());

        let s = upstream_res
            .bytes_stream()
            .eventsource()
            .filter_map(move |event_res| {
                let cid = Arc::clone(&stream_req_id);
                async move {
                    match event_res {
                        Ok(event) => {
                            if event.data == "[DONE]" {
                                return None;
                            }
                            if let Ok(mut chunk) =
                                serde_json::from_str::<ChatCompletionChunk>(&event.data)
                            {
                                chunk.id = cid.to_string();
                                return Some(Ok(chunk));
                            }
                            None
                        }
                        Err(e) => Some(Err(MgError::provider(
                            502,
                            format!("Stream read error: {}", e),
                            "upstream_error",
                        ))),
                    }
                }
            });

        Ok((Box::pin(s), account_id))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let mut account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        if account.expires_at <= mg_core::now_secs() + 300.0 && !account.refresh_token.is_empty() {
            self.refresh_credentials(account_id).await?;
            account = self
                .pool
                .get(account_id)
                .ok_or_else(|| MgError::not_found("Account not found"))?;
        }
        let region = if mg_core::account::regions_match(&account.region, "cn") {
            "cn"
        } else {
            "intl"
        };
        let (base, user_agent) = if region == "cn" {
            ("https://www.codebuddy.cn", "WorkBuddy/5.5.6")
        } else {
            ("https://www.workbuddy.ai", "WorkBuddy/5.5.2")
        };
        if account.access_token.is_empty() {
            return Err(MgError::unauthorized(
                "WorkBuddy account has no access token",
            ));
        }
        let enterprise_id = workbuddy_enterprise_id(&account);
        let usage_path = if enterprise_id.is_some() {
            "/billing/meter/get-enterprise-user-usage"
        } else {
            "/billing/meter/get-user-resource-summary"
        };

        let response = self
            .client
            .post(format!("{base}{usage_path}"))
            .bearer_auth(&account.access_token)
            .header("User-Agent", user_agent)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .header("Origin", base)
            .header("Referer", format!("{base}/"))
            .header("X-User-Id", &account.id)
            .header("X-Client-Platform", "web")
            .body("{}");
        let response = if let Some(enterprise_id) = enterprise_id {
            response
                .header("X-Enterprise-Id", enterprise_id)
                .send()
                .await
        } else {
            response.send().await
        }
        .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "WorkBuddy quota query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = if usage_path.ends_with("get-enterprise-user-usage") {
            parse_workbuddy_enterprise_quota(&payload, mg_core::now_secs())?
        } else {
            parse_workbuddy_quota(&payload, mg_core::now_secs())?
        };
        self.pool.update_quota(account_id, quota.clone())?;
        Ok(quota)
    }

    async fn checkin(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;

        if !mg_core::account::regions_match(&account.region, "cn") {
            return Err(MgError::bad_request(
                "WorkBuddy check-in is only available for CN accounts",
            ));
        }
        if workbuddy_enterprise_id(&account).is_some() {
            return Err(MgError::bad_request(
                "WorkBuddy check-in is not available for enterprise accounts",
            ));
        }

        if account.access_token.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "message": "账号无有效 Token"
            }));
        }

        let resp = self
            .client
            .post("https://www.codebuddy.cn/v2/billing/meter/daily-checkin")
            .header("Authorization", format!("Bearer {}", account.access_token))
            .header("User-Agent", "WorkBuddy/5.5.6")
            .header("Content-Type", "application/json")
            .header("Origin", "https://www.codebuddy.cn")
            .body("{}")
            .send()
            .await;

        match resp {
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(MgError::provider(
                        status.as_u16(),
                        format!(
                            "WorkBuddy check-in failed: {}",
                            crate::upstream_error_body(status.as_u16(), &text)
                        ),
                        "upstream_error",
                    ));
                }
                let v: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                    MgError::provider(502, error.to_string(), "invalid_response")
                })?;
                let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("未知响应");
                if code == 0 || code == 10001 {
                    let mut updated = account.clone();
                    if let Some(mut credits) = updated.credits.clone() {
                        if let Some(checkin) = credits.get_mut("checkin") {
                            if let Some(obj) = checkin.as_object_mut() {
                                obj.insert(
                                    "today_checked_in".to_string(),
                                    serde_json::Value::Bool(true),
                                );
                            }
                        }
                        updated.credits = Some(credits);
                        let pool = Arc::clone(&self.pool);
                        tokio::task::spawn_blocking(move || pool.save(updated))
                            .await
                            .map_err(|error| MgError::Io(error.to_string()))??;
                    }
                    Ok(serde_json::json!({
                        "ok": true,
                        "code": code,
                        "message": if code == 10001 { "今日已完成签到" } else { "签到成功" }
                    }))
                } else {
                    Ok(serde_json::json!({
                        "ok": false,
                        "code": code,
                        "message": format!("签到失败: {} ({})", msg, status)
                    }))
                }
            }
            Err(error) => Err(MgError::provider(502, error.to_string(), "network_error")),
        }
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let now = mg_core::now_secs();
        if account.refresh_token.is_empty() {
            if !account.access_token.is_empty() && account.expires_at > now + 300.0 {
                return Ok(serde_json::json!({ "ok": true, "message": "当前凭证仍有效" }));
            }
            return Err(MgError::unauthorized(
                "WorkBuddy account has no refresh token",
            ));
        }

        let is_cn = mg_core::account::regions_match(&account.region, "cn");
        let (chat_base, user_agent) = if is_cn {
            ("https://copilot.tencent.com", "WorkBuddy/5.5.6")
        } else {
            ("https://www.workbuddy.ai", "WorkBuddy/5.5.2")
        };
        let origin = if is_cn {
            "https://www.codebuddy.cn"
        } else {
            "https://www.workbuddy.ai"
        };
        let mut request = self
            .client
            .post(format!("{chat_base}/v2/plugin/auth/token/refresh"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("User-Agent", user_agent)
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"))
            .header("X-Refresh-Token", &account.refresh_token)
            .header(
                "X-Auth-Refresh-Source",
                if is_cn { "workbuddy" } else { "plugin" },
            )
            .header("X-User-Id", &account.id)
            .header("X-CodeBuddy-Request", "1")
            .header("Accept-Language", if is_cn { "zh-CN" } else { "en-US" })
            .json(&serde_json::json!({}));
        let domain = account
            .extra
            .get("domain")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(if is_cn {
                "codebuddy.cn"
            } else {
                "workbuddy.ai"
            });
        request = request.header("X-Domain", domain);
        if let Some(enterprise_id) = workbuddy_enterprise_id(&account) {
            request = request.header("X-Enterprise-Id", enterprise_id);
        }
        let response = request
            .send()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "WorkBuddy credential refresh failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let data = payload
            .get("data")
            .and_then(|value| value.get("data"))
            .or_else(|| payload.get("data"))
            .unwrap_or(&payload);
        let access_token = data
            .get("accessToken")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                MgError::provider(
                    502,
                    "WorkBuddy refresh response has no access token",
                    "invalid_response",
                )
            })?;
        let mut updated = account;
        updated.access_token = access_token.to_string();
        if let Some(refresh_token) = data.get("refreshToken").and_then(serde_json::Value::as_str) {
            updated.refresh_token = refresh_token.to_string();
        }
        updated.expires_at = workbuddy_token_expiry(access_token)
            .or_else(|| {
                data.get("expires_in")
                    .or_else(|| data.get("expiresIn"))
                    .and_then(serde_json::Value::as_f64)
                    .map(|seconds| mg_core::now_secs() + seconds)
            })
            .unwrap_or(updated.expires_at);
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || pool.save(updated))
            .await
            .map_err(|error| MgError::Io(error.to_string()))??;
        Ok(serde_json::json!({ "ok": true, "message": "WorkBuddy 凭证已刷新" }))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "status": "manual",
            "message": "请在本地账号池配置文件添加凭据"
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
        normalize_workbuddy_request, parse_workbuddy_catalog, parse_workbuddy_enterprise_quota,
        parse_workbuddy_quota, read_product_catalog, with_workbuddy_desktop_identity,
        workbuddy_fallback_models, workbuddy_fingerprint_id, workbuddy_upstream_code,
        ChatCompletionRequest, WORKBUDDY_REQUEST_BLOCKED_CODE,
    };
    use std::fs;

    #[test]
    fn derives_stable_fingerprint_ids_compatible_with_python() {
        assert_eq!(
            workbuddy_fingerprint_id("test-user", "machine"),
            "938da53a8d2a0fced0f3313dccee1ed9"
        );
        assert_eq!(
            workbuddy_fingerprint_id("test-user", "session"),
            "d9f45ea5c42595421aad7876734327ba"
        );
        assert_eq!(
            workbuddy_fingerprint_id("", "machine"),
            workbuddy_fingerprint_id("anonymous", "machine")
        );
    }

    #[test]
    fn sends_the_default_workbuddy_desktop_identity_for_each_region() {
        let client = reqwest::Client::new();
        let cn = with_workbuddy_desktop_identity(
            client.post("https://copilot.tencent.com/v2/chat/completions"),
            "test-user",
            true,
        )
        .build()
        .unwrap();
        let cn_headers = cn.headers();
        assert_eq!(cn_headers["x-product"], "WorkBuddy");
        assert_eq!(cn_headers["x-domain"], "copilot.tencent.com");
        assert_eq!(cn_headers["x-ide-type"], "WorkBuddy");
        assert_eq!(
            cn_headers["user-agent"],
            "WorkBuddy/5.5.6 WorkBuddy/5.5.6 CLI/2.137.1"
        );
        assert_eq!(
            cn_headers["x-machine-id"],
            "938da53a8d2a0fced0f3313dccee1ed9"
        );
        assert_eq!(
            cn_headers["x-session-id"],
            "d9f45ea5c42595421aad7876734327ba"
        );
        assert_eq!(cn_headers["accept-language"], "zh-CN");
        assert_eq!(cn_headers["x-request-id"].to_str().unwrap().len(), 32);

        let intl = with_workbuddy_desktop_identity(
            client.post("https://www.workbuddy.ai/v2/chat/completions"),
            "test-user",
            false,
        )
        .build()
        .unwrap();
        let intl_headers = intl.headers();
        assert_eq!(intl_headers["x-domain"], "www.workbuddy.ai");
        assert_eq!(
            intl_headers["user-agent"],
            "WorkBuddy/5.5.2 WorkBuddy AI/5.5.2 CLI/5.5.2"
        );
        assert_eq!(intl_headers["accept-language"], "en-US");
    }

    #[test]
    fn normalizes_roles_and_sanitizes_known_waf_fingerprints() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "hy3",
            "messages": [
                {"role": "developer", "content": "You are a coding agent running in the Codex CLI, a terminal-based coding assistant. Literal 11128."},
                {"role": "user", "content": [{"type": "text", "text": "sisyphus-junior - focused executor from ohmyopencode"}]},
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "run", "arguments": "Main branch (you will usually use this for PRs)"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "system", "content": "X-ANTHROPIC-BILLING-HEADER: legacy; cc_entrypoint=ClaudeCode; To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues"}
            ]
        }))
        .unwrap();

        normalize_workbuddy_request(&mut request);

        assert_eq!(
            request
                .messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "user", "assistant", "tool", "system"]
        );
        assert_eq!(
            request.messages[0].content,
            Some(serde_json::json!("You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant. Literal 11-128."))
        );
        assert_eq!(
            request.messages[1].content,
            Some(serde_json::json!([{"type":"text","text":"Sisyphus-Junior - Focused executor"}]))
        );
        assert_eq!(
            request.messages[2].tool_calls.as_ref().unwrap()[0]
                .function
                .arguments,
            "Default branch (you will usually use this for PRs)"
        );
        assert_eq!(
            request.messages[4].content,
            Some(serde_json::json!("To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues"))
        );
    }

    #[test]
    fn repairs_split_tool_batches_the_upstream_rejects() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "user", "content": "run both"},
                {"role": "assistant", "content": "running both", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}}
                ]},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {"name": "b", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "one"},
                {"role": "developer", "content": "image resized"},
                {"role": "tool", "tool_call_id": "call_2", "content": "two"},
                {"role": "tool", "tool_call_id": "call_stray", "content": "no such call"},
                {"role": "user", "content": "thanks"}
            ]
        }))
        .unwrap();

        normalize_workbuddy_request(&mut request);

        assert_eq!(
            request
                .messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "tool", "tool", "system", "user"]
        );
        let calls = request.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_1", "call_2"]
        );
        assert_eq!(
            request.messages[1].content,
            Some(serde_json::json!("running both"))
        );
        assert_eq!(request.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(request.messages[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(
            request.messages[4].content,
            Some(serde_json::json!("image resized"))
        );
    }

    #[test]
    fn drops_unanswered_tool_calls_so_the_conversation_can_continue() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "user", "content": "try"},
                {"role": "assistant", "content": "partial", "tool_calls": [
                    {"id": "call_ok", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                    {"id": "call_missing", "type": "function", "function": {"name": "b", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_ok", "content": "done"},
                {"role": "assistant", "tool_calls": [
                    {"id": "call_never", "type": "function", "function": {"name": "c", "arguments": "{}"}}
                ]},
                {"role": "user", "content": "continue"}
            ]
        }))
        .unwrap();

        normalize_workbuddy_request(&mut request);

        assert_eq!(
            request
                .messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "tool", "user"]
        );
        let calls = request.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_ok");
        assert_eq!(
            request.messages[1].content,
            Some(serde_json::json!("partial"))
        );
    }

    #[test]
    fn leaves_an_already_paired_tool_batch_unchanged() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "checking", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "user", "content": "thanks"}
            ]
        }))
        .unwrap();
        let before = serde_json::to_value(&request.messages).unwrap();

        normalize_workbuddy_request(&mut request);

        assert_eq!(serde_json::to_value(&request.messages).unwrap(), before);
    }

    #[test]
    fn requests_upstream_streaming_regardless_of_client_response_format() {
        for stream in [None, Some(false), Some(true)] {
            let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
                "model": "deepseek-v4.1-flash",
                "messages": [{"role": "user", "content": "Reply only OK."}],
                "stream": stream,
            }))
            .unwrap();

            normalize_workbuddy_request(&mut request);

            assert_eq!(request.stream, Some(true));
        }
    }

    #[test]
    fn sanitizes_confirmed_deepseek_harness_identity_fingerprints() {
        let mut request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "system", "content": "You are an AI agent powered by DeepSeek Harness.\nYou are interacting with the user through the DeepSeek Harness Web GUI whose URL the runtime context names."},
                {"role": "user", "content": "Keep the DeepSeek Harness name and my project instructions."}
            ]
        }))
        .unwrap();

        normalize_workbuddy_request(&mut request);

        assert_eq!(
            request.messages[0].content,
            Some(serde_json::json!("You are an AI agent powered by the DeepSeek Harness runtime.\nYou assist the user through the DeepSeek Harness Web GUI whose URL the runtime context names."))
        );
        assert_eq!(
            request.messages[1].content,
            Some(serde_json::json!(
                "Keep the DeepSeek Harness name and my project instructions."
            ))
        );
    }

    #[test]
    fn extracts_the_upstream_business_code_only_from_json_error_bodies() {
        assert_eq!(
            workbuddy_upstream_code(
                r#"{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}"#
            ),
            Some(WORKBUDDY_REQUEST_BLOCKED_CODE)
        );
        assert_eq!(
            workbuddy_upstream_code("<html><body>400 Bad Request</body></html>"),
            None
        );
        // A stringified code is not the numeric business field the gate expects.
        assert_eq!(workbuddy_upstream_code(r#"{"code":"11128"}"#), None);
    }

    #[test]
    fn sums_workbuddy_cycle_capacity_fields_and_rejects_failed_upstream_code() {
        let quota = parse_workbuddy_quota(
            &serde_json::json!({"code":0,"data":{"Packages":[
                {"PackageName":"Base","CycleRemainCapacity":"60","CycleUsedCapacity":40,"CycleTotalCapacity":100},
                {"Name":"Bonus","Remain":15,"Used":5,"Size":20}
            ]}}),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(75.0));
        assert_eq!(quota.limit, Some(120.0));
        assert!(
            parse_workbuddy_quota(&serde_json::json!({"code":1,"msg":"failed"}), 12.0).is_err()
        );
    }

    #[test]
    fn parses_workbuddy_enterprise_cycle_quota() {
        let quota = parse_workbuddy_enterprise_quota(
            &serde_json::json!({
                "code": 0,
                "data": {"credit": "35.5", "limitNum": 100, "cycleEndTime": "2026-11-01T00:00:00Z"}
            }),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(64.5));
        assert_eq!(quota.limit, Some(100.0));
        assert_eq!(
            quota.details["buckets"][0]["reset_at"],
            "2026-11-01T00:00:00Z"
        );
    }

    #[test]
    fn reads_nested_desktop_product_catalog_instead_of_object_keys() {
        let path =
            std::env::temp_dir().join(format!("mg-rust-models-{}.json", uuid::Uuid::new_v4()));
        fs::write(
            &path,
            r#"{"product":{"available":{"models":[{"id":"live-model","supportsImages":true},{"id":"live-model-2"}]}}}"#,
        )
        .unwrap();

        let models = read_product_catalog(&path).unwrap();
        let _ = fs::remove_file(path);
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["live-model", "live-model-2"]
        );
    }

    #[test]
    fn parses_workbuddy_v3_config_agent_model_ids_and_cli_priority() {
        let payload = serde_json::json!({
            "data": {
                "agents": {
                    "cli": {"models": ["cli-model", "shared-model", "default-model", "auto", "codewise-inline", "test-x", "work-model"]},
                    "work": {"models": ["work-model", "shared-model", "extra-model", "another-model"]}
                }
            },
            "models": [
                {"id": "cli-model", "display_name": "CLI Model", "credits": "x0.00"},
                {"id": "shared-model", "name": "Shared Model Label", "context_window": 200000},
                {"id": "work-model", "display_name": "Work Model"}
            ]
        });

        let models = parse_workbuddy_catalog(&payload);
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["cli-model", "shared-model", "work-model"]
        );
        assert_eq!(models[0].extra["display_name"], "CLI Model");
        assert_eq!(models[1].extra["display_name"], "Shared Model Label");
        assert_eq!(models[1].extra["context_window"], 200000);
    }

    #[test]
    fn fallback_catalogs_follow_the_selected_realm_and_include_display_names() {
        let cn = workbuddy_fallback_models("cn");
        let intl = workbuddy_fallback_models("intl");

        assert!(cn.iter().any(|model| model.id == "hy3-x"));
        assert!(!cn.iter().any(|model| model.id == "gpt-6-astra"));
        assert!(intl.iter().any(|model| model.id == "gpt-6-astra"));
        assert_eq!(intl[1].extra["display_name"], "GPT-6-Astra");
    }
}
