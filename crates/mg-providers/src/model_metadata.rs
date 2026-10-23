//! Model catalog metadata: turn whatever an upstream publishes into one
//! consistent description an agent client can act on.
//!
//! OpenAI's model schema only names `id`/`object`/`created`/`owned_by`, so a
//! client that needs to know "does this model read images, how much can it
//! write, how hard can it think" has to read conventions. Every provider here
//! publishes a different one - WorkBuddy's `supportsImages`/`maxOutputTokens`,
//! Trae's `reasoning_effort_config`, Qoder's `thinking_config`,
//! Copilot's `capabilities.limits`, MiniMax's Anthropic-protocol models - so
//! this module probes all of them and re-publishes the answer under every
//! spelling the common clients (OpenRouter, LobeChat, Cherry Studio/NextChat)
//! look for, plus a normalised `thinking` block.
//!
//! The rule throughout is: **never invent a limitation**. A capability that is
//! genuinely unknown stays `null` (and out of `input_modalities`/`output_modalities`),
//! while an explicit `false` from the upstream is preserved. The one exception
//! is [`ProviderDefaults`], which fills in what a provider is *known* to
//! support when its own catalog does not say - that only ever turns an unknown
//! into a fact, never overrides an explicit upstream answer.
//!
//! Consistency beats cleverness for the fields a client switches on: the
//! capability bools, `reasoning_can_disable`, `thinking`, and the two modality
//! lists are published on **every** entry, so a client reads one shape for all
//! providers instead of special-casing whichever upstream answered.

use mg_protocols::openai::ModelEntry;
use serde_json::{json, Map, Value};

/// Reasoning tiers in ascending strength. Every provider's own naming is mapped
/// onto this order so one client control can reach all of them.
pub const EFFORT_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Tier spellings folded onto [`EFFORT_LEVELS`].
fn canonical_effort(name: &str) -> Option<&'static str> {
    Some(
        match name.trim().to_ascii_lowercase().replace(' ', "_").as_str() {
            "minimal" | "min" => "minimal",
            "light" | "low" => "low",
            "medium" | "mid" | "normal" | "default" | "balanced" => "medium",
            "high" => "high",
            "extra_high" | "extra-high" | "x-high" | "xhigh" | "very_high" => "xhigh",
            "max" | "maximum" | "ultimate" | "ultra" => "max",
            _ => return None,
        },
    )
}

/// Tier spellings that mean "thinking off". They are advertised under
/// [`Thinking::can_disable`], never as the weakest strength tier.
fn is_disabled_effort(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "none" | "off" | "disabled" | "no" | "false" | "0"
    )
}

/// Capability facts that a provider is known to support, used to fill gaps the
/// upstream catalog leaves and to classify model families by name.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderDefaults {
    /// Whether the provider's chat models read images.
    pub vision: Option<bool>,
    /// Whether they call tools.
    pub tool_calls: Option<bool>,
    /// Whether they reason at all.
    pub reasoning: Option<bool>,
    /// Tiers to advertise when the catalog gives no ladder of its own.
    pub efforts: Option<&'static [&'static str]>,
    /// Tier to report as the default when none is published.
    pub default_effort: Option<&'static str>,
    /// Whether thinking can be switched off when no toggle is published.
    pub can_disable: bool,
    /// Families (matched as an id prefix) that read images.
    pub vision_families: &'static [&'static str],
    /// Families that are plain chat models with no thinking tier.
    pub plain_families: &'static [&'static str],
}

impl ProviderDefaults {
    /// The defaults for one provider id as registered in the gateway.
    pub fn for_provider(provider: &str) -> Self {
        match provider {
            // Every model the Antigravity client lists (Gemini and Claude)
            // accepts images; Gemini ids reason and vary by -high/-medium/-low.
            "antigravity" => Self {
                vision: Some(true),
                tool_calls: Some(true),
                reasoning: Some(true),
                efforts: Some(&["low", "medium", "high"]),
                default_effort: Some("high"),
                can_disable: false,
                vision_families: &[],
                plain_families: &[],
            },
            // Grok reads images and calls tools; `build`/`code-fast` ids are the
            // non-reasoning coding variant.
            "grok" => Self {
                vision: Some(true),
                tool_calls: Some(true),
                reasoning: Some(true),
                efforts: Some(&["low", "medium", "high", "xhigh"]),
                default_effort: Some("high"),
                can_disable: true,
                vision_families: &[],
                plain_families: &["grok-build", "grok-code-fast"],
            },

            // Copilot's own catalog carries `capabilities.supports`/`limits` for
            // most entries; these cover the ids that ship without either.
            "copilot" => Self {
                vision: Some(true),
                tool_calls: Some(true),
                reasoning: None,
                efforts: Some(&["low", "medium", "high"]),
                default_effort: Some("medium"),
                can_disable: true,
                vision_families: &[],
                plain_families: &["gpt-3.5", "gpt-4-0613", "gpt-4-0125-preview"],
            },
            // MiniMax's agent models speak the Anthropic protocol (images and
            // tool calls) and reason with an xhigh top tier.
            "minimax" => Self {
                vision: Some(true),
                tool_calls: Some(true),
                reasoning: Some(true),
                efforts: Some(&["low", "medium", "high", "xhigh"]),
                default_effort: Some("high"),
                can_disable: true,
                vision_families: &[],
                plain_families: &[],
            },
            // Trae is a coding agent upstream: every model it lists takes tool
            // calls, and its catalog names the ladder via reasoning_effort_config.
            "trae" => Self {
                vision: None,
                tool_calls: Some(true),
                reasoning: None,
                efforts: Some(&["low", "high", "xhigh"]),
                default_effort: Some("high"),
                can_disable: true,
                vision_families: &[],
                plain_families: &[],
            },
            _ => Self::default(),
        }
    }
}

/// The parts a [`ModelEntry`] exposes to the probes below.
struct Fields<'a> {
    id: &'a str,
    extra: &'a Map<String, Value>,
    capabilities: Option<&'a Value>,
}

impl<'a> Fields<'a> {
    fn of(model: &'a ModelEntry) -> Self {
        Self {
            id: &model.id,
            extra: &model.extra,
            capabilities: model.capabilities.as_ref(),
        }
    }

    /// Resolve a top-level key exactly like `serde_json::to_value(&model)` would:
    /// flattened `extra` entries win, falling back to the structured
    /// `capabilities` field (the only structured field the probes below read).
    fn field(&self, key: &str) -> Option<&'a Value> {
        self.extra.get(key).or_else(|| {
            if key == "capabilities" {
                self.capabilities
            } else {
                None
            }
        })
    }

    /// `raw.pointer("/<first>/<rest...>")` without serializing.
    fn pointer(&self, first: &str, rest: &[&str]) -> Option<&'a Value> {
        let mut current = self.field(first)?;
        for key in rest {
            current = current.get(*key)?;
        }
        Some(current)
    }
}

fn first_bool(values: &[Option<bool>]) -> Option<bool> {
    values.iter().flatten().copied().next()
}

fn bool_field(value: Option<&Value>) -> Option<bool> {
    value.and_then(Value::as_bool)
}

fn string_field(values: &[Option<&Value>]) -> Option<String> {
    values
        .iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

/// Effort labels from any of the shapes providers use: a list of names, a list
/// of objects, or a `{tier: {...}}` map.
fn effort_names(value: Option<&Value>) -> Vec<String> {
    let label = |item: &Value| -> Option<String> {
        item.as_str()
            .map(str::to_string)
            .or_else(|| {
                ["effort", "value", "level", "name", "id"]
                    .iter()
                    .find_map(|key| item.get(*key).and_then(Value::as_str))
                    .map(str::to_string)
            })
            .filter(|name| !name.is_empty())
    };
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(label).collect(),
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::String(name)) if !name.is_empty() => vec![name.clone()],
        _ => Vec::new(),
    }
}

/// The thinking tiers a model advertises, in ascending order and de-duplicated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Thinking {
    pub tiers: Vec<String>,
    pub default_effort: String,
    pub can_disable: bool,
}

impl Thinking {
    fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// The `thinking` block clients read: supported tiers, the default one, and
    /// whether thinking can be turned off entirely.
    fn block(&self) -> Value {
        let efforts: Vec<Value> = self
            .tiers
            .iter()
            .map(|tier| {
                json!({"effort": tier, "label": tier,
                       "is_default": tier == &self.default_effort})
            })
            .collect();
        let mut levels = Vec::with_capacity(self.tiers.len() + 1);
        if self.can_disable {
            levels.push("none".to_string());
        }
        levels.extend(self.tiers.iter().cloned());
        json!({
            "supported": true,
            "can_disable": self.can_disable,
            "default_effort": self.default_effort,
            "efforts": efforts,
            "levels": levels,
        })
    }
}

/// Collect the raw effort labels a catalog publishes (before normalisation).
fn raw_efforts(fields: &Fields<'_>, features: &Value) -> Vec<String> {
    let sources: [Option<&Value>; 8] = [
        fields.field("supported_reasoning_efforts"),
        fields.field("reasoning_efforts"),
        fields.pointer("reasoning_effort_config", &["options"]),
        fields.pointer("reasoning", &["supportedEfforts"]),
        fields.pointer("thinking", &["efforts"]),
        fields.field("supported_reasoning_levels"),
        features.pointer("/reasoning/efforts"),
        // Copilot publishes its ladder under `capabilities.supports`.
        fields.pointer("capabilities", &["supports", "reasoning_effort"]),
    ];
    for source in sources.into_iter().flatten() {
        let names = effort_names(Some(source));
        if !names.is_empty() {
            return names;
        }
    }
    // Qoder publishes `{tier: {is_default}}`; that shape needs its own read.
    effort_names(fields.pointer("thinking_config", &["enabled", "efforts"]))
}

/// Normalise raw labels into [`Thinking`], ordering by [`EFFORT_LEVELS`].
fn normalize_thinking(names: &[String]) -> Thinking {
    let mut thinking = Thinking::default();
    for name in names {
        if is_disabled_effort(name) {
            thinking.can_disable = true;
            continue;
        }
        if let Some(tier) = canonical_effort(name) {
            if !thinking.tiers.iter().any(|known| known == tier) {
                thinking.tiers.push(tier.to_string());
            }
        }
    }
    thinking.tiers.sort_by_key(|tier| {
        EFFORT_LEVELS
            .iter()
            .position(|level| level == tier)
            .unwrap_or(usize::MAX)
    });
    thinking
}

/// A single strength the model is pinned to (WorkBuddy's `reasoning.effort`).
fn fixed_effort(fields: &Fields<'_>) -> Option<String> {
    string_field(&[
        fields.field("reasoning_fixed_effort"),
        fields.pointer("reasoning", &["effort"]),
        fields.field("reasoning_effort"),
    ])
    .filter(|name| !is_disabled_effort(name))
}

/// Whether an effort map marks one tier as the default.
fn default_from_map(value: Option<&Value>) -> Option<String> {
    let map = value?.as_object()?;
    map.iter()
        .find(|(_, meta)| meta.get("is_default").and_then(Value::as_bool) == Some(true))
        .map(|(name, _)| name.clone())
}

/// The tier encoded in a model id, e.g. `gemini-3.8-flash-high` -> `high`.
///
/// Some providers publish one id per strength instead of one id with a ladder;
/// reading the suffix keeps the advertised default honest, so a client does not
/// see `-low` described as `high`.
fn tier_from_id(id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    let tail = lower.rsplit(['-', '_']).next()?;
    if tail.is_empty() || tail.len() > 3 && tail != "medium" {
        return None;
    }
    match tail {
        "low" | "light" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "max" | "xhigh" => Some("xhigh"),
        _ => None,
    }
}

/// Read every context-window / output-limit spelling a provider might publish.
fn limits(fields: &Fields<'_>) -> (Option<u64>, Option<u64>) {
    let positive = |value: Option<&Value>| {
        value
            .and_then(Value::as_u64)
            .or_else(|| value.and_then(Value::as_f64).map(|number| number as u64))
            .filter(|number| *number > 0)
    };
    let context = positive(fields.field("context_length"))
        .or_else(|| positive(fields.field("max_input_tokens")))
        .or_else(|| positive(fields.field("maxInputTokens")))
        .or_else(|| positive(fields.field("context_window")))
        .or_else(|| positive(fields.pointer("context_window", &["defaultLength"])))
        .or_else(|| {
            positive(fields.pointer("capabilities", &["limits", "max_context_window_tokens"]))
        });
    let output = positive(fields.field("max_output_tokens"))
        .or_else(|| positive(fields.field("max_completion_tokens")))
        .or_else(|| positive(fields.field("maxOutputTokens")))
        .or_else(|| positive(fields.pointer("capabilities", &["limits", "max_output_tokens"])))
        .or_else(|| {
            positive(fields.pointer(
                "capabilities",
                &["limits", "max_non_streaming_output_tokens"],
            ))
        });
    (context, output)
}

/// Model families that are text-only even though some gateways publish a
/// blanket `supportsImages: true` for every model they host. Sourced from the
/// vendor-official models.dev entries (zai / deepseek / tencent / openai).
fn is_known_text_only(id: &str) -> bool {
    let id = id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
    matches!(id.as_str(), "glm-5.1" | "glm-5.2" | "glm-5.3")
        || id.starts_with("deepseek-v4-pro")
        || id.starts_with("hy3")
        || id.starts_with("hy4-preview")
        || id.starts_with("gpt-oss-120b")
}

fn id_matches(id: &str, families: &[&str]) -> bool {
    let lower = id.to_ascii_lowercase();
    // A family also matches with a build/variant suffix (`grok-4.7-build-fast`
    // belongs to the `grok-4.7` line, not to the plain `grok-build` one), so a
    // stricter family is never shadowed by a broader sibling.
    families
        .iter()
        .any(|family| lower.starts_with(family) || lower.contains(&format!("-{family}-")))
}

/// Advertise upstream model metadata in a consistent form for agent clients.
pub fn with_agent_metadata(model: ModelEntry) -> ModelEntry {
    with_provider_metadata(model, ProviderDefaults::default())
}

/// Like [`with_agent_metadata`], with the provider's known facts available to
/// fill gaps the catalog leaves. An explicit upstream answer is never overridden.
pub fn with_provider_metadata(mut model: ModelEntry, defaults: ProviderDefaults) -> ModelEntry {
    let features = match Fields::of(&model).field("features") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    let fields = Fields::of(&model);
    let thinking_config = fields.field("thinking_config");

    // ---- thinking strength ----
    // A family the provider knows to be plain chat (no thinking) has no ladder
    // at all: advertising one would let a client ask for a tier it rejects.
    let is_plain = id_matches(fields.id, defaults.plain_families);
    let mut thinking = if is_plain {
        Thinking::default()
    } else {
        normalize_thinking(&raw_efforts(&fields, &features))
    };
    if thinking.tiers.is_empty() && !is_plain {
        // A model that publishes one fixed effort (`reasoning.effort`) reasons
        // at that strength only; a provider ladder is the broader fallback.
        if let Some(fixed) = fixed_effort(&fields) {
            thinking = normalize_thinking(&[fixed]);
        }
    }
    let reasoning_explicitly_off = bool_field(fields.field("supports_reasoning")) == Some(false)
        || bool_field(fields.field("supportsReasoning")) == Some(false)
        || bool_field(fields.field("is_reasoning")) == Some(false)
        || bool_field(fields.pointer("capabilities", &["supports", "reasoning"])) == Some(false);
    if thinking.tiers.is_empty()
        && !is_plain
        && defaults.reasoning == Some(true)
        && !reasoning_explicitly_off
    {
        if let Some(efforts) = defaults.efforts {
            thinking = normalize_thinking(
                &efforts
                    .iter()
                    .map(|tier| tier.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }
    let default_effort = string_field(&[
        fields.field("reasoning_default_effort"),
        fields.field("default_reasoning_effort"),
        fields.pointer("reasoning_effort_config", &["default_level"]),
        fields.pointer("reasoning", &["defaultEffort"]),
        fields.pointer("thinking", &["default_effort"]),
    ])
    .map(|name| canonical_effort(&name).map(str::to_string).unwrap_or(name))
    .filter(|name| thinking.tiers.contains(name))
    .or_else(|| default_from_map(fields.pointer("thinking_config", &["enabled", "efforts"])))
    .filter(|name| thinking.tiers.contains(name))
    .or_else(|| {
        // A provider that publishes one id per strength pins that strength.
        tier_from_id(fields.id)
            .map(str::to_string)
            .filter(|name| thinking.tiers.contains(name))
    })
    .or_else(|| {
        defaults
            .default_effort
            .map(str::to_string)
            .filter(|name| thinking.tiers.contains(name))
    })
    .or_else(|| thinking.tiers.first().cloned())
    .unwrap_or_default();
    thinking.default_effort = default_effort;

    let reasoning_budget_declared = fields
        .pointer("capabilities", &["supports", "max_thinking_budget"])
        .and_then(Value::as_u64)
        .is_some_and(|budget| budget > 0);
    let reasoning = first_bool(&[
        bool_field(fields.field("supports_reasoning")),
        bool_field(fields.field("supportsReasoning")),
        // Copilot marks reasoning-capable models only with
        // `capabilities.supports.adaptive_thinking`.
        bool_field(fields.pointer("capabilities", &["supports", "adaptive_thinking"])),
        reasoning_budget_declared.then_some(true),
        // `thinking_config` is a structured declaration of a toggle and a tier
        // ladder; Qoder sets `is_reasoning: false` on models that still carry it,
        // so the richer field has to win over the flat legacy flag.
        bool_field(
            thinking_config
                .and_then(|value| value.get("enabled"))
                .and_then(|enabled| enabled.get("is_default"))
                .filter(|value| value.is_boolean()),
        )
        .or_else(|| {
            thinking_config
                .and_then(|value| value.get("enabled"))
                .is_some_and(Value::is_object)
                .then_some(true)
        }),
        bool_field(fields.field("is_reasoning")),
        bool_field(fields.pointer("capabilities", &["reasoning"])),
        bool_field(fields.pointer("capabilities", &["supports", "reasoning"])),
        bool_field(features.pointer("/reasoning/enable")),
        bool_field(fields.pointer("reasoning_effort_config", &["support_thinking"])),
    ])
    .or_else(|| {
        if !thinking.is_empty()
            || bool_field(fields.field("onlyReasoning")) == Some(true)
            || thinking_config
                .and_then(|value| value.get("enabled"))
                .is_some_and(Value::is_object)
        {
            Some(true)
        } else {
            None
        }
    })
    .or_else(|| {
        // A plain family (known text-only, non-thinking) is a fact about the
        // model, so it outranks a provider-wide default.
        if is_plain {
            Some(false)
        } else {
            defaults.reasoning
        }
    });

    // ---- image input ----
    // A known text-only family outranks a provider's blanket flag: WorkBuddy
    // and Qoder both mark every hosted model as image-capable, including GLM,
    // DeepSeek-Pro and Hunyuan text models.
    let vision = if is_known_text_only(fields.id) {
        Some(false)
    } else if bool_field(fields.field("disabledMultimodal")) == Some(true) {
        Some(false)
    } else {
        first_bool(&[
            bool_field(fields.field("supports_vision")),
            bool_field(fields.field("supports_images")),
            bool_field(fields.field("supportsImages")),
            bool_field(fields.field("is_vl")),
            bool_field(fields.field("multimodal")),
            bool_field(fields.field("vision")),
            bool_field(fields.pointer("capabilities", &["vision"])),
            bool_field(fields.pointer("capabilities", &["supports", "vision"])),
            bool_field(features.pointer("/multimodal/enable")),
        ])
        .or_else(|| {
            fields
                .field("input_modalities")
                .or_else(|| fields.pointer("modalities", &["input"]))
                .or_else(|| fields.pointer("architecture", &["input_modalities"]))
                .and_then(Value::as_array)
                .map(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")))
        })
        .or_else(|| {
            // A family the provider knows to be text-only is a fact, not a gap,
            // so it outranks the provider-wide default.
            if id_matches(fields.id, defaults.plain_families) {
                Some(false)
            } else if id_matches(fields.id, defaults.vision_families) {
                Some(true)
            } else {
                defaults.vision
            }
        })
    };

    // ---- tool calls ----
    let tool_calls = first_bool(&[
        bool_field(fields.field("supports_tool_calls")),
        bool_field(fields.field("supportsToolCall")),
        bool_field(fields.pointer("capabilities", &["tool_calls"])),
        bool_field(fields.pointer("capabilities", &["supports", "tool_calls"])),
        bool_field(fields.pointer("abilities", &["functionCall"])),
    ])
    .or(defaults.tool_calls);
    // Qoder's catalog omits the flag on models it does call tools with, and the
    // gateway's own upstream is a coding agent either way; publish a definite
    // answer rather than leaving the key null on some providers only.
    let tool_calls = tool_calls.or(Some(true));

    // ---- limits ----
    let (context, output) = limits(&fields);

    // ---- thinking toggle ----
    let can_disable = first_bool(&[
        bool_field(fields.field("reasoning_can_disable")),
        bool_field(fields.pointer("reasoning", &["canDisableThinking"])),
        bool_field(fields.pointer("thinking", &["can_disable"])),
    ])
    .or_else(|| {
        thinking_config
            .and_then(Value::as_object)
            .map(|config| config.contains_key("disabled"))
    })
    .or_else(|| {
        let only = bool_field(fields.field("onlyReasoning"));
        match (only, defaults.can_disable) {
            (Some(true), _) => Some(false),
            (Some(false), _) => Some(true),
            (None, true) => Some(true),
            (None, false) => None,
        }
    })
    .or(if thinking.can_disable {
        Some(true)
    } else {
        None
    });
    thinking.can_disable = can_disable.unwrap_or(false) && reasoning == Some(true);

    // ---- audio / video ----
    let declared_inputs = fields
        .field("input_modalities")
        .or_else(|| fields.pointer("modalities", &["input"]))
        .or_else(|| fields.pointer("architecture", &["input_modalities"]))
        .and_then(Value::as_array);
    let audio_input = declared_inputs
        .map(|inputs| inputs.iter().any(|input| input.as_str() == Some("audio")))
        .unwrap_or(false);
    let video_input = declared_inputs
        .map(|inputs| inputs.iter().any(|input| input.as_str() == Some("video")))
        .unwrap_or(false);
    let output_modalities = fields
        .field("output_modalities")
        .or_else(|| fields.pointer("modalities", &["output"]))
        .or_else(|| fields.pointer("architecture", &["output_modalities"]))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![json!("text")]);
    let audio_output = output_modalities
        .iter()
        .any(|item| item.as_str() == Some("audio"));

    // ---- assemble ----
    let mut capabilities = model
        .capabilities
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    capabilities.insert("reasoning".into(), json!(reasoning));
    capabilities.insert("vision".into(), json!(vision));
    capabilities.insert("tool_calls".into(), json!(tool_calls));
    capabilities.insert("reasoning_toggle".into(), json!(thinking.can_disable));
    capabilities.insert("audio_input".into(), json!(audio_input));
    capabilities.insert("video_input".into(), json!(video_input));
    capabilities.insert("audio_output".into(), json!(audio_output));
    model.capabilities = Some(Value::Object(capabilities));

    // Published on every entry: with vision unknown, text is the only input the
    // gateway can promise, so the list says exactly that instead of vanishing.
    let inputs = {
        let mut inputs = vec![json!("text")];
        if vision == Some(true) {
            inputs.push(json!("image"));
        }
        if audio_input {
            inputs.push(json!("audio"));
        }
        if video_input {
            inputs.push(json!("video"));
        }
        inputs
    };

    let extra = &mut model.extra;
    extra.insert("supports_reasoning".into(), json!(reasoning));
    extra.insert("supports_vision".into(), json!(vision));
    extra.insert("supports_images".into(), json!(vision));
    extra.insert("supports_tool_calls".into(), json!(tool_calls));
    // Aliases different clients read for the same two facts.
    if let Some(vision) = vision {
        extra.insert("vision".into(), json!(vision));
        extra.insert("multimodal".into(), json!(vision));
        let abilities = {
            let mut abilities = extra
                .get("abilities")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            abilities.insert("vision".into(), json!(vision));
            Value::Object(abilities)
        };
        extra.insert("abilities".into(), abilities);
    }
    if let Some(tools) = tool_calls {
        if let Some(abilities) = extra.get_mut("abilities").and_then(Value::as_object_mut) {
            abilities.insert("functionCall".into(), json!(tools));
            abilities.insert("function_call".into(), json!(tools));
        }
    }
    extra.insert("reasoning_efforts".into(), json!(thinking.tiers));
    extra.insert("supported_reasoning_efforts".into(), json!(thinking.tiers));
    extra.insert("supported_efforts".into(), json!(thinking.tiers));
    extra.insert("effort".into(), json!(thinking.tiers));
    extra.insert(
        "reasoning_default_effort".into(),
        json!(thinking.default_effort),
    );
    // Always published, like the capability flags: an inconsistent shape (present
    // on one provider's models, absent on another's) forces every client to
    // special-case it.
    extra.insert("reasoning_can_disable".into(), json!(thinking.can_disable));
    extra.insert(
        "thinking".into(),
        if reasoning == Some(true) {
            thinking.block()
        } else {
            json!({"supported": false, "can_disable": false,
                   "default_effort": "", "efforts": [], "levels": []})
        },
    );
    extra.insert("input_modalities".into(), json!(inputs));
    extra.insert("output_modalities".into(), json!(output_modalities));
    extra.insert(
        "modalities".into(),
        json!({"input": inputs, "output": output_modalities}),
    );
    let modality = format!(
        "{}->{}",
        inputs
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("+"),
        output_modalities
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("+")
    );
    extra.insert(
        "architecture".into(),
        json!({"input_modalities": inputs, "output_modalities": output_modalities,
               "modality": modality}),
    );
    if let Some(context) = context {
        extra.insert("context_length".into(), json!(context));
        extra.insert("max_input_tokens".into(), json!(context));
    }
    if let Some(output) = output {
        extra.insert("max_output_tokens".into(), json!(output));
        extra.insert("max_completion_tokens".into(), json!(output));
    }
    model
}

#[cfg(test)]
mod tests {
    use super::{with_agent_metadata, with_provider_metadata, ProviderDefaults};
    use serde_json::{json, Value};

    fn normalize(raw: Value) -> Value {
        serde_json::to_value(with_agent_metadata(serde_json::from_value(raw).unwrap())).unwrap()
    }

    fn normalize_for(raw: Value, provider: &str) -> Value {
        serde_json::to_value(with_provider_metadata(
            serde_json::from_value(raw).unwrap(),
            ProviderDefaults::for_provider(provider),
        ))
        .unwrap()
    }

    #[test]
    fn exposes_trae_reasoning_levels_and_image_input_without_losing_raw_config() {
        let model = normalize(json!({"id":"seed", "multimodal":true,
            "reasoning_effort_config":{"support_thinking":true,"options":["light","high"],"default_level":"high"},
            "features":"{\"reasoning\":{\"enable\":true}}"}));
        assert_eq!(model["capabilities"]["reasoning"], json!(true));
        assert_eq!(model["capabilities"]["vision"], json!(true));
        // Trae's `light` is the gateway's `low`: one ladder for every provider.
        assert_eq!(model["reasoning_efforts"], json!(["low", "high"]));
        assert_eq!(model["reasoning_default_effort"], "high");
        assert_eq!(model["input_modalities"], json!(["text", "image"]));
        assert!(model["reasoning_effort_config"].is_object());
    }

    #[test]
    fn exposes_qoder_efforts_and_respects_disabled_image_input() {
        let model = normalize(json!({"id":"Qwen", "is_vl":true,"disabledMultimodal":true,
            "thinking_config":{"disabled":{},"enabled":{"efforts":{"xhigh":{},"low":{},"medium":{"is_default":true}}}}}));
        assert_eq!(model["supports_reasoning"], true);
        assert_eq!(model["supports_vision"], false);
        assert_eq!(
            model["supported_reasoning_efforts"],
            json!(["low", "medium", "xhigh"])
        );
        assert_eq!(model["reasoning_default_effort"], "medium");
        assert_eq!(model["input_modalities"], json!(["text"]));
        // `thinking_config.disabled` means thinking can be turned off.
        assert_eq!(model["reasoning_can_disable"], true);
        assert_eq!(model["thinking"]["can_disable"], true);
        assert_eq!(
            model["thinking"]["efforts"],
            json!([
                {"effort": "low", "label": "low", "is_default": false},
                {"effort": "medium", "label": "medium", "is_default": true},
                {"effort": "xhigh", "label": "xhigh", "is_default": false}
            ])
        );
        assert_eq!(
            model["thinking"]["levels"],
            json!(["none", "low", "medium", "xhigh"])
        );
    }

    #[test]
    fn preserves_explicit_false_and_does_not_invent_unknown_capabilities() {
        let model = normalize(json!({"id":"unknown"}));
        // Unknown stays unknown: no tier is invented and no limit is made up.
        assert!(model["supports_reasoning"].is_null());
        assert!(model["supports_vision"].is_null());
        assert_eq!(model["reasoning_efforts"], json!([]));
        assert_eq!(model["thinking"]["supported"], false);
        assert!(model["max_output_tokens"].is_null());
        // The fields a client switches on are always present and consistent.
        assert_eq!(model["input_modalities"], json!(["text"]));
        assert_eq!(model["output_modalities"], json!(["text"]));
        assert_eq!(model["supports_tool_calls"], true);
        let model = normalize(
            json!({"id":"plain","capabilities":{"type":"chat","supports":{"reasoning":false,"vision":false}}}),
        );
        assert_eq!(model["supports_reasoning"], false);
        assert_eq!(model["supports_vision"], false);
        assert_eq!(model["capabilities"]["type"], "chat");
    }

    #[test]
    fn structured_thinking_config_wins_over_a_contradicting_flat_flag() {
        // Qoder ships both: `is_reasoning: false` next to a full thinking toggle
        // with a tier ladder. The richer declaration is the true one.
        let model = normalize(json!({"id":"DeepSeek-Flash","is_reasoning":false,
            "thinking_config":{"disabled":{"description":"Disable thinking"},
                "enabled":{"is_default":true,
                           "efforts":{"high":{},"low":{},"max":{"is_default":true}}}}}));
        assert_eq!(model["supports_reasoning"], true);
        assert_eq!(model["reasoning_efforts"], json!(["low", "high", "max"]));
        assert_eq!(model["reasoning_default_effort"], "max");
        assert_eq!(
            model["supports_reasoning"],
            model["capabilities"]["reasoning"]
        );
        assert_eq!(model["thinking"]["supported"], true);
        assert_eq!(model["reasoning_can_disable"], true);
    }

    #[test]
    fn a_build_variant_of_a_model_keeps_its_family_capabilities() {
        // The coding build is a variant of grok-4.7, not the plain grok-build line.
        let variant = normalize_for(json!({"id": "grok-4.7-build-fast"}), "grok");
        assert_eq!(variant["supports_reasoning"], true);
        assert_eq!(
            variant["reasoning_efforts"],
            json!(["low", "medium", "high", "xhigh"])
        );
        // The plain family id still has its own, narrower answer.
        let family = normalize_for(json!({"id": "grok-build-0.1"}), "grok");
        assert_eq!(family["supports_reasoning"], false);
    }

    #[test]
    fn publishes_output_limits_and_tool_calls_from_every_known_spelling() {
        // WorkBuddy names them maxInputTokens/maxOutputTokens; Copilot nests
        // them under capabilities.limits; both must land on the same keys.
        let workbuddy = normalize(
            json!({"id":"wb","supportsToolCall":true,"maxInputTokens":272000,"maxOutputTokens":72000}),
        );
        assert_eq!(workbuddy["max_output_tokens"], 72000);
        assert_eq!(workbuddy["max_completion_tokens"], 72000);
        assert_eq!(workbuddy["max_input_tokens"], 272000);
        assert_eq!(workbuddy["supports_tool_calls"], true);

        let copilot = normalize(json!({"id":"gpt-x",
            "capabilities":{"type":"chat","supports":{"vision":true,"tool_calls":true},
                            "limits":{"max_context_window_tokens":128000,"max_output_tokens":16384}}}));
        assert_eq!(copilot["context_length"], 128000);
        assert_eq!(copilot["max_output_tokens"], 16384);
        assert_eq!(copilot["capabilities"]["vision"], true);
        assert_eq!(copilot["capabilities"]["tool_calls"], true);
        assert_eq!(copilot["output_modalities"], json!(["text"]));
    }

    #[test]
    fn provider_defaults_fill_gaps_but_never_override_the_upstream() {
        // Antigravity ids carry no capability fields at all: the provider's
        // known facts describe them instead of leaving the client blind.
        let antigravity = normalize_for(json!({"id": "gemini-3-flash"}), "antigravity");
        assert_eq!(antigravity["supports_vision"], true);
        assert_eq!(antigravity["supports_tool_calls"], true);
        assert_eq!(
            antigravity["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(antigravity["reasoning_default_effort"], "high");

        // An explicit upstream answer wins over the provider default.
        let explicit = normalize_for(
            json!({"id": "gemini-3-flash", "supportsImages": false}),
            "antigravity",
        );
        assert_eq!(explicit["supports_vision"], false);
        assert_eq!(explicit["input_modalities"], json!(["text"]));

        // A plain (non-reasoning) family stays non-reasoning even with a ladder.
        let plain = normalize_for(json!({"id": "grok-build-0.1"}), "grok");
        assert_eq!(plain["supports_reasoning"], false);
        assert_eq!(plain["thinking"]["supported"], false);
        assert_eq!(plain["reasoning_efforts"], json!([]));
    }

    #[test]
    fn reads_copilot_reasoning_ladder_and_keeps_gpt_41_multimodal() {
        let claude = normalize_for(
            json!({"id": "claude-sonnet-x", "capabilities": {
                "type": "chat",
                "supports": {
                    "adaptive_thinking": true,
                    "reasoning_effort": ["low", "medium", "high"],
                    "vision": true,
                    "tool_calls": true
                },
                "limits": {"max_context_window_tokens": 264000, "max_output_tokens": 64000}
            }}),
            "copilot",
        );
        assert_eq!(claude["supports_reasoning"], true);
        assert_eq!(
            claude["reasoning_efforts"],
            json!(["low", "medium", "high"])
        );
        assert_eq!(claude["reasoning_default_effort"], "medium");
        assert_eq!(claude["max_output_tokens"], 64000);
        assert_eq!(claude["context_length"], 264000);
        assert_eq!(claude["supports_vision"], true);

        // `gpt-4` used to be matched as a plain prefix family, which also
        // caught gpt-4.1/gpt-4o and erased their vision capability and the
        // provider's reasoning ladder.
        let gpt = normalize_for(
            json!({"id": "gpt-4.1", "capabilities": {
                "type": "chat",
                "supports": {"vision": true, "tool_calls": true},
                "limits": {"max_context_window_tokens": 128000, "max_output_tokens": 16384}
            }}),
            "copilot",
        );
        assert_eq!(gpt["supports_vision"], true);
        assert_eq!(gpt["max_output_tokens"], 16384);
        // No reasoning declaration means no invented tier ladder.
        assert_eq!(gpt["reasoning_efforts"], json!([]));
        assert!(gpt["supports_reasoning"].is_null());
    }

    #[test]
    fn known_text_only_families_override_blanket_vision_flags() {
        for id in [
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "deepseek-v4-pro",
            "deepseek-v4-pro-official",
            "hy3",
            "hy3-x",
            "hy4-preview",
            "gpt-oss-120b-medium",
        ] {
            let model = normalize_for(json!({"id": id, "supportsImages": true}), "workbuddy");
            assert_eq!(model["supports_vision"], false, "{id}");
            assert_eq!(model["input_modalities"], json!(["text"]), "{id}");
        }
        // The Flash line really is multimodal; do not let the GLM-5.3
        // text-only rule swallow it.
        let flash = normalize_for(
            json!({"id": "glm-5.3-flash", "supportsImages": true}),
            "workbuddy",
        );
        assert_eq!(flash["supports_vision"], true);
    }

    #[test]
    fn tier_encoded_in_the_id_sets_the_default_when_none_is_published() {
        // Antigravity ships one id per strength; `-low` must not claim `high`.
        let low = normalize_for(json!({"id": "gemini-3.8-flash-low"}), "antigravity");
        assert_eq!(low["reasoning_default_effort"], "low");
        let medium = normalize_for(json!({"id": "gemini-3.8-flash-medium"}), "antigravity");
        assert_eq!(medium["reasoning_default_effort"], "medium");
        // An id without a tier keeps the provider's documented default.
        let plain = normalize_for(json!({"id": "gemini-3-flash"}), "antigravity");
        assert_eq!(plain["reasoning_default_effort"], "high");
        // Every entry publishes the toggle key, whether or not it can disable.
        assert_eq!(low["reasoning_can_disable"], false);
        assert_eq!(low["thinking"]["can_disable"], false);
    }

    #[test]
    fn thinking_toggle_follows_only_reasoning_models_and_reports_friendly_levels() {
        let toggle = normalize_for(
            json!({"id":"Gpt","supportsReasoning":true,"onlyReasoning":false,
                   "reasoning":{"supportedEfforts":["low","high","max"],"defaultEffort":"high",
                                "canDisableThinking":true}}),
            "",
        );
        assert_eq!(toggle["thinking"]["supported"], true);
        assert_eq!(toggle["thinking"]["can_disable"], true);
        assert_eq!(toggle["reasoning_can_disable"], true);
        assert_eq!(
            toggle["thinking"]["levels"],
            json!(["none", "low", "high", "max"])
        );

        // onlyReasoning=true means thinking cannot be switched off at all.
        let forced = normalize(
            json!({"id":"Hy","supportsReasoning":true,"onlyReasoning":true,
            "reasoning":{"supportedEfforts":["high"]}}),
        );
        assert_eq!(forced["reasoning_can_disable"], false);
        assert_eq!(forced["thinking"]["levels"], json!(["high"]));

        // A fixed single-tier model still reports that tier as the default.
        let fixed = normalize(json!({"id":"Kimi","supportsReasoning":true,
            "reasoning":{"effort":"medium"}}));
        assert_eq!(fixed["reasoning_efforts"], json!(["medium"]));
        assert_eq!(fixed["reasoning_default_effort"], "medium");
    }
}
