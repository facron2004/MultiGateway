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
use mg_protocols::openai::{ChatCompletionChunk, ChatUsage};
use mg_protocols::responses::{GrokEvent, GrokResponseCompleted, GrokUsage, ResponsesRequest};
use mg_protocols::transform::responses_to_chat;
use tokio_stream::wrappers::ReceiverStream;

use crate::handlers::chat::copy_usage_to_record;
use crate::state::AppState;

/// A Responses `function_call` output item assembled from streamed fragments.
#[derive(Default)]
struct FunctionCallBuffer {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    /// Index of this item within the response `output` array.
    output_index: u32,
    /// Position of this item in the `output_items` vector being assembled.
    item_pos: Option<usize>,
}

impl FunctionCallBuffer {
    fn resolved_call_id(&self) -> String {
        if self.call_id.is_empty() {
            format!("call_{}", uuid::Uuid::new_v4().simple())
        } else {
            self.call_id.clone()
        }
    }
}

fn grok_usage(usage: &Option<ChatUsage>) -> GrokUsage {
    match usage {
        Some(u) => GrokUsage {
            input_tokens: u.prompt_tokens,
            input_tokens_details: u.prompt_tokens_details.clone(),
            output_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        },
        None => GrokUsage::default(),
    }
}

/// Serialize a Responses event into an SSE frame, using the event `type` as
/// the SSE event name (the OpenAI Responses streaming convention).
fn sse_frame(event: &GrokEvent) -> Option<Bytes> {
    let json = serde_json::to_value(event).ok()?;
    let name = json
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("response.created");
    Some(Bytes::from(format!("event: {}\ndata: {}\n\n", name, json)))
}

pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ResponsesRequest>,
) -> Response {
    // 1. Auth check (Bearer token, or x-api-key as in the messages handler)
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();

    let token = crate::handlers::chat::bearer_token(auth_header);
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

    // 3. Provider resolution (identical to /v1/chat/completions)
    let raw_model = req.model.clone();
    let (provider_name, actual_model) = match raw_model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => {
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

    // 4. Per-key model ACL (mirrors the chat/messages handlers)
    if !state.key_manager.check_model_allowed(token, &raw_model)
        && !state.key_manager.check_model_allowed(token, &actual_model)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(
                MgError::forbidden(format!("API key is not allowed to use model '{}'", raw_model))
                    .to_openai_json(),
            ),
        )
            .into_response();
    }

    // 5. Responses -> chat conversion
    let is_stream = req.stream;
    let mut chat_req = match responses_to_chat(req) {
        Ok(cr) => cr,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(e.to_openai_json())).into_response();
        }
    };
    chat_req.model = actual_model;

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

    // 6. Begin activity recording
    let record = state.activity.begin(
        &provider_name,
        &raw_model,
        "responses",
        state.config.port,
        session_id,
    );
    let record_id = record.id.clone();

    // 7. Invoke provider through the same chat path
    let (mut stream, account_id) = match provider.chat(chat_req, session_id).await {
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
    let resp_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let created_at = chrono::Utc::now().timestamp();

    if !is_stream {
        return aggregate(
            state, activity, stream, record, record_id, account_id, resp_id, raw_model, created_at,
        )
        .await;
    }

    // Streaming: translate chat chunks into Responses SSE events.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);
    let model_for_task = raw_model.clone();
    let resp_id_task = resp_id.clone();

    tokio::spawn(async move {
        let mut first_token = true;
        let mut first_ttft: Option<u64> = None;
        let mut total_usage: Option<ChatUsage> = None;
        let mut error_msg: Option<String> = None;
        let mut error_status: u16 = 200;
        let mut disconnected = false;

        let created_event = GrokEvent::Created {
            response: serde_json::json!({
                "id": resp_id_task,
                "object": "response",
                "created_at": created_at,
                "model": model_for_task,
                "status": "in_progress",
                "output": []
            }),
        };
        if let Some(frame) = sse_frame(&created_event) {
            if tx.send(Ok(frame)).await.is_err() {
                activity.finish(record, 200, None, false);
                return;
            }
        }

        // Output items in emission order; the message item's text and the
        // function_call items' arguments are finalized before response.completed.
        let mut output_items: Vec<serde_json::Value> = Vec::new();
        let mut message_pos: Option<usize> = None;
        let mut message_item_id = String::new();
        let mut message_index: u32 = 0;
        let mut next_index: u32 = 0;
        let mut full_text = String::new();
        let mut tools: BTreeMap<u32, FunctionCallBuffer> = BTreeMap::new();

        'outer: while let Some(chunk_res) = stream.next().await {
            match chunk_res {
                Ok(chunk) => {
                    if first_token {
                        first_ttft = activity.record_first_token(&record_id);
                        first_token = false;
                    }
                    if let Some(usage) = &chunk.usage {
                        total_usage = Some(usage.clone());
                    }

                    for choice in chunk.choices {
                        let delta = choice.delta;

                        // Reasoning deltas have no Responses event counterpart in
                        // the GrokEvent conventions (grok.rs drops them too), so
                        // they are not streamed here.

                        if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                            if message_pos.is_none() {
                                message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
                                let item = serde_json::json!({
                                    "type": "message",
                                    "id": message_item_id,
                                    "role": "assistant",
                                    "status": "in_progress",
                                    "content": [{ "type": "output_text", "text": "", "annotations": [] }]
                                });
                                message_index = next_index;
                                next_index += 1;
                                let added = GrokEvent::OutputItemAdded {
                                    output_index: Some(message_index),
                                    item: item.clone(),
                                };
                                if let Some(frame) = sse_frame(&added) {
                                    if tx.send(Ok(frame)).await.is_err() {
                                        disconnected = true;
                                        break 'outer;
                                    }
                                }
                                output_items.push(item);
                                message_pos = Some(output_items.len() - 1);
                            }
                            let event = GrokEvent::OutputTextDelta {
                                output_index: Some(message_index),
                                item_id: Some(message_item_id.clone()),
                                content_index: Some(0),
                                delta: text.clone(),
                            };
                            if let Some(frame) = sse_frame(&event) {
                                if tx.send(Ok(frame)).await.is_err() {
                                    disconnected = true;
                                    break 'outer;
                                }
                            }
                            full_text.push_str(&text);
                        }

                        if let Some(calls) = delta.tool_calls {
                            for call in calls {
                                let buffer = tools.entry(call.index).or_default();
                                if buffer.item_pos.is_none() {
                                    buffer.item_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                                    // Resolve the call id once so every event for
                                    // this item carries the same value.
                                    buffer.call_id = call
                                        .id
                                        .clone()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| {
                                            format!("call_{}", uuid::Uuid::new_v4().simple())
                                        });
                                    buffer.name = call
                                        .function
                                        .as_ref()
                                        .and_then(|f| f.name.clone())
                                        .unwrap_or_default();
                                    buffer.output_index = next_index;
                                    next_index += 1;
                                    let item = serde_json::json!({
                                        "type": "function_call",
                                        "id": buffer.item_id,
                                        "call_id": buffer.resolved_call_id(),
                                        "name": buffer.name,
                                        "arguments": "",
                                        "status": "in_progress"
                                    });
                                    let added = GrokEvent::OutputItemAdded {
                                        output_index: Some(buffer.output_index),
                                        item: item.clone(),
                                    };
                                    if let Some(frame) = sse_frame(&added) {
                                        if tx.send(Ok(frame)).await.is_err() {
                                            disconnected = true;
                                            break 'outer;
                                        }
                                    }
                                    output_items.push(item);
                                    buffer.item_pos = Some(output_items.len() - 1);
                                } else if buffer.name.is_empty() {
                                    if let Some(name) =
                                        call.function.as_ref().and_then(|f| f.name.clone())
                                    {
                                        buffer.name = name;
                                    }
                                }

                                if let Some(arguments) =
                                    call.function.as_ref().and_then(|f| f.arguments.clone())
                                {
                                    if !arguments.is_empty() {
                                        let event = GrokEvent::FunctionCallDelta {
                                            output_index: Some(buffer.output_index),
                                            item_id: Some(buffer.item_id.clone()),
                                            call_id: Some(buffer.resolved_call_id()),
                                            delta: arguments.clone(),
                                        };
                                        if let Some(frame) = sse_frame(&event) {
                                            if tx.send(Ok(frame)).await.is_err() {
                                                disconnected = true;
                                                break 'outer;
                                            }
                                        }
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
                    let err_frame =
                        Bytes::from(format!("event: error\ndata: {}\n\n", e.to_openai_json()));
                    let _ = tx.send(Ok(err_frame)).await;
                    break;
                }
            }
        }

        // Finalize output items and emit response.completed (skipped on error,
        // which already emitted an `error` event above).
        if error_msg.is_none() {
            for buffer in tools.values() {
                let done = GrokEvent::FunctionCallDone {
                    output_index: Some(buffer.output_index),
                    item_id: Some(buffer.item_id.clone()),
                    call_id: Some(buffer.resolved_call_id()),
                    arguments: Some(buffer.arguments.clone()),
                };
                if let Some(frame) = sse_frame(&done) {
                    let _ = tx.send(Ok(frame)).await;
                }
                if let Some(pos) = buffer.item_pos {
                    if let Some(item) = output_items.get_mut(pos) {
                        item["arguments"] = serde_json::json!(buffer.arguments);
                        item["call_id"] = serde_json::json!(buffer.resolved_call_id());
                        item["name"] = serde_json::json!(buffer.name);
                        item["status"] = serde_json::json!("completed");
                    }
                }
            }
            if let Some(pos) = message_pos {
                if let Some(item) = output_items.get_mut(pos) {
                    item["content"][0]["text"] = serde_json::json!(full_text);
                    item["status"] = serde_json::json!("completed");
                }
            }

            for (index, item) in output_items.iter().enumerate() {
                let done = GrokEvent::OutputItemDone {
                    output_index: Some(index as u32),
                    item: item.clone(),
                };
                if let Some(frame) = sse_frame(&done) {
                    let _ = tx.send(Ok(frame)).await;
                }
            }

            let completed = GrokEvent::Completed {
                response: GrokResponseCompleted {
                    id: resp_id_task.clone(),
                    model: model_for_task.clone(),
                    usage: grok_usage(&total_usage),
                    object: Some("response".to_string()),
                    status: Some("completed".to_string()),
                    created_at: Some(created_at),
                    output: Some(output_items),
                },
            };
            if let Some(frame) = sse_frame(&completed) {
                let _ = tx.send(Ok(frame)).await;
            }
        }

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
}

/// Collects the provider stream into a single Responses JSON object.
#[allow(clippy::too_many_arguments)]
async fn aggregate(
    state: AppState,
    activity: std::sync::Arc<mg_core::Activity>,
    mut stream: mg_providers::BoxStream<ChatCompletionChunk>,
    record: mg_core::RequestRecord,
    record_id: String,
    account_id: String,
    resp_id: String,
    model: String,
    created_at: i64,
) -> Response {
    let mut first_token = true;
    let mut first_ttft: Option<u64> = None;
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut tools: BTreeMap<u32, FunctionCallBuffer> = BTreeMap::new();
    let mut total_usage: Option<ChatUsage> = None;
    let mut error_msg: Option<String> = None;
    let mut error_status: u16 = 200;

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
                for choice in chunk.choices {
                    let delta = choice.delta;
                    if let Some(value) = delta.reasoning_content {
                        reasoning.push_str(&value);
                    }
                    if let Some(value) = delta.content {
                        text.push_str(&value);
                    }
                    if let Some(calls) = delta.tool_calls {
                        for call in calls {
                            let buffer = tools.entry(call.index).or_default();
                            if let Some(id) = call.id {
                                if buffer.call_id.is_empty() {
                                    buffer.call_id = id;
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
    if final_record.ttft_ms.is_none() {
        final_record.ttft_ms = first_ttft;
    }
    if let Some(usage) = &total_usage {
        copy_usage_to_record(&mut final_record, usage);
    }

    let mut output: Vec<serde_json::Value> = Vec::new();
    if !reasoning.is_empty() {
        output.push(serde_json::json!({
            "type": "reasoning",
            "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
            "summary": [{ "type": "summary_text", "text": reasoning }]
        }));
    }
    if !text.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        }));
    }
    for buffer in tools.values() {
        output.push(serde_json::json!({
            "type": "function_call",
            "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
            "call_id": buffer.resolved_call_id(),
            "name": buffer.name,
            "arguments": buffer.arguments,
            "status": "completed"
        }));
    }

    if output.is_empty() {
        let status = if error_status >= 400 { error_status } else { 502 };
        let message = error_msg
            .unwrap_or_else(|| "Upstream returned an empty response".to_string());
        state
            .activity
            .finish(final_record, status, Some(message.clone()), false);
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(MgError::provider(status, message, "api_error").to_openai_json()),
        )
            .into_response();
    }

    state.activity.finish(final_record, 200, error_msg, false);

    Json(serde_json::json!({
        "id": resp_id,
        "object": "response",
        "created_at": created_at,
        "model": model,
        "status": "completed",
        "output": output,
        "usage": grok_usage(&total_usage)
    }))
    .into_response()
}

pub async fn provider_responses(
    axum::extract::Path(provider): axum::extract::Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ResponsesRequest>,
) -> Response {
    if !req.model.contains('/') {
        req.model = format!("{}/{}", provider, req.model);
    }
    responses(State(state), headers, Json(req)).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use futures::stream;
    use mg_core::account::AccountQuota;
    use mg_core::error::Result;
    use mg_core::{Activity, Config, KeyManager};
    use mg_protocols::openai::{
        ChatCompletionChunk, ChatCompletionRequest, ChatUsage, ChunkChoice, ChunkDelta,
        FunctionCallChunk, ModelEntry, PromptTokensDetails, ToolCallChunk,
    };
    use mg_protocols::responses::ResponsesRequest;
    use mg_providers::{BoxStream, Provider};
    use parking_lot::RwLock;
    use serde_json::{json, Value};

    use crate::state::AppState;

    /// Replays a fixed chunk script and records the converted chat requests so
    /// the Responses -> chat mapping can be asserted.
    struct ScriptedProvider {
        chunks: Vec<ChatCompletionChunk>,
        requests: Arc<Mutex<Vec<ChatCompletionRequest>>>,
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
            req: ChatCompletionRequest,
            _session_id: Option<&str>,
        ) -> Result<(BoxStream<ChatCompletionChunk>, String)> {
            self.requests.lock().unwrap().push(req);
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
        let dir = std::env::temp_dir().join(format!("mg-rust-responses-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(
        dir: &std::path::Path,
        chunks: Vec<ChatCompletionChunk>,
        error: Option<u16>,
    ) -> (AppState, Arc<Mutex<Vec<ChatCompletionRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert(
            "test".to_string(),
            Arc::new(ScriptedProvider {
                chunks,
                requests: requests.clone(),
                error,
            }),
        );
        let state = AppState {
            config: Arc::new(Config {
                data_dir: dir.to_path_buf(),
                ..Config::default()
            }),
            activity: Arc::new(Activity::new(dir)),
            key_manager: Arc::new(KeyManager::new(dir, Some("test-key".to_string())).unwrap()),
            providers: Arc::new(RwLock::new(providers)),
            pools: Arc::new(HashMap::new()),
        };
        (state, requests)
    }

    fn headers() -> HeaderMap {
        HeaderMap::from_iter([(
            header::AUTHORIZATION,
            "Bearer test-key".parse().unwrap(),
        )])
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

    fn tool_call(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> ToolCallChunk {
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
            credits: None,
        }
    }

    #[tokio::test]
    async fn cached_tokens_reach_responses_clients_in_both_modes() {
        for streaming in [false, true] {
            for cached_tokens in [Some(9900), Some(0), None] {
                let dir = temp_dir();
                let chunks = vec![chunk(
                    ChunkDelta {
                        content: Some("Hello".to_string()),
                        ..ChunkDelta::default()
                    },
                    Some(ChatUsage {
                        prompt_tokens: 10000,
                        completion_tokens: 7,
                        total_tokens: 10007,
                        prompt_tokens_details: cached_tokens
                            .map(|cached_tokens| PromptTokensDetails { cached_tokens }),
                        credits: None,
                    }),
                )];
                let (state, _) = state(&dir, chunks, None);
                let req = serde_json::from_value(json!({
                    "model": "test/model-a",
                    "input": "hi",
                    "stream": streaming
                }))
                .unwrap();
                let response =
                    super::responses(State(state.clone()), headers(), axum::Json(req)).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body: Value = if streaming {
                    let event = std::str::from_utf8(&bytes)
                        .unwrap()
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .map(|data| serde_json::from_str::<Value>(data).unwrap())
                        .find(|event| event["type"] == "response.completed")
                        .unwrap();
                    event["response"].clone()
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };

                assert_eq!(body["usage"]["input_tokens"], 10000);
                assert_eq!(body["usage"]["output_tokens"], 7);
                assert_eq!(body["usage"]["total_tokens"], 10007);
                match cached_tokens {
                    Some(cached) => {
                        assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], cached);
                    }
                    None => assert!(body["usage"].get("input_tokens_details").is_none()),
                }
                let board = state.activity.board();
                assert_eq!(board["rows"][0]["input_tokens"], 10000);
                assert_eq!(board["rows"][0]["cached_tokens"], json!(cached_tokens));

                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    #[tokio::test]
    async fn string_input_non_streaming_returns_a_responses_object() {
        let dir = temp_dir();
        let chunks = vec![
            chunk(
                ChunkDelta {
                    content: Some("Hello".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    content: Some(" world".to_string()),
                    ..ChunkDelta::default()
                },
                Some(usage()),
            ),
        ];
        let (state, requests) = state(&dir, chunks, None);

        // A plain-string `input` (no `messages` field anywhere) must deserialize.
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "test/model-a",
            "input": "hi there",
            "instructions": "be brief",
            "stream": false
        }))
        .unwrap();

        let response = super::responses(State(state.clone()), headers(), axum::Json(req)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json"));

        // Conversion: instructions -> system, string input -> user message.
        // Clone so the mutex guard does not live across the await below.
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let chat_req = &captured[0];
        assert_eq!(chat_req.model, "model-a");
        assert_eq!(chat_req.stream, Some(false));
        assert_eq!(chat_req.messages.len(), 2);
        assert_eq!(chat_req.messages[0].role, "system");
        assert_eq!(chat_req.messages[0].content, Some(json!("be brief")));
        assert_eq!(chat_req.messages[1].role, "user");
        assert_eq!(chat_req.messages[1].content, Some(json!("hi there")));

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["object"], "response");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["model"], "test/model-a");
        assert!(body["id"].as_str().unwrap().starts_with("resp_"));

        let output = body["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        assert_eq!(output[0]["status"], "completed");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "Hello world");

        assert_eq!(body["usage"]["input_tokens"], 11);
        assert_eq!(body["usage"]["output_tokens"], 7);
        assert_eq!(body["usage"]["total_tokens"], 18);

        let board = state.activity.board();
        assert_eq!(board["rows"].as_array().unwrap().len(), 1);
        assert_eq!(board["rows"][0]["protocol"], "responses");
        assert_eq!(board["rows"][0]["account"], "account-1");
        assert_eq!(board["rows"][0]["input_tokens"], 11);
        assert_eq!(board["rows"][0]["output_tokens"], 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn array_input_with_parts_and_function_items_is_converted_to_chat_messages() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some("ok".to_string()),
                ..ChunkDelta::default()
            },
            Some(usage()),
        )];
        let (state, requests) = state(&dir, chunks, None);

        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "test/model-a",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hello" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"SH\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "sunny"
                }
            ],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "weather lookup",
                "parameters": { "type": "object" }
            }],
            "temperature": 0.5,
            "max_output_tokens": 64,
            "stream": false
        }))
        .unwrap();

        let response = super::responses(State(state.clone()), headers(), axum::Json(req)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["output"][0]["content"][0]["text"], "ok");

        let captured = requests.lock().unwrap();
        let chat_req = &captured[0];
        assert_eq!(chat_req.temperature, Some(0.5));
        assert_eq!(chat_req.max_completion_tokens, Some(64));

        let messages = &chat_req.messages;
        assert_eq!(messages.len(), 3, "{:?}", messages);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, Some(json!("hello")));
        assert_eq!(messages[1].role, "assistant");
        let tool_calls = messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].function.name, "get_weather");
        assert_eq!(tool_calls[0].function.arguments, "{\"city\":\"SH\"}");
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[2].content, Some(json!("sunny")));

        // Flat Responses tools are wrapped into the chat nested form.
        let tools = chat_req.tools.as_ref().unwrap();
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        drop(captured);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn streaming_defaults_to_true_and_emits_responses_events() {
        let dir = temp_dir();
        let chunks = vec![
            chunk(
                ChunkDelta {
                    content: Some("Hello".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    content: Some(" world".to_string()),
                    ..ChunkDelta::default()
                },
                None,
            ),
            // Arguments arrive split across chunks and must be reassembled.
            chunk(
                ChunkDelta {
                    content: Some(String::new()),
                    tool_calls: Some(vec![tool_call(0, Some("call_9"), Some("search"), Some("{\"q\":"))]),
                    ..ChunkDelta::default()
                },
                None,
            ),
            chunk(
                ChunkDelta {
                    content: Some(String::new()),
                    tool_calls: Some(vec![tool_call(0, None, None, Some("1}"))]),
                    ..ChunkDelta::default()
                },
                Some(usage()),
            ),
        ];
        let (state, _requests) = state(&dir, chunks, None);

        // `stream` omitted entirely: ResponsesRequest defaults it to true.
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "test/model-a",
            "input": [{ "role": "user", "content": "hi" }]
        }))
        .unwrap();
        assert!(req.stream);

        let response = super::responses(State(state), headers(), axum::Json(req)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream; charset=utf-8"
        );

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(text.contains("event: response.created"), "{text}");
        assert!(text.contains("event: response.output_item.added"), "{text}");
        assert!(text.contains("event: response.output_text.delta"), "{text}");
        assert!(text.contains("\"delta\":\"Hello\""), "{text}");
        assert!(text.contains("\"delta\":\" world\""), "{text}");
        assert!(text.contains("event: response.function_call_arguments.delta"), "{text}");
        assert!(text.contains("event: response.function_call_arguments.done"), "{text}");
        assert!(text.contains("\"arguments\":\"{\\\"q\\\":1}\""), "{text}");
        assert!(text.contains("\"call_id\":\"call_9\""), "{text}");
        assert!(text.contains("event: response.completed"), "{text}");
        assert!(text.contains("\"status\":\"completed\""), "{text}");
        assert!(text.contains("\"input_tokens\":11"), "{text}");
        assert!(text.contains("\"output_tokens\":7"), "{text}");

        // The completed event carries the aggregated output text.
        let completed_pos = text.find("event: response.completed").unwrap();
        let tail = &text[completed_pos..];
        assert!(tail.contains("Hello world"), "{tail}");

        // One message item plus one function_call item.
        assert_eq!(text.matches("event: response.output_item.added").count(), 2, "{text}");
        assert_eq!(text.matches("event: response.output_item.done").count(), 2, "{text}");

        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let added = events.iter().find(|event| {
            event["type"] == "response.output_item.added"
                && event["item"]["type"] == "function_call"
        }).unwrap();
        for event in events.iter().filter(|event| {
            event["type"] == "response.function_call_arguments.delta"
                || event["type"] == "response.function_call_arguments.done"
        }) {
            assert_eq!(event["output_index"], added["output_index"], "{event}");
            assert_eq!(event["item_id"], added["item"]["id"], "{event}");
        }
        let done = events.iter().find(|event| {
            event["type"] == "response.output_item.done"
                && event["item"]["type"] == "function_call"
        }).unwrap();
        assert_eq!(done["item"]["arguments"], "{\"q\":1}");
        assert_eq!(done["item"]["id"], added["item"]["id"]);
        assert_eq!(done["item"]["status"], "completed");
        assert!(text.rfind("event: response.output_item.done").unwrap() < completed_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn streaming_tool_only_response_preserves_parallel_calls_with_empty_text() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some(String::new()),
                tool_calls: Some(vec![
                    tool_call(0, Some("call_a"), Some("first"), Some("{\"value\":1}")),
                    tool_call(1, Some("call_b"), Some("second"), Some("{\"value\":2}")),
                ]),
                ..ChunkDelta::default()
            },
            Some(usage()),
        )];
        let (state, _requests) = state(&dir, chunks, None);
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "test/model-a", "input": "call both tools", "stream": true
        })).unwrap();
        let response = super::responses(State(state), headers(), axum::Json(req)).await;
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let events: Vec<Value> = text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        let completed = events.iter().find(|event| event["type"] == "response.completed").unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "{text}");
        for (index, item) in output.iter().enumerate() {
            assert_eq!(item["type"], "function_call");
            assert_eq!(item["status"], "completed");
            let done = events.iter().find(|event| {
                event["type"] == "response.output_item.done"
                    && event["output_index"] == index
            }).unwrap();
            assert_eq!(&done["item"], item);
        }
        assert_eq!(output[0]["call_id"], "call_a");
        assert_eq!(output[0]["arguments"], "{\"value\":1}");
        assert_eq!(output[1]["call_id"], "call_b");
        assert_eq!(output[1]["arguments"], "{\"value\":2}");
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
        let (state, _requests) = state(&dir, chunks, Some(502));

        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "test/model-a",
            "input": "hi",
            "stream": true
        }))
        .unwrap();

        let response = super::responses(State(state.clone()), headers(), axum::Json(req)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        let board = state.activity.board();
        let row = &board["rows"][0];
        assert_eq!(row["status"], "failed", "{row}");
        assert_eq!(row["http_status"], 502, "{row}");
        assert_eq!(row["protocol"], "responses", "{row}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn provider_responses_prefixes_model() {
        let dir = temp_dir();
        let chunks = vec![chunk(
            ChunkDelta {
                content: Some("plain answer".to_string()),
                ..ChunkDelta::default()
            },
            None,
        )];

        let req: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "model-a",
            "input": "hi",
            "stream": false
        }))
        .unwrap();

        let (state, _requests) = state(&dir, chunks, None);

        let response = super::provider_responses(
            axum::extract::Path("test".to_string()),
            State(state),
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
