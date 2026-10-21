use crate::anthropic::MessagesRequest;
use crate::gemini::{
    GeminiContent, GeminiEnvelope, GeminiFunctionCall, GeminiGenerationConfig,
    GeminiPart, GeminiRequest, GeminiResponse, GeminiSystemInstruction,
};
use crate::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChatMessage, ChatUsage, ChunkChoice, ChunkDelta,
    PromptTokensDetails, ToolCall, ToolCallChunk, FunctionCallChunk,
};
use crate::responses::ResponsesRequest;
use mg_core::error::Result;

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
                let mut tool_calls = Vec::new();

                for block in blocks {
                    let b_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match b_type {
                        "text" => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                text_parts.push(t.to_string());
                            }
                        }
                        "tool_use" => {
                            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
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
                            let tool_call_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
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

                if !text_parts.is_empty() || !tool_calls.is_empty() {
                    let content_val = if !text_parts.is_empty() {
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
                        let id = map
                            .get("call_id")
                            .or_else(|| map.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = map
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = map
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        messages.push(ChatMessage {
                            role: "assistant".to_string(),
                            content: None,
                            name: None,
                            tool_calls: Some(vec![ToolCall {
                                id,
                                tool_type: "function".to_string(),
                                function: crate::openai::FunctionCall { name, arguments },
                            }]),
                            tool_call_id: None,
                        });
                    }
                    // Tool result for a previous function_call.
                    "function_call_output" => {
                        let tool_call_id = map
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let output = map.get("output").map(extract_text).unwrap_or_default();
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(serde_json::Value::String(output)),
                            name: None,
                            tool_calls: None,
                            tool_call_id: Some(tool_call_id),
                        });
                    }
                    // Reasoning items and item references carry no replayable
                    // content for chat providers.
                    "reasoning" | "item_reference" => {}
                    _ => {
                        let role = map
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("user");
                        let role = if role == "developer" { "system" } else { role };
                        let content = map.get("content").or_else(|| map.get("text"));
                        let text = content.map(extract_text).unwrap_or_default();
                        if !text.is_empty() {
                            messages.push(ChatMessage {
                                role: role.to_string(),
                                content: Some(serde_json::Value::String(text)),
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
    let tools = req.tools.map(|list| list.into_iter().map(responses_tool_to_chat).collect());

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
            let text = extract_text(c);
            if !text.is_empty() {
                parts.push(GeminiPart::Text {
                    text,
                    thought: None,
                });
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

    let generation_config = GeminiGenerationConfig {
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: req.max_tokens.or(req.max_completion_tokens),
        stop_sequences: req.stop.as_ref().and_then(|s| {
            if let Some(arr) = s.as_array() {
                Some(arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            } else {
                s.as_str().map(|st| vec![st.to_string()])
            }
        }),
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
