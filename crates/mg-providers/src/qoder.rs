use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes128;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use md5::{Digest, Md5};
use mg_core::account::AccountQuota;
use mg_core::error::{MgError, Result};
use mg_core::AccountPool;
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta, ModelEntry,
};
use parking_lot::RwLock;
use rand::rngs::OsRng;
use reqwest::Client;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use sha2::Sha512;
use tracing::info;
use url::Url;

use crate::catalog::{fallback_models, representative_account};
use crate::traits::{BoxStream, Provider};

const QODER_MODEL_PATH: &str = "/algo/api/v2/model/list?Encode=1";
const QODER_CHAT_PATH: &str = "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1";
const QODER_BASEPROMPT: &str = include_str!("../assets/qoder_baseprompt.json");
const QODER_DEFAULT_USER_TYPE: &str = "personal_professional_trial";
const QODER_QUOTA_PATH: &str = "/api/v2/quota/usage";
const QODER_CHECKIN_STATUS_PATH: &str = "/sash/api/v1/me/daily-check-in/status";
const QODER_CHECKIN_CLAIM_PATH: &str = "/sash/api/v1/me/daily-check-in/claim";
const QODER_DEVICE_REFRESH_PATH: &str = "/api/v1/deviceToken/refresh";
const QODER_JOB_REFRESH_PATH: &str = "/api/v1/jobToken/refresh";
const QODER_JOB_EXCHANGE_PATH: &str = "/api/v1/jobToken/exchange";
const QODER_COSY_VERSION: &str = "1.1.64";
const QODER_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc\n4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l\n6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17\nXcW+ML9FoCI6AOvOzwIDAQAB\n-----END PUBLIC KEY-----";
const QODER_CUSTOM_ALPHABET: &str =
    "_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const QODER_STANDARD_ALPHABET: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const QODER_INTL_FALLBACK: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("ultimate", "Ultimate"),
    ("performance", "Performance"),
    ("efficient", "Efficient"),
    ("smodel", "Sonus"),
    ("cmodel", "Cantus"),
    ("qmodel_38max", "Qwen3.8-Max"),
    ("qfmodel", "Qwen3.8-Flash"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel", "Qwen3.7-Plus"),
    ("kmodel_latest", "Kimi-K3"),
    ("kmodel", "Kimi-K2.8-Preview"),
    ("gmodel", "GLM-5.3"),
    ("gfmodel", "GLM-5.3-Flash"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("dfmodel", "DeepSeek-Flash"),
    ("mmodel", "MiniMax-M3"),
];
const QODER_CN_FALLBACK: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("qmodel_38max", "Qwen3.8-Max"),
    ("qfmodel", "Qwen3.8-Flash"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel", "Qwen3.7-Plus"),
    ("q37fmodel", "Qwen3.7-Flash"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("dfmodel", "DeepSeek-Flash"),
    ("gmodel", "GLM-5.3"),
    ("gfmodel", "GLM-5.3-Flash"),
    ("gm51model", "GLM-5.2"),
    ("kmodel_latest", "Kimi-K3"),
    ("kmodel", "Kimi-K2.8-Preview"),
    ("mmodel", "MiniMax-M2.7"),
];

fn qoder_fallback_models(region: &str) -> Vec<ModelEntry> {
    let catalog = if region == "intl" {
        QODER_INTL_FALLBACK
    } else {
        QODER_CN_FALLBACK
    };
    let ids: Vec<_> = catalog.iter().map(|(id, _)| *id).collect();
    let models = fallback_models(&ids, "qoder")
        .into_iter()
        .zip(catalog)
        .map(|(mut model, (upstream_key, display_name))| {
            model.id = display_name.to_string();
            model
                .extra
                .insert("name".to_string(), serde_json::json!(display_name));
            model
                .extra
                .insert("display_name".to_string(), serde_json::json!(display_name));
            model
                .extra
                .insert("upstream_key".to_string(), serde_json::json!(upstream_key));
            model
        })
        .collect();
    enrich_qoder_models(models, region)
}

fn qoder_model_key_map(models: &[ModelEntry]) -> HashMap<String, String> {
    let mut model_keys = HashMap::new();
    for model in models {
        let upstream_key = model
            .extra
            .get("upstream_key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&model.id)
            .trim();
        for alias in [
            Some(model.id.as_str()),
            model.extra.get("name").and_then(serde_json::Value::as_str),
            model
                .extra
                .get("display_name")
                .and_then(serde_json::Value::as_str),
            Some(upstream_key),
        ]
        .into_iter()
        .flatten()
        {
            let alias = alias.trim();
            if !alias.is_empty() {
                model_keys.insert(alias.to_ascii_lowercase(), upstream_key.to_string());
            }
        }
    }
    model_keys
}

fn canonical_qoder_model_key(model: &str, model_keys: &HashMap<String, String>) -> String {
    model_keys
        .get(&model.trim().to_ascii_lowercase())
        .cloned()
        .unwrap_or_else(|| model.to_string())
}

fn qoder_text_content(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| part.get("content").and_then(serde_json::Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(serde_json::Value::Object(object)) => object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn qoder_image_urls(content: Option<&serde_json::Value>, into: &mut Vec<String>) {
    let Some(serde_json::Value::Array(parts)) = content else {
        return;
    };
    for part in parts {
        let kind = part
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !matches!(kind, "image_url" | "input_image" | "image") {
            continue;
        }
        let url = part
            .pointer("/image_url/url")
            .or_else(|| part.get("image_url"))
            .or_else(|| part.get("url"))
            .and_then(serde_json::Value::as_str);
        if let Some(url) = url.filter(|url| !url.is_empty()) {
            into.push(url.to_string());
        }
    }
}

struct QoderConversation {
    system: Option<String>,
    messages: Vec<serde_json::Value>,
    prompt: String,
    images: Vec<String>,
}

fn qoder_conversation(req: &ChatCompletionRequest) -> QoderConversation {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    let mut images = Vec::new();
    let mut prompt = String::new();
    for message in &req.messages {
        let text = qoder_text_content(message.content.as_ref());
        qoder_image_urls(message.content.as_ref(), &mut images);
        match message.role.as_str() {
            "system" => {
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "tool" => {
                let identifier = message.tool_call_id.as_deref().unwrap_or("unknown");
                let name = message.name.as_deref().unwrap_or("tool");
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!("[Tool Result: {identifier} {name}]\n{text}")
                }));
            }
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
                if !content.is_empty() {
                    messages.push(serde_json::json!({"role": "assistant", "content": content}));
                }
            }
            _ => {
                if text.is_empty() {
                    continue;
                }
                prompt = text.clone();
                messages.push(serde_json::json!({"role": "user", "content": text}));
            }
        }
    }
    QoderConversation {
        system: (!system_parts.is_empty()).then(|| system_parts.join("\n\n")),
        messages,
        prompt,
        images,
    }
}

fn qoder_requested_effort(req: &ChatCompletionRequest) -> Option<String> {
    let extra = &req.extra;
    let value = extra
        .get("reasoning_effort")
        .or_else(|| extra.get("reasoning").and_then(|value| value.get("effort")))
        .or_else(|| {
            extra
                .get("thinking")
                .and_then(|value| value.get("effort").or_else(|| value.get("level")))
        })?;
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn qoder_chat_body(
    req: &ChatCompletionRequest,
    account: &mg_core::Account,
    model_key: &str,
    display_name: &str,
) -> Result<String> {
    let mut body: serde_json::Value = serde_json::from_str(QODER_BASEPROMPT).map_err(|error| {
        MgError::provider(
            500,
            format!("Qoder base prompt is invalid: {error}"),
            "invalid_response",
        )
    })?;
    let conversation = qoder_conversation(req);
    let template_system = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message.get("role").and_then(serde_json::Value::as_str) == Some("system")
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut messages = Vec::new();
    match &conversation.system {
        Some(system) => messages.push(serde_json::json!({"role": "system", "content": system})),
        None => messages.extend(template_system),
    }
    messages.extend(conversation.messages.iter().cloned());
    body["messages"] = serde_json::Value::Array(messages);
    body["request_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    body["chat_record_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    body["request_set_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    body["session_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    body["stream"] = serde_json::Value::Bool(true);
    body["agent_id"] = serde_json::json!("agent_common");
    body["aliyun_user_type"] = serde_json::json!(account
        .user_type
        .clone()
        .unwrap_or_else(|| QODER_DEFAULT_USER_TYPE.to_string()));

    let is_vl = !conversation.images.is_empty();
    let model_config = serde_json::json!({
        "key": model_key,
        "display_name": display_name,
        "model": "",
        "format": "openai",
        "is_vl": is_vl,
        "is_reasoning": false,
        "api_key": "",
        "url": "",
        "source": "system",
        "max_input_tokens": 180000
    });
    body["model_config"] = model_config.clone();

    let mut parameters = body
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(max_tokens) = req.max_tokens.or(req.max_completion_tokens) {
        parameters["max_tokens"] = serde_json::json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        parameters["temperature"] = serde_json::json!(temperature);
    }
    if let Some(top_p) = req.top_p {
        parameters["top_p"] = serde_json::json!(top_p);
    }
    if let Some(effort) = qoder_requested_effort(req) {
        parameters["reasoning_effort"] = serde_json::json!(effort);
    }
    body["parameters"] = parameters;
    body["tools"] = serde_json::json!(req.tools.clone().unwrap_or_default());

    let mut business = body
        .get("business")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let product = business
        .get("product")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cli")
        .to_string();
    let business_type = business
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("agent")
        .to_string();
    let stage = business
        .get("stage")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("start")
        .to_string();
    let name: String = conversation.prompt.chars().take(30).collect();
    business["id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    business["begin_at"] = serde_json::json!((mg_core::now_secs() * 1000.0) as i64);
    business["name"] = serde_json::json!(if name.is_empty() {
        "chat".to_string()
    } else {
        name
    });
    business["product"] = serde_json::json!(product);
    business["type"] = serde_json::json!(business_type);
    business["stage"] = serde_json::json!(stage);
    body["business"] = business;

    let mut chat_context = body
        .get("chat_context")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    chat_context["text"] = serde_json::json!({"type": "text", "text": conversation.prompt});
    chat_context["extra"]["originalContent"] =
        serde_json::json!({"type": "text", "text": conversation.prompt});
    chat_context["extra"]["modelConfig"] = model_config;
    if !conversation.images.is_empty() {
        chat_context["imageUrls"] = serde_json::json!(conversation.images);
        body["image_urls"] = serde_json::json!(conversation.images);
    }
    body["chat_context"] = chat_context;

    let plain = serde_json::to_vec(&body).map_err(|error| MgError::Json(error.to_string()))?;
    Ok(qoder_encode(&plain))
}

fn qoder_inner_error(value: &serde_json::Value) -> Option<String> {
    if let Some(error) = value.get("error") {
        if let Some(message) = error.get("message").and_then(serde_json::Value::as_str) {
            return Some(message.to_string());
        }
        if let Some(text) = error.as_str() {
            return Some(text.to_string());
        }
        if error.is_object() {
            return Some(error.to_string());
        }
    }
    let code = value.get("code").and_then(serde_json::Value::as_i64);
    if code.is_some_and(|code| code != 0 && code != 200) {
        if let Some(message) = value.get("message").and_then(serde_json::Value::as_str) {
            return Some(message.to_string());
        }
    }
    None
}

fn qoder_chunk(
    value: serde_json::Value,
    fallback_model: &str,
    stream_id: &str,
) -> Result<Option<ChatCompletionChunk>> {
    if let Some(error) = qoder_inner_error(&value) {
        return Err(MgError::provider(
            502,
            format!("Qoder upstream error: {error}"),
            "upstream_error",
        ));
    }
    if let Ok(mut chunk) = serde_json::from_value::<ChatCompletionChunk>(value.clone()) {
        if chunk.id.is_empty() {
            chunk.id = stream_id.to_string();
        }
        if chunk.object.is_empty() {
            chunk.object = "chat.completion.chunk".to_string();
        }
        if chunk.model.is_empty() {
            chunk.model = fallback_model.to_string();
        }
        return Ok(Some(chunk));
    }
    let choices = value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usage = value
        .get("usage")
        .and_then(|usage| serde_json::from_value::<ChatUsage>(usage.clone()).ok());
    if choices.is_empty() && usage.is_none() {
        return Ok(None);
    }
    let choices = choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| ChunkChoice {
            index: choice
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(index as u64) as u32,
            delta: choice
                .get("delta")
                .cloned()
                .and_then(|delta| serde_json::from_value::<ChunkDelta>(delta).ok())
                .unwrap_or_default(),
            finish_reason: choice
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
        .collect();
    Ok(Some(ChatCompletionChunk {
        id: stream_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: value
            .get("created")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| mg_core::now_secs() as u64),
        model: value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|model| !model.is_empty())
            .unwrap_or(fallback_model)
            .to_string(),
        choices,
        usage,
    }))
}

fn qoder_status_ok(value: &serde_json::Value) -> bool {
    match value.get("statusCodeValue") {
        None => true,
        Some(status) => {
            status.as_u64() == Some(200)
                || status.as_i64() == Some(200)
                || status.as_str() == Some("200")
        }
    }
}

fn qoder_stream_event(
    data: &str,
    fallback_model: &str,
    stream_id: &str,
) -> Option<Result<ChatCompletionChunk>> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let outer = serde_json::from_str::<serde_json::Value>(data).ok()?;
    if outer.get("body").is_some() || outer.get("statusCodeValue").is_some() {
        if !qoder_status_ok(&outer) {
            let detail = outer
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("upstream status");
            return Some(Err(MgError::provider(
                502,
                format!("Qoder upstream status: {detail}"),
                "upstream_error",
            )));
        }
        let body = outer.get("body")?.as_str()?;
        if body == "[DONE]" {
            return None;
        }
        let inner = match serde_json::from_str::<serde_json::Value>(body) {
            Ok(inner) => inner,
            Err(_) => return None,
        };
        return match qoder_chunk(inner, fallback_model, stream_id) {
            Ok(chunk) => chunk.map(Ok),
            Err(error) => Some(Err(error)),
        };
    }
    match qoder_chunk(outer, fallback_model, stream_id) {
        Ok(chunk) => chunk.map(Ok),
        Err(error) => Some(Err(error)),
    }
}

fn error_chain(error: &impl std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(inner) = source {
        parts.push(inner.to_string());
        source = inner.source();
    }
    parts.join(": ")
}

fn qoder_encode(plain: &[u8]) -> String {
    // 256-entry translation table from the standard base64 alphabet to the
    // Qoder custom alphabet ('=' maps to '$'), built once at compile time.
    const ENCODE_TABLE: [u8; 256] = {
        let mut table = [0u8; 256];
        let standard = QODER_STANDARD_ALPHABET.as_bytes();
        let custom = QODER_CUSTOM_ALPHABET.as_bytes();
        let mut index = 0;
        while index < standard.len() {
            table[standard[index] as usize] = custom[index];
            index += 1;
        }
        table[b'=' as usize] = b'$';
        table
    };

    let standard = STANDARD.encode(plain);
    let n = standard.len();
    let a = n / 3;
    standard[n - a..]
        .bytes()
        .chain(standard[a..n - a].bytes())
        .chain(standard[..a].bytes())
        .map(|byte| ENCODE_TABLE[byte as usize] as char)
        .collect()
}

fn aes_cbc_encrypt(plain: &[u8], key: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let cipher = Aes128::new_from_slice(key).map_err(|error| error.to_string())?;
    let padding = 16 - plain.len() % 16;
    let mut output = plain.to_vec();
    output.resize(output.len() + padding, padding as u8);
    let mut previous = [0u8; 16];
    previous.copy_from_slice(key);
    let (chunks, remainder) = output.as_chunks_mut::<16>();
    debug_assert!(remainder.is_empty());
    for chunk in chunks {
        for (byte, previous_byte) in chunk.iter_mut().zip(previous) {
            *byte ^= previous_byte;
        }
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
        previous.copy_from_slice(&block);
    }
    Ok(output)
}

fn md5_hex(value: &[u8]) -> String {
    Md5::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn qoder_identity(account: &mg_core::Account) -> BTreeMap<&'static str, String> {
    let extra = &account.extra;
    let value = |name: &str, alternate_name: &str, direct: Option<&str>, fallback: &str| {
        direct
            .filter(|value| !value.is_empty())
            .or_else(|| {
                extra
                    .get(name)
                    .or_else(|| extra.get(alternate_name))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or(fallback)
            .to_string()
    };
    let mut identity = BTreeMap::new();
    identity.insert("name", value("nickname", "name", Some(&account.label), ""));
    identity.insert("aid", account.id.clone());
    identity.insert("uid", account.id.clone());
    identity.insert("yx_uid", String::new());
    identity.insert(
        "organization_id",
        value(
            "organization_id",
            "organizationId",
            account.organization_id.as_deref(),
            "",
        ),
    );
    identity.insert(
        "organization_name",
        value(
            "organization_name",
            "organizationName",
            account.organization_name.as_deref(),
            "",
        ),
    );
    identity.insert(
        "user_type",
        value(
            "user_type",
            "userType",
            account.user_type.as_deref(),
            "personal_professional_trial",
        ),
    );
    identity.insert("security_oauth_token", account.access_token.clone());
    identity.insert("refresh_token", account.refresh_token.clone());
    identity
}

/// Cache for the parsed RSA public key (the PEM is a compile-time constant,
/// so parsing it on every models call is wasted CPU).
static QODER_RSA_PUBLIC_KEY: OnceLock<RsaPublicKey> = OnceLock::new();

fn qoder_public_key() -> std::result::Result<&'static RsaPublicKey, String> {
    if let Some(key) = QODER_RSA_PUBLIC_KEY.get() {
        return Ok(key);
    }
    let key =
        RsaPublicKey::from_public_key_pem(QODER_PUBLIC_KEY).map_err(|error| error.to_string())?;
    Ok(QODER_RSA_PUBLIC_KEY.get_or_init(|| key))
}

fn qoder_cosy_headers(
    account: &mg_core::Account,
    url: &str,
    body: &str,
) -> std::result::Result<Vec<(&'static str, String)>, String> {
    let temp_key = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    let identity = qoder_identity(account);
    let identity_json = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
    let encrypted_info = aes_cbc_encrypt(&identity_json, temp_key.as_bytes())?;
    let public_key = qoder_public_key()?;
    let encrypted_key = public_key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, temp_key.as_bytes())
        .map_err(|error| error.to_string())?;
    let cosy_key = STANDARD.encode(encrypted_key);
    let mut payload = BTreeMap::new();
    payload.insert("cosyVersion", QODER_COSY_VERSION.to_string());
    payload.insert("ideVersion", String::new());
    payload.insert("info", STANDARD.encode(encrypted_info));
    payload.insert("requestId", uuid::Uuid::new_v4().to_string());
    payload.insert("version", "v1".to_string());
    let payload_json = serde_json::to_vec(&payload).map_err(|error| error.to_string())?;
    let payload_b64 = STANDARD.encode(payload_json);
    let date = chrono::Utc::now().timestamp().to_string();
    let parsed = Url::parse(url).map_err(|error| error.to_string())?;
    let full_path = parsed.path();
    let path = full_path.strip_prefix("/algo").unwrap_or(full_path);
    let signed = format!("{payload_b64}\n{cosy_key}\n{date}\n{body}\n{path}");
    let authorization = format!("Bearer COSY.{payload_b64}.{}", md5_hex(signed.as_bytes()));
    let uid = &account.id;
    let machine_id = md5_hex(format!("machine:{uid}").as_bytes());
    let machine_type = md5_hex(format!("machinetype:{uid}").as_bytes())[..18].to_string();
    let machine_token = {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        URL_SAFE_NO_PAD.encode(Sha512::digest(format!("machinetoken:{uid}").as_bytes()))[..43]
            .to_string()
    };

    Ok(vec![
        ("cosy-data-policy", "AGREE".to_string()),
        ("content-type", "application/json".to_string()),
        ("cosy-machinetype", machine_type),
        ("cosy-clienttype", "5".to_string()),
        ("cosy-date", date),
        ("cosy-user", account.id.clone()),
        ("cosy-key", cosy_key),
        ("accept", "application/json".to_string()),
        ("cosy-clientip", "169.254.198.161".to_string()),
        ("authorization", authorization),
        ("accept-encoding", "identity".to_string()),
        ("cosy-version", QODER_COSY_VERSION.to_string()),
        ("cosy-machineid", machine_id),
        ("cosy-machinetoken", machine_token),
        ("login-version", "v2".to_string()),
        ("user-agent", "Go-http-client/2.0".to_string()),
    ])
}

fn qoder_gateway_candidates(region: &str) -> &'static [&'static str] {
    if region == "intl" {
        &[
            "https://api1.qoder.sh",
            "https://api2.qoder.sh",
            "https://api3.qoder.sh",
        ]
    } else {
        &["https://gateway.qoder.com.cn"]
    }
}

fn qoder_openapi(region: &str) -> &'static str {
    if matches!(region, "intl" | "global") {
        "https://openapi.qoder.sh"
    } else {
        "https://openapi.qoder.com.cn"
    }
}

fn qoder_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn parse_qoder_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {
    let user = payload
        .get("userQuota")
        .and_then(serde_json::Value::as_object);
    let addon = payload
        .get("addOnQuota")
        .and_then(serde_json::Value::as_object);
    if user.is_none() && addon.is_none() {
        return Err(MgError::provider(
            502,
            "Qoder returned no quota data",
            "invalid_response",
        ));
    }
    let has_usage = [user, addon].into_iter().flatten().any(|object| {
        ["remaining", "used", "total"]
            .iter()
            .any(|key| object.get(*key).and_then(qoder_number).is_some())
    });
    if !has_usage {
        return Err(MgError::provider(
            502,
            "Qoder returned empty quota buckets",
            "invalid_response",
        ));
    }
    let value = |object: Option<&serde_json::Map<String, serde_json::Value>>, key: &str| {
        object
            .and_then(|object| object.get(key))
            .and_then(qoder_number)
            .unwrap_or(0.0)
    };
    let user_remaining = value(user, "remaining");
    let user_used = value(user, "used");
    let user_limit = value(user, "total");
    let addon_remaining = value(addon, "remaining");
    let addon_used = value(addon, "used");
    let addon_limit = value(addon, "total");
    let remaining = user_remaining + addon_remaining;
    let used = user_used + addon_used;
    let limit = user_limit + addon_limit;
    Ok(AccountQuota {
        checked_at,
        remaining: Some(remaining),
        limit: Some(limit),
        reset_at: None,
        details: serde_json::json!({
            "label": "Qoder 额度",
            "unit": "额度",
            "used": used,
            "buckets": [
                {"label": "基础额度", "remaining": user_remaining, "used": user_used, "limit": user_limit, "unit": "额度"},
                {"label": "赠送/签到额度", "remaining": addon_remaining, "used": addon_used, "limit": addon_limit, "unit": "额度"}
            ]
        }),
    })
}

struct QoderRate {
    price: f64,
    original: Option<f64>,
    peak: Option<f64>,
    discount: Option<f64>,
    window_start: Option<&'static str>,
    window_end: Option<&'static str>,
    timezone: Option<&'static str>,
    context: Option<u64>,
    max_context: Option<u64>,
}

fn qoder_rate(
    price: f64,
    original: Option<f64>,
    peak: Option<f64>,
    discount: Option<f64>,
    window: Option<(&'static str, &'static str, &'static str)>,
    context: Option<u64>,
    max_context: Option<u64>,
) -> QoderRate {
    QoderRate {
        price,
        original,
        peak,
        discount,
        window_start: window.map(|value| value.0),
        window_end: window.map(|value| value.1),
        timezone: window.map(|value| value.2),
        context,
        max_context,
    }
}

fn qoder_known_rate(realm: &str, key: &str) -> Option<QoderRate> {
    let intl = realm == "intl";
    match key {
        "auto" => Some(qoder_rate(0.5, None, None, None, None, Some(200_000), None)),
        "ultimate" if intl => Some(qoder_rate(
            2.0,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "performance" if intl => Some(qoder_rate(
            1.1,
            None,
            None,
            None,
            None,
            Some(272_000),
            Some(1_000_000),
        )),
        "efficient" if intl => Some(qoder_rate(0.3, None, None, None, None, Some(200_000), None)),
        "smodel" if intl => Some(qoder_rate(
            8.0,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "cmodel" if intl => Some(qoder_rate(
            4.0,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "qmodel_38max" => Some(qoder_rate(
            0.5,
            None,
            Some(0.5),
            Some(0.4),
            Some((
                "22:00",
                "08:00",
                if intl {
                    "Asia/Singapore"
                } else {
                    "Asia/Shanghai"
                },
            )),
            Some(200_000),
            Some(1_000_000),
        )),
        "qfmodel" => Some(qoder_rate(
            0.0,
            Some(0.1),
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "qmodel_latest" => Some(qoder_rate(
            0.5,
            intl.then_some(0.5),
            Some(0.5),
            Some(0.2),
            Some((
                "22:00",
                "08:00",
                if intl {
                    "Asia/Singapore"
                } else {
                    "Asia/Shanghai"
                },
            )),
            Some(200_000),
            Some(1_000_000),
        )),
        "qmodel" => Some(qoder_rate(
            0.1,
            None,
            Some(0.1),
            Some(0.4),
            Some((
                "22:00",
                "08:00",
                if intl {
                    "Asia/Singapore"
                } else {
                    "Asia/Shanghai"
                },
            )),
            Some(200_000),
            Some(1_000_000),
        )),
        "q37fmodel" if !intl => Some(qoder_rate(
            0.1,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "dmodel" => Some(qoder_rate(
            0.5,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "dfmodel" => Some(qoder_rate(
            0.1,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "gmodel" => Some(qoder_rate(
            0.8,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "gfmodel" => Some(qoder_rate(
            0.1,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "gm51model" if !intl => Some(qoder_rate(
            0.6,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "kmodel_latest" => Some(qoder_rate(
            1.4,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "kmodel" => Some(qoder_rate(
            0.8,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "mmodel" if intl => Some(qoder_rate(
            0.2,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(1_000_000),
        )),
        "mmodel" => Some(qoder_rate(
            0.2,
            None,
            None,
            None,
            None,
            Some(200_000),
            Some(200_000),
        )),
        _ => None,
    }
}

fn qoder_json_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn minutes_of_day(value: &str) -> Option<u32> {
    let (hour, minute) = value.split_once(':')?;
    let hour = hour.parse::<u32>().ok()?;
    let minute = minute.parse::<u32>().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

fn off_peak_active_now(start: &str, end: &str, timezone: Option<&str>, now: i64) -> Option<bool> {
    let start = minutes_of_day(start)?;
    let end = minutes_of_day(end)?;
    let offset_hours = timezone
        .and_then(|timezone| {
            let upper = timezone.to_ascii_uppercase();
            upper
                .strip_prefix("UTC")
                .and_then(|value| value.parse::<i32>().ok())
        })
        .unwrap_or(8);
    let shifted = now + i64::from(offset_hours) * 3600;
    let minute = shifted.rem_euclid(86_400) / 60;
    let minute = u32::try_from(minute).ok()?;
    if start == end {
        return Some(true);
    }
    Some(if start < end {
        start <= minute && minute < end
    } else {
        minute >= start || minute < end
    })
}

fn context_from_config(config: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let Some(windows) = config.as_object() else {
        return (None, None);
    };
    let mut default = None;
    let mut smallest = None;
    let mut largest = None;
    for window in windows.values() {
        let Some(tokens) = window
            .get("token_count")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        if tokens == 0 {
            continue;
        }
        smallest = Some(smallest.unwrap_or(u64::MAX).min(tokens));
        largest = Some(largest.unwrap_or(0).max(tokens));
        if window
            .get("is_default")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            default = Some(tokens);
        }
    }
    let base = default.or(smallest.filter(|value| *value != u64::MAX));
    let max_context = largest.filter(|value| base.is_none_or(|base| *value > base));
    (base, max_context)
}

fn enrich_qoder_model(model: &mut ModelEntry, realm: &str, now: i64) {
    let key = model
        .extra
        .get("upstream_key")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&model.id)
        .to_string();
    let known = qoder_known_rate(realm, &key);
    let promotion = model.extra.get("promotion").cloned();
    let promotion = promotion.as_ref().filter(|value| value.is_object());
    let price = model
        .extra
        .get("price_factor")
        .and_then(qoder_json_number)
        .or_else(|| model.credits.as_ref().and_then(qoder_json_number))
        .or_else(|| known.as_ref().map(|rate| rate.price));
    if let Some(price) = price {
        model.credits = Some(serde_json::json!(price));
        model
            .extra
            .insert("price_factor".to_string(), serde_json::json!(price));
    }
    let original = model
        .extra
        .get("original_price_factor")
        .and_then(qoder_json_number)
        .or_else(|| known.as_ref().and_then(|rate| rate.original));
    if let Some(original) = original {
        model.extra.insert(
            "original_price_factor".to_string(),
            serde_json::json!(original),
        );
    }
    let peak = promotion
        .and_then(|value| value.get("before_promotion_price_factor"))
        .and_then(qoder_json_number)
        .or_else(|| {
            model
                .extra
                .get("price_factor_peak")
                .and_then(qoder_json_number)
        })
        .or(original)
        .or_else(|| known.as_ref().and_then(|rate| rate.peak));
    let discount = promotion
        .and_then(|value| value.get("discount_factor"))
        .and_then(qoder_json_number)
        .or_else(|| known.as_ref().and_then(|rate| rate.discount));
    let valley = peak.and_then(|peak| {
        discount
            .filter(|discount| *discount > 0.0 && *discount <= 1.0)
            .map(|discount| (peak * discount * 10_000.0).round() / 10_000.0)
    });
    if let Some(peak) = peak {
        model
            .extra
            .insert("price_factor_peak".to_string(), serde_json::json!(peak));
    }
    if let Some(valley) = valley {
        model
            .extra
            .insert("price_factor_valley".to_string(), serde_json::json!(valley));
    }
    let window_start = promotion
        .and_then(|value| value.get("window_start"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| known.as_ref().and_then(|rate| rate.window_start));
    let window_end = promotion
        .and_then(|value| value.get("window_end"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| known.as_ref().and_then(|rate| rate.window_end));
    let timezone = promotion
        .and_then(|value| value.get("timezone"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| known.as_ref().and_then(|rate| rate.timezone));
    if let (Some(start), Some(end)) = (window_start, window_end) {
        let active = off_peak_active_now(start, end, timezone, now);
        model.extra.insert(
            "off_peak".to_string(),
            serde_json::json!({
                "window_start": start,
                "window_end": end,
                "timezone": timezone.unwrap_or("Asia/Shanghai"),
                "discount_factor": discount,
            }),
        );
        if let Some(active) = active {
            model
                .extra
                .insert("off_peak_active_now".to_string(), serde_json::json!(active));
        }
    }
    let configured = model
        .extra
        .get("context_config")
        .map(context_from_config)
        .unwrap_or((None, None));
    let context = configured
        .0
        .or_else(|| {
            model
                .extra
                .get("context_window")
                .and_then(serde_json::Value::as_u64)
        })
        .or_else(|| {
            model
                .extra
                .get("max_input_tokens")
                .or_else(|| model.extra.get("maxInputTokens"))
                .and_then(serde_json::Value::as_u64)
        })
        .or_else(|| known.as_ref().and_then(|rate| rate.context));
    let max_context = configured
        .1
        .or_else(|| known.as_ref().and_then(|rate| rate.max_context))
        .filter(|max| context.is_none_or(|context| *max > context));
    if let Some(context) = context {
        model
            .extra
            .insert("context_window".to_string(), serde_json::json!(context));
        model
            .extra
            .insert("context".to_string(), serde_json::json!(context));
    }
    if let Some(max_context) = max_context {
        model
            .extra
            .insert("max_context".to_string(), serde_json::json!(max_context));
        model
            .extra
            .insert("max_mode".to_string(), serde_json::Value::Bool(true));
    }
}

fn enrich_qoder_models(mut models: Vec<ModelEntry>, realm: &str) -> Vec<ModelEntry> {
    let now = chrono::Utc::now().timestamp();
    for model in &mut models {
        enrich_qoder_model(model, realm, now);
    }
    models
}

fn qoder_chat_items(payload: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    payload
        .get("chat")
        .and_then(serde_json::Value::as_array)
        .or_else(|| {
            payload
                .pointer("/data/chat")
                .and_then(serde_json::Value::as_array)
        })
        .or_else(|| payload.get("models").and_then(serde_json::Value::as_array))
        .or_else(|| {
            payload
                .pointer("/data/models")
                .and_then(serde_json::Value::as_array)
        })
}

fn decode_qoder_body(encoded: &str) -> Option<Vec<u8>> {
    if encoded.is_empty()
        || encoded
            .chars()
            .any(|character| character != '$' && !QODER_CUSTOM_ALPHABET.contains(character))
    {
        return None;
    }
    let standard: String = encoded
        .chars()
        .map(|character| {
            if character == '$' {
                Some('=')
            } else {
                let index = QODER_CUSTOM_ALPHABET.find(character)?;
                QODER_STANDARD_ALPHABET.chars().nth(index)
            }
        })
        .collect::<Option<_>>()?;
    let n = standard.len();
    let a = n / 3;
    let restored = format!(
        "{}{}{}",
        &standard[n - a..],
        &standard[a..n - a],
        &standard[..a]
    );
    STANDARD.decode(restored).ok()
}

fn qoder_response_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(encoded) = value.as_str() {
            if let Some(bytes) = decode_qoder_body(encoded) {
                if let Ok(decoded) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    return Some(decoded);
                }
            }
        }
        return Some(value);
    }
    decode_qoder_body(trimmed)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
}

fn parse_qoder_catalog(payload: &serde_json::Value) -> Vec<ModelEntry> {
    qoder_chat_items(payload)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("key").and_then(serde_json::Value::as_str)?.trim();
            if id.is_empty()
                || id == "lite"
                || id.starts_with("codewise-")
                || id.starts_with("completion-")
                || id.ends_with("-image-alpha")
                || id.ends_with("-image-alpha-edit")
                || id.ends_with("-taco-completion")
            {
                return None;
            }
            let display_name = item
                .get("name")
                .or_else(|| item.get("display_name"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id)
                .to_string();
            let mut extra = item.as_object().cloned().unwrap_or_default();
            for key in [
                "id",
                "key",
                "object",
                "owned_by",
                "created",
                "permission",
                "capabilities",
                "credits",
                "price_factor",
            ] {
                extra.remove(key);
            }
            extra.insert("name".to_string(), serde_json::json!(display_name));
            extra.insert("display_name".to_string(), serde_json::json!(display_name));
            extra.insert("upstream_key".to_string(), serde_json::json!(id));
            Some(ModelEntry {
                id: display_name,
                object: "model".to_string(),
                owned_by: "qoder".to_string(),
                created: None,
                permission: None,
                capabilities: None,
                credits: item
                    .get("credits")
                    .or_else(|| item.get("price_factor"))
                    .cloned(),
                extra,
            })
        })
        .collect()
}

pub struct QoderProvider {
    /// Client for short non-streaming calls (model catalog fetch). Keeps a
    /// total timeout so a trickling response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
    model_keys: RwLock<HashMap<String, String>>,
}

impl QoderProvider {
    pub fn new(pool: Arc<AccountPool>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .http1_only()
            .build()
            .unwrap_or_default();

        let mut model_keys = qoder_model_key_map(&qoder_fallback_models("cn"));
        model_keys.extend(qoder_model_key_map(&qoder_fallback_models("intl")));

        Self {
            client,
            stream_client,
            pool,
            model_keys: RwLock::new(model_keys),
        }
    }

    async fn token_grant(&self, url: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(url)
            .timeout(Duration::from_secs(20))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", "Go-http-client/2.0")
            .json(&body)
            .send()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Qoder credential refresh failed: {}",
                    crate::upstream_error_body(status.as_u16(), &text)
                ),
                "upstream_error",
            ));
        }
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))
    }

    pub fn pool(&self) -> &Arc<AccountPool> {
        &self.pool
    }

    async fn qoder_sash(
        &self,
        account: &mg_core::Account,
        method: reqwest::Method,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String)> {
        let origin = if matches!(account.region.as_str(), "intl" | "global") {
            "https://qoder.com"
        } else {
            "https://qoder.com.cn"
        };
        let url = format!("{}{path}", qoder_openapi(&account.region));
        let mut request = self
            .client
            .request(method, &url)
            .timeout(Duration::from_secs(20))
            .bearer_auth(&account.access_token)
            .header("Accept", "application/json, text/plain, */*")
            .header("Content-Type", "application/json")
            .header("User-Agent", "Qoder")
            .header("cosy-clienttype", "10")
            .header("cosy-version", "0.4.3")
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"));
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        Ok((status, text))
    }

    async fn exchange_credentials(&self, account: mg_core::Account) -> Result<serde_json::Value> {
        let base = qoder_openapi(&account.region);
        let personal_token = account
            .extra
            .get("personal_token")
            .or_else(|| account.extra.get("personalToken"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        let (path, body, is_device) = if account.refresh_token.starts_with("drt-") {
            (
                QODER_DEVICE_REFRESH_PATH,
                serde_json::json!({"refresh_token": account.refresh_token}),
                true,
            )
        } else if account.refresh_token.starts_with("jrt-") {
            (
                QODER_JOB_REFRESH_PATH,
                serde_json::json!({"refresh_token": account.refresh_token}),
                false,
            )
        } else if !personal_token.is_empty() {
            (
                QODER_JOB_EXCHANGE_PATH,
                serde_json::json!({"personal_token": personal_token}),
                false,
            )
        } else if account.expires_at > mg_core::now_secs() && !account.access_token.is_empty() {
            return Ok(serde_json::json!({"ok": true, "message": "当前凭证仍有效"}));
        } else {
            return Err(MgError::unauthorized(
                "Qoder account has no refresh credential",
            ));
        };

        let grant = match self.token_grant(&format!("{base}{path}"), body).await {
            Ok(grant) => grant,
            Err(error) if path == QODER_JOB_REFRESH_PATH && !personal_token.is_empty() => self
                .token_grant(
                    &format!("{base}{QODER_JOB_EXCHANGE_PATH}"),
                    serde_json::json!({"personal_token": personal_token}),
                )
                .await
                .map_err(|_| error)?,
            Err(error) => return Err(error),
        };
        let data = grant.get("data").unwrap_or(&grant);
        let token = data
            .get("token")
            .or_else(|| data.get("device_token"))
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                MgError::provider(
                    502,
                    "Qoder refresh response has no token",
                    "invalid_response",
                )
            })?;
        let access_token = token.to_string();
        let refresh_token = data
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let expires_at = if is_device {
            Some(
                data.get("expires_at")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
                    .map(|timestamp| timestamp.timestamp() as f64)
                    .or_else(|| {
                        data.get("expires_in")
                            .and_then(qoder_number)
                            .map(|millis| mg_core::now_secs() + millis / 1000.0)
                    })
                    .unwrap_or_else(|| mg_core::now_secs() + 30.0 * 86400.0),
            )
        } else {
            data.get("expires_in")
                .and_then(qoder_number)
                .map(|millis| mg_core::now_secs() + millis / 1000.0)
        };
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
        Ok(serde_json::json!({"ok": true, "message": "Qoder 凭证已刷新"}))
    }

    fn token_needs_refresh(account: &mg_core::Account) -> bool {
        account.access_token.is_empty()
            || (account.expires_at > 0.0 && account.expires_at <= mg_core::now_secs() + 300.0)
    }

    fn has_refresh_credential(account: &mg_core::Account) -> bool {
        if !account.refresh_token.is_empty() {
            return true;
        }
        account
            .extra
            .get("personal_token")
            .or_else(|| account.extra.get("personalToken"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| !token.is_empty())
    }

    /// Serialize refreshes per account: Qoder rotates refresh tokens, so a
    /// second exchange with the pre-rotation token would fail and could
    /// invalidate the account even though the first refresh succeeded.
    async fn refresh_after_401(&self, account_id: &str, failed_token: &str) -> Result<()> {
        let lock = self.pool.refresh_lock(account_id);
        let _guard = lock.lock().await;
        if let Some(current) = self.pool.get(account_id) {
            if current.access_token != failed_token && !Self::token_needs_refresh(&current) {
                return Ok(());
            }
        }
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        self.exchange_credentials(account).await?;
        Ok(())
    }

    async fn ensure_fresh_account(&self, account: mg_core::Account) -> Result<mg_core::Account> {
        if !Self::token_needs_refresh(&account) || !Self::has_refresh_credential(&account) {
            return Ok(account);
        }
        let lock = self.pool.refresh_lock(&account.id);
        let _guard = lock.lock().await;
        let account = self.pool.get(&account.id).unwrap_or(account);
        if !Self::token_needs_refresh(&account) {
            return Ok(account);
        }
        let account_id = account.id.clone();
        match self.exchange_credentials(account).await {
            Ok(_) => self
                .pool
                .get(&account_id)
                .ok_or_else(|| MgError::not_found("Account not found")),
            Err(error) => {
                if matches!(error.status_code(), 401 | 403) {
                    self.pool
                        .set_credential_invalid(&account_id, "凭证已失效，请重新登录 Qoder 账号");
                }
                Err(error)
            }
        }
    }
}

fn qoder_china_date(now: i64) -> String {
    let shifted = chrono::DateTime::from_timestamp(now, 0).unwrap_or_else(chrono::Utc::now)
        + chrono::Duration::hours(8);
    shifted.format("%Y-%m-%d").to_string()
}

fn qoder_claim_date(value: &serde_json::Value) -> String {
    let Some(raw) = value
        .as_i64()
        .or_else(|| value.as_f64().map(|value| value as i64))
    else {
        return String::new();
    };
    let seconds = if raw > 1_000_000_000_000 {
        raw / 1000
    } else {
        raw
    };
    qoder_china_date(seconds)
}

fn save_qoder_checkin(
    pool: &AccountPool,
    account: mg_core::Account,
    checked_in: bool,
) -> Result<()> {
    pool.update(&account.id, |current| {
        current.checkin = Some(serde_json::json!({
            "checked_in": checked_in,
            "today_checked_in": checked_in,
            "updated_at": mg_core::now_secs()
        }));
    })?;
    Ok(())
}

#[async_trait]
impl Provider for QoderProvider {
    fn id(&self) -> &'static str {
        "qoder"
    }

    fn name(&self) -> &'static str {
        "Qoder"
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
        Some(
            if matches!(region.as_str(), "intl" | "global") {
                "intl"
            } else {
                "cn"
            }
            .to_string(),
        )
    }

    async fn set_region(&self, region: &str) -> Result<serde_json::Value> {
        let region = match region.trim().to_ascii_lowercase().as_str() {
            "cn" | "china" => "cn",
            "intl" | "global" => "intl",
            _ => return Err(MgError::bad_request("Unsupported Qoder region")),
        };
        self.pool.set_active_region(region)?;
        Ok(serde_json::json!({ "ok": true, "region": region }))
    }

    async fn models(&self) -> Result<Vec<ModelEntry>> {
        self.models_for_region(None).await
    }

    async fn models_for_region(&self, region: Option<&str>) -> Result<Vec<ModelEntry>> {
        let region = region.map(str::to_owned).or_else(|| self.active_region());
        let realm = match region
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("intl" | "global") => "intl",
            _ => "cn",
        };
        let fallback = qoder_fallback_models(realm);
        let Some(account) = representative_account(&self.pool, Some(realm)) else {
            return Ok(fallback);
        };
        let account = match self.ensure_fresh_account(account).await {
            Ok(account) => account,
            Err(_) => return Ok(fallback),
        };
        let body = qoder_encode(b"{}");
        for host in qoder_gateway_candidates(realm) {
            let url = format!("{host}{QODER_MODEL_PATH}");
            let Ok(headers) = qoder_cosy_headers(&account, &url, &body) else {
                continue;
            };
            let mut request = self
                .client
                .get(&url)
                .timeout(Duration::from_secs(15))
                .body(body.clone());
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = match request.send().await {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    tracing::warn!(provider = "qoder", status = %response.status(), "Qoder model catalog request failed");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(provider = "qoder", error = %error, "Qoder model catalog request failed");
                    continue;
                }
            };
            let Ok(text) = response.text().await else {
                continue;
            };
            let Some(payload) = qoder_response_json(&text) else {
                tracing::warn!(
                    provider = "qoder",
                    realm,
                    "Qoder model catalog was not JSON"
                );
                continue;
            };
            let models = enrich_qoder_models(parse_qoder_catalog(&payload), realm);
            if !models.is_empty() {
                self.model_keys.write().extend(qoder_model_key_map(&models));
                return Ok(models);
            }
        }
        Ok(fallback)
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
        let region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let mut account = self
            .pool
            .choose_in_region(session_id, None, Some(&region))?;
        account = self.ensure_fresh_account(account).await?;
        let requested_model = req.model.clone();
        let model_key = canonical_qoder_model_key(&requested_model, &self.model_keys.read());

        info!(
            provider = "qoder",
            model = %requested_model,
            account = %account.id,
            stream = req.stream,
            "Forwarding chat completion to Qoder"
        );

        let mut refreshed_after_401 = false;
        let (upstream_res, account_id) = 'request: loop {
            let account_id = account.id.clone();
            let realm = if matches!(account.region.as_str(), "intl" | "global") {
                "intl"
            } else {
                "cn"
            };
            let body = qoder_chat_body(&req, &account, &model_key, &requested_model)?;

            let mut last_error = None;
            let mut upstream_res = None;
            for host in qoder_gateway_candidates(realm) {
                let url = format!("{host}{QODER_CHAT_PATH}");
                let mut headers = match qoder_cosy_headers(&account, &url, &body) {
                    Ok(headers) => headers,
                    Err(error) => {
                        return Err(MgError::provider(500, error, "invalid_request"));
                    }
                };
                for (name, value) in &mut headers {
                    if *name == "accept" {
                        *value = "text/event-stream".to_string();
                    }
                }
                let mut request = self.stream_client.post(&url).body(body.clone());
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                request = request
                    .header("cache-control", "no-cache")
                    .header("x-model-key", &model_key)
                    .header("x-model-source", "system");
                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        upstream_res = Some(response);
                        break;
                    }
                    Ok(response) => {
                        let status = response.status();
                        let err_text = response.text().await.unwrap_or_default();
                        if matches!(status.as_u16(), 401 | 403) {
                            // The cached access token was revoked or expired early.
                            // Refresh once and resend instead of declaring the
                            // account dead on the first rejection.
                            if !refreshed_after_401 && Self::has_refresh_credential(&account) {
                                refreshed_after_401 = true;
                                let failed_token = account.access_token.clone();
                                match self.refresh_after_401(&account_id, &failed_token).await {
                                    Ok(_) => {
                                        if let Some(updated) = self.pool.get(&account_id) {
                                            account = updated;
                                        }
                                        tracing::warn!(
                                            provider = "qoder",
                                            account = %account_id,
                                            "Qoder rejected the access token; refreshed and retrying"
                                        );
                                        continue 'request;
                                    }
                                    Err(error) => {
                                        if !matches!(error.status_code(), 401 | 403) {
                                            self.pool.set_error(&account_id, &error.message());
                                            return Err(error);
                                        }
                                    }
                                }
                            }
                            self.pool.set_credential_invalid(
                                &account_id,
                                "凭证已失效，请重新登录 Qoder 账号",
                            );
                            return Err(MgError::provider(
                                401,
                                "Qoder 账号凭证已失效，请重新登录后再试",
                                "credential_invalid",
                            ));
                        }
                        if crate::is_account_level_failure(status.as_u16()) {
                            self.pool
                                .mark_cooldown(&account_id, crate::ACCOUNT_COOLDOWN_SECS);
                            self.pool.set_error(
                                &account_id,
                                &format!("upstream HTTP {}", status.as_u16()),
                            );
                        }
                        last_error = Some(MgError::provider(
                            status.as_u16(),
                            format!(
                                "Qoder request failed: {}",
                                crate::upstream_error_body(status.as_u16(), &err_text)
                            ),
                            "upstream_error",
                        ));
                        if crate::is_account_level_failure(status.as_u16()) || status.as_u16() < 500
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        last_error = Some(MgError::provider(
                            502,
                            format!("Qoder connection failed: {}", error_chain(&error)),
                            "network_error",
                        ));
                    }
                }
            }
            let Some(response) = upstream_res else {
                return Err(last_error.unwrap_or_else(|| {
                    MgError::provider(502, "Qoder connection failed", "network_error")
                }));
            };
            break (response, account_id);
        };

        let stream_req_id: Arc<str> =
            Arc::from(format!("chatcmpl-qoder-{}", uuid::Uuid::new_v4().simple()).as_str());
        let fallback_model: Arc<str> = Arc::from(requested_model.as_str());

        let s = upstream_res
            .bytes_stream()
            .eventsource()
            .filter_map(move |event_res| {
                let cid = Arc::clone(&stream_req_id);
                let model = Arc::clone(&fallback_model);
                async move {
                    match event_res {
                        Ok(event) => qoder_stream_event(&event.data, &model, &cid),
                        Err(error) => Some(Err(MgError::provider(
                            502,
                            format!("Stream read error: {error}"),
                            "upstream_error",
                        ))),
                    }
                }
            });

        Ok((Box::pin(s), account_id))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let mut account = self.ensure_fresh_account(account).await?;

        let url = format!("{}{}", qoder_openapi(&account.region), QODER_QUOTA_PATH);
        let origin = if matches!(account.region.as_str(), "intl" | "global") {
            "https://qoder.com"
        } else {
            "https://qoder.com.cn"
        };
        let mut refreshed = false;
        let response = loop {
            let response = self
                .client
                .get(&url)
                .timeout(Duration::from_secs(20))
                .bearer_auth(&account.access_token)
                .header("Accept", "application/json")
                .header("User-Agent", "Go-http-client/2.0")
                .header("Origin", origin)
                .header("Referer", format!("{origin}/"))
                .send()
                .await
                .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
            if matches!(response.status().as_u16(), 401 | 403)
                && !refreshed
                && Self::has_refresh_credential(&account)
            {
                refreshed = true;
                let failed_token = account.access_token.clone();
                match self.refresh_after_401(account_id, &failed_token).await {
                    Ok(_) => {
                        account = self
                            .pool
                            .get(account_id)
                            .ok_or_else(|| MgError::not_found("Account not found"))?;
                        continue;
                    }
                    Err(error) => {
                        if matches!(error.status_code(), 401 | 403) {
                            self.pool.set_credential_invalid(
                                account_id,
                                "凭证已失效，请重新登录 Qoder 账号",
                            );
                        } else {
                            self.pool.set_error(account_id, &error.message());
                        }
                        return Err(error);
                    }
                }
            }
            break response;
        };
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 401 | 403) {
                self.pool
                    .set_credential_invalid(account_id, "凭证已失效，请重新登录 Qoder 账号");
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Qoder quota query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &text)
                ),
                "upstream_error",
            ));
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = parse_qoder_quota(&payload, mg_core::now_secs())?;
        self.pool.update_quota(account_id, quota.clone())?;
        Ok(quota)
    }

    async fn checkin(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let account = self.ensure_fresh_account(account).await?;
        if account.access_token.is_empty() {
            return Err(MgError::unauthorized("Qoder account has no token"));
        }
        let (status_code, status_body) = self
            .qoder_sash(
                &account,
                reqwest::Method::GET,
                QODER_CHECKIN_STATUS_PATH,
                None,
            )
            .await?;
        if matches!(status_code, 404 | 405 | 410) {
            return Ok(serde_json::json!({
                "ok": false,
                "unavailable": true,
                "message": format!("本区域未开放每日签到接口（HTTP {status_code}）")
            }));
        }
        if !(200..300).contains(&status_code) {
            if crate::is_already_checked_in_message(&status_body) {
                save_qoder_checkin(&self.pool, account, true)?;
                return Ok(serde_json::json!({
                    "ok": true,
                    "already": true,
                    "message": "今日已签到"
                }));
            }
            return Err(MgError::provider(
                status_code,
                format!(
                    "Qoder check-in status failed: {}",
                    crate::upstream_error_body(status_code, &status_body)
                ),
                "upstream_error",
            ));
        }
        let status = serde_json::from_str::<serde_json::Value>(&status_body)
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let state = status
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let claimed_today = state == "CLAIMED"
            && status
                .get("lastClaimedAt")
                .map(qoder_claim_date)
                .is_some_and(|date| date == qoder_china_date(chrono::Utc::now().timestamp()));
        if claimed_today {
            save_qoder_checkin(&self.pool, account, true)?;
            return Ok(serde_json::json!({"ok": true, "already": true, "message": "今日已签到"}));
        }
        if !matches!(state, "CLAIMABLE" | "CLAIMED") {
            return Ok(serde_json::json!({
                "ok": false,
                "message": format!("官方签到活动未开放 (status={})", if state.is_empty() { "未知" } else { state })
            }));
        }
        let (claim_code, claim_body) = self
            .qoder_sash(
                &account,
                reqwest::Method::POST,
                QODER_CHECKIN_CLAIM_PATH,
                Some("{}"),
            )
            .await?;
        if claim_body.contains("ALREADY_CLAIMED")
            || crate::is_already_checked_in_message(&claim_body)
        {
            save_qoder_checkin(&self.pool, account, true)?;
            return Ok(serde_json::json!({"ok": true, "already": true, "message": "今日已签到"}));
        }
        if !(200..300).contains(&claim_code) {
            return Err(MgError::provider(
                claim_code,
                format!(
                    "Qoder check-in failed: {}",
                    crate::upstream_error_body(claim_code, &claim_body)
                ),
                "upstream_error",
            ));
        }
        let claim =
            serde_json::from_str::<serde_json::Value>(&claim_body).unwrap_or(serde_json::json!({}));
        if claim.get("success").and_then(serde_json::Value::as_bool) == Some(false)
            && !claim
                .get("result")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|result| result == "ALREADY_CLAIMED")
        {
            return Ok(serde_json::json!({
                "ok": false,
                "message": claim.get("error").and_then(serde_json::Value::as_str).unwrap_or("签到失败")
            }));
        }
        let reward = claim
            .get("rewardCredits")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        save_qoder_checkin(&self.pool, account, true)?;
        Ok(serde_json::json!({
            "ok": true,
            "message": if reward > 0 { format!("签到成功 +{reward} 积分") } else { "签到成功".to_string() }
        }))
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let lock = self.pool.refresh_lock(account_id);
        let _guard = lock.lock().await;
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        match self.exchange_credentials(account).await {
            Ok(value) => Ok(value),
            Err(error) => {
                if matches!(error.status_code(), 401 | 403) {
                    self.pool
                        .set_credential_invalid(account_id, "凭证已失效，请重新登录 Qoder 账号");
                }
                Err(error)
            }
        }
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "status": "manual",
            "message": "Qoder 账号在本地配置文件读取"
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
        canonical_qoder_model_key, enrich_qoder_model, off_peak_active_now, parse_qoder_catalog,
        parse_qoder_quota, qoder_cosy_headers, qoder_encode, qoder_fallback_models, qoder_identity,
        qoder_model_key_map, qoder_response_json, qoder_stream_event, QODER_CUSTOM_ALPHABET,
        QoderProvider,
    };
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde_json::json;

    #[test]
    fn aggregates_qoder_user_and_addon_quota() {
        let quota = parse_qoder_quota(
            &json!({
                "userQuota": {"remaining": 70, "used": 30, "total": 100},
                "addOnQuota": {"remaining": 45, "used": 5, "total": 50}
            }),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(115.0));
        assert_eq!(quota.limit, Some(150.0));
        assert!(parse_qoder_quota(&json!({}), 12.0).is_err());
    }

    #[test]
    fn unwraps_qoder_sse_envelopes_into_chat_chunks() {
        let envelope = r#"{"statusCodeValue":200,"body":"{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"}}]}"}"#;
        let chunk = qoder_stream_event(envelope, "fallback", "stream-1")
            .unwrap()
            .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("pong"));
        assert!(qoder_stream_event(r#"{"body":"[DONE]"}"#, "fallback", "stream-1").is_none());
        assert!(qoder_stream_event(
            r#"{"statusCodeValue":400,"body":"blocked"}"#,
            "fallback",
            "stream-1"
        )
        .unwrap()
        .is_err());
    }

    fn qoder_decode(encoded: &str) -> Vec<u8> {
        let standard = encoded
            .chars()
            .map(|character| {
                if character == '$' {
                    '='
                } else {
                    let index = QODER_CUSTOM_ALPHABET.find(character).unwrap();
                    super::QODER_STANDARD_ALPHABET.chars().nth(index).unwrap()
                }
            })
            .collect::<String>();
        let n = standard.len();
        let a = n / 3;
        let restored = format!(
            "{}{}{}",
            &standard[n - a..],
            &standard[a..n - a],
            &standard[..a]
        );
        STANDARD.decode(restored).unwrap()
    }

    #[test]
    fn reads_qoder_chat_catalog_and_preserves_display_metadata() {
        let models = parse_qoder_catalog(&json!({
            "chat": [
                {"key": "qmodel_38max", "name": "Qwen3.8-Max", "display_name": "Qwen 3.8 Max", "price_factor": 0.5, "context_config": {"200K": {"token_count": 200000}}},
                {"key": "lite", "display_name": "Lite"},
                {"key": "codewise-complete", "display_name": "Internal"}
            ]
        }));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "Qwen3.8-Max");
        assert_eq!(models[0].extra["upstream_key"], "qmodel_38max");
        assert_eq!(models[0].extra["display_name"], "Qwen3.8-Max");
        assert_eq!(
            models[0].extra["context_config"]["200K"]["token_count"],
            200000
        );

        let model_keys = qoder_model_key_map(&models);
        assert_eq!(
            canonical_qoder_model_key("QWEN3.8-MAX", &model_keys),
            "qmodel_38max"
        );
        assert_eq!(
            canonical_qoder_model_key("qmodel_38max", &model_keys),
            "qmodel_38max"
        );
    }

    #[test]
    fn fallback_catalogs_include_official_display_names_by_realm() {
        let cn = qoder_fallback_models("cn");
        let intl = qoder_fallback_models("intl");

        assert_eq!(cn[1].id, "Qwen3.8-Max");
        assert_eq!(cn[1].extra["display_name"], "Qwen3.8-Max");
        assert_eq!(cn[1].extra["upstream_key"], "qmodel_38max");
        assert!(cn
            .iter()
            .any(|model| model.extra["upstream_key"] == "gm51model"));
        assert!(intl
            .iter()
            .any(|model| model.extra["upstream_key"] == "smodel"));
        assert!(!intl
            .iter()
            .any(|model| model.extra["upstream_key"] == "gm51model"));
        let ultimate = intl
            .iter()
            .find(|model| model.extra["upstream_key"] == "ultimate")
            .unwrap();
        assert_eq!(ultimate.credits, Some(json!(2.0)));
        assert_eq!(ultimate.extra["price_factor"], json!(2.0));
        assert_eq!(ultimate.extra["context_window"], json!(200000));
        assert_eq!(ultimate.extra["max_context"], json!(1000000));
    }

    #[test]
    fn live_catalog_keeps_its_credit_factor_and_fills_a_missing_off_peak_window() {
        let mut models = parse_qoder_catalog(&json!({
            "data": {"chat": [
                {"key": "qmodel_38max", "display_name": "Qwen3.8-Max", "price_factor": 0.5,
                 "context_config": {"200K": {"token_count": 200000, "is_default": true}, "1M": {"token_count": 1000000}}}
            ]}
        }));
        enrich_qoder_model(&mut models[0], "intl", 1_767_279_600);
        assert_eq!(models[0].credits, Some(json!(0.5)));
        assert_eq!(models[0].extra["price_factor"], json!(0.5));
        assert_eq!(models[0].extra["price_factor_peak"], json!(0.5));
        assert_eq!(models[0].extra["price_factor_valley"], json!(0.2));
        assert_eq!(models[0].extra["off_peak"]["window_start"], "22:00");
        assert_eq!(models[0].extra["off_peak_active_now"], true);
        assert_eq!(models[0].extra["context_window"], json!(200000));
        assert_eq!(models[0].extra["max_context"], json!(1000000));
    }

    #[test]
    fn missing_live_credit_factor_falls_back_to_the_official_snapshot() {
        let mut models = parse_qoder_catalog(&json!({
            "chat": [{"key": "ultimate", "display_name": "Ultimate", "price_factor": null}]
        }));
        enrich_qoder_model(&mut models[0], "intl", 1_767_232_800);
        assert_eq!(models[0].credits, Some(json!(2.0)));
        assert_eq!(models[0].extra["price_factor"], json!(2.0));
        assert!(models[0].extra.get("off_peak").is_none());
    }

    #[test]
    fn decodes_an_encoded_catalog_body_and_recognizes_the_off_peak_window() {
        let encoded = qoder_encode(br#"{"chat":[{"key":"auto","display_name":"Auto"}]}"#);
        let payload = qoder_response_json(&encoded).unwrap();
        assert_eq!(
            parse_qoder_catalog(&payload)[0].extra["upstream_key"],
            "auto"
        );
        assert_eq!(
            off_peak_active_now("22:00", "08:00", Some("Asia/Shanghai"), 1_767_279_600),
            Some(true)
        );
        assert_eq!(
            off_peak_active_now("22:00", "08:00", Some("UTC+8"), 1_767_225_600),
            Some(false)
        );
    }

    #[test]
    fn cosy_identity_uses_qoder_top_level_account_fields() {
        let account: mg_core::Account = serde_json::from_value(json!({
            "uid": "qoder-user",
            "accessToken": "token",
            "userType": "organization_member",
            "organizationId": "org-1",
            "organizationName": "Example"
        }))
        .unwrap();

        let identity = qoder_identity(&account);
        assert_eq!(identity["user_type"], "organization_member");
        assert_eq!(identity["organization_id"], "org-1");
        assert_eq!(identity["organization_name"], "Example");
    }

    #[test]
    fn qoder_request_body_uses_the_custom_base64_variant() {
        let encoded = qoder_encode(b"{}");
        assert!(encoded
            .chars()
            .all(|character| QODER_CUSTOM_ALPHABET.contains(character) || character == '$'));
        assert_eq!(qoder_decode(&encoded), b"{}");
    }

    #[test]
    fn detects_tokens_that_need_refresh_and_personal_token_credentials() {
        let mut account: mg_core::Account = serde_json::from_value(json!({
            "id": "qoder-user",
            "access_token": "test-access-token",
            "expires_at": mg_core::now_secs() + 3600.0
        }))
        .unwrap();
        assert!(!QoderProvider::token_needs_refresh(&account));
        assert!(!QoderProvider::has_refresh_credential(&account));

        account
            .extra
            .insert("personal_token".to_string(), json!("personal-token"));
        assert!(QoderProvider::has_refresh_credential(&account));

        account.expires_at = mg_core::now_secs() + 60.0;
        assert!(QoderProvider::token_needs_refresh(&account));

        account.access_token.clear();
        assert!(QoderProvider::token_needs_refresh(&account));
    }

    #[test]
    fn builds_cosy_signed_model_request_headers() {
        let account: mg_core::Account = serde_json::from_value(json!({
            "id": "qoder-user",
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "label": "Qoder Test",
            "extra": {"user_type": "personal_professional_trial"}
        }))
        .unwrap();
        let body = qoder_encode(b"{}");
        let url = "https://gateway.qoder.com.cn/algo/api/v2/model/list?Encode=1";
        let headers = qoder_cosy_headers(&account, url, &body).unwrap();
        let authorization = headers
            .iter()
            .find(|(name, _)| *name == "authorization")
            .unwrap()
            .1
            .clone();
        assert!(authorization.starts_with("Bearer COSY."));
        let parts: Vec<_> = authorization.split('.').collect();
        let payload: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["cosyVersion"], "1.1.64");
        assert!(headers.iter().any(|(name, _)| *name == "cosy-key"));
    }
}
