use std::convert::Infallible;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::StreamExt;
use mg_core::error::MgError;
use mg_core::RequestRecord;
use mg_protocols::openai::{ChatCompletionRequest, ChatUsage};
use tokio_stream::wrappers::ReceiverStream;

use crate::state::AppState;

#[derive(Default)]
struct ChatToolCallBuffer {
    id: String,
    tool_type: String,
    name: String,
    arguments: String,
}

/// Extracts a bearer token, stripping the scheme exactly once and case-insensitively.
/// `trim_start_matches("Bearer ")` would also eat a literal repeated prefix.
pub(crate) fn bearer_token(value: &str) -> &str {
    let trimmed = value.trim();
    for scheme in ["Bearer ", "bearer "] {
        if let Some(rest) = trimmed.strip_prefix(scheme) {
            return rest.trim();
        }
    }
    trimmed
}

pub(crate) fn copy_usage_to_record(record: &mut RequestRecord, usage: &ChatUsage) {
    record.input_tokens = Some(usage.prompt_tokens);
    record.output_tokens = Some(usage.completion_tokens);
    if let Some(details) = &usage.prompt_tokens_details {
        record.cached_tokens = Some(details.cached_tokens);
    }
    if usage.credits.is_some() {
        record.credits = usage.credits;
    }
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ChatCompletionRequest>,
) -> Response {
    // 1. Auth check. Accept x-api-key as well as Authorization so clients that use
    // the Anthropic-style header work on the OpenAI route too (messages.rs already did).
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();

    let token = bearer_token(auth_header);
    if !state.key_manager.is_authorized(token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(MgError::unauthorized("Invalid or missing API key").to_openai_json()),
        )
            .into_response();
    }

    // 2. Session ID
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok());

    // 3. Provider resolution
    let raw_model = req.model.clone();
    let (provider_name, actual_model) = match raw_model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => {
            // Default provider logic
            let p_name = if raw_model.contains("gemini") || raw_model.contains("claude") {
                "antigravity"
            } else if raw_model.contains("grok") {
                "grok"
            } else if raw_model.contains("MiniMax") || raw_model.contains("abab") {
                "minimax"
            } else {
                "copilot"
            };
            (p_name.to_string(), raw_model.clone())
        }
    };

    // 4. Per-key model ACL (mirrors /v1/models filtering, which is advisory only)
    if !state.key_manager.check_model_allowed(token, &raw_model)
        && !state.key_manager.check_model_allowed(token, &actual_model)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(MgError::forbidden(format!("API key is not allowed to use model '{}'", raw_model)).to_openai_json()),
        )
            .into_response();
    }

    req.model = actual_model;

    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    MgError::bad_request(format!("Provider '{}' not supported or enabled", provider_name))
                        .to_openai_json(),
                ),
            )
                .into_response();
        }
    };

    // 5. Begin activity recording
    let record = state.activity.begin(
        &provider_name,
        &raw_model,
        "chat",
        state.config.port,
        session_id,
    );
    let record_id = record.id.clone();

    // 6. Invoke provider
    // OpenAI's `stream` defaults to false; defaulting to true here made clients that
    // omit it receive SSE when they expected a single JSON completion.
    let is_stream = req.stream.unwrap_or(false);
    let (mut stream, account_id) = match provider.chat(req, session_id).await {
        Ok(s) => s,
        Err(err) => {
            state.activity.finish(
                record,
                err.status_code(),
                Some(err.message()),
                false,
            );
            return (
                StatusCode::from_u16(err.status_code()).unwrap_or(StatusCode::BAD_REQUEST),
                Json(err.to_openai_json()),
            )
                .into_response();
        }
    };

    let activity = state.activity.clone();

    if is_stream {
        // SSE streaming response
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);

        tokio::spawn(async move {
            let mut first_token = true;
            let mut first_ttft: Option<u64> = None;
            let mut total_usage = None;
            let mut error_msg = None;
            let mut error_status = 200u16;
            let mut disconnected = false;

            while let Some(chunk_res) = stream.next().await {
                match chunk_res {
                    Ok(chunk) => {
                        if first_token {
                            first_ttft = activity.record_first_token(&record_id);
                            first_token = false;
                        }

                        if let Some(usage) = &chunk.usage {
                            total_usage = Some(usage.clone());
                        }

                        // One allocation per chunk instead of three: serialize straight
                        // into the frame buffer, then hand the Vec to Bytes zero-copy.
                        let mut frame: Vec<u8> = b"data: ".to_vec();
                        if serde_json::to_writer(&mut frame, &chunk).is_ok() {
                            frame.extend_from_slice(b"\n\n");
                            if tx.send(Ok(Bytes::from(frame))).await.is_err() {
                                // Client hung up mid-stream: not a success even
                                // though the upstream behaved.
                                disconnected = true;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error_status = e.status_code();
                        error_msg = Some(e.message());
                        let err_frame = format!("data: {}\n\n", e.to_openai_json());
                        let _ = tx.send(Ok(Bytes::from(err_frame))).await;
                        break;
                    }
                }
            }

            let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;

            let mut final_record = record;
            final_record.account = account_id;
            if final_record.ttft_ms.is_none() {
                final_record.ttft_ms = first_ttft;
            }
            if let Some(usage) = &total_usage {
                copy_usage_to_record(&mut final_record, usage);
            }
            activity.finish(final_record, error_status, error_msg, disconnected);
        });

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("connection", "keep-alive")
            .header("access-control-allow-origin", "*")
            .body(Body::from_stream(ReceiverStream::new(rx)))
            .unwrap()
    } else {
        // Non-streaming response: accumulate content, reasoning, and tool calls
        let mut full_content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_buffers: std::collections::BTreeMap<u32, ChatToolCallBuffer> =
            std::collections::BTreeMap::new();
        let mut usage = None;
        let mut error_msg = None;
        let mut error_status = 200u16;
        let mut first_token = true;
        let mut first_ttft: Option<u64> = None;

        while let Some(chunk_res) = stream.next().await {
            match chunk_res {
                Ok(chunk) => {
                    if first_token {
                        first_ttft = state.activity.record_first_token(&record_id);
                        first_token = false;
                    }
                    for choice in chunk.choices {
                        if let Some(c) = choice.delta.content {
                            full_content.push_str(&c);
                        }
                        if let Some(rc) = choice.delta.reasoning_content {
                            reasoning_content.push_str(&rc);
                        }
                        if let Some(calls) = choice.delta.tool_calls {
                            for call in calls {
                                let buffer = tool_buffers.entry(call.index).or_default();
                                if let Some(id) = call.id {
                                    if buffer.id.is_empty() {
                                        buffer.id = id;
                                    }
                                }
                                if let Some(tt) = call.tool_type {
                                    if buffer.tool_type.is_empty() {
                                        buffer.tool_type = tt;
                                    }
                                }
                                if let Some(function) = call.function {
                                    if let Some(name) = function.name {
                                        if buffer.name.is_empty() {
                                            buffer.name = name;
                                        }
                                    }
                                    if let Some(args) = function.arguments {
                                        buffer.arguments.push_str(&args);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(u) = chunk.usage {
                        usage = Some(u);
                    }
                }
                Err(e) => {
                    error_status = e.status_code();
                    error_msg = Some(e.message());
                    break;
                }
            }
        }

        let mut final_record = record;
        final_record.account = account_id;
        if final_record.ttft_ms.is_none() {
            final_record.ttft_ms = first_ttft;
        }
        if let Some(usage) = &usage {
            copy_usage_to_record(&mut final_record, usage);
        }

        let has_tools = !tool_buffers.is_empty();

        if error_status >= 400 || (full_content.is_empty() && !has_tools && error_msg.is_some()) {
            let status = if error_status >= 400 { error_status } else { 502 };
            let msg = error_msg.unwrap_or_else(|| "Upstream returned an error".to_string());
            state.activity.finish(final_record, status, Some(msg.clone()), false);
            return (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(MgError::provider(status, msg, "upstream_error").to_openai_json()),
            )
                .into_response();
        }

        let assembled_tool_calls: Vec<serde_json::Value> = tool_buffers
            .into_iter()
            .map(|(_idx, buf)| {
                let call_id = if buf.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4().simple())
                } else {
                    buf.id
                };
                let call_type = if buf.tool_type.is_empty() {
                    "function".to_string()
                } else {
                    buf.tool_type
                };
                serde_json::json!({
                    "id": call_id,
                    "type": call_type,
                    "function": {
                        "name": buf.name,
                        "arguments": buf.arguments
                    }
                })
            })
            .collect();

        let finish_reason = if has_tools {
            "tool_calls"
        } else {
            "stop"
        };

        let mut message = serde_json::json!({
            "role": "assistant",
            "content": if full_content.is_empty() && has_tools {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(full_content)
            },
        });
        if !reasoning_content.is_empty() {
            message["reasoning_content"] = serde_json::Value::String(reasoning_content);
        }
        if has_tools {
            message["tool_calls"] = serde_json::Value::Array(assembled_tool_calls);
        }

        let resp_json = serde_json::json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": raw_model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason
            }],
            "usage": usage
        });

        state.activity.finish(final_record, error_status, error_msg, false);

        Json(resp_json).into_response()
    }
}

pub async fn provider_chat_completions(
    axum::extract::Path(provider): axum::extract::Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ChatCompletionRequest>,
) -> Response {
    if !req.model.contains('/') {
        req.model = format!("{}/{}", provider, req.model);
    }
    chat_completions(State(state), headers, Json(req)).await
}

#[cfg(test)]
mod tests {
    use super::copy_usage_to_record;
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use futures::stream;
    use mg_core::account::AccountQuota;
    use mg_core::error::Result;
    use mg_core::{Activity, Config, KeyManager};
    use mg_protocols::openai::{
        ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta, ModelEntry,
        PromptTokensDetails,
    };
    use mg_providers::{BoxStream, Provider};
    use parking_lot::RwLock;
    use serde_json::{json, Value};

    use crate::state::AppState;

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            "test"
        }

        fn name(&self) -> &'static str {
            "Test"
        }

        fn capabilities(&self) -> Value {
            json!({})
        }

        async fn models(&self) -> Result<Vec<ModelEntry>> {
            Ok(Vec::new())
        }

        async fn chat(
            &self,
            _req: ChatCompletionRequest,
            _session_id: Option<&str>,
        ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
            let chunk = ChatCompletionChunk {
                id: "chunk-1".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1,
                model: "model-a".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        content: Some("hello".to_string()),
                        ..ChunkDelta::default()
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: Some(ChatUsage {
                    prompt_tokens: 8,
                    completion_tokens: 4,
                    total_tokens: 12,
                    prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 3 }),
                    credits: Some(0.15),
                }),
            };
            Ok((Box::pin(stream::iter(vec![Ok(chunk)])), "account-1".to_string()))
        }

        async fn refresh_quota(&self, _account_id: &str) -> Result<AccountQuota> {
            Ok(AccountQuota {
                checked_at: 0.0,
                remaining: None,
                limit: None,
                reset_at: None,
                details: json!({}),
            })
        }

        async fn start_login(&self, _redirect_url: Option<&str>) -> Result<Value> {
            Ok(json!({"ok": true}))
        }

        async fn poll_login(&self, _login_id: &str) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
    }

    #[test]
    fn records_reported_zero_tokens_and_cached_input() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let activity = Activity::new(&dir);
        let mut record = activity.begin("copilot", "gpt-test", "chat", 8791, None);
        copy_usage_to_record(
            &mut record,
            &ChatUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 0 }),
                credits: None,
            },
        );

        assert_eq!(record.input_tokens, Some(0));
        assert_eq!(record.output_tokens, Some(0));
        assert_eq!(record.cached_tokens, Some(0));

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn streaming_and_non_streaming_usage_are_persisted_in_the_usage_board() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("test".to_string(), Arc::new(MockProvider));
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.clone(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(&dir)),
            key_manager: Arc::new(KeyManager::new(&dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        let request = ChatCompletionRequest {
            model: "test/model-a".to_string(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(false),
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: HashMap::new(),
        };
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )]);

        let response = super::chat_completions(
            State(state.clone()),
            headers,
            axum::Json(request.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["usage"]["prompt_tokens"], 8);

        let mut streaming_request = request;
        streaming_request.stream = Some(true);
        let headers = HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )]);
        let response = super::chat_completions(
            State(state.clone()),
            headers,
            axum::Json(streaming_request),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        let board = state.activity.board();
        assert_eq!(board["rows"].as_array().unwrap().len(), 2);
        assert!(board["rows"].as_array().unwrap().iter().all(|row| {
            row["input_tokens"] == 8
                && row["output_tokens"] == 4
                && row["cached_tokens"] == 3
                && row["account"] == "account-1"
        }));

        drop(state);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn openai_route_accepts_x_api_key_and_returns_json_when_stream_is_omitted() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("test".to_string(), Arc::new(MockProvider));
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.clone(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(&dir)),
            key_manager: Arc::new(KeyManager::new(&dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        let request = ChatCompletionRequest {
            model: "test/model-a".to_string(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: HashMap::new(),
        };

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "test-key".parse().unwrap());

        let response =
            super::chat_completions(State(state), headers, axum::Json(request)).await;

        assert_eq!(response.status(), StatusCode::OK);
        // OpenAI's stream defaults to false, so an omitted stream must yield JSON.
        assert!(response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json"));

        let _ = std::fs::remove_dir_all(dir);
    }

    struct MockToolProvider;

    #[async_trait]
    impl Provider for MockToolProvider {
        fn id(&self) -> &'static str {
            "test"
        }
        fn name(&self) -> &'static str {
            "Test"
        }
        fn capabilities(&self) -> Value {
            json!({})
        }
        async fn models(&self) -> Result<Vec<ModelEntry>> {
            Ok(Vec::new())
        }
        async fn chat(
            &self,
            _req: ChatCompletionRequest,
            _session_id: Option<&str>,
        ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
            use mg_protocols::openai::{FunctionCallChunk, ToolCallChunk};
            let chunk = ChatCompletionChunk {
                id: "chunk-tool".to_string(),
                object: "chat.completion.chunk".to_string(),
                created: 1,
                model: "model-tool".to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        content: None,
                        reasoning_content: None,
                        role: Some("assistant".to_string()),
                        tool_calls: Some(vec![ToolCallChunk {
                            index: 0,
                            id: Some("call_abc123".to_string()),
                            tool_type: Some("function".to_string()),
                            function: Some(FunctionCallChunk {
                                name: Some("get_weather".to_string()),
                                arguments: Some("{\"city\":\"Beijing\"}".to_string()),
                            }),
                        }]),
                    },
                    finish_reason: Some("tool_calls".to_string()),
                }],
                usage: None,
            };
            Ok((Box::pin(stream::iter(vec![Ok(chunk)])), "acc-tool".to_string()))
        }
        async fn refresh_quota(&self, _account_id: &str) -> Result<AccountQuota> {
            Ok(AccountQuota {
                checked_at: 0.0,
                remaining: None,
                limit: None,
                reset_at: None,
                details: json!({}),
            })
        }
        async fn start_login(&self, _redirect_url: Option<&str>) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
        async fn poll_login(&self, _login_id: &str) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn non_streaming_accumulates_tool_calls_and_sets_tool_calls_finish_reason() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("test".to_string(), Arc::new(MockToolProvider));
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.clone(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(&dir)),
            key_manager: Arc::new(KeyManager::new(&dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        let request = ChatCompletionRequest {
            model: "test/model-tool".to_string(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(false),
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: HashMap::new(),
        };

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer test-key".parse().unwrap());

        let response =
            super::chat_completions(State(state), headers, axum::Json(request)).await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        let tool_call = &body["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tool_call["id"], "call_abc123");
        assert_eq!(tool_call["function"]["name"], "get_weather");
        assert_eq!(tool_call["function"]["arguments"], "{\"city\":\"Beijing\"}");

        let _ = std::fs::remove_dir_all(dir);
    }

    struct MockErrorProvider;

    #[async_trait]
    impl Provider for MockErrorProvider {
        fn id(&self) -> &'static str {
            "test"
        }
        fn name(&self) -> &'static str {
            "Test"
        }
        fn capabilities(&self) -> Value {
            json!({})
        }
        async fn models(&self) -> Result<Vec<ModelEntry>> {
            Ok(Vec::new())
        }
        async fn chat(
            &self,
            _req: ChatCompletionRequest,
            _session_id: Option<&str>,
        ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
            use mg_core::error::MgError;
            let err = Err(MgError::provider(429, "rate limited by upstream", "rate_limit"));
            Ok((Box::pin(stream::iter(vec![err])), "acc-err".to_string()))
        }
        async fn refresh_quota(&self, _account_id: &str) -> Result<AccountQuota> {
            Ok(AccountQuota {
                checked_at: 0.0,
                remaining: None,
                limit: None,
                reset_at: None,
                details: json!({}),
            })
        }
        async fn start_login(&self, _redirect_url: Option<&str>) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
        async fn poll_login(&self, _login_id: &str) -> Result<Value> {
            Ok(json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn non_streaming_mid_stream_error_returns_error_status_code() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-err-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("test".to_string(), Arc::new(MockErrorProvider));
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.clone(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(&dir)),
            key_manager: Arc::new(KeyManager::new(&dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        let request = ChatCompletionRequest {
            model: "test/model-err".to_string(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(false),
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: HashMap::new(),
        };

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer test-key".parse().unwrap());

        let response =
            super::chat_completions(State(state), headers, axum::Json(request)).await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"]["message"].as_str().unwrap().contains("rate limited"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn provider_chat_completions_prefixes_model() {
        let dir = std::env::temp_dir().join(format!("mg-rust-chat-prefix-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("test".to_string(), Arc::new(MockProvider));
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.clone(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(&dir)),
            key_manager: Arc::new(KeyManager::new(&dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        // Model has no "test/" prefix initially
        let request = ChatCompletionRequest {
            model: "model-a".to_string(),
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            n: None,
            stream: Some(false),
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            extra: HashMap::new(),
        };

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer test-key".parse().unwrap());

        let response = super::provider_chat_completions(
            axum::extract::Path("test".to_string()),
            State(state),
            headers,
            axum::Json(request),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["model"], "test/model-a");

        let _ = std::fs::remove_dir_all(dir);
    }
}
