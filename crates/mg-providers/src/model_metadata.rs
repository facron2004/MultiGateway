use mg_protocols::openai::ModelEntry;
use serde_json::{json, Value};

fn first_bool(values: &[Option<&Value>]) -> Option<bool> {
    values.iter().flatten().find_map(|value| value.as_bool())
}

fn effort_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str().or_else(|| {
                ["effort", "value", "level"]
                    .iter()
                    .find_map(|key| item.get(*key).and_then(Value::as_str))
            })
        })
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a top-level key exactly like `serde_json::to_value(&model)` would:
/// flattened `extra` entries win, falling back to the structured
/// `capabilities` field (the only structured field the probes below read).
fn entry_field<'a>(model: &'a ModelEntry, key: &str) -> Option<&'a Value> {
    model.extra.get(key).or_else(|| {
        if key == "capabilities" {
            model.capabilities.as_ref()
        } else {
            None
        }
    })
}

/// Equivalent of `raw.pointer("/<first>/<rest...>")` without serializing.
fn entry_pointer<'a>(model: &'a ModelEntry, first: &str, rest: &[&str]) -> Option<&'a Value> {
    let mut current = entry_field(model, first)?;
    for key in rest {
        current = current.get(*key)?;
    }
    Some(current)
}

/// Advertise upstream model metadata in a consistent form for agent clients.
pub fn with_agent_metadata(mut model: ModelEntry) -> ModelEntry {
    let features = match entry_field(&model, "features") {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    };
    let thinking = entry_field(&model, "thinking_config");
    let effort_map = thinking
        .and_then(|value| value.pointer("/enabled/efforts"))
        .and_then(Value::as_object);
    let mut efforts = [
        entry_field(&model, "supported_reasoning_efforts"),
        entry_field(&model, "reasoning_efforts"),
        entry_pointer(&model, "reasoning_effort_config", &["options"]),
        entry_pointer(&model, "reasoning", &["supportedEfforts"]),
        entry_field(&model, "supported_reasoning_levels"),
    ]
    .into_iter()
    .map(effort_names)
    .find(|names| !names.is_empty())
    .unwrap_or_default();
    if efforts.is_empty() {
        if let Some(map) = effort_map {
            efforts.extend(map.keys().cloned());
            let order = [
                "none", "minimal", "light", "low", "medium", "high", "xhigh", "max", "ultra",
            ];
            efforts.sort_by_key(|name| {
                order
                    .iter()
                    .position(|value| *value == name)
                    .unwrap_or(order.len())
            });
        }
    }
    efforts.dedup();
    let reasoning = first_bool(&[
        entry_field(&model, "supports_reasoning"),
        entry_field(&model, "supportsReasoning"),
        entry_field(&model, "is_reasoning"),
        entry_pointer(&model, "capabilities", &["reasoning"]),
        entry_pointer(&model, "capabilities", &["supports", "reasoning"]),
        features.pointer("/reasoning/enable"),
        entry_pointer(&model, "reasoning_effort_config", &["support_thinking"]),
    ])
    .or_else(|| {
        if !efforts.is_empty()
            || entry_field(&model, "onlyReasoning").and_then(Value::as_bool) == Some(true)
            || thinking
                .and_then(|value| value.get("enabled"))
                .is_some_and(Value::is_object)
        {
            Some(true)
        } else {
            None
        }
    });
    let vision = if entry_field(&model, "disabledMultimodal").and_then(Value::as_bool) == Some(true)
    {
        Some(false)
    } else {
        first_bool(&[
            entry_field(&model, "supports_vision"),
            entry_field(&model, "supports_images"),
            entry_field(&model, "supportsImages"),
            entry_field(&model, "is_vl"),
            entry_field(&model, "multimodal"),
            entry_field(&model, "vision"),
            entry_pointer(&model, "capabilities", &["vision"]),
            entry_pointer(&model, "capabilities", &["supports", "vision"]),
            features.pointer("/multimodal/enable"),
        ])
        .or_else(|| {
            entry_field(&model, "input_modalities")
                .or_else(|| entry_pointer(&model, "modalities", &["input"]))
                .or_else(|| entry_pointer(&model, "architecture", &["input_modalities"]))
                .and_then(Value::as_array)
                .map(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")))
        })
    };
    let default_effort = [
        entry_field(&model, "reasoning_default_effort"),
        entry_field(&model, "default_reasoning_effort"),
        entry_pointer(&model, "reasoning_effort_config", &["default_level"]),
        entry_pointer(&model, "reasoning", &["defaultEffort"]),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|value| !value.is_empty())
    .map(str::to_string)
    .or_else(|| {
        effort_map.and_then(|map| {
            map.iter()
                .find(|(_, value)| value.get("is_default").and_then(Value::as_bool) == Some(true))
                .map(|(name, _)| name.clone())
        })
    });
    // Computed from the pre-mutation model, matching the previous snapshot-then-probe order.
    let can_disable = first_bool(&[
        entry_field(&model, "reasoning_can_disable"),
        entry_pointer(&model, "reasoning", &["canDisableThinking"]),
    ])
    .or_else(|| {
        thinking
            .and_then(Value::as_object)
            .map(|config| config.contains_key("disabled"))
    });

    let mut capabilities = model
        .capabilities
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    capabilities.insert("reasoning".into(), json!(reasoning));
    capabilities.insert("vision".into(), json!(vision));
    model.capabilities = Some(Value::Object(capabilities));
    model
        .extra
        .insert("supports_reasoning".into(), json!(reasoning));
    model.extra.insert("supports_vision".into(), json!(vision));
    model
        .extra
        .insert("reasoning_efforts".into(), json!(efforts));
    model
        .extra
        .insert("supported_reasoning_efforts".into(), json!(efforts));
    model
        .extra
        .insert("reasoning_default_effort".into(), json!(default_effort));
    if let Some(can_disable) = can_disable {
        model
            .extra
            .insert("reasoning_can_disable".into(), json!(can_disable));
    }
    if let Some(vision) = vision {
        let inputs = if vision {
            json!(["text", "image"])
        } else {
            json!(["text"])
        };
        model
            .extra
            .insert("input_modalities".into(), inputs.clone());
        model
            .extra
            .insert("output_modalities".into(), json!(["text"]));
        model.extra.insert(
            "modalities".into(),
            json!({"input": inputs, "output": ["text"]}),
        );
    }
    model
}

#[cfg(test)]
mod tests {
    use super::with_agent_metadata;
    use serde_json::{json, Value};

    fn normalize(raw: Value) -> Value {
        serde_json::to_value(with_agent_metadata(serde_json::from_value(raw).unwrap())).unwrap()
    }

    #[test]
    fn exposes_trae_reasoning_levels_and_image_input_without_losing_raw_config() {
        let model = normalize(json!({"id":"seed", "multimodal":true,
            "reasoning_effort_config":{"support_thinking":true,"options":["light","high"],"default_level":"high"},
            "features":"{\"reasoning\":{\"enable\":true}}"}));
        assert_eq!(
            model["capabilities"],
            json!({"reasoning":true,"vision":true})
        );
        assert_eq!(model["reasoning_efforts"], json!(["light", "high"]));
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
    }

    #[test]
    fn preserves_explicit_false_and_does_not_invent_unknown_capabilities() {
        let model = normalize(json!({"id":"unknown"}));
        assert!(model["supports_reasoning"].is_null());
        assert!(model["supports_vision"].is_null());
        assert_eq!(model["reasoning_efforts"], json!([]));
        let model = normalize(
            json!({"id":"plain","capabilities":{"type":"chat","supports":{"reasoning":false,"vision":false}}}),
        );
        assert_eq!(model["supports_reasoning"], false);
        assert_eq!(model["supports_vision"], false);
        assert_eq!(model["capabilities"]["type"], "chat");
    }
}
