use crate::account::restrict_private_file;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{fs, io, io::Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default = "default_name")]
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub masked: String,
    #[serde(default)]
    pub realm: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: String,
}

fn new_id() -> String {
    format!("{:x}", uuid::Uuid::new_v4().as_u128() & 0xFFFFFF)
}
fn default_name() -> String {
    "API Key".to_string()
}

fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 14 {
        format!(
            "{}********{}",
            chars[..6].iter().collect::<String>(),
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else {
        "********".to_string()
    }
}

fn read_json(path: &Path) -> io::Result<Option<serde_json::Value>> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(content.trim_start_matches('\u{feff}'))
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn legacy_keys(data_dir: &Path) -> io::Result<Vec<ApiKeyEntry>> {
    let Some(settings) = read_json(&data_dir.join("workbuddy/accounts/settings.json"))? else {
        return Ok(Vec::new());
    };
    let entries = if let Some(entries) = settings.get("api_keys") {
        entries
            .as_array()
            .ok_or_else(|| io::Error::other("旧版 api_keys 格式无效"))?
            .clone()
    } else if settings
        .get("api_key_set")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        match settings
            .get("api_key")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
        {
            Some(key) => {
                vec![serde_json::json!({"id":"legacy", "name":"默认（跟随面板切换）", "key":key})]
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let mut keys: Vec<ApiKeyEntry> = Vec::new();
    for value in entries {
        if value
            .get("deleted_at")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
        {
            continue;
        }
        let entry: ApiKeyEntry = serde_json::from_value(value).map_err(io::Error::other)?;
        if !entry.key.is_empty() && !keys.iter().any(|key| key.key == entry.key) {
            keys.push(entry);
        }
    }
    Ok(keys)
}

fn default_true() -> bool {
    true
}

pub struct KeyManager {
    path: PathBuf,
    master_key: Option<String>,
    keys: RwLock<Vec<ApiKeyEntry>>,
}

impl KeyManager {
    pub fn new(data_dir: impl AsRef<Path>, master_key: Option<String>) -> io::Result<Self> {
        let path = data_dir.as_ref().join("api_keys.json");
        let master_key = master_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        let (mut keys, imported) = match read_json(&path)? {
            Some(value) => (
                serde_json::from_value::<Vec<ApiKeyEntry>>(value).map_err(io::Error::other)?,
                false,
            ),
            None => (legacy_keys(data_dir.as_ref())?, true),
        };
        for key in &mut keys {
            if key.masked.is_empty() {
                key.masked = mask_key(&key.key);
            }
        }
        let manager = Self {
            path,
            master_key,
            keys: RwLock::new(keys),
        };
        for path in [
            manager.path.clone(),
            manager.path.with_extension("json.bak"),
        ] {
            if path.exists() {
                restrict_private_file(&path);
            }
        }
        if imported && !manager.keys.read().is_empty() {
            manager.persist(&manager.keys.read())?;
        }
        Ok(manager)
    }

    fn persist(&self, keys: &[ApiKeyEntry]) -> io::Result<()> {
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
        let mut file = fs::File::create(&temporary)?;
        restrict_private_file(&temporary);
        file.write_all(&serde_json::to_vec_pretty(keys)?)?;
        file.sync_all()?;
        drop(file);
        if self.path.exists() {
            let backup = self.path.with_extension("json.bak");
            fs::copy(&self.path, &backup)?;
            restrict_private_file(&backup);
        }
        fs::rename(temporary, &self.path)
    }

    pub fn generate(&self, name: &str) -> io::Result<ApiKeyEntry> {
        let raw_key = format!("sk-mg-{}", uuid::Uuid::new_v4().simple());
        let id = new_id();
        let now_str = chrono::Local::now().format("%Y/%m/%d %H:%M").to_string();
        let masked = mask_key(&raw_key);

        let entry = ApiKeyEntry {
            id,
            name: if name.is_empty() {
                "API Key".to_string()
            } else {
                name.to_string()
            },
            key: raw_key,
            masked,
            realm: String::new(),
            models: Vec::new(),
            enabled: true,
            created_at: now_str,
        };

        let mut keys = self.keys.write();
        let mut updated = keys.clone();
        updated.push(entry.clone());
        self.persist(&updated)?;
        *keys = updated;
        Ok(entry)
    }

    pub fn delete(&self, id: &str) -> io::Result<bool> {
        let mut keys = self.keys.write();
        let updated: Vec<_> = keys.iter().filter(|key| key.id != id).cloned().collect();
        let removed = updated.len() < keys.len();
        if removed {
            self.persist(&updated)?;
            *keys = updated;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Vec<ApiKeyEntry> {
        self.keys.read().clone()
    }

    pub fn reveal(&self, id: &str) -> Option<String> {
        self.keys
            .read()
            .iter()
            .find(|k| k.id == id)
            .map(|k| k.key.clone())
    }

    pub fn auth_required(&self) -> bool {
        self.master_key.is_some() || !self.keys.read().is_empty()
    }

    pub fn is_authorized(&self, token: &str) -> bool {
        // Keyless desktop mode intentionally accepts anonymous requests.
        if !self.auth_required() {
            return true;
        }
        // An empty configured key must never turn an unauthenticated request
        // into an authorized one.
        if token.is_empty() {
            return false;
        }
        if let Some(master) = &self.master_key {
            if master == token {
                return true;
            }
        }

        let keys = self.keys.read();
        keys.iter()
            .any(|k| k.enabled && !k.key.is_empty() && k.key == token)
    }

    pub fn check_model_allowed(&self, token: &str, model: &str) -> bool {
        if !self.auth_required() {
            return true;
        }
        if token.is_empty() {
            return false;
        }
        if let Some(master) = &self.master_key {
            if master == token {
                return true;
            }
        }

        let keys = self.keys.read();
        if let Some(entry) = keys.iter().find(|k| k.enabled && k.key == token) {
            if entry.models.is_empty() {
                return true; // No restrictions
            }
            return entry.models.iter().any(|m| m == model);
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::KeyManager;
    use serde_json::json;
    use std::{fs, path::PathBuf};

    fn directory() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-key-persistence-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn imports_original_keys_and_does_not_resurrect_deleted_keys_after_restart() {
        let dir = directory();
        fs::create_dir_all(dir.join("workbuddy/accounts")).unwrap();
        let legacy = json!({"api_keys":[
            {"id":"old", "name":"Existing", "key":"existing-key", "realm":"cn", "models":["trae/model"], "created_at":"2026/10/01 22:25"},
            {"id":"retired", "key":"revoked-key", "enabled":true,"deleted_at":"2026/10/16 14:56"}
        ], "accounts_collapsed":true});
        let path = dir.join("workbuddy/accounts/settings.json");
        let bytes = serde_json::to_vec(&legacy).unwrap();
        fs::write(&path, &bytes).unwrap();
        let keys = KeyManager::new(&dir, None).unwrap();
        assert_eq!(keys.list().len(), 1);
        assert!(keys.is_authorized("existing-key"));
        assert!(!keys.is_authorized("revoked-key"));
        assert!(!keys.is_authorized("wrong-key"));
        assert_eq!(keys.list()[0].realm, "cn");
        assert_eq!(keys.list()[0].created_at, "2026/10/01 22:25");
        assert!(keys.check_model_allowed("existing-key", "trae/model"));
        assert!(!keys.check_model_allowed("existing-key", "grok/model"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(keys.delete("old").unwrap());
        assert!(KeyManager::new(&dir, None).unwrap().list().is_empty());
    }

    #[test]
    fn generated_keys_and_previous_snapshot_survive_restarts() {
        let dir = directory();
        let first = KeyManager::new(&dir, None)
            .unwrap()
            .generate("First")
            .unwrap();
        let second = KeyManager::new(&dir, None)
            .unwrap()
            .generate("Second")
            .unwrap();
        let keys = KeyManager::new(&dir, None).unwrap();
        assert_eq!(keys.list().len(), 2);
        assert!(keys.is_authorized(&first.key));
        assert!(keys.is_authorized(&second.key));
        let previous: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("api_keys.json.bak")).unwrap()).unwrap();
        assert_eq!(previous.as_array().unwrap().len(), 1);
        assert_eq!(previous[0]["key"], first.key);
        assert!(keys.delete(&first.id).unwrap());
        let restarted = KeyManager::new(&dir, None).unwrap();
        assert!(!restarted.is_authorized(&first.key));
        assert!(restarted.is_authorized(&second.key));
    }

    #[test]
    fn malformed_storage_is_an_error_instead_of_an_empty_key_list() {
        let dir = directory();
        let path = dir.join("api_keys.json");
        fs::write(&path, "[{broken").unwrap();
        assert!(KeyManager::new(&dir, None).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "[{broken");
    }

    #[test]
    fn failed_write_returns_an_error_and_keeps_the_in_memory_keys() {
        let dir = directory();
        let keys = KeyManager::new(&dir, None).unwrap();
        let existing = keys.generate("Existing").unwrap();
        fs::create_dir(dir.join("api_keys.json.bak")).unwrap();
        assert!(keys.generate("Cannot save").is_err());
        assert!(keys.delete(&existing.id).is_err());
        assert_eq!(keys.list().len(), 1);
        assert_eq!(KeyManager::new(&dir, None).unwrap().list().len(), 1);
        assert!(keys.is_authorized(&existing.key));
    }

    #[cfg(unix)]
    #[test]
    fn api_key_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = directory();
        let keys = KeyManager::new(&dir, None).unwrap();
        keys.generate("private").unwrap();
        keys.generate("second").unwrap();
        for path in [
            dir.join("api_keys.json"),
            dir.join("api_keys.json.bak"),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_configured_keys_never_authorize_anonymous_requests() {
        let dir = directory();
        let keyless = KeyManager::new(&dir, Some("   ".to_string())).unwrap();
        assert!(!keyless.auth_required());
        assert!(keyless.is_authorized(""));

        let trimmed = KeyManager::new(&dir, Some(" secret ".to_string())).unwrap();
        assert!(trimmed.auth_required());
        assert!(!trimmed.is_authorized(""));
        assert!(!trimmed.is_authorized("secret "));
        assert!(trimmed.is_authorized("secret"));

        let empty_entry_dir = directory();
        fs::write(empty_entry_dir.join("api_keys.json"), r#"[{"key":""}]"#).unwrap();
        let with_entry = KeyManager::new(&empty_entry_dir, None).unwrap();
        assert!(with_entry.auth_required());
        assert!(!with_entry.is_authorized(""));
        assert!(!with_entry.check_model_allowed("", "any-model"));

        let _ = fs::remove_dir_all(dir);
        let _ = fs::remove_dir_all(empty_entry_dir);
    }
}
