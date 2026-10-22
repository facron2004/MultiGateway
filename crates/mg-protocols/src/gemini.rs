use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub request: GeminiRequest,
    pub model: String,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    #[serde(rename = "requestType")]
    pub request_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    pub role: String, // "user" or "model"
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GeminiPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    Raw(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(rename = "stopSequences", skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Thinking budget/visibility. Only sent when the caller asked for a
    /// strength: -1 means "model default", 0 means thinking is off.
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<GeminiThinkingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    pub thinking_budget: i32,
    #[serde(rename = "includeThoughts")]
    pub include_thoughts: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiChunk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<GeminiResponse>,
    #[serde(flatten)]
    pub raw: Option<GeminiResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata", skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: Option<GeminiContent>,
    #[serde(rename = "finishReason", skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    pub prompt_token_count: u64,
    #[serde(rename = "candidatesTokenCount", default)]
    pub candidates_token_count: u64,
    #[serde(rename = "totalTokenCount", default)]
    pub total_token_count: u64,
    #[serde(rename = "cachedContentTokenCount", default)]
    pub cached_content_token_count: u64,
}
