use crate::anthropic::MessagesRequest;
use crate::gemini::{
    GeminiContent, GeminiEnvelope, GeminiFunctionCall, GeminiGenerationConfig, GeminiInlineData,
    GeminiPart, GeminiRequest, GeminiResponse, GeminiSystemInstruction, GeminiThinkingConfig,
};
use crate::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, ChatUsage, ChunkChoice, ChunkDelta,
    FunctionCallChunk, PromptTokensDetails, ToolCall, ToolCallChunk,
};
use crate::responses::ResponsesRequest;
use mg_core::error::Result;

/// Thinking budget requested for each reasoning tier. The tiers match what the
/// gateway advertises in `/v1/models`, so a client that reads the catalog and
/// sends the tier it saw gets the strength it expected.
pub const REASONING_BUDGETS: [(&str, i32); 6] = [
    ("minimal", 512),
    ("low", 2048),
    ("medium", 8192),
    ("high", 16384),
    ("xhigh", 24576),
    ("max", 32768),
];

/// Room kept between the thinking budget and `maxOutputTokens`: Gemini rejects a
/// request whose output ceiling does not exceed the budget.
pub const OUTPUT_HEADROOM: u32 = 1024;

/// The tier a caller asked for, from any of the spellings in use
/// (`reasoning_effort`, `reasoning.effort`, anthropic `thinking`).
fn requested_effort(req: &ChatCompletionRequest) -> Option<String> {
    let extra = &req.extra;
    let raw = extra
        .get("reasoning_effort")
        .or_else(|| extra.get("reasoningEffort"))
        .or_else(|| extra.get("thinking_effort"))
        .or_else(|| extra.get("reasoning").and_then(|v| v.get("effort")))
        .or_else(|| extra.get("thinking").and_then(|v| v.get("effort")))
        .or_else(|| extra.get("thinking").and_then(|v| v.get("level")))?;
    raw.as_str().map(str::to_string)
}

/// Translate the caller's tier into Gemini's `thinkingConfig`.
///
/// Returns `None` when the caller expressed no preference, so the model keeps
/// its own default instead of the gateway forcing thinking on or off.
fn gemini_thinking_config(req: &ChatCompletionRequest) -> Option<GeminiThinkingConfig> {
    // An explicit token budget wins over a named tier.
    if let Some(budget) = req
        .extra
        .get("thinking_budget")
        .or_else(|| req.extra.get("thinking").and_then(|v| v.get("budget_tokens")))
        .and_then(|v| v.as_i64())
    {
        return Some(GeminiThinkingConfig {
            thinking_budget: budget.clamp(0, i32::MAX as i64) as i32,
            include_thoughts: budget > 0,
        });
    }
    let tier = requested_effort(req)?;
    let label = tier.trim().to_ascii_lowercase();
    let budget = match label.as_str() {
        "none" | "off" | "disabled" => 0,
        "default" | "auto" => -1,
        _ => REASONING_BUDGETS
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, budget)| *budget)?,
    };
    Some(GeminiThinkingConfig {
        thinking_budget: budget,
        include_thoughts: budget != 0,
    })
}

pub fn anthropic_to_chat(req: MessagesRequest) -> Result<ChatCompletionRequest> {
    let mut messages = Vec::new();

    // 1. System prompt
    if let Some(sys) = req.system {
        let text = extract_text(&sys);
        if !text.is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(text)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    // 2. Messages
    for msg in req.messages {
        let role = msg.role;
        match msg.content {
            serde_json::Value::String(s) => {
                messages.push(ChatMessage {
                    role,
                    content: Some(serde_json::Value::String(s)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            serde_json::Value::Array(blocks) => {
                let mut text_parts = Vec::new();
                let mut content_parts = Vec::new();
                let mut has_image = false;
                let mut tool_calls = Vec::new();

                for block in blocks {
                    let b_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match b_type {
                        "text" => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                text_parts.push(t.to_string());
                                content_parts
                                    .push(serde_json::json!({"type": "text", "text": t}));
                            }
                        }
                        "image" => {
                            if let Some(url) = image_url(&block) {
                                has_image = true;
                                content_parts.push(
                                    serde_json::json!({"type": "image_url", "image_url": {"url": url}}),
                                );
                            }
                        }
                        "tool_use" => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let input =
                                block.get("input").cloned().unwrap_or(serde_json::json!({}));
                            tool_calls.push(ToolCall {
                                id,
                                tool_type: "function".to_string(),
                                function: crate::openai::FunctionCall {
                                    name,
                                    arguments: input.to_string(),
                                },
                            });
                        }
                        "tool_result" => {
                            let tool_call_id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let content_val = block.get("content");
                            let content_text = content_val.map(extract_text).unwrap_or_default();
                            messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: Some(serde_json::Value::String(content_text)),
                                name: None,
                                tool_calls: None,
                                tool_call_id: Some(tool_call_id),
                            });
                        }
                        _ => {}
                    }
                }

                if !text_parts.is_empty() || !tool_calls.is_empty() || has_image {
                    let content_val = if has_image {
                        Some(serde_json::Value::Array(content_parts))
                    } else if !text_parts.is_empty() {
                        Some(serde_json::Value::String(text_parts.join("\n")))
                    } else {
                        None
                    };
                    let tools_val = if !tool_calls.is_empty() {
                        Some(tool_calls)
                    } else {
                        None
                    };
                    messages.push(ChatMessage {
                        role,
                        content: content_val,
                        name: None,
                        tool_calls: tools_val,
                        tool_call_id: None,
                    });
                }
            }
            _ => {}
        }
    }

    // 3. Tools
    let tools = req.tools.map(|list| {
        list.into_iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema
                    }
                })
            })
            .collect()
    });

    Ok(ChatCompletionRequest {
        model: req.model,
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        n: None,
        stream: req.stream,
        stop: req.stop_sequences.map(|s| serde_json::json!(s)),
        max_tokens: Some(req.max_tokens),
        max_completion_tokens: None,
        tools,
        tool_choice: req.tool_choice,
        parallel_tool_calls: None,
        extra: std::collections::HashMap::new(),
    })
}

fn extract_text(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(t);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn text_part(part: &serde_json::Value) -> Option<&str> {
    match part.get("type").and_then(serde_json::Value::as_str) {
        Some("text" | "input_text" | "output_text") => {
            part.get("text").and_then(serde_json::Value::as_str)
        }
        _ => None,
    }
}

/// Extracts an image URL from OpenAI (`image_url`/`input_image`) and Anthropic
/// (`image.source`) content parts. Base64 images become data URLs.
fn image_url(part: &serde_json::Value) -> Option<String> {
    if part.get("type").and_then(serde_json::Value::as_str) == Some("image") {
        let source = part.get("source")?;
        return match source
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
        {
            "base64" => {
                let media = source
                    .get("media_type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("image/png");
                let data = source.get("data").and_then(serde_json::Value::as_str)?;
                Some(format!("data:{media};base64,{data}"))
            }
            "url" => source
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        };
    }
    if !matches!(
        part.get("type").and_then(serde_json::Value::as_str),
        Some("image_url" | "input_image")
    ) {
        return None;
    }
    part.pointer("/image_url/url")
        .and_then(serde_json::Value::as_str)
        .or_else(|| part.get("image_url").and_then(serde_json::Value::as_str))
        .or_else(|| part.get("url").and_then(serde_json::Value::as_str))
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

fn gemini_inline_image(part: &serde_json::Value) -> Option<(String, String)> {
    let url = image_url(part)?;
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

/// Converts a message content value into OpenAI chat content, preserving text
/// and image parts in order when any image is present.
fn openai_content(content: &serde_json::Value) -> Option<serde_json::Value> {
    match content {
        serde_json::Value::String(text) if !text.is_empty() => {
            Some(serde_json::Value::String(text.clone()))
        }
        serde_json::Value::Array(parts) => {
            let mut converted = Vec::new();
            let mut has_image = false;
            for part in parts {
                if let Some(text) = text_part(part) {
                    if !text.is_empty() {
                        converted.push(serde_json::json!({"type": "text", "text": text}));
                    }
                } else if let Some(url) = image_url(part) {
                    has_image = true;
                    converted.push(
                        serde_json::json!({"type": "image_url", "image_url": {"url": url}}),
                    );
                }
            }
            if has_image {
                Some(serde_json::Value::Array(converted))
            } else if converted.is_empty() {
                None
            } else {
                let text = extract_text(content);
                if text.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::String(text))
                }
            }
        }
        _ => None,
    }
}

fn responses_call_id(map: &serde_json::Map<String, serde_json::Value>) -> String {
    map.get("call_id")
        .or_else(|| map.get("tool_call_id"))
        .or_else(|| map.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn responses_arguments(map: &serde_json::Map<String, serde_json::Value>) -> String {
    match map.get("arguments") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => "{}".to_string(),
    }
}

/// Fold a Responses function_call into the open assistant turn.
///
/// A parallel batch arrives as several adjacent `function_call` items. Emitting
/// one assistant message per item is what DeepSeek-family upstreams reject
/// (tool calls and tool results do not match): the results then follow only
/// the last declaration.
fn append_responses_tool_call(
    messages: &mut Vec<ChatMessage>,
    id: String,
    name: String,
    arguments: String,
) {
    let call = ToolCall {
        id,
        tool_type: "function".to_string(),
        function: crate::openai::FunctionCall { name, arguments },
    };
    if let Some(prev) = messages.last_mut() {
        if prev.role == "assistant" {
            prev.tool_calls.get_or_insert_with(Vec::new).push(call);
            return;
        }
    }
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: None,
        name: None,
        tool_calls: Some(vec![call]),
        tool_call_id: None,
    });
}

/// Convert an OpenAI Responses API request into the internal chat form.
///
/// `instructions` becomes a leading system message. Each `input` entry is
/// either a bare string (treated as a user turn) or an object with `role` and
/// `content`, where `content` may itself be a string or an array of typed
/// parts (`input_text`/`output_text`/`text`). This is the inverse of the
/// chat -> Responses mapping in `mg-providers/src/grok.rs`.
pub fn responses_to_chat(req: ResponsesRequest) -> Result<ChatCompletionRequest> {
    let mut messages = Vec::new();

    // 1. Instructions -> system prompt.
    if let Some(instructions) = req.instructions {
        if !instructions.is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(instructions)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    // 2. Input items.
    for item in req.input {
        match item {
            serde_json::Value::String(s) => {
                if !s.is_empty() {
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: Some(serde_json::Value::String(s)),
                        name: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            serde_json::Value::Object(map) => {
                let item_type = map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("message");
                match item_type {
                    // Assistant tool invocation echoed back by the client.
                    "function_call" => {
                        append_responses_tool_call(
                            &mut messages,
                            responses_call_id(&map),
                            map.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            responses_arguments(&map),
                        );
                    }
                    // Tool result for a previous function_call. `id` is only a
                    // fallback: compliant items carry `call_id`, and an item id
                    // must not win over it.
                    "function_call_output" => {
                        let output = map.get("output").map(extract_text).unwrap_or_default();
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(serde_json::Value::String(output)),
                            name: None,
                            tool_calls: None,
                            tool_call_id: Some(responses_call_id(&map)),
                        });
                    }
                    // Reasoning items and item references carry no replayable
                    // content for chat providers.
                    "reasoning" | "item_reference" => {}
                    _ => {
                        let role = map.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                        let role = if role == "developer" { "system" } else { role };
                        let content = map.get("content").or_else(|| map.get("text"));
                        if let Some(content) = content.and_then(openai_content) {
                            messages.push(ChatMessage {
                                role: role.to_string(),
                                content: Some(content),
                                name: None,
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 3. Tools: the Responses API uses a flat function-tool shape
    // ({type, name, description, parameters}) while chat providers expect the
    // nested {type, function:{...}} shape. Wrap flat tools; pass through any
    // that already carry a `function` object (the shape grok.rs sends upstream).
    let tools = req
        .tools
        .map(|list| list.into_iter().map(responses_tool_to_chat).collect());

    Ok(ChatCompletionRequest {
        model: req.model,
        messages,
        temperature: req.temperature,
        top_p: None,
        n: None,
        stream: Some(req.stream),
        stop: None,
        max_tokens: None,
        max_completion_tokens: req.max_output_tokens,
        tools,
        tool_choice: None,
        parallel_tool_calls: None,
        extra: std::collections::HashMap::new(),
    })
}

fn responses_tool_to_chat(tool: serde_json::Value) -> serde_json::Value {
    if tool.get("function").is_some() {
        return tool;
    }
    let mut function = serde_json::Map::new();
    for key in ["name", "description", "parameters", "strict"] {
        if let Some(v) = tool.get(key) {
            function.insert(key.to_string(), v.clone());
        }
    }
    serde_json::json!({ "type": "function", "function": serde_json::Value::Object(function) })
}

pub fn chat_to_gemini(
    req: &ChatCompletionRequest,
    project: Option<String>,
) -> Result<GeminiEnvelope> {
    let mut contents = Vec::new();
    let mut system_instruction = None;

    for msg in &req.messages {
        let role = match msg.role.as_str() {
            "system" => {
                if let Some(c) = &msg.content {
                    system_instruction = Some(GeminiSystemInstruction {
                        parts: vec![GeminiPart::Text {
                            text: extract_text(c),
                            thought: None,
                        }],
                    });
                }
                continue;
            }
            "assistant" => "model",
            "tool" => "user",
            _ => "user",
        };

        let mut parts = Vec::new();

        if let Some(c) = &msg.content {
            match c {
                serde_json::Value::Array(items) => {
                    for item in items {
                        if let Some(text) = text_part(item) {
                            if !text.is_empty() {
                                parts.push(GeminiPart::Text {
                                    text: text.to_string(),
                                    thought: None,
                                });
                            }
                        } else if let Some((mime_type, data)) = gemini_inline_image(item) {
                            parts.push(GeminiPart::InlineData {
                                inline_data: GeminiInlineData { mime_type, data },
                            });
                        }
                    }
                }
                _ => {
                    let text = extract_text(c);
                    if !text.is_empty() {
                        parts.push(GeminiPart::Text {
                            text,
                            thought: None,
                        });
                    }
                }
            }
        }

        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                let args_val = serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({}));
                parts.push(GeminiPart::FunctionCall {
                    function_call: GeminiFunctionCall {
                        name: tc.function.name.clone(),
                        args: args_val,
                    },
                });
            }
        }

        if msg.role == "tool" {
            if let Some(tool_call_id) = &msg.tool_call_id {
                let resp_val = msg
                    .content
                    .as_ref()
                    .map(|c| serde_json::json!({ "result": extract_text(c) }))
                    .unwrap_or_else(|| serde_json::json!({}));
                parts.push(GeminiPart::FunctionResponse {
                    function_response: crate::gemini::GeminiFunctionResponse {
                        name: tool_call_id.clone(),
                        response: resp_val,
                    },
                });
            }
        }

        if !parts.is_empty() {
            contents.push(GeminiContent {
                role: role.to_string(),
                parts,
            });
        }
    }

    let thinking_config = gemini_thinking_config(req);
    let mut max_output_tokens = req.max_tokens.or(req.max_completion_tokens);
    // Gemini rejects a request whose output ceiling does not exceed its thinking
    // budget, so a caller combining a small max_tokens with a strong tier would
    // otherwise get a hard 400. Widen the ceiling instead of dropping the tier.
    if let (Some(limit), Some(thinking)) = (max_output_tokens, thinking_config.as_ref()) {
        let budget = thinking.thinking_budget.max(0) as u32;
        if budget > 0 && limit <= budget {
            max_output_tokens = Some(budget.saturating_add(OUTPUT_HEADROOM));
        }
    }

    let generation_config = GeminiGenerationConfig {
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens,
        stop_sequences: req.stop.as_ref().and_then(|s| {
            if let Some(arr) = s.as_array() {
                Some(
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                )
            } else {
                s.as_str().map(|st| vec![st.to_string()])
            }
        }),
        thinking_config,
    };

    Ok(GeminiEnvelope {
        project,
        request_id: uuid::Uuid::new_v4().to_string(),
        request: GeminiRequest {
            contents,
            system_instruction,
            generation_config: Some(generation_config),
            tools: req.tools.clone(),
        },
        model: req.model.clone(),
        user_agent: "antigravity-rust/0.10.0".to_string(),
        request_type: "AGENT_REQUEST_TYPE_CHAT".to_string(),
    })
}

pub fn gemini_to_chat_chunk(
    resp: GeminiResponse,
    model: &str,
    chunk_id: &str,
) -> Option<ChatCompletionChunk> {
    let now = chrono::Utc::now().timestamp() as u64;
    let candidate = resp.candidates.into_iter().next();

    let mut delta = ChunkDelta::default();
    let mut finish_reason = None;

    if let Some(cand) = candidate {
        if let Some(fr) = cand.finish_reason {
            finish_reason = Some(match fr.as_str() {
                "STOP" => "stop".to_string(),
                "MAX_TOKENS" => "length".to_string(),
                _ => fr.to_lowercase(),
            });
        }

        if let Some(content) = cand.content {
            for part in content.parts {
                match part {
                    GeminiPart::Text { text, thought } => {
                        if thought == Some(true) {
                            delta.reasoning_content = Some(text);
                        } else {
                            delta.content = Some(text);
                        }
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        delta.tool_calls = Some(vec![ToolCallChunk {
                            index: 0,
                            id: Some(format!("call_{}", uuid::Uuid::new_v4().simple())),
                            tool_type: Some("function".to_string()),
                            function: Some(FunctionCallChunk {
                                name: Some(function_call.name),
                                arguments: Some(function_call.args.to_string()),
                            }),
                        }]);
                    }
                    _ => {}
                }
            }
        }
    }

    let usage = resp.usage_metadata.map(|u| ChatUsage {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count,
        total_tokens: u.total_token_count,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: u.cached_content_token_count,
        }),
        credits: None,
    });

    if delta.content.is_none()
        && delta.reasoning_content.is_none()
        && delta.tool_calls.is_none()
        && finish_reason.is_none()
        && usage.is_none()
    {
        return None;
    }

    Some(ChatCompletionChunk {
        id: chunk_id.to_string(),
        object: "chat.completion.chunk".to_string(),
        created: now,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason,
        }],
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::ResponsesRequest;

    fn converted(input: serde_json::Value) -> ChatCompletionRequest {
        let req: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "input": input,
        }))
        .unwrap();
        responses_to_chat(req).unwrap()
    }

    #[test]
    fn anthropic_images_become_openai_image_parts() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "gemini-3-flash",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is in this screenshot?"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "aGk="
                    }}
                ]
            }]
        }))
        .unwrap();
        let chat = anthropic_to_chat(req).unwrap();
        assert_eq!(
            chat.messages[0].content,
            Some(serde_json::json!([
                {"type": "text", "text": "what is in this screenshot?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
            ]))
        );
    }

    #[test]
    fn responses_input_images_reach_chat_content() {
        let chat = converted(serde_json::json!([
            {"role": "user", "content": [
                {"type": "input_text", "text": "look at this"},
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ]}
        ]));
        assert_eq!(
            chat.messages[0].content,
            Some(serde_json::json!([
                {"type": "text", "text": "look at this"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]))
        );
    }

    #[test]
    fn gemini_request_inlines_base64_images() {
        let req: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "gemini-3-flash",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,Zm9v"}}
            ]}]
        }))
        .unwrap();
        let body = serde_json::to_value(chat_to_gemini(&req, None).unwrap()).unwrap();
        assert_eq!(
            body["request"]["contents"][0]["parts"][0]["text"],
            "describe this"
        );
        assert_eq!(
            body["request"]["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/jpeg"
        );
        assert_eq!(
            body["request"]["contents"][0]["parts"][1]["inlineData"]["data"],
            "Zm9v"
        );
    }

    #[test]
    fn parallel_function_calls_share_one_assistant_message() {
        let chat = converted(serde_json::json!([
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "running both"}
            ]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "a", "arguments": "{}"},
            {"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "b", "arguments": "{}"},
            {"type": "function_call_output", "id": "fco_1", "call_id": "call_1", "output": "one"},
            {"type": "message", "role": "developer", "content": "resized"},
            {"type": "function_call_output", "id": "fco_2", "call_id": "call_2", "output": "two"}
        ]));

        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[0].role, "assistant");
        assert_eq!(
            chat.messages[0].content,
            Some(serde_json::json!("running both"))
        );
        let calls = chat.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_1", "call_2"]
        );
        assert_eq!(chat.messages[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(chat.messages[2].role, "system");
        assert_eq!(chat.messages[3].tool_call_id.as_deref(), Some("call_2"));
    }

    #[test]
    fn a_later_function_call_does_not_join_a_closed_batch() {
        let chat = converted(serde_json::json!([
            {"type": "function_call", "call_id": "call_1", "name": "a", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "one"},
            {"type": "function_call", "call_id": "call_2", "name": "b", "arguments": "{}"}
        ]));

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[0].tool_calls.as_ref().unwrap().len(), 1);
        assert_eq!(
            chat.messages[0].tool_calls.as_ref().unwrap()[0].id,
            "call_1"
        );
        assert_eq!(chat.messages[1].role, "tool");
        assert_eq!(
            chat.messages[2].tool_calls.as_ref().unwrap()[0].id,
            "call_2"
        );
    }

    fn gemini_request(extra: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": "gemini-3-flash",
            "messages": [{"role": "user", "content": "hi"}]
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        let req: ChatCompletionRequest = serde_json::from_value(body).unwrap();
        serde_json::to_value(chat_to_gemini(&req, None).unwrap()).unwrap()
    }

    #[test]
    fn gemini_thinking_follows_the_advertised_tier_names() {
        // A client that reads `reasoning_efforts` from /v1/models sends one of
        // those names back; each must map onto a Gemini thinking budget.
        for (tier, budget) in [("low", 2048), ("medium", 8192), ("high", 16384)] {
            let request = gemini_request(serde_json::json!({"reasoning_effort": tier}));
            let config = &request["request"]["generationConfig"]["thinkingConfig"];
            assert_eq!(config["thinkingBudget"], budget, "tier {tier}");
            assert_eq!(config["includeThoughts"], true);
        }
        // "none" turns thinking off rather than asking for the weakest tier.
        let off = gemini_request(serde_json::json!({"reasoning_effort": "none"}));
        assert_eq!(
            off["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
        assert_eq!(
            off["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
            false
        );
        // No preference at all leaves the model's own default in place.
        let silent = gemini_request(serde_json::json!({}));
        assert!(silent["request"]["generationConfig"]["thinkingConfig"].is_null());
    }

    #[test]
    fn gemini_output_ceiling_always_exceeds_the_thinking_budget() {
        // Gemini rejects maxOutputTokens <= thinkingBudget; a strong tier with a
        // small caller budget must widen the ceiling instead of failing.
        let request = gemini_request(serde_json::json!({
            "reasoning_effort": "high", "max_tokens": 1024
        }));
        let config = &request["request"]["generationConfig"];
        let budget = config["thinkingConfig"]["thinkingBudget"].as_u64().unwrap();
        let output = config["maxOutputTokens"].as_u64().unwrap();
        assert!(output > budget, "output {output} must exceed budget {budget}");
        assert_eq!(output, budget + OUTPUT_HEADROOM as u64);

        // An explicit token budget is honoured, and still guarded.
        let request = gemini_request(serde_json::json!({
            "thinking_budget": 32000, "max_tokens": 8000
        }));
        let config = &request["request"]["generationConfig"];
        assert_eq!(config["thinkingConfig"]["thinkingBudget"], 32000);
        assert!(config["maxOutputTokens"].as_u64().unwrap() > 32000);

        // A generous ceiling is left exactly as the caller set it.
        let request = gemini_request(serde_json::json!({
            "reasoning_effort": "low", "max_tokens": 65536
        }));
        assert_eq!(request["request"]["generationConfig"]["maxOutputTokens"], 65536);
    }
}
