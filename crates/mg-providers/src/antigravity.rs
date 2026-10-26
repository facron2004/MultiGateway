use std::collections::HashMap;
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
use parking_lot::RwLock;
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
    let mut buckets: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (model, info) in models {
        // The catalog also carries internal helper ids (`chat_*`, `tab_*`)
        // that have no display name and no real user quota. Counting them
        // made the dashboard advertise "26 models" and could show a bogus
        // lowest remaining quota for a bucket the user cannot consume.
        if is_internal_antigravity_model(model) {
            continue;
        }
        let Some(display_name) = info
            .get("displayName")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let Some(fraction) = info
            .pointer("/quotaInfo/remainingFraction")
            .and_then(quota_number)
        else {
            continue;
        };
        let remaining = (fraction * 100.0).clamp(0.0, 100.0);
        let reset_at = info.pointer("/quotaInfo/resetTime").cloned();
        let bucket = serde_json::json!({
            "label": display_name,
            "remaining": remaining,
            "unit": "%",
            "reset_at": reset_at
        });
        // Several aliases can share one display name (Gemini 3.5 Flash Lite
        // appears three times). Keep the tightest remaining value per model.
        if let Some(index) = seen.get(display_name).copied() {
            let current = buckets[index]
                .get("remaining")
                .and_then(quota_number)
                .unwrap_or(f64::MAX);
            if remaining < current {
                buckets[index] = bucket;
            }
            continue;
        }
        seen.insert(display_name.to_string(), buckets.len());
        buckets.push(bucket);
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

fn is_internal_antigravity_model(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    id.starts_with("chat_") || id.starts_with("tab_")
}

/// A tier encoded in the display name, e.g. "Gemini 3.8 Flash (High)".
fn antigravity_tier_from_name(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("(extra high)") || lower.contains("(xhigh)") {
        return Some("xhigh");
    }
    if lower.contains("(high)") {
        return Some("high");
    }
    if lower.contains("(medium)") {
        return Some("medium");
    }
    if lower.contains("(low)") {
        return Some("low");
    }
    if lower.contains("(max)") {
        return Some("max");
    }
    None
}

fn antigravity_tier_from_id(id: &str) -> Option<&'static str> {
    match id.to_ascii_lowercase().rsplit(['-', '_']).next()? {
        "high" => Some("high"),
        "medium" | "med" => Some("medium"),
        "low" | "light" => Some("low"),
        "xhigh" | "max" => Some("xhigh"),
        _ => None,
    }
}

fn antigravity_tier_from_budget(budget: f64) -> Option<&'static str> {
    if budget <= 0.0 {
        return None;
    }
    Some(if budget <= 2048.0 {
        "low"
    } else if budget <= 8192.0 {
        "medium"
    } else if budget <= 16384.0 {
        "high"
    } else if budget <= 24576.0 {
        "xhigh"
    } else {
        "max"
    })
}

/// Copy the Cloud Code Assist catalog's own fields onto the keys the shared
/// model-metadata layer reads. Without this every Antigravity model looked
/// text-only/reasoning-optional and its context window was dropped.
fn enrich_antigravity_model(mut model: ModelEntry) -> ModelEntry {
    let display_name = model
        .extra
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    if let Some(name) = &display_name {
        model
            .extra
            .insert("display_name".to_string(), serde_json::json!(name));
        model
            .extra
            .insert("name".to_string(), serde_json::json!(name));
    }
    if let Some(context) = model
        .extra
        .get("maxTokens")
        .or_else(|| model.extra.get("maxContextWindowTokens"))
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value > 0)
    {
        model
            .extra
            .insert("context_window".to_string(), serde_json::json!(context));
        model
            .extra
            .insert("context_length".to_string(), serde_json::json!(context));
        model
            .extra
            .insert("max_input_tokens".to_string(), serde_json::json!(context));
    }
    if let Some(output) = model
        .extra
        .get("maxOutputTokens")
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value > 0)
    {
        model
            .extra
            .insert("max_output_tokens".to_string(), serde_json::json!(output));
    }
    match model
        .extra
        .get("supportsImages")
        .and_then(serde_json::Value::as_bool)
    {
        Some(vision) => {
            model
                .extra
                .insert("supports_images".to_string(), serde_json::json!(vision));
            model
                .extra
                .insert("supports_vision".to_string(), serde_json::json!(vision));
        }
        // The Cloud Code catalog omits the field on text-only entries; the
        // provider-wide default would otherwise mark them image-capable.
        None => {
            model
                .extra
                .insert("supports_images".to_string(), serde_json::json!(false));
            model
                .extra
                .insert("supports_vision".to_string(), serde_json::json!(false));
        }
    }

    match model
        .extra
        .get("supportsThinking")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => {
            model
                .extra
                .insert("supports_reasoning".to_string(), serde_json::json!(true));
            let budget = model
                .extra
                .get("thinkingBudget")
                .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|v| v as f64)));
            let fixed_tier = display_name
                .as_deref()
                .and_then(antigravity_tier_from_name)
                .or_else(|| antigravity_tier_from_id(&model.id))
                .or_else(|| budget.and_then(antigravity_tier_from_budget));
            // A variant pinned to one tier advertises only that tier; a model
            // with no tier in its id/name keeps the dynamic ladder.
            if let Some(tier) = fixed_tier {
                model.extra.insert(
                    "reasoning_effort_config".to_string(),
                    serde_json::json!({
                        "support_thinking": true,
                        "options": [tier],
                        "default_level": tier
                    }),
                );
            }
        }
        Some(false) | None => {
            model
                .extra
                .insert("supports_reasoning".to_string(), serde_json::json!(false));
        }
    }
    model
}

/// One advertised model family and the upstream variant to use per tier.
#[derive(Debug, Clone, Default)]
struct AntigravityRoute {
    default_upstream: String,
    tiers: HashMap<String, String>,
}

fn effort_rank(tier: &str) -> usize {
    crate::EFFORT_LEVELS
        .iter()
        .position(|level| *level == tier)
        .unwrap_or(crate::EFFORT_LEVELS.len())
}

fn canonical_effort(name: &str) -> Option<&'static str> {
    Some(match name.trim().to_ascii_lowercase().replace(' ', "_").as_str() {
        "minimal" | "min" => "minimal",
        "light" | "low" => "low",
        "medium" | "mid" | "normal" | "default" | "balanced" => "medium",
        "high" => "high",
        "extra_high" | "extra-high" | "x-high" | "xhigh" | "very_high" => "xhigh",
        "max" | "maximum" | "ultimate" | "ultra" => "max",
        "none" | "off" | "disabled" => "minimal",
        _ => return None,
    })
}

/// A display name without its trailing strength marker.
fn display_base(name: &str) -> String {
    let name = name.trim();
    if let Some(index) = name.rfind(" (") {
        if name.ends_with(')') {
            return name[..index].trim().to_string();
        }
    }
    name.to_string()
}

fn family_slug(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            slug.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_' | '/')
            && !slug.ends_with('-')
            && !slug.is_empty()
        {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Group strength variants under one family key.
///
/// The catalog publishes e.g. `gemini-3.8-flash-low` / `-medium` / `-high` as
/// three models. The display name is the most reliable family key because some
/// families mix id shapes (`gemini-3.5-flash-low` vs `gemini-3-flash-agent`
/// are both "Gemini 3.5 Flash (Medium/High)").
fn family_key(model: &ModelEntry) -> String {
    if let Some(name) = model
        .extra
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
    {
        return family_slug(&display_base(name));
    }
    let mut id = model.id.to_ascii_lowercase();
    for suffix in [
        "-extra-low",
        "-extra-high",
        "-xhigh",
        "-high",
        "-medium",
        "-low",
        "-max",
        "-tiered",
    ] {
        if let Some(stripped) = id.strip_suffix(suffix) {
            id = stripped.to_string();
            break;
        }
    }
    id
}

fn model_tier(model: &ModelEntry) -> Option<String> {
    model
        .extra
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .and_then(antigravity_tier_from_name)
        .or_else(|| antigravity_tier_from_id(&model.id))
        .map(str::to_string)
}

fn preferred_default_tier(tiers: &[String]) -> String {
    for preferred in ["high", "medium", "low", "xhigh", "max", "minimal"] {
        if tiers.iter().any(|tier| tier == preferred) {
            return preferred.to_string();
        }
    }
    tiers.first().cloned().unwrap_or_else(|| "high".to_string())
}

/// The id clients see for a merged family.
///
/// Prefer stripping the default variant's own tier suffix (`claude-opus-5-5-high`
/// -> `claude-opus-5-5`, `gemini-3.8-flash-high` -> `gemini-3.8-flash`). Some
/// families use unrelated ids per tier (`gemini-3-flash-agent` is the High
/// variant of "Gemini 3.5 Flash"); those fall back to the display-name slug.
fn advertised_model_id(chosen: &ModelEntry, default_tier: &str) -> String {
    let suffix = match default_tier {
        "minimal" => Some("-minimal"),
        "low" => Some("-low"),
        "medium" => Some("-medium"),
        "high" => Some("-high"),
        "xhigh" => Some("-xhigh"),
        "max" => Some("-max"),
        _ => None,
    };
    if let Some(suffix) = suffix {
        if let Some(base) = chosen.id.strip_suffix(suffix) {
            if !base.is_empty() {
                return base.to_string();
            }
        }
    }
    let display = chosen
        .extra
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(display_base)
        .unwrap_or_else(|| chosen.id.clone());
    family_slug(&display)
}

/// Merge per-strength ids into one model per family and remember how to route
/// a requested tier back to the corresponding upstream id.
fn consolidate_models(
    models: Vec<ModelEntry>,
) -> (Vec<ModelEntry>, HashMap<String, AntigravityRoute>) {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<(ModelEntry, Option<String>)>> = HashMap::new();
    for model in models {
        let key = family_key(&model);
        let tier = model_tier(&model);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push((model, tier));
    }

    let mut out = Vec::new();
    let mut routes = HashMap::new();
    for key in order {
        let Some(members) = groups.remove(&key) else {
            continue;
        };
        let mut tiers: Vec<String> = members.iter().filter_map(|(_, tier)| tier.clone()).collect();
        tiers.sort_by_key(|tier| effort_rank(tier));
        tiers.dedup();

        if tiers.len() >= 2 {
            let default_tier = preferred_default_tier(&tiers);
            let mut tier_ids: HashMap<String, String> = HashMap::new();
            for (model, tier) in &members {
                if let Some(tier) = tier {
                    tier_ids
                        .entry(tier.clone())
                        .or_insert_with(|| model.id.clone());
                }
            }
            let default_upstream = tier_ids
                .get(&default_tier)
                .cloned()
                .or_else(|| members.first().map(|(model, _)| model.id.clone()))
                .unwrap_or_default();
            let chosen = members
                .iter()
                .position(|(_, tier)| tier.as_deref() == Some(default_tier.as_str()))
                .unwrap_or(0);
            let mut model = members[chosen].0.clone();
            let advertised = advertised_model_id(&model, &default_tier);
            let display = model
                .extra
                .get("displayName")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .map(display_base)
                .unwrap_or_else(|| advertised.clone());
            model.id = advertised.clone();
            model
                .extra
                .insert("display_name".to_string(), serde_json::json!(display));
            model.extra.insert("name".to_string(), serde_json::json!(display));
            // Replace the single-variant ladder with the family's ladder.
            for stale in [
                "reasoning_effort_config",
                "reasoning_efforts",
                "supported_reasoning_efforts",
                "reasoning_default_effort",
            ] {
                model.extra.remove(stale);
            }
            model
                .extra
                .insert("supports_reasoning".to_string(), serde_json::json!(true));
            model
                .extra
                .insert("reasoning_efforts".to_string(), serde_json::json!(tiers));
            model.extra.insert(
                "reasoning_default_effort".to_string(),
                serde_json::json!(default_tier),
            );
            model
                .extra
                .insert("reasoning_can_disable".to_string(), serde_json::json!(false));
            model
                .extra
                .insert("upstream_models".to_string(), serde_json::json!(tier_ids));
            routes.insert(
                advertised,
                AntigravityRoute {
                    default_upstream,
                    tiers: tier_ids,
                },
            );
            out.push(model);
        } else if tiers.is_empty() && members.len() > 1 {
            // Aliases that share one display name and no strength marker are
            // the same model reached through different ids; keep the id that
            // actually matches the family name.
            let chosen = members
                .iter()
                .position(|(model, _)| model.id.eq_ignore_ascii_case(&key))
                .unwrap_or(0);
            out.push(members.into_iter().nth(chosen).unwrap().0);
        } else {
            out.extend(members.into_iter().map(|(model, _)| model));
        }
    }
    (out, routes)
}

fn requested_effort(req: &ChatCompletionRequest) -> Option<String> {
    let extra = &req.extra;
    extra
        .get("reasoning_effort")
        .or_else(|| extra.get("reasoningEffort"))
        .or_else(|| extra.get("thinking_effort"))
        .or_else(|| extra.get("reasoning").and_then(|v| v.get("effort")))
        .or_else(|| extra.get("thinking").and_then(|v| v.get("effort")))
        .or_else(|| extra.get("thinking").and_then(|v| v.get("level")))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn resolve_upstream_model(
    routes: &HashMap<String, AntigravityRoute>,
    model: &str,
    effort: Option<&str>,
) -> String {
    let key = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    let Some(route) = routes.get(&key) else {
        return model.to_string();
    };
    if let Some(effort) = effort {
        let target = canonical_effort(effort).unwrap_or("high");
        if let Some(upstream) = route.tiers.get(target) {
            return upstream.clone();
        }
        // A family that lacks the exact tier gets the nearest one instead of
        // silently falling back to the strongest variant.
        if let Some((_, upstream)) = route
            .tiers
            .iter()
            .min_by_key(|(tier, _)| effort_rank(tier).abs_diff(effort_rank(target)))
        {
            return upstream.clone();
        }
    }
    if route.default_upstream.is_empty() {
        model.to_string()
    } else {
        route.default_upstream.clone()
    }
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
    /// Advertised consolidated model id -> upstream per-tier ids.
    routes: RwLock<HashMap<String, AntigravityRoute>>,
    /// Unix seconds of the last catalog refresh performed from `chat`.
    routes_refreshed_at: AtomicU64,
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
            routes: RwLock::new(HashMap::new()),
            routes_refreshed_at: AtomicU64::new(0),
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
            return Err(MgError::unauthorized("Account has no refresh token"));
        }
        match self.exchange_fresh_token(account).await {
            Ok(token) => Ok(token),
            Err(error) => {
                if matches!(error.status_code(), 401 | 403) {
                    self.pool.set_credential_invalid(
                        &account.id,
                        "凭证已失效，请重新登录 Antigravity 账号",
                    );
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
        let account_id = account.id.clone();
        let access_token = new_token.clone();
        let expires_at = account.expires_at;
        tokio::task::spawn_blocking(move || {
            pool.update(&account_id, |current| {
                current.access_token = access_token;
                current.expires_at = expires_at;
            })
        })
        .await
        .map_err(|error| MgError::Io(error.to_string()))??;

        Ok(new_token)
    }

    async fn discover_project_id(&self, token: &str, account: &mut Account) -> Result<String> {
        if let Some(pid) = account.extra.get("project_id").and_then(|v| v.as_str()) {
            if !pid.is_empty() && pid != "antigravity-default" {
                return Ok(pid.to_string());
            }
        }

        // `client` is not a field of ClientMetadata; sending it makes Google
        // reject the whole call with 400 and every fresh account fell back to
        // the placeholder project (which returns no quotas).
        let body = serde_json::json!({
            "metadata": { "ideType": "ANTIGRAVITY" }
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
                    let pid = data
                        .get("cloudaicompanionProject")
                        .or_else(|| data.get("cloudaicompanionProjectId"))
                        .and_then(|value| {
                            value
                                .as_str()
                                .or_else(|| value.get("id").and_then(serde_json::Value::as_str))
                        })
                        .filter(|pid| !pid.is_empty());
                    if let Some(pid) = pid {
                        account
                            .extra
                            .insert("project_id".to_string(), serde_json::json!(pid));
                        let pool = Arc::clone(&self.pool);
                        let account_id = account.id.clone();
                        let pid = pid.to_string();
                        let saved_pid = pid.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            pool.update(&account_id, |current| {
                                current
                                    .extra
                                    .insert("project_id".to_string(), serde_json::json!(saved_pid));
                            })
                        })
                        .await;
                        return Ok(pid);
                    }
                }
            }
        }

        // An empty project is a valid state: the caller sends `{}` and the
        // upstream resolves the default project for the account.
        Ok(String::new())
    }

    async fn fetch_live_models(&self, account: &mut Account) -> Vec<ModelEntry> {
        let token = match self.ensure_fresh_token(account).await {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(provider = "antigravity", %error, "model catalog skipped: token refresh failed");
                return Vec::new();
            }
        };
        let url = format!("{}{}", HOST_PROD, FETCH_MODELS);
        let project = self.discover_project_id(&token, account).await.ok();
        let request_body = project
            .as_deref()
            .filter(|project| !project.is_empty())
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
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    provider = "antigravity",
                    status = status.as_u16(),
                    body = %crate::upstream_error_body(status.as_u16(), &body),
                    "model catalog request rejected"
                );
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!(provider = "antigravity", %error, "model catalog request failed");
                return Vec::new();
            }
        };
        let Ok(data) = res.json::<serde_json::Value>().await else {
            tracing::warn!(provider = "antigravity", "model catalog was not valid JSON");
            return Vec::new();
        };
        parse_model_entries(&data, "google")
            .into_iter()
            .filter(|model| !is_internal_antigravity_model(&model.id))
            .map(enrich_antigravity_model)
            .collect()
    }

    /// Publish one model per family and replace the routing table used by chat.
    fn store_consolidated(&self, models: Vec<ModelEntry>) -> Vec<ModelEntry> {
        let (models, routes) = consolidate_models(models);
        *self.routes.write() = routes;
        models
    }

    /// API clients can call chat without ever listing models; build the tier
    /// routing table once (throttled) so a consolidated id still resolves.
    async fn ensure_routes(&self, account: &mut Account) {
        let now = unix_now_secs();
        if now.saturating_sub(self.routes_refreshed_at.load(Ordering::Relaxed)) < 60 {
            return;
        }
        self.routes_refreshed_at.store(now, Ordering::Relaxed);
        let models = self.fetch_live_models(account).await;
        if !models.is_empty() {
            let _ = self.store_consolidated(models);
        }
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
            "login": false,
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
            Ok(account) => account,
            Err(_) => return Ok(fallback),
        };
        let models = self.fetch_live_models(&mut account).await;
        if models.is_empty() {
            // An account exists, so this is a transient upstream failure. Tell
            // the caller instead of replacing a good cached catalog with the
            // three bare fallback ids.
            return Err(MgError::provider(
                502,
                "Antigravity model catalog is temporarily unavailable",
                "upstream_error",
            ));
        }
        self.routes_refreshed_at
            .store(unix_now_secs(), Ordering::Relaxed);
        Ok(self.store_consolidated(models))
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)> {
        let mut account = self.pool.choose(session_id, None)?;
        let mut token = self.ensure_fresh_token(&mut account).await?;
        let project_id = self
            .discover_project_id(&token, &mut account)
            .await
            .ok()
            .filter(|project| !project.is_empty());

        // Clients see one model per family; the requested thinking tier picks
        // the upstream variant that implements it.
        if self.routes.read().is_empty() {
            self.ensure_routes(&mut account).await;
        }
        let client_model = req.model.clone();
        let upstream_model = {
            let effort = requested_effort(&req);
            let routes = self.routes.read();
            resolve_upstream_model(&routes, &client_model, effort.as_deref())
        };
        let mut req = req;
        if upstream_model != client_model {
            tracing::info!(
                provider = "antigravity",
                requested = %client_model,
                upstream = %upstream_model,
                "mapped thinking tier to upstream model variant"
            );
        }
        req.model = upstream_model;
        let model_name = client_model;
        let envelope = chat_to_gemini(&req, project_id)?;

        // Primary: daily host, Secondary: prod host.
        // The dev (daily) host is only probed when it is not marked broken, or
        // when the re-probe interval has elapsed, so an unreachable dev host
        // costs at most one bounded connect attempt per interval instead of
        // stalling every single request.
        let mut refreshed = false;
        let response = loop {
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

            if matches!(response.status().as_u16(), 401 | 403)
                && !refreshed
                && !account.refresh_token.is_empty()
            {
                refreshed = true;
                let failed_token = account.access_token.clone();
                match self.refresh_after_401(&mut account, &failed_token).await {
                    Ok(_) => {
                        token = account.access_token.clone();
                        continue;
                    }
                    Err(error) => {
                        if matches!(error.status_code(), 401 | 403) {
                            self.pool.set_credential_invalid(
                                &account.id,
                                "凭证已失效，请重新登录 Antigravity 账号",
                            );
                        } else {
                            self.pool.set_error(&account.id, &error.message());
                        }
                        return Err(error);
                    }
                }
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();
                if matches!(status, 401 | 403) {
                    self.pool.set_credential_invalid(
                        &account.id,
                        "凭证已失效，请重新登录 Antigravity 账号",
                    );
                    return Err(MgError::provider(
                        401,
                        "Antigravity 账号凭证已失效，请重新登录后再试",
                        "credential_invalid",
                    ));
                }
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

            break response;
        };

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

        Ok((Box::pin(stream), account.id, None))
    }

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota> {
        let mut account = self
            .pool
            .get(account_id)
            .ok_or_else(|| MgError::not_found("Account not found"))?;

        let mut token = self.ensure_fresh_token(&mut account).await?;
        let project = self.discover_project_id(&token, &mut account).await?;
        let url = format!("{}{}", HOST_PROD, FETCH_MODELS);
        let mut refreshed = false;
        let res = loop {
            let res = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("User-Agent", USER_AGENT)
                .json(&if project.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"project": project})
                })
                .send()
                .await
                .map_err(|e| MgError::provider(502, e.to_string(), "network_error"))?;
            if matches!(res.status().as_u16(), 401 | 403)
                && !refreshed
                && !account.refresh_token.is_empty()
            {
                refreshed = true;
                let failed_token = account.access_token.clone();
                match self.refresh_after_401(&mut account, &failed_token).await {
                    Ok(_) => {
                        token = account.access_token.clone();
                        continue;
                    }
                    Err(error) => {
                        if matches!(error.status_code(), 401 | 403) {
                            self.pool.set_credential_invalid(
                                account_id,
                                "凭证已失效，请重新登录 Antigravity 账号",
                            );
                        } else {
                            self.pool.set_error(account_id, &error.message());
                        }
                        return Err(error);
                    }
                }
            }
            break res;
        };
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 401 | 403) {
                self.pool.set_credential_invalid(
                    account_id,
                    "凭证已失效，请重新登录 Antigravity 账号",
                );
                return Err(MgError::provider(
                    401,
                    "Antigravity 账号凭证已失效，请重新登录后再试",
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
    use super::{
        consolidate_models, enrich_antigravity_model, family_key, is_internal_antigravity_model,
        parse_antigravity_quota, resolve_upstream_model, AntigravityProvider, ModelEntry,
    };
    use serde_json::json;

    #[test]
    fn refreshes_oauth_tokens_before_they_expire() {
        let mut account: mg_core::Account = serde_json::from_value(json!({
            "id": "antigravity-test",
            "access_token": "a",
            "expires_at": mg_core::now_secs() + 3600.0
        }))
        .unwrap();
        assert!(!AntigravityProvider::token_needs_refresh(&account));
        account.expires_at = mg_core::now_secs() + 60.0;
        assert!(AntigravityProvider::token_needs_refresh(&account));
        account.access_token.clear();
        assert!(AntigravityProvider::token_needs_refresh(&account));
    }

    #[test]
    fn parses_model_remaining_fractions_and_rejects_missing_quota() {
        let quota = parse_antigravity_quota(
            &json!({"response":{"models":{
                "gemini": {"displayName":"Gemini", "quotaInfo":{"remainingFraction":0.42,"resetTime":"2026-10-22T00:00:00Z"}},
                "claude": {"displayName":"Claude", "quotaInfo":{"remainingFraction":0.1}},
                "claude-alias": {"displayName":"Claude", "quotaInfo":{"remainingFraction":0.3}},
                "chat_20706": {"quotaInfo":{"remainingFraction":0.0}},
                "tab_flash": {"displayName":"Tab", "quotaInfo":{"remainingFraction":0.0}}
            }}}),
            12.0,
        )
        .unwrap();
        assert_eq!(quota.remaining, Some(10.0));
        assert_eq!(quota.details["buckets"].as_array().unwrap().len(), 2);
        assert_eq!(quota.details["buckets"][0]["label"], "Claude");
        assert_eq!(quota.details["buckets"][0]["remaining"], 10.0);
        assert!(parse_antigravity_quota(&json!({"models":{}}), 12.0).is_err());
    }

    #[test]
    fn drops_internal_catalog_entries() {
        assert!(is_internal_antigravity_model("chat_20706"));
        assert!(is_internal_antigravity_model("tab_flash_lite_preview"));
        assert!(!is_internal_antigravity_model("gemini-3.8-flash-high"));
    }

    #[test]
    fn maps_catalog_fields_to_the_shared_metadata_keys() {
        let model: ModelEntry = serde_json::from_value(json!({
            "id": "gemini-3.8-flash-high",
            "displayName": "Gemini 3.8 Flash (High)",
            "supportsImages": true,
            "supportsThinking": true,
            "thinkingBudget": 1000,
            "maxTokens": 1048576,
            "maxOutputTokens": 65536
        }))
        .unwrap();
        let model = enrich_antigravity_model(model);
        assert_eq!(model.extra["display_name"], "Gemini 3.8 Flash (High)");
        assert_eq!(model.extra["context_length"], 1048576);
        assert_eq!(model.extra["max_output_tokens"], 65536);
        assert_eq!(model.extra["supports_images"], true);
        assert_eq!(model.extra["supports_reasoning"], true);
        assert_eq!(
            model.extra["reasoning_effort_config"]["default_level"],
            "high"
        );
        assert_eq!(
            model.extra["reasoning_effort_config"]["options"],
            json!(["high"])
        );
    }

    #[test]
    fn consolidates_strength_variants_into_one_model_per_family() {
        let raw = vec![
            json!({"id":"gemini-3.8-flash-high","displayName":"Gemini 3.8 Flash (High)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3.8-flash-medium","displayName":"Gemini 3.8 Flash (Medium)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3.8-flash-low","displayName":"Gemini 3.8 Flash (Low)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3.8-flash-tiered","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3.5-flash-extra-low","displayName":"Gemini 3.5 Flash (Low)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3.5-flash-low","displayName":"Gemini 3.5 Flash (Medium)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"gemini-3-flash-agent","displayName":"Gemini 3.5 Flash (High)","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65536}),
            json!({"id":"claude-opus-5-5-high","displayName":"Claude Opus 5.5 (High)","supportsImages":true,"supportsThinking":true,"maxTokens":1000000,"maxOutputTokens":128000}),
            json!({"id":"claude-opus-5-5-medium","displayName":"Claude Opus 5.5 (Medium)","supportsImages":true,"supportsThinking":true,"maxTokens":1000000,"maxOutputTokens":128000}),
            json!({"id":"claude-opus-5-5-low","displayName":"Claude Opus 5.5 (Low)","supportsImages":true,"supportsThinking":true,"maxTokens":1000000,"maxOutputTokens":128000}),
            json!({"id":"gpt-oss-120b-medium","displayName":"GPT-OSS 120B (Medium)","supportsThinking":true,"maxTokens":131072,"maxOutputTokens":32768}),
            json!({"id":"gemini-3.5-flash-lite","displayName":"Gemini 3.5 Flash Lite","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65535}),
            json!({"id":"gemini-2.5-flash","displayName":"Gemini 3.5 Flash Lite","supportsImages":true,"supportsThinking":true,"maxTokens":1048576,"maxOutputTokens":65535}),
        ]
        .into_iter()
        .map(|value| enrich_antigravity_model(serde_json::from_value(value).unwrap()))
        .collect();

        let (models, routes) = consolidate_models(raw);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gemini-3.8-flash",
                "gemini-3.5-flash",
                "claude-opus-5-5",
                "gpt-oss-120b-medium",
                "gemini-3.5-flash-lite"
            ]
        );

        let gemini = models
            .iter()
            .find(|model| model.id == "gemini-3.8-flash")
            .unwrap();
        assert_eq!(gemini.extra["display_name"], "Gemini 3.8 Flash");
        assert_eq!(gemini.extra["reasoning_efforts"], json!(["low", "medium", "high"]));
        assert_eq!(gemini.extra["reasoning_default_effort"], "high");
        assert_eq!(
            gemini.extra["upstream_models"],
            json!({
                "high": "gemini-3.8-flash-high",
                "medium": "gemini-3.8-flash-medium",
                "low": "gemini-3.8-flash-low"
            })
        );

        let route = routes.get("gemini-3.8-flash").unwrap();
        assert_eq!(route.default_upstream, "gemini-3.8-flash-high");
        assert_eq!(
            resolve_upstream_model(&routes, "gemini-3.8-flash", Some("low")),
            "gemini-3.8-flash-low"
        );
        assert_eq!(
            resolve_upstream_model(&routes, "gemini-3.8-flash", None),
            "gemini-3.8-flash-high"
        );
        // "none" has no dedicated variant; the weakest tier is the closest.
        assert_eq!(
            resolve_upstream_model(&routes, "gemini-3.8-flash", Some("none")),
            "gemini-3.8-flash-low"
        );
        // The mixed id family still routes high to the agent id.
        assert_eq!(
            resolve_upstream_model(&routes, "gemini-3.5-flash", Some("high")),
            "gemini-3-flash-agent"
        );
        // Unconsolidated and legacy ids pass through untouched.
        assert_eq!(
            resolve_upstream_model(&routes, "gpt-oss-120b-medium", Some("low")),
            "gpt-oss-120b-medium"
        );
        assert_eq!(
            resolve_upstream_model(&routes, "gemini-3.8-flash-high", None),
            "gemini-3.8-flash-high"
        );
    }

    #[test]
    fn family_keys_ignore_the_strength_marker() {
        let high: ModelEntry = serde_json::from_value(json!({
            "id":"claude-opus-5-5-high","displayName":"Claude Opus 5.5 (High)"
        }))
        .unwrap();
        let low: ModelEntry = serde_json::from_value(json!({
            "id":"claude-opus-5-5-low","displayName":"Claude Opus 5.5 (Low)"
        }))
        .unwrap();
        assert_eq!(family_key(&high), "claude-opus-5.5");
        assert_eq!(family_key(&high), family_key(&low));
    }
}
