use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mg_core::account::{Account, AccountPool, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_protocols::gemini::GeminiChunk;
use mg_protocols::openai::{ChatCompletionChunk, ChatCompletionRequest, ModelEntry};
use mg_protocols::transform::{chat_to_gemini, gemini_to_chat_chunk};
use reqwest::Client;

use crate::catalog::{fallback_models, parse_model_entries};
use crate::traits::{BoxStream, Provider};

fn quota_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn parse_antigravity_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {
    let data = payload.get("response").unwrap_or(payload);
    let models = data
        .get("models")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            MgError::provider(
                502,
                "Antigravity returned no model quota data",
                "invalid_response",
            )
        })?;
    let mut buckets = Vec::new();
    for (model, info) in models {
        let Some(fraction) = info
            .pointer("/quotaInfo/remainingFraction")
            .and_then(quota_number)
        else {
            continue;
        };
        buckets.push(serde_json::json!({
            "label": info.get("displayName").and_then(serde_json::Value::as_str).filter(|name| !name.is_empty()).unwrap_or(model),
            "remaining": (fraction * 100.0).clamp(0.0, 100.0),
            "unit": "%",
            "reset_at": info.pointer("/quotaInfo/resetTime")
        }));
    }
    buckets.sort_by(|left, right| {
        left.get("remaining")
            .and_then(quota_number)
            .partial_cmp(&right.get("remaining").and_then(quota_number))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let primary = buckets.first().ok_or_else(|| {
        MgError::provider(
            502,
            "Antigravity returned no remaining quotas",
            "invalid_response",
        )
    })?;
    let remaining = primary
        .get("remaining")
        .and_then(quota_number)
        .unwrap_or(0.0);
    let reset_at = primary.get("reset_at").and_then(quota_number);
    Ok(AccountQuota {
        checked_at,
        remaining: Some(remaining),
        limit: Some(100.0),
        reset_at,
        details: serde_json::json!({
            "label": "最低模型剩余",
            "unit": "%",
            "buckets": buckets
        }),
    })
}

pub const HOST_PROD: &str = "https://cloudcode-pa.googleapis.com";
pub const HOST_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const STREAM_GENERATE: &str = "/v1internal:streamGenerateContent?alt=sse";
pub const LOAD_CODE_ASSIST: &str = "/v1internal:loadCodeAssist";
pub const ONBOARD_USER: &str = "/v1internal:onboardUser";
pub const FETCH_MODELS: &str = "/v1internal:fetchAvailableModels";

pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo";

pub const CALLBACK_PORT: u16 = 51121;
pub const USER_AGENT: &str = "antigravity/4.3.0 (Windows NT 10.0; Win64; x64)";

/// Marks the daily (dev) host as unusable so subsequent requests go straight to
/// the production host instead of stalling on a connect timeout every time.
static DAILY_HOST_BROKEN: AtomicU8 = AtomicU8::new(0);
/// Unix seconds at which the daily host may be probed again after a failure.
static DAILY_HOST_RETRY_AT: AtomicU64 = AtomicU64::new(0);
/// The dev host is probed at most once per this interval while it is failing.
const DAILY_PROBE_INTERVAL_SECS: u64 = 300;

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct AntigravityProvider {
    /// Client for short non-streaming calls (token refresh, model catalog, quota).
    /// Keeps a total timeout so a trickling response cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
    client_id: String,
    client_secret: String,
}

impl AntigravityProvider {
    pub fn new(pool: Arc<AccountPool>, client_id: String, client_secret: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        Self {
            client,
            stream_client,
            pool,
            client_id,
            client_secret,
        }
    }

    async fn ensure_fresh_token(&self, account: &mut Account) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        if account.expires_at > now + 300.0 && !account.access_token.is_empty() {
            return Ok(account.access_token.clone());
        }

        if account.refresh_token.is_empty() {
            if !account.access_token.is_empty() {
                return Ok(account.access_token.clone());
            }
            return Err(MgError::unauthorized("Account has no refresh token"));
        }

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
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
                    format!("Google token refresh failed: {}", e),
                    "network_error",
                )
            })?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                401,
                format!(
                    "Antigravity: Google refresh token rejected: {}",
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

        account.access_token = new_token.clone();
        account.expires_at = now + expires_in;
        let pool = Arc::clone(&self.pool);
        let snapshot = account.clone();
        tokio::task::spawn_blocking(move || pool.save(snapshot))
            .await
            .map_err(|error| MgError::Io(error.to_string()))??;

        Ok(new_token)
    }

    async fn discover_project_id(&self, token: &str, account: &mut Account) -> Result<String> {
        if let Some(pid) = account.extra.get("project_id").and_then(|v| v.as_str()) {
            if !pid.is_empty() {
                return Ok(pid.to_string());
            }
        }

        let body = serde_json::json!({
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "client": "998881661"
            }
        });

        let url = format!("{}{}", HOST_PROD, LOAD_CODE_ASSIST);
        let res = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(15))
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await;

        if let Ok(r) = res {
            if r.status().is_success() {
                if let Ok(data) = r.json::<serde_json::Value>().await {
                    if let Some(pid) = data
                        .get("cloudaicompanionProjectId")
                        .and_then(|v| v.as_str())
                    {
                        account
                            .extra
                            .insert("project_id".to_string(), serde_json::json!(pid));
                        let pool = Arc::clone(&self.pool);
                        let snapshot = account.clone();
                        let _ = tokio::task::spawn_blocking(move || pool.save(snapshot)).await;
                        return Ok(pid.to_string());
                    }
                }
            }
        }

        Ok("antigravity-default".to_string())
    }
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn name(&self) -> &'static str {
        "Google Antigravity"
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
            &["gemini-2.5-pro", "gemini-2.5-flash", "claude-3-7-sonnet"],
            "google",
        );

        let mut account = match self.pool.choose(None, None) {
            Ok(acc) => acc,
            Err(_) => return Ok(fallback),
        };

        let token = match self.ensure_fresh_token(&mut account).await {
            Ok(t) => t,
            Err(_) => return Ok(fallback),
        };

        let url = format!("{}{}", HOST_PROD, FETCH_MODELS);
        let project = self.discover_project_id(&token, &mut account).await.ok();
        let request_body = project
            .as_deref()
            .map(|project| serde_json::json!({"project": project}))
            .unwrap_or_else(|| serde_json::json!({}));
        let res = match self
            .client
            .post(&url)
            .timeout(Duration::from_secs(15))
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", USER_AGENT)
            .json(&request_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return Ok(fallback),
        };

        if !res.status().is_success() {
            return Ok(fallback);
        }
        let data = match res.json::<serde_json::Value>().await {
            Ok(data) => data,
            Err(_) => return Ok(fallback),
        };
        let models = parse_model_entries(&data, "google");
        Ok(if models.is_empty() { fallback } else { models })
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
        let mut account = self.pool.choose(session_id, None)?;
        let token = self.ensure_fresh_token(&mut account).await?;
        let project_id = self.discover_project_id(&token, &mut account).await?;

        let model_name = req.model.clone();
        let envelope = chat_to_gemini(&req, Some(project_id))?;

        // Primary: daily host, Secondary: prod host.
        // The dev (daily) host is only probed when it is not marked broken, or
        // when the re-probe interval has elapsed, so an unreachable dev host
        // costs at most one bounded connect attempt per interval instead of
        // stalling every single request.
        let now = unix_now_secs();
        let probe_daily = DAILY_HOST_BROKEN.load(Ordering::Relaxed) == 0
            || now >= DAILY_HOST_RETRY_AT.load(Ordering::Relaxed);

        let mut daily_res: Option<reqwest::Result<reqwest::Response>> = None;
        if probe_daily {
            let primary_url = format!("{}{}", HOST_DAILY, STREAM_GENERATE);
            let attempt = self
                .stream_client
                .post(&primary_url)
                .header("Authorization", format!("Bearer {}", token))
                .header("User-Agent", USER_AGENT)
                .header("Accept", "text/event-stream")
                .json(&envelope)
                .send()
                .await;

            // Only transport/connect errors and 5xx justify a second host; a 4xx
            // means the request itself was rejected and must not be double-sent.
            let retryable = match &attempt {
                Ok(response) => response.status().is_server_error(),
                Err(_) => true,
            };
            if retryable {
                DAILY_HOST_BROKEN.store(1, Ordering::Relaxed);
                DAILY_HOST_RETRY_AT.store(
                    now.saturating_add(DAILY_PROBE_INTERVAL_SECS),
                    Ordering::Relaxed,
                );
            } else {
                DAILY_HOST_BROKEN.store(0, Ordering::Relaxed);
                daily_res = Some(attempt);
            }
        }

        // Fallback to prod host if daily failed with a retryable error
        // (or was skipped because it is known-broken).
        let res = match daily_res {
            Some(res) => res,
            None => {
                let prod_url = format!("{}{}", HOST_PROD, STREAM_GENERATE);
                self.stream_client
                    .post(&prod_url)
                    .header("Authorization", format!("Bearer {}", token))
                    .header("User-Agent", USER_AGENT)
                    .header("Accept", "text/event-stream")
                    .json(&envelope)
                    .send()
                    .await
            }
        };

        let response = res.map_err(|e| {
            MgError::provider(
                502,
                format!("Antigravity upstream error: {}", e),
                "network_error",
            )
        })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            // This is the FINAL attempt for the account (the intermediate daily-host
            // probe only sets a flag and falls through to prod, so it never reaches
            // here). Only an account-level status should bench the account.
            if crate::is_account_level_failure(status) {
                self.pool
                    .mark_cooldown(&account.id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(&account.id, &format!("upstream HTTP {}", status));
            }
            return Err(MgError::provider(
                status,
                format!(
                    "Antigravity rejected request: {}",
                    crate::upstream_error_body(status, &body)
                ),
                "upstream_error",
            ));
        }

        let chunk_id: Arc<str> = Arc::from(format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()));
        let model_name: Arc<str> = Arc::from(model_name);
        let stream = response
            .bytes_stream()
            .eventsource()
            .filter_map(move |event_res| {
                let model = Arc::clone(&model_name);
                let cid = Arc::clone(&chunk_id);
                async move {
                    match event_res {
                        Ok(event) => {
                            if event.data == "[DONE]" {
                                return None;
                            }
                            if let Ok(gemini_resp) =
                                serde_json::from_str::<GeminiChunk>(&event.data)
                            {
                                let resp = gemini_resp.response.or(gemini_resp.raw)?;
                                if let Some(chunk) = gemini_to_chat_chunk(resp, &model, &cid) {
                                    return Some(Ok(chunk));
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

        let token = self.ensure_fresh_token(&mut account).await?;
        let project = self.discover_project_id(&token, &mut account).await?;
        let url = format!("{}{}", HOST_PROD, FETCH_MODELS);
        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("User-Agent", USER_AGENT)
            .json(&serde_json::json!({"project": project}))
            .send()
            .await
            .map_err(|e| MgError::provider(502, e.to_string(), "network_error"))?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            if crate::is_account_level_failure(status.as_u16()) {
                self.pool
                    .mark_cooldown(account_id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(account_id, &format!("upstream HTTP {}", status.as_u16()));
            }
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "Antigravity quota query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = res
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = parse_antigravity_quota(&payload, mg_core::now_secs())?;
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
                "Antigravity account has no refresh token; sign in again",
            ));
        }
        self.ensure_fresh_token(&mut account).await?;
        Ok(serde_json::json!({
            "ok": true,
            "message": if was_expiring { "Antigravity 凭证已刷新" } else { "当前凭证仍有效" }
        }))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        let auth_url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id={}&redirect_uri=http://localhost:{}/oauth-callback&scope=https://www.googleapis.com/auth/cloud-platform%20https://www.googleapis.com/auth/userinfo.email&access_type=offline&prompt=consent",
            self.client_id, CALLBACK_PORT
        );

        let login_id = uuid::Uuid::new_v4().simple().to_string();

        Ok(serde_json::json!({
            "login_id": login_id,
            "url": auth_url,
            "callback_port": CALLBACK_PORT,
        }))
    }

    async fn poll_login(&self, _login_id: &str) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "status": "pending",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_antigravity_quota;
    use serde_json::json;

    #[test]
    fn parses_model_remaining_fractions_and_rejects_missing_quota() {
        let quota = parse_antigravity_quota(
            &json!({"response":{"models":{
                "gemini": {"displayName":"Gemini", "quotaInfo":{"remainingFraction":0.42,"resetTime":"2026-10-22T00:00:00Z"}},
                "claude": {"quotaInfo":{"remainingFraction":0.1}}
            }}}),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(10.0));
        assert_eq!(quota.details["buckets"][0]["label"], "claude");
        assert!(parse_antigravity_quota(&json!({"models":{}}), 12.0).is_err());
    }
}
