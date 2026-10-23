//! Calibration for context/output limits the upstream catalogs omit.
//!
//! WorkBuddy's live catalog is authoritative when it publishes a limit, but
//! some entries (and every offline/fallback entry) carry no output ceiling.
//! The old WorkBuddy panel filled those gaps from a measured knowledge table,
//! then from the persisted `model.json` index. This module keeps that chain:
//! upstream value > known table > local cache, and never invents a value.

use std::collections::HashMap;
use std::path::Path;

use mg_protocols::openai::ModelEntry;

/// Measured / models.dev consensus pairs `(context, output)`.
const KNOWN_LIMITS: &[(&str, u64, u64)] = &[
    ("glm-5.1", 200_000, 131_072),
    ("glm-5.2", 1_000_000, 131_072),
    ("glm-5.3", 1_000_000, 131_072),
    ("glm-5.3-flash", 1_000_000, 131_072),
    ("glm-5v-turbo", 200_000, 131_072),
    ("kimi-k2.5", 164_000, 262_144),
    ("kimi-k2.6", 256_000, 262_144),
    ("kimi-k2.7", 256_000, 65_536),
    ("kimi-k3", 1_048_576, 131_072),
    ("kimi-k3-1", 1_048_576, 131_072),
    ("minimax-m3", 512_000, 512_000),
    ("hy3", 192_000, 64_000),
    ("hy4-preview", 1_000_000, 64_000),
    ("deepseek-v4-flash", 1_000_000, 384_000),
    ("deepseek-v4-pro", 1_000_000, 384_000),
    ("deepseek-v4.1-flash", 1_000_000, 384_000),
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
    let context = positive(extra.get("context_length"))
        .or_else(|| positive(extra.get("max_input_tokens")))
        .or_else(|| positive(extra.get("maxInputTokens")))
        .or_else(|| positive(extra.get("context_window")))
        .or_else(|| positive(extra.get("max_context")));
    let output = positive(extra.get("max_output_tokens"))
        .or_else(|| positive(extra.get("max_completion_tokens")))
        .or_else(|| positive(extra.get("maxOutputTokens")));
    (context, output)
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
    let id = bare_id(id);
    KNOWN_LIMITS
        .iter()
        .find(|(known, _, _)| *known == id)
        .map(|(_, context, output)| (*context, *output))
}

/// Fill only the limits the catalog left unknown.
pub fn apply_known_limits(models: &mut [ModelEntry], cache: &HashMap<String, (u64, u64)>) {
    for model in models {
        let (context, output) = remote_limits(model);
        let key = bare_id(&model.id);
        let known = known_limits(&key);
        let cached = cache.get(&key).copied();
        let resolved_context = context
            .or_else(|| known.map(|pair| pair.0).filter(|value| *value > 0))
            .or_else(|| cached.map(|pair| pair.0).filter(|value| *value > 0));
        let resolved_output = output
            .or_else(|| known.map(|pair| pair.1).filter(|value| *value > 0))
            .or_else(|| cached.map(|pair| pair.1).filter(|value| *value > 0));
        if let Some(context) = resolved_context {
            model
                .extra
                .insert("context_length".to_string(), serde_json::json!(context));
            model
                .extra
                .insert("max_input_tokens".to_string(), serde_json::json!(context));
        }
        if let Some(output) = resolved_output {
            model
                .extra
                .insert("max_output_tokens".to_string(), serde_json::json!(output));
            model.extra.insert(
                "max_completion_tokens".to_string(),
                serde_json::json!(output),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_known_limits, known_limits, read_limit_cache};
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
    fn fills_only_missing_limits_and_prefers_upstream_values() {
        let mut models = vec![
            model("glm-5.3", None, None),
            model("deepseek-v4.1-flash", Some(1_000_000), Some(128_000)),
        ];
        apply_known_limits(&mut models, &HashMap::new());
        assert_eq!(models[0].extra["context_length"], 1_000_000);
        assert_eq!(models[0].extra["max_output_tokens"], 131_072);
        // Upstream explicitly says 128K; calibration must not overrule it.
        assert_eq!(models[1].extra["max_output_tokens"], 128_000);
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
