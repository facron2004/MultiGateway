use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mg_core::account::{regions_match, Account, AccountPool, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta, ModelEntry,
};
use reqwest::Client;
use serde_json::Value;

use crate::catalog::fallback_models;
use crate::traits::{BoxStream, Provider};

pub const REGION_CN_AUTH: &str = "https://account.minimax.cn";
pub const REGION_CN_AGENT: &str = "https://agent.minimax.cn";
pub const REGION_GLOBAL_AUTH: &str = "https://account.minimax.io";
pub const REGION_GLOBAL_AGENT: &str = "https://agent.minimax.io";

pub const CLIENT_ID: &str = "mcode-public";

/// Completion budget used when the caller asks for none. MiniMax's agent
/// models write long answers (and reasoning tokens count against the same
/// budget), so the old 8192 silently truncated them mid-sentence.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32768;

/// Published output ceiling for the M3 family. The request default above is a
/// budget, not the model's maximum; advertising it as `max_output_tokens` made
/// the catalog show 32K for models that can emit 512K.
const MODEL_MAX_OUTPUT_TOKENS: u64 = 524_288;

/// The completion budget for one request: the caller's value when it gave one,
/// their `max_completion_tokens` alias otherwise, else the provider default.
fn max_output_tokens(req: &mg_protocols::openai::ChatCompletionRequest) -> u32 {
    req.max_tokens
        .or(req.max_completion_tokens)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub struct MiniMaxProvider {
    /// Client for short non-streaming calls (token refresh, model catalog).
    /// Keeps a total timeout so a trickling response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
}

fn minimax_model_entries(data: &Value) -> Vec<ModelEntry> {
    let provider = match data
        .get("providers")
        .and_then(Value::as_array)
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider.get("providerId").and_then(Value::as_str) == Some("minimax")
            })
        }) {
        Some(provider) => provider,
        None => return Vec::new(),
    };
    let config = match provider.get("config") {
        Some(config) => config,
        None => return Vec::new(),
    };
    let model_map = match config.get("models").and_then(Value::as_object) {
        Some(models) => models,
        None => return Vec::new(),
    };

    let mut ids = Vec::new();
    if let Some(order) = config.get("model_order").and_then(Value::as_array) {
        for id in order.iter().filter_map(Value::as_str) {
            if model_map.contains_key(id) && !ids.iter().any(|seen| seen == id) {
                ids.push(id.to_string());
            }
        }
    }
    let mut remaining: Vec<_> = model_map
        .keys()
        .filter(|id| !ids.contains(id))
        .cloned()
        .collect();
    remaining.sort();
    ids.extend(remaining);

    ids.into_iter()
        .filter_map(|id| {
            let metadata = model_map.get(&id)?;
            // MiniMax's catalog ships no limits or modality list, so the
            // provider's own facts (images, tools, the reasoning ladder) and the
            // model's published output ceiling are attached here; the shared
            // envelope then publishes them in every client spelling.
            let mut extra = serde_json::Map::new();
            extra.insert("supports_vision".to_string(), Value::Bool(true));
            extra.insert("supports_tool_calls".to_string(), Value::Bool(true));
            extra.insert(
                "max_output_tokens".to_string(),
                Value::from(MODEL_MAX_OUTPUT_TOKENS),
            );
            extra.insert(
                "reasoning_efforts".to_string(),
                serde_json::json!(["low", "medium", "high", "xhigh"]),
            );
            extra.insert("reasoning_default_effort".to_string(), Value::from("high"));
            extra.insert("reasoning_can_disable".to_string(), Value::Bool(true));
            Some(ModelEntry {
                id,
                object: "model".to_string(),
                owned_by: "minimax".to_string(),
                created: None,
                permission: None,
                capabilities: metadata.get("capabilities").cloned(),
                credits: metadata
                    .get("credits")
                    .or_else(|| metadata.get("quota"))
                    .cloned(),
                extra,
            })
        })
        .collect()
}

impl MiniMaxProvider {
    pub fn new(pool: Arc<AccountPool>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
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
        }
    }

    fn agent_url(&self, region: &str) -> &'static str {
        if regions_match(region, "cn") {
            REGION_CN_AGENT
        } else {
            REGION_GLOBAL_AGENT
        }
    }

    fn auth_url(&self, region: &str) -> &'static str {
        if regions_match(region, "cn") {
            REGION_CN_AUTH
        } else {
            REGION_GLOBAL_AUTH
        }
    }

    fn token_needs_refresh(account: &Account) -> bool {
        account.access_token.is_empty()
            || account.expires_at
                <= SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    + 300.0
    }

    async fn ensure_fresh_token(&self, account: &mut Account) -> Result<String> {
        if !Self::token_needs_refresh(account) {
            return Ok(account.access_token.clone());
        }
        // Refresh tokens can rotate; serialize per account and re-read after
        // waiting so two requests never exchange the same token at once.
        let lock = self.pool.refresh_lock(&account.id);
        let _guard = lock.lock().await;
        if let Some(current) = self.pool.get(&account.id) {
            *account = current;
        }
        if !Self::token_needs_refresh(account) {
            return Ok(account.access_token.clone());
        }
        if account.refresh_token.is_empty() {
            if !account.access_token.is_empty() {
                return Ok(account.access_token.clone());
            }
            return Err(MgError::unauthorized(
                "MiniMax account has no refresh token",
            ));
        }
        match self.exchange_fresh_token(account).await {
            Ok(token) => Ok(token),
            Err(error) => {
                if matches!(error.status_code(), 401 | 403) {
                    self.pool
                        .set_credential_invalid(&account.id, "凭证已失效，请重新登录 MiniMax 账号");
                }
                Err(error)
            }
        }
    }

    async fn refresh_after_401(&self, account: &mut Account, failed_token: &str) -> Result<()> {
        let lock = self.pool.refresh_lock(&account.id);
        let _guard = lock.lock().await;
        if let Some(current) = self.pool.get(&account.id) {
            *account = current;
            if account.access_token != failed_token && !Self::token_needs_refresh(account) {
                return Ok(());
            }
        }
        self.exchange_fresh_token(account).await?;
        Ok(())
    }

    async fn exchange_fresh_token(&self, account: &mut Account) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let auth_base = self.auth_url(&account.region);
        let url = format!("{}/oauth2/token", auth_base);

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", account.refresh_token.as_str()),
        ];

        let res = self
            .client
            .post(&url)
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                MgError::provider(
                    502,
                    format!("MiniMax token refresh failed: {}", e),
                    "network_error",
                )
            })?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                401,
                format!(
                    "MiniMax refresh token rejected: {}",
                    crate::upstream_error_body(status, &body)
                ),
                "invalid_grant",
            ));
        }

        let data: serde_json::Value = res.json().await.map_err(|e| MgError::Json(e.to_string()))?;
        let new_token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MgError::provider(500, "Missing access_token", "invalid_response"))?
            .to_string();

        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_f64())
            .unwrap_or(3600.0);

        let refresh_token = data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        account.access_token = new_token.clone();
        if let Some(refresh_token) = &refresh_token {
            account.refresh_token = refresh_token.clone();
        }
        account.expires_at = now + expires_in;
        let pool = Arc::clone(&self.pool);
        let account_id = account.id.clone();
        let access_token = new_token.clone();
        let expires_at = account.expires_at;
        tokio::task::spawn_blocking(move || {
            pool.update(&account_id, |current| {
                current.access_token = access_token;
                if let Some(refresh_token) = refresh_token {
                    current.refresh_token = refresh_token;
                }
                current.expires_at = expires_at;
            })
        })
        .await
        .map_err(|error| MgError::Io(error.to_string()))??;

        Ok(new_token)
    }
}

#[async_trait]
impl Provider for MiniMaxProvider {
    fn id(&self) -> &'static str {
        "minimax"
    }

    fn name(&self) -> &'static str {
        "MiniMax Code"
    }

    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({
            "login": false,
            "import": true,
            "credits": false,
            "regions": true,
            "checkin": false,
            "credentials": true,
        })
    }

    fn active_region(&self) -> Option<String> {
        let region = self.pool.active_region();
        Some(
            if regions_match(&region, "cn") {
                "cn"
            } else {
                "global"
            }
            .to_string(),
        )
    }

    async fn set_region(&self, region: &str) -> Result<serde_json::Value> {
        let region = match region.trim().to_ascii_lowercase().as_str() {
            "cn" | "china" => "cn",
            "global" | "intl" => "global",
            _ => return Err(MgError::bad_request("Unsupported MiniMax region")),
        };
        self.pool.set_active_region(region)?;
        Ok(serde_json::json!({ "ok": true, "region": region }))
    }

    async fn models(&self) -> Result<Vec<ModelEntry>> {
        self.models_for_region(None).await
    }

    async fn models_for_region(&self, region: Option<&str>) -> Result<Vec<ModelEntry>> {
        let fallback = fallback_models(&["MiniMax-Text-01", "abab6.5s-chat"], "minimax");
        let active_region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let selected_region = match region
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("cn" | "china") => "cn",
            Some("global" | "intl") => "global",
            Some("all") | None => active_region.as_str(),
            Some(_) => return Err(MgError::bad_request("Unsupported MiniMax region")),
        };
        let account = match self
            .pool
            .choose_in_region(None, None, Some(selected_region))
        {
            Ok(account) => account,
            Err(_) => return Ok(fallback),
        };
        let model_region = if selected_region == "cn" { "cn" } else { "en" };
        let url = format!(
            "{}/mavis/api/v1/models?region={}&buildEnv=prod",
            self.agent_url(&account.region),
            model_region
        );
        let response = match self
            .client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            _ => return Ok(fallback),
        };
        let data = match response.json::<Value>().await {
            Ok(data) => data,
            Err(_) => return Ok(fallback),
        };
        let models = minimax_model_entries(&data);
        Ok(if models.is_empty() { fallback } else { models })
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
        self.ensure_fresh_token(&mut account).await?;
        let agent_base = self.agent_url(&account.region);

        let url = format!("{}/mavis/api/v1/llm/v1/messages", agent_base);
        let session_str = session_id.unwrap_or("default-session");

        // Read the caller's budget before `messages` is consumed below.
        let max_tokens = max_output_tokens(&req);

        // Convert ChatCompletionRequest to Anthropic Messages format
        let mut messages = Vec::new();
        let mut system = None;

        for msg in req.messages {
            if msg.role == "system" {
                if let Some(serde_json::Value::String(s)) = msg.content {
                    system = Some(serde_json::json!([{"type": "text", "text": s}]));
                }
            } else {
                let role = if msg.role == "assistant" {
                    "assistant"
                } else {
                    "user"
                };
                let text = match msg.content {
                    Some(serde_json::Value::String(s)) => s,
                    Some(val) => val.to_string(),
                    None => String::new(),
                };
                messages.push(serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": text}]
                }));
            }
        }

        let body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "system": system,
            "max_tokens": max_tokens,
            "stream": true,
            "temperature": req.temperature,
            "top_p": req.top_p,
        });

        let mut refreshed = false;
        let res = loop {
            let res = self
                .stream_client
                .post(&url)
                .header(
                    "Authorization",
                    format!("Bearer {}", account.access_token),
                )
                .header("x-api-key", "sk-xxx")
                .header("anthropic-version", "2023-06-01")
                .header("User-Agent", "MiniMaxAgent")
                .header("X-Mavis-Agent-Id", "main")
                .header("X-Mavis-Session-Id", session_str)
                .header("Accept", "text/event-stream")
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    MgError::provider(
                        502,
                        format!("MiniMax upstream error: {}", e),
                        "network_error",
                    )
                })?;

            if matches!(res.status().as_u16(), 401 | 403)
                && !refreshed
                && !account.refresh_token.is_empty()
            {
                refreshed = true;
                let failed_token = account.access_token.clone();
                match self.refresh_after_401(&mut account, &failed_token).await {
                    Ok(_) => continue,
                    Err(error) => {
                        if matches!(error.status_code(), 401 | 403) {
                            self.pool.set_credential_invalid(
                                &account.id,
                                "凭证已失效，请重新登录 MiniMax 账号",
                            );
                        } else {
                            self.pool.set_error(&account.id, &error.message());
                        }
                        return Err(error);
                    }
                }
            }
            break res;
        };

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body_text = res.text().await.unwrap_or_default();
            if matches!(status, 401 | 403) {
                self.pool.set_credential_invalid(
                    &account.id,
                    "凭证已失效，请重新登录 MiniMax 账号",
                );
                return Err(MgError::provider(
                    401,
                    "MiniMax 账号凭证已失效，请重新登录后再试",
                    "credential_invalid",
                ));
            }
            if crate::is_account_level_failure(status) {
                self.pool
                    .mark_cooldown(&account.id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(&account.id, &format!("upstream HTTP {}", status));
            }
            return Err(MgError::provider(
                status,
                format!(
                    "MiniMax rejected request: {}",
                    crate::upstream_error_body(status, &body_text)
                ),
                "upstream_error",
            ));
        }

        let chunk_id: Arc<str> = Arc::from(format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()));
        let model: Arc<str> = Arc::from(req.model.as_str());

        let stream = res
            .bytes_stream()
            .eventsource()
            .filter_map(move |event_res| {
                let m = Arc::clone(&model);
                let cid = Arc::clone(&chunk_id);
                async move {
                    match event_res {
                        Ok(event) => {
                            if event.data == "[DONE]" {
                                return None;
                            }
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&event.data)
                            {
                                let event_type =
                                    val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                match event_type {
                                    "content_block_delta" => {
                                        if let Some(delta_text) =
                                            val.pointer("/delta/text").and_then(|v| v.as_str())
                                        {
                                            return Some(Ok(ChatCompletionChunk {
                                                id: cid.to_string(),
                                                object: "chat.completion.chunk".to_string(),
                                                created: chrono::Utc::now().timestamp() as u64,
                                                model: m.to_string(),
                                                choices: vec![ChunkChoice {
                                                    index: 0,
                                                    delta: ChunkDelta {
                                                        role: None,
                                                        content: Some(delta_text.to_string()),
                                                        reasoning_content: None,
                                                        tool_calls: None,
                                                    },
                                                    finish_reason: None,
                                                }],
                                                usage: None,
                                            }));
                                        }
                                    }
                                    "message_delta" => {
                                        let usage = val.get("usage").map(|u| ChatUsage {
                                            prompt_tokens: u
                                                .get("input_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0),
                                            completion_tokens: u
                                                .get("output_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0),
                                            total_tokens: u
                                                .get("input_tokens")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0)
                                                + u.get("output_tokens")
                                                    .and_then(|v| v.as_u64())
                                                    .unwrap_or(0),
                                            prompt_tokens_details: None,
                                            credits: None,
                                        });

                                        return Some(Ok(ChatCompletionChunk {
                                            id: cid.to_string(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: chrono::Utc::now().timestamp() as u64,
                                            model: m.to_string(),
                                            choices: vec![ChunkChoice {
                                                index: 0,
                                                delta: ChunkDelta::default(),
                                                finish_reason: Some("stop".to_string()),
                                            }],
                                            usage,
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                            None
                        }
                        Err(e) => Some(Err(MgError::provider(500, e.to_string(), "stream_error"))),
                    }
                }
            });

        Ok((Box::pin(stream), account.id))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let _ = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        Err(MgError::bad_request(
            "MiniMax Code OAuth credentials cannot query Token Plan quota; a Subscription Key is required",
        ))
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let mut account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let was_expiring = account.expires_at <= mg_core::now_secs() + 300.0;
        if was_expiring && account.refresh_token.is_empty() {
            return Err(MgError::unauthorized(
                "MiniMax account has no refresh token; sign in again",
            ));
        }
        self.ensure_fresh_token(&mut account).await?;
        Ok(serde_json::json!({
            "ok": true,
            "message": if was_expiring { "MiniMax Code 凭证已刷新" } else { "当前凭证仍有效" }
        }))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        let region = self.active_region().unwrap_or_else(|| "cn".to_string());
        let login_id = uuid::Uuid::new_v4().simple().to_string();
        Ok(serde_json::json!({
            "login_id": login_id,
            "url": self.auth_url(&region),
        }))
    }

    async fn poll_login(&self, _login_id: &str) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "status": "pending" }))
    }
}

#[cfg(test)]
mod tests {
    use super::{minimax_model_entries, MiniMaxProvider};
    use serde_json::json;

    #[test]
    fn refreshes_oauth_tokens_before_they_expire() {
        let mut account: mg_core::Account = serde_json::from_value(json!({
            "id": "minimax-test",
            "access_token": "a",
            "expires_at": mg_core::now_secs() + 3600.0
        }))
        .unwrap();
        assert!(!MiniMaxProvider::token_needs_refresh(&account));
        account.expires_at = mg_core::now_secs() + 60.0;
        assert!(MiniMaxProvider::token_needs_refresh(&account));
        account.access_token.clear();
        assert!(MiniMaxProvider::token_needs_refresh(&account));
    }

    #[test]
    fn reads_minimax_catalog_in_upstream_order_then_sorts_remaining_models() {
        let response = json!({
            "providers": [{
                "providerId": "minimax",
                "config": {
                    "model_order": ["second", "missing", "first"],
                    "models": {
                        "first": {"capabilities": {"vision": true}},
                        "third": {},
                        "second": {"quota": {"remaining": 1}}
                    }
                }
            }]
        });

        let rows = minimax_model_entries(&response);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["second", "first", "third"]
        );
        assert!(rows[0].credits.is_some());
        assert!(rows[1].capabilities.is_some());
    }
}
