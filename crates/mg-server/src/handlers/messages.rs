use std::collections::BTreeMap;
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
use mg_protocols::anthropic::MessagesRequest;
use mg_protocols::transform::anthropic_to_chat;
use tokio_stream::wrappers::ReceiverStream;

use crate::handlers::chat::copy_usage_to_record;
use crate::state::AppState;

/// A tool call being assembled from streamed fragments.
#[derive(Default)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallBuffer {
    fn resolved_id(&self) -> String {
        if self.id.is_empty() {
            format!("toolu_{}", uuid::Uuid::new_v4().simple())
        } else {
            self.id.clone()
        }
    }

    fn input(&self) -> serde_json::Value {
        if self.arguments.trim().is_empty() {
            return serde_json::json!({});
        }
        serde_json::from_str(&self.arguments).unwrap_or_else(|_| serde_json::json!({}))
    }
}

pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MessagesRequest>,
) -> Response {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();

    let token = crate::handlers::chat::bearer_token(auth_header);
    if !state.key_manager.is_authorized(token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(MgError::unauthorized("Invalid or missing API key").to_anthropic_json()),
        )
            .into_response();
    }

    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok());

    let raw_model = req.model.clone();
    // Anthropic defaults to a non-streaming JSON response; the handler used to
    // emit SSE unconditionally, which broke clients that sent "stream": false.
    let is_stream = req.stream.unwrap_or(false);

    let chat_req = match anthropic_to_chat(req) {
        Ok(cr) => cr,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(e.to_anthropic_json()),
            )
                .into_response();
        }
    };

    let (provider_name, actual_model) = match raw_model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => {
            let p_name = if raw_model.contains("claude") || raw_model.contains("gemini") {
                "antigravity"
            } else if raw_model.contains("MiniMax") || raw_model.contains("abab") {
                "minimax"
            } else {
                "copilot"
            };
            (p_name.to_string(), raw_model.clone())
        }
    };

    // Mirror the /v1/chat/completions ACL so a scoped key cannot bypass it by
    // switching protocol.
    if !state.key_manager.check_model_allowed(token, &raw_model)
        && !state.key_manager.check_model_allowed(token, &actual_model)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(
                MgError::forbidden(format!("API key is not allowed to use model '{}'", raw_model))
                    .to_anthropic_json(),
            ),
        )
            .into_response();
    }

    let mut final_chat_req = chat_req;
    final_chat_req.model = actual_model;

    let provider = match state.get_provider(&provider_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    MgError::bad_request(format!("Provider '{}' not supported or enabled", provider_name))
                        .to_anthropic_json(),
                ),
            )
                .into_response();
        }
    };

    let record = state.activity.begin(
        &provider_name,
        &raw_model,
        "messages",
        state.config.port,
        session_id,
    );
    let record_id = record.id.clone();

    let (mut stream, account_id) = match provider.chat(final_chat_req, session_id).await {
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
                Json(err.to_anthropic_json()),
            )
                .into_response();
        }
    };

    let activity = state.activity.clone();
    let msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

    if !is_stream {
        return aggregate(
            state, activity, stream, record, record_id, account_id, msg_id, raw_model,
        )
        .await;
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);
    let model_for_task = raw_model.clone();

    tokio::spawn(async move {
        let mut first_token = true;
        let mut total_completion_tokens = 0;
        let mut total_input_tokens = 0;
        let mut total_usage = None;
        let mut error_msg = None;
        let mut error_status = 200u16;
        let mut disconnected = false;

        // Anthropic content blocks are sequential and must be opened and closed in
        // order, so track which block is live and allocate a new index per kind.
        let mut next_block: usize = 0;
        let mut open_block: Option<usize> = None;
        let mut open_kind = String::new();
        let mut tool_blocks: BTreeMap<u32, usize> = BTreeMap::new();
        let mut tool_names: BTreeMap<u32, String> = BTreeMap::new();

        let start_event = format!(
            "event: message_start\ndata: {}\n\n",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": msg_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model_for_task,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0
                    }
                }
            })
        );
        let _ = tx.send(Ok(Bytes::from(start_event))).await;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(chunk) => chunk,
                Err(e) => {
                    error_status = e.status_code();
                    error_msg = Some(e.message());
                    let err_event = format!("event: error\ndata: {}\n\n", e.to_anthropic_json());
                    let _ = tx.send(Ok(Bytes::from(err_event))).await;
                    break;
                }
            };

            if first_token {
                activity.record_first_token(&record_id);
                first_token = false;
            }

            if let Some(usage) = &chunk.usage {
                total_completion_tokens = usage.completion_tokens;
                total_input_tokens = usage.prompt_tokens;
                total_usage = Some(usage.clone());
            }

            for choice in chunk.choices {
                let delta = choice.delta;

                if let Some(thinking) = delta.reasoning_content {
                    if !thinking.is_empty() {
                        if open_block.is_some() && open_kind != "thinking" {
                            let _ = close_block(&tx, open_block.take()).await;
                        }
                        if open_block.is_none() {
                            let index = next_block;
                            next_block += 1;
                            let _ = tx
                                .send(Ok(Bytes::from(format!(
                                    "event: content_block_start\ndata: {}\n\n",
                                    serde_json::json!({
                                        "type": "content_block_start",
                                        "index": index,
                                        "content_block": { "type": "thinking", "thinking": "" }
                                    })
                                ))))
                                .await;
                            open_block = Some(index);
                            open_kind = "thinking".to_string();
                        }
                        let index = open_block.unwrap_or(0);
                        let _ = tx
                            .send(Ok(Bytes::from(format!(
                                "event: content_block_delta\ndata: {}\n\n",
                                serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": { "type": "thinking_delta", "thinking": thinking }
                                })
                            ))))
                            .await;
                    }
                }

                if let Some(text) = delta.content {
                    if !text.is_empty() {
                        if open_block.is_some() && open_kind != "text" {
                            let _ = close_block(&tx, open_block.take()).await;
                        }
                        if open_block.is_none() {
                            let index = next_block;
                            next_block += 1;
                            let _ = tx
                                .send(Ok(Bytes::from(format!(
                                    "event: content_block_start\ndata: {}\n\n",
                                    serde_json::json!({
                                        "type": "content_block_start",
                                        "index": index,
                                        "content_block": { "type": "text", "text": "" }
                                    })
                                ))))
                                .await;
                            open_block = Some(index);
                            open_kind = "text".to_string();
                        }
                        let index = open_block.unwrap_or(0);
                        if tx
                            .send(Ok(Bytes::from(format!(
                                "event: content_block_delta\ndata: {}\n\n",
                                serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": { "type": "text_delta", "text": text }
                                })
                            ))))
                            .await
                            .is_err()
                        {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if disconnected {
                    break;
                }

                if let Some(calls) = delta.tool_calls {
                    for call in calls {
                        let block_index = match tool_blocks.get(&call.index) {
                            Some(index) => *index,
                            None => {
                                if open_block.is_some() {
                                    let _ = close_block(&tx, open_block.take()).await;
                                    open_kind.clear();
                                }
                                let index = next_block;
                                next_block += 1;
                                let name = call
                                    .function
                                    .as_ref()
                                    .and_then(|f| f.name.clone())
                                    .unwrap_or_default();
                                let id = call.id.clone().unwrap_or_default();
                                tool_names.insert(call.index, name.clone());
                                let _ = tx
                                    .send(Ok(Bytes::from(format!(
                                        "event: content_block_start\ndata: {}\n\n",
                                        serde_json::json!({
                                            "type": "content_block_start",
                                            "index": index,
                                            "content_block": {
                                                "type": "tool_use",
                                                "id": if id.is_empty() {
                                                    format!("toolu_{}", uuid::Uuid::new_v4().simple())
                                                } else {
                                                    id
                                                },
                                                "name": name,
                                                "input": {}
                                            }
                                        })
                                    ))))
                                    .await;
                                tool_blocks.insert(call.index, index);
                                index
                            }
                        };

                        if let Some(arguments) =
                            call.function.as_ref().and_then(|f| f.arguments.clone())
                        {
                            if !arguments.is_empty() {
                                let _ = tx
                                    .send(Ok(Bytes::from(format!(
                                        "event: content_block_delta\ndata: {}\n\n",
                                        serde_json::json!({
                                            "type": "content_block_delta",
                                            "index": block_index,
                                            "delta": {
                                                "type": "input_json_delta",
                                                "partial_json": arguments
                                            }
                                        })
                                    ))))
                                    .await;
                            }
                        }
                    }
                }
            }
        }

        // Close any trailing text/thinking block, then every tool block in order.
        if open_block.is_some() {
            let _ = close_block(&tx, open_block.take()).await;
        }
        for index in tool_blocks.values() {
            let _ = close_block(&tx, Some(*index)).await;
        }

        let stop_reason = if tool_blocks.is_empty() {
            "end_turn"
        } else {
            "tool_use"
        };

        let msg_delta = format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null
                },
                "usage": {
                    "input_tokens": total_input_tokens,
                    "output_tokens": total_completion_tokens
                }
            })
        );
        let _ = tx.send(Ok(Bytes::from(msg_delta))).await;

        let msg_stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let _ = tx.send(Ok(Bytes::from(msg_stop))).await;

        let mut final_record = record;
        final_record.account = account_id;
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
}

async fn close_block(
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
    index: Option<usize>,
) -> Result<(), tokio::sync::mpsc::error::SendError<Result<Bytes, Infallible>>> {
    let Some(index) = index else {
        return Ok(());
    };
    tx.send(Ok(Bytes::from(format!(
        "event: content_block_stop\ndata: {}\n\n",
        serde_json::json!({ "type": "content_block_stop", "index": index })
    ))))
    .await
}

/// Collects the provider stream into a single Anthropic `message` JSON body.
#[allow(clippy::too_many_arguments)]
async fn aggregate(
    state: AppState,
    activity: std::sync::Arc<mg_core::Activity>,
    mut stream: mg_providers::BoxStream<mg_protocols::openai::ChatCompletionChunk>,
    record: mg_core::RequestRecord,
    record_id: String,
    account_id: String,
    msg_id: String,
    model: String,
) -> Response {
    let mut first_token = true;
    let mut thinking = String::new();
    let mut text = String::new();
    let mut tools: BTreeMap<u32, ToolCallBuffer> = BTreeMap::new();
    let mut total_usage = None;
    let mut error_msg: Option<String> = None;
    let mut error_status: u16 = 200;

    while let Some(chunk_res) = stream.next().await {
        match chunk_res {
            Ok(chunk) => {
                if first_token {
                    activity.record_first_token(&record_id);
                    first_token = false;
                }
                if let Some(usage) = &chunk.usage {
                    total_usage = Some(usage.clone());
                }
                for choice in chunk.choices {
                    let delta = choice.delta;
                    if let Some(value) = delta.reasoning_content {
                        thinking.push_str(&value);
                    }
                    if let Some(value) = delta.content {
                        text.push_str(&value);
                    }
                    if let Some(calls) = delta.tool_calls {
                        for call in calls {
                            let buffer = tools.entry(call.index).or_default();
                            if let Some(id) = call.id {
                                if buffer.id.is_empty() {
                                    buffer.id = id;
                                }
                            }
                            if let Some(function) = call.function {
                                if let Some(name) = function.name {
                                    if buffer.name.is_empty() {
                                        buffer.name = name;
                                    }
                                }
                                if let Some(arguments) = function.arguments {
                                    buffer.arguments.push_str(&arguments);
                                }
                            }
                        }
                    }
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
    if let Some(usage) = &total_usage {
        copy_usage_to_record(&mut final_record, usage);
    }

    let mut content: Vec<serde_json::Value> = Vec::new();
    if !thinking.is_empty() {
        content.push(serde_json::json!({ "type": "thinking", "thinking": thinking }));
    }
    if !text.is_empty() {
        content.push(serde_json::json!({ "type": "text", "text": text }));
    }
    for buffer in tools.values() {
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": buffer.resolved_id(),
            "name": buffer.name,
            "input": buffer.input()
        }));
    }

    if content.is_empty() {
        let status = if error_status >= 400 { error_status } else { 502 };
        let message = error_msg.unwrap_or_else(|| "Upstream returned an empty response".to_string());
        state
            .activity
            .finish(final_record, status, Some(message.clone()), false);
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(MgError::provider(status, message, "api_error").to_anthropic_json()),
        )
            .into_response();
    }

    let stop_reason = if tools.is_empty() { "end_turn" } else { "tool_use" };
    let (input_tokens, output_tokens) = match &total_usage {
        Some(usage) => (usage.prompt_tokens, usage.completion_tokens),
        None => (0, 0),
    };

    state.activity.finish(final_record, error_status, error_msg, false);

    Json(serde_json::json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    }))
    .into_response()
}

pub async fn provider_messages(
    axum::extract::Path(provider): axum::extract::Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<MessagesRequest>,
) -> Response {
    if !req.model.contains('/') {
        req.model = format!("{}/{}", provider, req.model);
    }
    messages(State(state), headers, Json(req)).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use futures::stream;
    use mg_core::account::AccountQuota;
    use mg_core::error::Result;
    use mg_core::{Activity, Config, KeyManager};
    use mg_protocols::anthropic::{AnthropicMessage, MessagesRequest};
    use mg_protocols::openai::{
        ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta,
        FunctionCallChunk, ModelEntry, ToolCallChunk,
    };
    use mg_providers::{BoxStream, Provider};
    use parking_lot::RwLock;
    use serde_json::{json, Value};

    use crate::state::AppState;

    /// Replays a fixed chunk script so protocol translation can be asserted.
    struct ScriptedProvider {
        chunks: Vec<ChatCompletionChunk>,
        error: Option<u16>,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
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
            let mut items: Vec<Result<ChatCompletionChunk>> =
                self.chunks.clone().into_iter().map(Ok).collect();
            if let Some(code) = self.error {
                items.push(Err(mg_core::error::MgError::provider(
                    code,
                    "upstream exploded mid-stream",
                    "upstream_error",
                )));
            }
            Ok((Box::pin(stream::iter(items)), "account-1".to_string()))
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

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-rust-messages-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(dir: &std::path::Path, chunks: Vec<ChatCompletionChunk>) -> AppState {
        state_with(dir, chunks, None)
    }

    fn state_with(
        dir: &std::path::Path,
        chunks: Vec<ChatCompletionChunk>,
        error: Option<u16>,
    ) -> AppState {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert(
            "test".to_string(),
            Arc::new(ScriptedProvider { chunks, error }),
        );
        AppState {
            config: Arc::new(Config {
                data_dir: dir.to_path_buf(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(dir)),
            key_manager: Arc::new(KeyManager::new(dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        }
    }

    fn headers() -> HeaderMap {
        HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )])
    }

    fn request(streaming: bool) -> MessagesRequest {
        MessagesRequest {
            model: "test/model-a".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: json!("hi"),
            }],
            system: None,
            max_tokens: 64,
            stream: Some(streaming),
            temperature: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            stop_sequences: None,
        }
    }

    fn chunk(delta: ChunkDelta, usage: Option<ChatUsage>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: "chunk-1".to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1,
            model: "model-a".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta,
                finish_reason: None,
            }],
            usage,
        }
    }

    fn tool_call(index: u32, id: Option<&str>, name: Option<&str>, arguments: Option<&str>) -> ToolCallChunk {
        ToolCallChunk {
            index,
            id: id.map(str::to_string),
            tool_type: Some("function".to_string()),
            function: Some(FunctionCallChunk {
                name: name.map(str::to_string),
                arguments: arguments.map(str::to_string),
            }),
        }
    }

    fn usage() -> ChatUsage {
        ChatUsage {
            prompt_tokens: 11,
            completion_tokens: 7,
            total_tokens: 18,
            prompt_tokens_details: None,
        }
    }

    #[tokio::test]
    async fn stream_false_returns_anthropic_json_with_thinking_text_and_tool_use() {
        let dir = temp_dir();
        let chunks = vec![
            chunk(
                ChunkDelta {
                    reasoning_content: Some("let me think".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    content: Some("Hello".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            // Arguments arrive split across chunks and must be reassembled.
            chunk(
                ChunkDelta {
                    tool_calls: Some(vec![tool_call(0, Some("call_1"), Some("get_weather"), Some("{\"ci"))]),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    tool_calls: Some(vec![tool_call(0, None, None, Some("ty\":\"SH\"}"))]),
                    ..ChunkDelta::default()
                },
                Some(usage()),
            ),
        ];

        let state = state(&dir, chunks);
        let response =
            super::messages(State(state.clone()), headers(), axum::Json(request(false))).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json"));

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["model"], "test/model-a");
        assert_eq!(body["stop_reason"], "tool_use");

        let content = body["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "let me think");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Hello");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "call_1");
        assert_eq!(content[2]["name"], "get_weather");
        assert_eq!(content[2]["input"], json!({"city": "SH"}));

        assert_eq!(body["usage"]["input_tokens"], 11);
        assert_eq!(body["usage"]["output_tokens"], 7);

        let board = state.activity.board();
        assert_eq!(board["rows"].as_array().unwrap().len(), 1);
        assert_eq!(board["rows"][0]["protocol"], "messages");
        assert_eq!(board["rows"][0]["account"], "account-1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn streaming_emits_tool_use_blocks_and_reports_tool_use_stop_reason() {
        let dir = temp_dir();
        let chunks = vec![
            chunk(
                ChunkDelta {
                    content: Some("Hi".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    tool_calls: Some(vec![tool_call(
                        0,
                        Some("call_9"),
                        Some("search"),
                        Some("{\"q\":1}"),
                    )]),
                    ..ChunkDelta::default()
                },
                Some(usage()),
            ),
        ];

        let response =
            super::messages(State(state(&dir, chunks)), headers(), axum::Json(request(true))).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream; charset=utf-8"
        );

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains("\"type\":\"text_delta\""), "{text}");
        assert!(text.contains("\"type\":\"tool_use\""), "{text}");
        assert!(text.contains("\"id\":\"call_9\""), "{text}");
        assert!(text.contains("\"name\":\"search\""), "{text}");
        assert!(text.contains("\"type\":\"input_json_delta\""), "{text}");
        assert!(text.contains("\"partial_json\":\"{\\\"q\\\":1}\""), "{text}");
        assert!(text.contains("\"stop_reason\":\"tool_use\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");

        // The text block must close before the tool block opens, and the tool
        // block must close before message_delta: exactly two stops.
        assert_eq!(text.matches("event: content_block_stop").count(), 2, "{text}");
        let text_stop = text.find("event: content_block_stop").unwrap();
        let tool_start = text.find("\"type\":\"tool_use\"").unwrap();
        assert!(text_stop < tool_start, "blocks interleaved: {text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn streaming_without_tool_calls_ends_with_end_turn() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some("plain answer".to_string()),
                ..ChunkDelta::default()
            },
            Some(usage()),
        )];

        let response =
            super::messages(State(state(&dir, chunks)), headers(), axum::Json(request(true))).await;
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(text.contains("\"stop_reason\":\"end_turn\""), "{text}");
        assert!(!text.contains("\"type\":\"tool_use\""), "{text}");
        assert_eq!(text.matches("event: content_block_stop").count(), 1, "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mid_stream_upstream_error_is_recorded_as_failed_with_its_status() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some("partial".to_string()),
                ..ChunkDelta::default()
            },
            None,
        )];

        let state = state_with(&dir, chunks, Some(502));
        let response =
            super::messages(State(state.clone()), headers(), axum::Json(request(true))).await;
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        let board = state.activity.board();
        let row = &board["rows"][0];
        assert_eq!(row["status"], "failed", "{row}");
        assert_eq!(row["http_status"], 502, "{row}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn provider_messages_prefixes_model() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some("plain answer".to_string()),
                ..ChunkDelta::default()
            },
            Some(usage()),
        )];

        let mut req = request(false);
        req.model = "model-a".to_string();

        let response = super::provider_messages(
            axum::extract::Path("test".to_string()),
            State(state(&dir, chunks)),
            headers(),
            axum::Json(req),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["model"], "test/model-a");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
