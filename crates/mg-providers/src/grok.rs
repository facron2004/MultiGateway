use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mg_core::account::{Account, AccountPool, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_protocols::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta, ModelEntry,
};
use mg_protocols::responses::{GrokEvent, ResponsesRequest};
use reqwest::{Client, RequestBuilder};

use crate::catalog::{fallback_models, parse_model_entries};
use crate::traits::{BoxStream, Provider};

pub const ISSUER: &str = "https://auth.x.ai";
pub const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

pub const API_BASE: &str = "https://api.x.ai/v1";
pub const CLI_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
pub const CLIENT_VERSION: &str = "1.0.13";
const CLIENT_IDENTIFIER: &str = "grok-shell";
const CLIENT_MODE: &str = "headless";
const TOKEN_AUTH: &str = "xai-grok-cli";
const AUTHENTICATE_RESPONSE: &str = "authenticate-response";

pub const REDIRECT_PORT: u16 = 56121;

fn with_cli_headers(mut request: RequestBuilder, account: &Account) -> RequestBuilder {
    request = request
        .header("x-grok-client-version", CLIENT_VERSION)
        .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
        .header("x-grok-client-mode", CLIENT_MODE)
        .header("X-XAI-Token-Auth", TOKEN_AUTH)
        .header("x-authenticateresponse", AUTHENTICATE_RESPONSE)
        .header(
            "User-Agent",
            format!("grok-shell/{CLIENT_VERSION} (windows; x86_64)"),
        );
    if let Some(principal_id) = account
        .extra
        .get("principal_id")
        .and_then(serde_json::Value::as_str)
    {
        request = request
            .header("x-userid", principal_id)
            .header("x-grok-user-id", principal_id);
    }
    if !account.email.is_empty() && account.organization_id.is_none() {
        request = request.header("x-email", &account.email);
    }
    request
}

fn quota_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

/// Grok publishes the standard window in `context_window` and the selectable
/// long-context versions in `context_windows` (256K and 500K for 4.5/4.6/4.7).
/// Keep the base window for the dashboard, but expose the largest window as
/// `max_context`/`context_length` so clients do not truncate at 256K.
fn enrich_grok_context(model: &mut ModelEntry) {
    let windows: Vec<u64> = model
        .extra
        .get("context_windows")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .filter(|value| *value > 0)
                .collect()
        })
        .unwrap_or_default();
    let base = model
        .extra
        .get("context_window")
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value > 0)
        .or_else(|| windows.iter().copied().min());
    let Some(base) = base else {
        return;
    };
    model
        .extra
        .insert("context_window".to_string(), serde_json::json!(base));
    model
        .extra
        .insert("context".to_string(), serde_json::json!(base));
    model
        .extra
        .insert("context_length".to_string(), serde_json::json!(base));
    model
        .extra
        .insert("max_input_tokens".to_string(), serde_json::json!(base));
    if let Some(max) = windows.iter().copied().max().filter(|max| *max > base) {
        model
            .extra
            .insert("max_context".to_string(), serde_json::json!(max));
        model
            .extra
            .insert("context_length".to_string(), serde_json::json!(max));
        model
            .extra
            .insert("max_input_tokens".to_string(), serde_json::json!(max));
        model
            .extra
            .insert("context_windows".to_string(), serde_json::json!(windows));
    }
}

fn parse_grok_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {    let config = payload
        .get("config")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            MgError::provider(
                502,
                "Grok returned no subscription quota",
                "invalid_response",
            )
        })?;
    let period = config
        .get("currentPeriod")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            MgError::provider(502, "Grok returned no quota period", "invalid_response")
        })?;
    let period_type = period
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut usage = config.get("creditUsagePercent").and_then(quota_number);
    if usage.is_none()
        && matches!(
            period_type,
            "USAGE_PERIOD_TYPE_WEEKLY" | "USAGE_PERIOD_TYPE_MONTHLY"
        )
    {
        usage = Some(0.0);
    }
    if usage.is_none() {
        let total = config
            .get("monthlyLimit")
            .and_then(|value| value.get("val"))
            .and_then(quota_number);
        let used = config
            .get("used")
            .and_then(|value| value.get("val"))
            .and_then(quota_number);
        if let (Some(total), Some(used)) = (total.filter(|total| *total > 0.0), used) {
            usage = Some(used / total * 100.0);
        }
    }
    let usage = usage.ok_or_else(|| {
        MgError::provider(502, "Grok returned no usage percentage", "invalid_response")
    })?;
    let remaining = (100.0 - usage).clamp(0.0, 100.0);
    let label = if period_type == "USAGE_PERIOD_TYPE_WEEKLY" {
        "本周额度"
    } else {
        "本月额度"
    };
    let reset_at = period
        .get("end")
        .or_else(|| config.get("billingPeriodEnd"))
        .cloned();
    Ok(AccountQuota {
        checked_at,
        remaining: Some(remaining),
        limit: Some(100.0),
        reset_at: reset_at.as_ref().and_then(quota_number),
        details: serde_json::json!({
            "label": label,
            "unit": "%",
            "buckets": [{"label": label, "remaining": remaining, "unit": "%", "reset_at": reset_at}]
        }),
    })
}

pub struct GrokProvider {
    /// Client for short non-streaming calls (token refresh, model catalog).
    /// Keeps a total timeout so a trickling response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
}

impl GrokProvider {
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
            return Err(MgError::unauthorized("Grok account has no refresh token"));
        }
        match self.exchange_fresh_token(account).await {
            Ok(token) => Ok(token),
            Err(error) => {
                if matches!(error.status_code(), 401 | 403) {
                    self.pool
                        .set_credential_invalid(&account.id, "凭证已失效，请重新登录 Grok 账号");
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

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", account.refresh_token.as_str()),
        ];

        let res = self
            .client
            .post(TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                MgError::provider(
                    502,
                    format!("Grok token refresh failed: {}", e),
                    "network_error",
                )
            })?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                401,
                format!(
                    "Grok refresh token rejected: {}",
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
            .unwrap_or(21600.0);

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

    fn select_host(&self, account: &Account, model: &str) -> &'static str {
        if model.starts_with("grok-composer-") {
            return CLI_BASE;
        }

        // Check if JWT claims has tier > 0
        if let Some(tier) = account.extra.get("tier").and_then(|v| v.as_u64()) {
            if tier > 0 {
                return API_BASE;
            }
        }

        CLI_BASE
    }
}

#[async_trait]
impl Provider for GrokProvider {
    fn id(&self) -> &'static str {
        "grok"
    }

    fn name(&self) -> &'static str {
        "xAI Grok"
    }

    fn capabilities(&self) -> serde_json::Value {
        serde_json::json!({
            "login": true,
            "import": true,
            "credits": true,
            "regions": false,
            "checkin": false,
            "credentials": true,
        })
    }

    async fn models(&self) -> Result<Vec<ModelEntry>> {
        let fallback = fallback_models(
            &["grok-4.7", "grok-4.6", "grok-4.5", "grok-build-0.1"],
            "xai",
        );
        let mut account = match self.pool.choose(None, None) {
            Ok(account) => account,
            Err(_) => return Ok(fallback),
        };
        let token = match self.ensure_fresh_token(&mut account).await {
            Ok(token) => token,
            Err(_) => return Ok(fallback),
        };
        let host = self.select_host(&account, "");
        let url = format!("{}/models", host);
        let mut request = self
            .client
            .get(url)
            .timeout(Duration::from_secs(20))
            .bearer_auth(token);
        if host == CLI_BASE {
            request = with_cli_headers(request, &account);
        }

        let response = match request.send().await {
            Ok(response) if response.status().is_success() => response,
            _ => return Ok(fallback),
        };
        match response.json::<serde_json::Value>().await {
            Ok(data) => {
                let mut models = parse_model_entries(&data, "xai");
                for model in &mut models {
                    enrich_grok_context(model);
                }
                Ok(if models.is_empty() { fallback } else { models })
            }
            Err(_) => Ok(fallback),
        }
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
        let mut account = self.pool.choose(session_id, None)?;
        self.ensure_fresh_token(&mut account).await?;

        // Convert messages to Responses input format
        let mut input = Vec::new();
        let mut instructions = None;

        for msg in req.messages {
            if msg.role == "system" {
                if let Some(serde_json::Value::String(s)) = msg.content {
                    instructions = Some(s);
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
                input.push(serde_json::json!({
                    "role": role,
                    "content": text
                }));
            }
        }

        let responses_req = ResponsesRequest {
            model: req.model.clone(),
            input,
            instructions,
            tools: req.tools,
            temperature: req.temperature,
            max_output_tokens: req.max_tokens.or(req.max_completion_tokens),
            stream: true,
        };

        let mut refreshed = false;
        let res = loop {
            let host = self.select_host(&account, &req.model);
            let url = format!("{}/responses", host);
            let mut request_builder = self
                .stream_client
                .post(&url)
                .header(
                    "Authorization",
                    format!("Bearer {}", account.access_token),
                )
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream");

            if host == CLI_BASE {
                request_builder = with_cli_headers(request_builder, &account);
            }

            let res = request_builder
                .json(&responses_req)
                .send()
                .await
                .map_err(|e| {
                    MgError::provider(502, format!("Grok upstream error: {}", e), "network_error")
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
                            self.pool
                                .set_credential_invalid(&account.id, "凭证已失效，请重新登录 Grok 账号");
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
            let body = res.text().await.unwrap_or_default();
            if matches!(status, 401 | 403) {
                self.pool
                    .set_credential_invalid(&account.id, "凭证已失效，请重新登录 Grok 账号");
                return Err(MgError::provider(
                    401,
                    "Grok 账号凭证已失效，请重新登录后再试",
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
                    "Grok rejected request: {}",
                    crate::upstream_error_body(status, &body)
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
                            if let Ok(grok_event) = serde_json::from_str::<GrokEvent>(&event.data) {
                                match grok_event {
                                    GrokEvent::OutputTextDelta { delta, .. } => {
                                        return Some(Ok(ChatCompletionChunk {
                                            id: cid.to_string(),
                                            object: "chat.completion.chunk".to_string(),
                                            created: chrono::Utc::now().timestamp() as u64,
                                            model: m.to_string(),
                                            choices: vec![ChunkChoice {
                                                index: 0,
                                                delta: ChunkDelta {
                                                    role: None,
                                                    content: Some(delta),
                                                    reasoning_content: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                            usage: None,
                                        }));
                                    }
                                    GrokEvent::Completed { response } => {
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
                                            usage: Some(ChatUsage {
                                                prompt_tokens: response.usage.input_tokens,
                                                completion_tokens: response.usage.output_tokens,
                                                total_tokens: response.usage.total_tokens,
                                                prompt_tokens_details: None,
                                                credits: None,
                                            }),
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
        let mut account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        if account
            .extra
            .get("kind")
            .and_then(serde_json::Value::as_str)
            == Some("api_key")
            || account
                .extra
                .get("using_api")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            return Err(MgError::bad_request(
                "Grok API Key accounts do not expose subscription quota",
            ));
        }
        self.ensure_fresh_token(&mut account).await?;
        let base = account
            .extra
            .get("base_url")
            .or_else(|| account.extra.get("baseUrl"))
            .and_then(serde_json::Value::as_str)
            .filter(|base| !base.trim().is_empty())
            .unwrap_or(CLI_BASE)
            .trim_end_matches('/')
            .to_string();
        let mut refreshed = false;
        let response = loop {
            let mut request = self
                .client
                .get(format!("{base}/billing?format=credits"))
                .header(
                    "Authorization",
                    format!("Bearer {}", account.access_token),
                )
                .header("Accept", "application/json");
            if base != API_BASE {
                request = with_cli_headers(request, &account);
            }
            let response = request
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .map_err(|error| MgError::provider(502, error.to_string(), "network_error"))?;
            if matches!(response.status().as_u16(), 401 | 403)
                && !refreshed
                && !account.refresh_token.is_empty()
            {
                refreshed = true;
                let failed_token = account.access_token.clone();
                match self.refresh_after_401(&mut account, &failed_token).await {
                    Ok(_) => continue,
                    Err(error) => {
                        if matches!(error.status_code(), 401 | 403) {
                            self.pool
                                .set_credential_invalid(account_id, "凭证已失效，请重新登录 Grok 账号");
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
            let body = response.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 401 | 403) {
                self.pool
                    .set_credential_invalid(account_id, "凭证已失效，请重新登录 Grok 账号");
                return Err(MgError::provider(
                    401,
                    "Grok 账号凭证已失效，请重新登录后再试",
                    "credential_invalid",
                ));
            }
            if crate::is_account_level_failure(status.as_u16()) {
                self.pool
                    .mark_cooldown(account_id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(account_id, &format!("upstream HTTP {}", status.as_u16()));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Grok quota query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = parse_grok_quota(&payload, mg_core::now_secs())?;
        self.pool.update_quota(account_id, quota.clone())?;
        Ok(quota)
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let mut account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        let was_expiring = account.expires_at <= mg_core::now_secs() + 300.0;
        if was_expiring && account.refresh_token.is_empty() {
            return Err(MgError::unauthorized(
                "Grok account has no refresh token; sign in again",
            ));
        }
        self.ensure_fresh_token(&mut account).await?;
        Ok(serde_json::json!({
            "ok": true,
            "message": if was_expiring { "Grok 凭证已刷新" } else { "当前凭证仍有效" }
        }))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri=http://127.0.0.1:{}/callback&scope=openid%20profile%20email%20offline_access%20grok-cli:access%20api:access",
            AUTHORIZE_URL, CLIENT_ID, REDIRECT_PORT
        );

        let login_id = uuid::Uuid::new_v4().simple().to_string();

        Ok(serde_json::json!({
            "login_id": login_id,
            "url": auth_url,
            "callback_port": REDIRECT_PORT,
        }))
    }

    async fn poll_login(&self, _login_id: &str) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "status": "pending" }))
    }
}

#[cfg(test)]
mod tests {
    use super::{enrich_grok_context, parse_grok_quota, GrokProvider, ModelEntry};
    use serde_json::json;

    #[test]
    fn refreshes_oauth_tokens_before_they_expire() {
        let mut account: mg_core::Account = serde_json::from_value(json!({
            "id": "grok-test",
            "access_token": "a",
            "expires_at": mg_core::now_secs() + 3600.0
        }))
        .unwrap();
        assert!(!GrokProvider::token_needs_refresh(&account));
        account.expires_at = mg_core::now_secs() + 60.0;
        assert!(GrokProvider::token_needs_refresh(&account));
        account.access_token.clear();
        assert!(GrokProvider::token_needs_refresh(&account));
    }

    #[test]
    fn exposes_the_long_context_window_as_the_client_limit() {
        let mut model: ModelEntry = serde_json::from_value(json!({
            "id": "grok-4.7",
            "context_window": 256000,
            "context_windows": [256000, 500000]
        }))
        .unwrap();
        enrich_grok_context(&mut model);
        assert_eq!(model.extra["context_window"], 256000);
        assert_eq!(model.extra["max_context"], 500000);
        assert_eq!(model.extra["context_length"], 500000);
        assert_eq!(model.extra["context_windows"], json!([256000, 500000]));
    }

    #[test]
    fn parses_grok_usage_as_remaining_percentage() {
        let quota = parse_grok_quota(
            &json!({"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-25T00:00:00Z"},"creditUsagePercent":37}}),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(63.0));
        assert_eq!(quota.details["buckets"][0]["label"], "本周额度");
        assert!(parse_grok_quota(&json!({}), 12.0).is_err());
    }
}
