use serde::{Deserialize, Serialize};

use crate::openai::PromptTokensDetails;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// Responses clients may send `input` as a plain string or as an array of
    /// items. Normalize to an array on the way in; serialization (used by the
    /// Grok provider when calling upstream) always emits the array form.
    #[serde(deserialize_with = "deserialize_input")]
    pub input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

/// Accepts either a JSON array of input items or a single plain string, which
/// the OpenAI Responses API also allows. A string is wrapped into a one-item
/// array so downstream conversion only handles the array case.
fn deserialize_input<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Array(items) => items,
        other => vec![other],
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GrokEvent {
    #[serde(rename = "response.created")]
    Created { response: serde_json::Value },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        output_index: Option<u32>,
        item_id: Option<String>,
        content_index: Option<u32>,
        delta: String,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: Option<u32>,
        item: serde_json::Value,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        output_index: Option<u32>,
        item: serde_json::Value,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallDelta {
        output_index: Option<u32>,
        item_id: Option<String>,
        call_id: Option<String>,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallDone {
        output_index: Option<u32>,
        item_id: Option<String>,
        call_id: Option<String>,
        arguments: Option<String>,
    },
    #[serde(rename = "response.completed")]
    Completed { response: GrokResponseCompleted },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrokResponseCompleted {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub usage: GrokUsage,
    /// Optional full-response fields so the server-side Responses handler can
    /// emit a complete `response.completed` payload. Additive: providers that
    /// only parse id/model/usage (e.g. grok) ignore them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrokUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}
