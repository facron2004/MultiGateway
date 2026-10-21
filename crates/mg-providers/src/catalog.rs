use std::collections::HashSet;

use mg_core::{Account, AccountPool};
use mg_protocols::openai::ModelEntry;
use serde_json::Value;

pub fn fallback_models(ids: &[&str], owner: &str) -> Vec<ModelEntry> {
    ids.iter()
        .map(|id| ModelEntry {
            id: (*id).to_string(),
            object: "model".to_string(),
            owned_by: owner.to_string(),
            created: None,
            permission: None,
            capabilities: None,
            credits: None,
            extra: serde_json::Map::new(),
        })
        .collect()
}

pub fn representative_account(pool: &AccountPool, region: Option<&str>) -> Option<Account> {
    let region = region.map(str::trim).filter(|value| !value.is_empty() && *value != "all");
    let mut accounts: Vec<_> = pool
        .list()
        .into_iter()
        .filter(|account| {
            account.is_ready()
                && region.is_none_or(|region| mg_core::account::regions_match(&account.region, region))
        })
        .collect();
    accounts.sort_by(|left, right| left.id.cmp(&right.id));
    accounts.into_iter().next()
}

pub fn parse_model_entries(value: &Value, owner: &str) -> Vec<ModelEntry> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    collect_model_entries(value, owner, &mut seen, &mut rows);
    rows
}

fn collect_model_entries(
    value: &Value,
    owner: &str,
    seen: &mut HashSet<String>,
    rows: &mut Vec<ModelEntry>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(entry) = model_entry(item, None, owner) {
                    if seen.insert(entry.id.clone()) {
                        rows.push(entry);
                    }
                }
            }
        }
        Value::Object(map) => {
            for key in ["data", "response", "result", "catalog", "items"] {
                if let Some(child) = map.get(key) {
                    if child.is_array() || child.is_object() {
                        collect_model_entries(child, owner, seen, rows);
                        if !rows.is_empty() {
                            return;
                        }
                    }
                }
            }

            if let Some(models) = map.get("models") {
                match models {
                    Value::Array(_) => collect_model_entries(models, owner, seen, rows),
                    Value::Object(models) => {
                        for (id, metadata) in models {
                            if let Some(entry) = model_entry(metadata, Some(id), owner) {
                                if seen.insert(entry.id.clone()) {
                                    rows.push(entry);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn model_entry(value: &Value, map_id: Option<&str>, owner: &str) -> Option<ModelEntry> {
    let id = value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
        .or(map_id)?
        .trim();
    if id.is_empty() {
        return None;
    }

    if value.get("model_picker_enabled") == Some(&Value::Bool(false)) {
        return None;
    }
    if value
        .pointer("/capabilities/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "chat")
    {
        return None;
    }

    let mut extra = value.as_object().cloned().unwrap_or_default();
    for key in ["id", "object", "owned_by", "ownedBy", "created", "permission", "capabilities", "credits", "quota"] {
        extra.remove(key);
    }

    Some(ModelEntry {
        id: id.to_string(),
        object: value
            .get("object")
            .and_then(Value::as_str)
            .unwrap_or("model")
            .to_string(),
        owned_by: value
            .get("owned_by")
            .or_else(|| value.get("ownedBy"))
            .and_then(Value::as_str)
            .unwrap_or(owner)
            .to_string(),
        created: value.get("created").and_then(Value::as_u64),
        permission: value
            .get("permission")
            .and_then(|item| serde_json::from_value(item.clone()).ok()),
        capabilities: value.get("capabilities").cloned(),
        credits: value
            .get("credits")
            .or_else(|| value.get("quota"))
            .cloned(),
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_model_entries;
    use serde_json::json;

    #[test]
    fn parses_openai_catalog_and_filters_non_chat_models() {
        let catalog = json!({
            "data": [
                {"id": "gpt-a", "created": 123, "capabilities": {"type": "chat"}},
                {"id": "embed-a", "capabilities": {"type": "embedding"}},
                {"id": "hidden", "model_picker_enabled": false},
                "gpt-b"
            ]
        });

        let rows = parse_model_entries(&catalog, "copilot");
        assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["gpt-a", "gpt-b"]);
        assert_eq!(rows[0].owned_by, "copilot");
        assert_eq!(rows[0].created, Some(123));
    }

    #[test]
    fn parses_wrapped_object_catalog_and_deduplicates_ids() {
        let catalog = json!({
            "response": {
                "models": {
                    "gemini-pro": {"quota": {"remaining": 4}},
                    "gemini-flash": {"capabilities": {"vision": true}}
                }
            }
        });

        let rows = parse_model_entries(&catalog, "google");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.id == "gemini-pro" && row.credits.is_some()));
    }

    #[test]
    fn parses_string_lists_and_ignores_empty_ids() {
        let rows = parse_model_entries(&json!(["m1", "", "m1", null]), "provider");
        assert_eq!(rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["m1"]);
    }
}
