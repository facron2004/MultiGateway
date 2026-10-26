use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use mg_core::account::{Account, AccountPool, AccountQuota};
use mg_core::error::{MgError, Result};
use mg_protocols::openai::{ChatCompletionChunk, ChatCompletionRequest, ModelEntry};
use parking_lot::RwLock;
use reqwest::Client;
use tracing::debug;

use crate::catalog::{fallback_models, parse_model_entries};
use crate::traits::{BoxStream, Provider};

pub const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const GITHUB_USER_URL: &str = "https://api.github.com/user";
pub const DEFAULT_API_BASE: &str = "https://api.githubcopilot.com";

pub const EDITOR_VERSION: &str = "vscode/1.104.3";
pub const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
pub const USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
pub const GITHUB_API_VERSION: &str = "2025-04-01";

fn quota_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))?;
    number.is_finite().then_some(number)
}

fn parse_copilot_quota(payload: &serde_json::Value, checked_at: f64) -> Result<AccountQuota> {
    let snapshots = payload
        .get("quota_snapshots")
        .and_then(serde_json::Value::as_object);
    let legacy = payload
        .get("limited_user_quotas")
        .and_then(serde_json::Value::as_object);
    let totals = payload
        .get("monthly_quotas")
        .and_then(serde_json::Value::as_object);
    let reset = payload
        .get("quota_reset_date_utc")
        .or_else(|| payload.get("quota_reset_date"))
        .cloned();
    let mut buckets = Vec::new();

    for (key, label) in [
        ("premium_interactions", "高级请求"),
        ("chat", "对话"),
        ("completions", "代码补全"),
    ] {
        let snapshot = snapshots.and_then(|items| items.get(key)).or_else(|| {
            (key == "premium_interactions")
                .then(|| snapshots.and_then(|items| items.get("premium_models")))
                .flatten()
        });
        let legacy_remaining = legacy.and_then(|items| items.get(key));
        let entitlement = totals
            .and_then(|items| items.get(key))
            .and_then(quota_number);
        let has_quota = snapshot
            .and_then(|value| value.get("has_quota"))
            .and_then(serde_json::Value::as_bool);
        let unlimited = snapshot
            .and_then(|value| value.get("unlimited"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if has_quota == Some(false) && entitlement.unwrap_or(0.0) == 0.0 && !unlimited {
            continue;
        }

        let limit = snapshot
            .and_then(|value| value.get("entitlement"))
            .and_then(quota_number)
            .or(entitlement);
        let mut unit = if snapshot
            .and_then(|value| value.get("token_based_billing"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            && key != "completions"
        {
            "额度"
        } else {
            "次"
        };
        let mut remaining = snapshot
            .and_then(|value| value.get("quota_remaining"))
            .and_then(quota_number)
            .or_else(|| {
                snapshot
                    .and_then(|value| value.get("remaining"))
                    .and_then(quota_number)
            })
            .or_else(|| legacy_remaining.and_then(quota_number));
        let percent = snapshot
            .and_then(|value| value.get("percent_remaining"))
            .and_then(quota_number);
        if unlimited {
            remaining = None;
        } else if remaining.is_none() {
            if let Some(percent) = percent {
                remaining = Some(
                    limit
                        .map(|limit| limit * percent / 100.0)
                        .unwrap_or(percent),
                );
                if limit.is_none() {
                    unit = "%";
                }
            }
        }
        if !unlimited && remaining.is_none() {
            continue;
        }
        let reset_at = snapshot
            .and_then(|value| value.get("quota_reset_at"))
            .cloned()
            .or_else(|| reset.clone());
        buckets.push(serde_json::json!({
            "label": label,
            "remaining": if unlimited { serde_json::json!("unlimited") } else { serde_json::json!(remaining.unwrap_or(0.0)) },
            "limit": limit,
            "unit": unit,
            "reset_at": reset_at
        }));
    }
    if buckets.is_empty() {
        return Err(MgError::provider(
            502,
            "GitHub Copilot returned no quota buckets",
            "invalid_response",
        ));
    }
    let primary = &buckets[0];
    let primary_unlimited =
        primary.get("remaining").and_then(serde_json::Value::as_str) == Some("unlimited");
    let remaining = (!primary_unlimited)
        .then(|| primary.get("remaining").and_then(quota_number))
        .flatten();
    let limit = primary.get("limit").and_then(quota_number);
    let unit = primary
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("次");
    let label = primary
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Copilot 额度");
    Ok(AccountQuota {
        checked_at,
        remaining,
        limit,
        reset_at: None,
        details: serde_json::json!({
            "label": label,
            "unit": unit,
            "unlimited": primary_unlimited,
            "buckets": buckets
        }),
    })
}

#[derive(Debug, Clone)]
struct CachedJwt {
    token: String,
    expires_at: f64,
    api_base: String,
}

#[derive(Debug, Clone)]
struct PendingLogin {
    device_code: String,
    #[allow(dead_code)]
    expires_at: f64,
    interval: u64,
    /// Unix timestamp (seconds) when this pending login was created. Used to
    /// garbage-collect device-login sessions the user never completed.
    created_at: f64,
}

/// Pending device logins older than this are considered abandoned and are
/// pruned on the next start_login/poll_login call.
const PENDING_LOGIN_TTL_SECS: f64 = 600.0;

pub struct CopilotProvider {
    /// Client for short non-streaming calls (JWT exchange, catalog, quota,
    /// device-code login). Keeps a total timeout so a trickling response
    /// cannot hang forever.
    client: Client,
    /// Client for streaming chat: no total timeout (long agent streams are
    /// legitimate); bounded by connect + per-read timeouts instead.
    stream_client: Client,
    pool: Arc<AccountPool>,
    jwt_cache: RwLock<HashMap<String, CachedJwt>>, // account_id -> CachedJwt
    pending: RwLock<HashMap<String, PendingLogin>>, // login_id -> PendingLogin
}

impl CopilotProvider {
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
            jwt_cache: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
        }
    }

    async fn get_copilot_token(&self, account: &Account) -> Result<(String, String)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        // 1. Check cached JWT
        {
            let cache = self.jwt_cache.read();
            if let Some(jwt) = cache.get(&account.id) {
                if jwt.expires_at > now + 180.0 {
                    return Ok((jwt.token.clone(), jwt.api_base.clone()));
                }
            }
        }

        // 2. Fetch new Copilot JWT from GitHub token
        let res = self
            .client
            .get(COPILOT_TOKEN_URL)
            .header("Authorization", format!("Bearer {}", account.access_token))
            .header("User-Agent", USER_AGENT)
            .header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("X-Github-Api-Version", GITHUB_API_VERSION)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                MgError::provider(
                    502,
                    format!("Failed to reach GitHub Copilot auth: {}", e),
                    "network_error",
                )
            })?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status,
                format!(
                    "GitHub Copilot token exchange failed: {}",
                    crate::upstream_error_body(status, &body)
                ),
                "copilot_token_error",
            ));
        }

        let data: serde_json::Value = res.json().await.map_err(|e| MgError::Json(e.to_string()))?;

        let token = data
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MgError::provider(500, "Missing token in response", "invalid_response"))?
            .to_string();

        let expires_at = data
            .get("expires_at")
            .and_then(|v| v.as_f64())
            .unwrap_or(now + 1200.0);

        let api_base = data
            .pointer("/endpoints/api")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_API_BASE)
            .to_string();

        let cached = CachedJwt {
            token: token.clone(),
            expires_at,
            api_base: api_base.clone(),
        };

        self.jwt_cache.write().insert(account.id.clone(), cached);

        Ok((token, api_base))
    }

    /// Drops pending device-login sessions created more than
    /// [`PENDING_LOGIN_TTL_SECS`] ago so abandoned logins cannot accumulate.
    fn prune_pending(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.pending
            .write()
            .retain(|_, p| now - p.created_at < PENDING_LOGIN_TTL_SECS);
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn id(&self) -> &'static str {
        "copilot"
    }

    fn name(&self) -> &'static str {
        "GitHub Copilot"
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
        // Fallback models when no account or offline
        let fallback = fallback_models(
            &["gpt-4.1", "gpt-4o", "claude-sonnet-4", "o3-mini"],
            "github",
        );

        let account = match self.pool.choose(None, None) {
            Ok(acc) => acc,
            Err(_) => return Ok(fallback),
        };

        let (jwt, api_base) = match self.get_copilot_token(&account).await {
            Ok(t) => t,
            Err(_) => return Ok(fallback),
        };

        let url = format!("{}/models", api_base);
        let res = match self
            .client
            .get(&url)
            .timeout(Duration::from_secs(20))
            .header("Authorization", format!("Bearer {}", jwt))
            .header("User-Agent", USER_AGENT)
            .header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("X-Github-Api-Version", GITHUB_API_VERSION)
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
        let models = parse_model_entries(&data, "github");
        Ok(if models.is_empty() { fallback } else { models })
    }

    async fn chat(
        &self,
        mut req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)> {
        let account = self.pool.choose(session_id, None)?;
        let (jwt, api_base) = self.get_copilot_token(&account).await?;

        req.stream = Some(true); // Always stream to upstream for ultra-low TTFT

        let url = format!("{}/chat/completions", api_base);
        let res = self
            .stream_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("User-Agent", USER_AGENT)
            .header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("X-Github-Api-Version", GITHUB_API_VERSION)
            .header("Openai-Organization", "github-copilot")
            .header("Copilot-Integration-Id", "vscode-chat")
            .header("X-Initiator", "user")
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                MgError::provider(
                    502,
                    format!("Copilot upstream error: {}", e),
                    "network_error",
                )
            })?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            if crate::is_account_level_failure(status) {
                self.pool
                    .mark_cooldown(&account.id, crate::ACCOUNT_COOLDOWN_SECS);
                self.pool
                    .set_error(&account.id, &format!("upstream HTTP {}", status));
            }
            return Err(MgError::provider(
                status,
                format!(
                    "Copilot upstream rejected request: {}",
                    crate::upstream_error_body(status, &body)
                ),
                "upstream_error",
            ));
        }

        let stream = res
            .bytes_stream()
            .eventsource()
            .filter_map(|event_res| async move {
                match event_res {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            return None;
                        }
                        match serde_json::from_str::<ChatCompletionChunk>(&event.data) {
                            Ok(chunk) => Some(Ok(chunk)),
                            Err(e) => {
                                debug!("Failed to parse Copilot SSE chunk: {}", e);
                                None
                            }
                        }
                    }
                    Err(e) => Some(Err(MgError::provider(500, e.to_string(), "stream_error"))),
                }
            });

        Ok((Box::pin(stream), account.id, None))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;

        let (jwt, _) = self.get_copilot_token(&account).await?;

        // Query Copilot user metadata
        let res = self
            .client
            .get("https://api.github.com/copilot_internal/user")
            .header("Authorization", format!("Bearer {}", jwt))
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| MgError::provider(502, e.to_string(), "network_error"))?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                status.as_u16(),
                format!(
                    "GitHub Copilot quota query failed: {}",
                    crate::upstream_error_body(status.as_u16(), &body)
                ),
                "upstream_error",
            ));
        }
        let payload = res
            .json::<serde_json::Value>()
            .await
            .map_err(|error| MgError::provider(502, error.to_string(), "invalid_response"))?;
        let quota = parse_copilot_quota(&payload, mg_core::now_secs())?;
        self.pool.update_quota(account_id, quota.clone())?;
        Ok(quota)
    }

    async fn refresh_credentials(&self, account_id: &str) -> Result<serde_json::Value> {
        let account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;
        self.get_copilot_token(&account).await?;
        Ok(serde_json::json!({"ok": true, "message": "GitHub Copilot 访问凭证已更新"}))
    }

    async fn start_login(&self, _redirect_url: Option<&str>) -> Result<serde_json::Value> {
        self.prune_pending();

        let params = [("client_id", GITHUB_CLIENT_ID), ("scope", "read:user")];

        let res = self
            .client
            .post(DEVICE_CODE_URL)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| MgError::provider(502, e.to_string(), "network_error"))?;

        if !res.status().is_success() {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(MgError::provider(
                400,
                format!(
                    "Copilot device code request failed: {}",
                    crate::upstream_error_body(status, &body)
                ),
                "device_code_error",
            ));
        }

        let data: serde_json::Value = res.json().await.map_err(|e| MgError::Json(e.to_string()))?;

        let device_code = data
            .get("device_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MgError::bad_request("Missing device_code"))?
            .to_string();

        let user_code = data
            .get("user_code")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let verification_uri = data
            .get("verification_uri")
            .and_then(|v| v.as_str())
            .unwrap_or("https://github.com/login/device")
            .to_string();

        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(900);

        let interval = data.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);

        let login_id = uuid::Uuid::new_v4().simple().to_string();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        self.pending.write().insert(
            login_id.clone(),
            PendingLogin {
                device_code,
                expires_at: now + expires_in as f64,
                interval,
                created_at: now,
            },
        );

        Ok(serde_json::json!({
            "login_id": login_id,
            "user_code": user_code,
            "url": verification_uri,
            "interval": interval,
            "expires_at": now + expires_in as f64,
        }))
    }

    async fn poll_login(&self, login_id: &str) -> Result<serde_json::Value> {
        self.prune_pending();

        let pending = {
            let p_map = self.pending.read();
            p_map.get(login_id).cloned().ok_or_else(|| {
                MgError::provider(400, "Device code expired or invalid", "expired_token")
            })?
        };

        let params = [
            ("client_id", GITHUB_CLIENT_ID),
            ("device_code", pending.device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];

        let res = self
            .client
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| MgError::provider(502, e.to_string(), "network_error"))?;

        let data: serde_json::Value = res.json().await.map_err(|e| MgError::Json(e.to_string()))?;

        if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
            match err {
                "authorization_pending" => {
                    return Ok(serde_json::json!({
                        "status": "pending",
                        "interval": pending.interval,
                    }));
                }
                "slow_down" => {
                    return Ok(serde_json::json!({
                        "status": "pending",
                        "interval": pending.interval + 5,
                    }));
                }
                _ => {
                    self.pending.write().remove(login_id);
                    return Err(MgError::provider(400, format!("OAuth error: {}", err), err));
                }
            }
        }

        let access_token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MgError::bad_request("Missing access_token"))?
            .to_string();

        // Query user info
        let user_res = self
            .client
            .get(GITHUB_USER_URL)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", USER_AGENT)
            .send()
            .await;

        let mut login_name = "github-user".to_string();
        let mut email = String::new();

        if let Ok(u_res) = user_res {
            if let Ok(u_data) = u_res.json::<serde_json::Value>().await {
                if let Some(login) = u_data.get("login").and_then(|v| v.as_str()) {
                    login_name = login.to_string();
                }
                if let Some(em) = u_data.get("email").and_then(|v| v.as_str()) {
                    email = em.to_string();
                }
            }
        }

        let account_id = format!("copilot_{}", login_name.to_lowercase());
        let account = Account {
            id: account_id.clone(),
            label: login_name,
            email,
            access_token,
            refresh_token: String::new(),
            expires_at: 0.0,
            region: "intl".to_string(),
            enabled: true,
            weight: 1,
            daily_tokens: 0,
            error: String::new(),
            credential_invalid: false,
            quota: None,
            credits: None,
            checkin: None,
            extra: HashMap::new(),
            provider_specific: None,
            user_type: None,
            organization_id: None,
            organization_name: None,
        };

        self.pool.save(account)?;
        self.pending.write().remove(login_id);

        Ok(serde_json::json!({
            "status": "authenticated",
            "account_id": account_id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_copilot_quota;
    use serde_json::json;

    #[test]
    fn parses_copilot_quota_buckets_and_skips_unentitled_premium_quota() {
        let quota = parse_copilot_quota(
            &json!({
                "quota_snapshots": {
                    "premium_interactions": {"entitlement": 300, "quota_remaining": 210},
                    "chat": {"entitlement": 0, "has_quota": false},
                    "completions": {"percent_remaining": 75}
                },
                "monthly_quotas": {"chat": 0},
                "quota_reset_date_utc": "2026-11-01T00:00:00Z"
            }),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(210.0));
        assert_eq!(quota.limit, Some(300.0));
        assert_eq!(quota.details["buckets"].as_array().unwrap().len(), 2);
        assert!(parse_copilot_quota(&json!({}), 12.0).is_err());
    }
}
