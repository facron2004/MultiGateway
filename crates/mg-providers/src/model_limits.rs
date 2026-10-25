//! Calibration for context/output limits the upstream catalogs omit or cap.
//!
//! Provider catalogs are authoritative for context, but their `maxOutputTokens`
//! is often the product's default cap rather than the model's published
//! ceiling (WorkBuddy publishes 64K for GLM-5.3, whose spec is 128K). The known
//! table — measured values plus vendor/models.dev data — therefore wins for
//! output, while the original upstream value is retained as `output_clamp` so
//! clients can still see a lower route-specific cap. Unknown models are never
//! guessed: the output field stays absent when neither source has a value.

use std::collections::HashMap;
use std::path::Path;

use mg_protocols::openai::ModelEntry;

/// Measured / vendor-defined pairs `(context, output)`.
const KNOWN_LIMITS: &[(&str, u64, u64)] = &[
    ("glm-5.1", 200_000, 131_072),
    ("glm-5.2", 1_000_000, 131_072),
    ("glm-5.3", 1_000_000, 131_072),
    ("glm-5.3-flash", 1_000_000, 131_072),
    ("glm-5.3-flashx", 1_000_000, 131_072),
    ("glm-5v-turbo", 200_000, 131_072),
    ("kimi-k2.5", 164_000, 262_144),
    ("kimi-k2.6", 256_000, 262_144),
    ("kimi-k2.7", 256_000, 65_536),
    ("kimi-k2.7-code", 262_144, 262_144),
    ("kimi-k3", 1_048_576, 1_048_576),
    ("kimi-k3-1", 1_048_576, 1_048_576),
    ("kimi-k3.1-pro", 1_048_576, 1_048_576),
    ("kimi-k2.8-preview", 1_048_576, 1_048_576),
    ("minimax-m3", 1_048_576, 524_288),
    ("minimax-m3.1-flash", 1_000_000, 524_288),
    ("hy3", 192_000, 64_000),
    ("hy4-preview", 1_000_000, 64_000),
    ("space-bunny", 1_048_576, 524_288),
    ("step-5-preview", 1_000_000, 65_536),
    ("deepseek-v4-flash", 1_000_000, 384_000),
    ("deepseek-v4-pro", 1_048_576, 393_216),
    ("deepseek-v4.1-flash", 1_000_000, 384_000),
    ("mimo-v2.6-pro", 1_048_576, 131_072),
    ("mimo-v2.6-flash", 1_048_576, 131_072),
    ("qwen3.8-max", 1_000_000, 131_072),
    ("qwen3.8-flash", 1_000_000, 131_072),
    ("qwen-3.7-plus", 1_000_000, 131_072),
    ("deepseek-flash", 1_048_576, 393_216),
    ("doubao-seed-2.1-pro", 256_000, 256_000),
    ("doubao-seed-2.1-turbo", 256_000, 256_000),
    ("doubao-seed-code", 256_000, 4_096),
    ("doubao-seed-evolving", 256_000, 256_000),
    ("claude-haiku-4.5", 200_000, 64_000),
    ("claude-opus-5-5", 1_000_000, 128_000),
    ("claude-sonnet-5-5", 1_000_000, 128_000),
    ("gemini-3.8-flash", 1_048_576, 65_536),
    ("gemini-3.5-flash-lite", 1_048_576, 65_536),
    ("gemini-3.1-flash-lite", 1_048_576, 65_536),
    ("gemini-2.5-pro", 1_048_576, 65_536),
    ("gemini-3.1-flash-image", 131_072, 32_768),
    ("gpt-5.3-codex", 400_000, 128_000),
    ("gpt-5.4", 1_050_000, 128_000),
    ("gpt-5.5", 1_050_000, 128_000),
    ("gpt-5.6-luna", 1_050_000, 128_000),
    ("gpt-5.6-sol", 1_050_000, 128_000),
    ("gpt-5.6-terra", 1_050_000, 128_000),
    ("gpt-6-astra", 1_050_000, 128_000),
    ("gemini-3.5-flash", 1_048_576, 65_536),
];

fn bare_id(id: &str) -> String {
    id.trim()
        .rsplit('/')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
}

/// `deepseek-v4-pro-official` is the same model as `deepseek-v4-pro`; the
/// suffix only marks which upstream channel serves it.
fn lookup_key(id: &str) -> String {
    let bare = bare_id(id);
    match bare.strip_suffix("-official") {
        Some(alias) => alias.to_string(),
        None => bare,
    }
}

fn positive(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|number| number as u64))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<f64>().ok())
                .map(|number| number as u64)
        })
        .filter(|number| *number > 0)
}

fn remote_limits(model: &ModelEntry) -> (Option<u64>, Option<u64>) {
    let extra = &model.extra;
    let capabilities = model.capabilities.as_ref();
    let context = positive(extra.get("context_length"))
        .or_else(|| positive(extra.get("max_input_tokens")))
        .or_else(|| positive(extra.get("maxInputTokens")))
        .or_else(|| positive(extra.get("context_window")))
        .or_else(|| positive(extra.get("max_context")))
        .or_else(|| {
            capabilities.and_then(|value| {
                positive(
                    value
                        .pointer("/limits/max_context_window_tokens")
                        .or_else(|| value.pointer("/limits/max_input_tokens")),
                )
            })
        });
    let output = positive(extra.get("max_output_tokens"))
        .or_else(|| positive(extra.get("max_completion_tokens")))
        .or_else(|| positive(extra.get("maxOutputTokens")))
        .or_else(|| {
            capabilities.and_then(|value| {
                positive(
                    value
                        .pointer("/limits/max_output_tokens")
                        .or_else(|| value.pointer("/limits/maxOutputTokens")),
                )
            })
        });
    (context, output)
}

/// Write the limit under every spelling clients read, including a nested
/// `capabilities.limits` block when the upstream catalog used that shape.
fn set_output_limit(model: &mut ModelEntry, output: u64) {
    for key in ["max_output_tokens", "max_completion_tokens", "maxOutputTokens"] {
        model
            .extra
            .insert(key.to_string(), serde_json::json!(output));
    }
    let Some(capabilities) = model
        .capabilities
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let Some(limits) = capabilities
        .get_mut("limits")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for key in ["max_output_tokens", "maxOutputTokens"] {
        if limits.contains_key(key) {
            limits.insert(key.to_string(), serde_json::json!(output));
        }
    }
}

fn mark_output_clamp(model: &mut ModelEntry, clamp: u64) {
    for key in ["output_clamp", "max_output_tokens_clamped"] {
        model.extra.insert(key.to_string(), serde_json::json!(clamp));
    }
}

/// `model.json` maps a bare id to `[context, output]`; tolerate both shapes.
pub fn read_limit_cache(path: &Path) -> HashMap<String, (u64, u64)> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return HashMap::new();
    };
    let Some(entries) = value.as_object() else {
        return HashMap::new();
    };
    let mut limits = HashMap::new();
    for (id, entry) in entries {
        let pair = if let Some(items) = entry.as_array() {
            let context = items
                .first()
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let output = items
                .get(1)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            (context, output)
        } else {
            let context = positive(entry.get("context")).unwrap_or(0);
            let output = positive(entry.get("output")).unwrap_or(0);
            (context, output)
        };
        if pair.0 > 0 || pair.1 > 0 {
            limits.insert(bare_id(id), pair);
        }
    }
    limits
}

fn known_limits(id: &str) -> Option<(u64, u64)> {
    let id = lookup_key(id);
    KNOWN_LIMITS
        .iter()
        .find(|(known, _, _)| *known == id)
        .map(|(_, context, output)| (*context, *output))
}

/// Resolve context from the catalog (or fill its gap) and replace the output
/// ceiling with the model's known spec. The upstream output is kept as a clamp
/// annotation when it is lower than the spec.
pub fn apply_known_limits(models: &mut [ModelEntry], cache: &HashMap<String, (u64, u64)>) {
    for model in models {
        let (context, output) = remote_limits(model);
        let key = lookup_key(&model.id);
        let known = known_limits(&key);
        let cached = cache.get(&key).copied();
        let resolved_context = context
            .or_else(|| known.map(|pair| pair.0).filter(|value| *value > 0))
            .or_else(|| cached.map(|pair| pair.0).filter(|value| *value > 0));
        let spec_output = known
            .map(|pair| pair.1)
            .filter(|value| *value > 0)
            .or_else(|| cached.map(|pair| pair.1).filter(|value| *value > 0));
        if let Some(context) = resolved_context {
            model
                .extra
                .insert("context_length".to_string(), serde_json::json!(context));
            model
                .extra
                .insert("max_input_tokens".to_string(), serde_json::json!(context));
        }
        if let Some(output) = spec_output.or(output) {
            set_output_limit(model, output);
        }
        if let (Some(spec), Some(remote)) = (spec_output, output) {
            if remote < spec {
                mark_output_clamp(model, remote);
            }
        }
    }
}

/// Publish the model's known output ceiling. Catalogs that omitted the field
/// (Trae and Qoder do) get the spec added; catalogs that published a lower
/// product cap keep that cap as an [`output_clamp`] annotation.
pub fn calibrate_model_limits(model: &mut ModelEntry) {
    let (_, remote_output) = remote_limits(model);
    let Some(spec) = known_limits(&model.id)
        .map(|pair| pair.1)
        .filter(|value| *value > 0)
    else {
        return;
    };
    if Some(spec) != remote_output {
        set_output_limit(model, spec);
        if let Some(remote_output) = remote_output {
            if remote_output < spec {
                mark_output_clamp(model, remote_output);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_known_limits, calibrate_model_limits, known_limits, read_limit_cache};
    use mg_protocols::openai::ModelEntry;
    use std::collections::HashMap;

    fn model(id: &str, context: Option<u64>, output: Option<u64>) -> ModelEntry {
        let mut extra = serde_json::Map::new();
        if let Some(context) = context {
            extra.insert("context_length".into(), serde_json::json!(context));
        }
        if let Some(output) = output {
            extra.insert("max_output_tokens".into(), serde_json::json!(output));
        }
        ModelEntry {
            id: id.to_string(),
            object: "model".to_string(),
            owned_by: "workbuddy".to_string(),
            created: None,
            permission: None,
            capabilities: None,
            credits: None,
            extra,
        }
    }

    #[test]
    fn known_spec_overrides_the_products_output_cap() {
        let mut models = vec![
            model("glm-5.3", None, None),
            model("deepseek-v4.1-flash", Some(1_000_000), Some(128_000)),
        ];
        apply_known_limits(&mut models, &HashMap::new());
        assert_eq!(models[0].extra["context_length"], 1_000_000);
        assert_eq!(models[0].extra["max_output_tokens"], 131_072);
        // WorkBuddy caps this at 128K; the model's published ceiling is 384K.
        assert_eq!(models[1].extra["max_output_tokens"], 384_000);
        assert_eq!(models[1].extra["output_clamp"], 128_000);
    }

    #[test]
    fn calibrate_corrects_and_fills_known_output_ceilings() {
        let mut published = model("GLM-5.3", Some(1_000_000), Some(64_000));
        calibrate_model_limits(&mut published);
        assert_eq!(published.extra["max_output_tokens"], 131_072);
        assert_eq!(published.extra["maxOutputTokens"], 131_072);
        assert_eq!(published.extra["output_clamp"], 64_000);

        // Trae/Qoder leave the field out; the vendor spec still fills it.
        let mut silent = model("kimi-k3", Some(1_048_576), None);
        calibrate_model_limits(&mut silent);
        assert_eq!(silent.extra["max_output_tokens"], 1_048_576);

        // A model with no evidence stays unknown instead of guessing.
        let mut unknown = model("not-a-real-model", Some(128_000), None);
        calibrate_model_limits(&mut unknown);
        assert!(unknown.extra.get("max_output_tokens").is_none());
    }

    #[test]
    fn official_channel_suffix_resolves_to_the_same_spec() {
        assert_eq!(
            known_limits("trae/deepseek-v4-pro-official").map(|pair| pair.1),
            Some(393_216)
        );
    }

    #[test]
    fn falls_back_to_the_persisted_model_json_index() {
        let dir = std::env::temp_dir().join(format!("mg-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.json");
        std::fs::write(&path, br#"{"space-bunny":[1048576,524288]}"#).unwrap();
        let cache = read_limit_cache(&path);
        assert_eq!(cache.get("space-bunny"), Some(&(1_048_576, 524_288)));
        let mut models = vec![model("space-bunny", None, None)];
        apply_known_limits(&mut models, &cache);
        assert_eq!(models[0].extra["max_output_tokens"], 524_288);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn table_is_keyed_by_bare_lowercase_id() {
        assert_eq!(
            known_limits("workbuddy/GLM-5.3").map(|pair| pair.1),
            Some(131_072)
        );
    }
}
