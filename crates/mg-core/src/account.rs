use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{MgError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountQuota {
    #[serde(default)]
    pub checked_at: f64,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub reset_at: Option<f64>,
    #[serde(default)]
    pub details: serde_json::Value,
}

fn deserialize_flexible_timestamp<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Number(n) => {
            let mut val = n.as_f64().unwrap_or(0.0);
            if val > 1e11 {
                val /= 1000.0;
            }
            Ok(val)
        }
        serde_json::Value::String(s) => {
            if let Ok(mut n) = s.parse::<f64>() {
                if n > 1e11 {
                    n /= 1000.0;
                }
                Ok(n)
            } else {
                Ok(0.0)
            }
        }
        _ => Ok(0.0),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(alias = "uid", alias = "user_id")]
    pub id: String,
    #[serde(
        default,
        alias = "nickname",
        alias = "name",
        alias = "username",
        alias = "preferred_username",
        alias = "account_name"
    )]
    pub label: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, alias = "accessToken", alias = "token")]
    pub access_token: String,
    #[serde(default, alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(
        default,
        deserialize_with = "deserialize_flexible_timestamp",
        alias = "expiresAt",
        alias = "expired_at"
    )]
    pub expires_at: f64,
    #[serde(default = "default_region", alias = "realm", alias = "edition")]
    pub region: String,
    #[serde(default = "default_true", alias = "model_enabled")]
    pub enabled: bool,
    #[serde(default = "default_one")]
    pub weight: u32,
    #[serde(default)]
    pub daily_tokens: u64,
    #[serde(default, alias = "lastError")]
    pub error: String,
    #[serde(default)]
    pub quota: Option<serde_json::Value>,
    #[serde(default)]
    pub credits: Option<serde_json::Value>,
    #[serde(default)]
    pub checkin: Option<serde_json::Value>,
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
    #[serde(
        default,
        alias = "providerSpecificData",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_specific: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "userType",
        alias = "user_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_type: Option<String>,
    #[serde(
        default,
        rename = "organizationId",
        alias = "organization_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub organization_id: Option<String>,
    #[serde(
        default,
        rename = "organizationName",
        alias = "organization_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub organization_name: Option<String>,
}

fn default_region() -> String {
    "intl".to_string()
}
fn default_true() -> bool {
    true
}
fn default_one() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::Account;
    use serde_json::json;

    #[test]
    fn reads_and_preserves_provider_specific_account_fields() {
        let account: Account = serde_json::from_value(json!({
            "uid": "qoder-user",
            "realm": "cn",
            "accessToken": "token",
            "userType": "organization_member",
            "organizationId": "org-1",
            "organizationName": "Example",
            "providerSpecificData": {"userRegion": "CN"}
        }))
        .unwrap();

        assert_eq!(account.user_type.as_deref(), Some("organization_member"));
        assert_eq!(account.organization_id.as_deref(), Some("org-1"));
        assert_eq!(account.organization_name.as_deref(), Some("Example"));
        assert_eq!(
            account.provider_specific.as_ref().unwrap()["userRegion"],
            "CN"
        );

        let saved = serde_json::to_value(account).unwrap();
        assert_eq!(saved["userType"], "organization_member");
        assert_eq!(saved["organizationId"], "org-1");
        assert_eq!(saved["provider_specific"]["userRegion"], "CN");
    }

    fn temp_pool_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mg-rust-account-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ready_account(id: &str) -> Account {
        serde_json::from_value(json!({ "id": id, "refresh_token": "r" })).unwrap()
    }

    #[test]
    fn session_stickiness_map_stays_bounded_under_churn() {
        use super::{AccountPool, MAX_SESSION_ENTRIES};

        let dir = temp_pool_dir("cap");
        let pool = AccountPool::new(&dir);
        pool.save(ready_account("a1")).unwrap();

        for i in 0..(MAX_SESSION_ENTRIES + 500) {
            let sid = format!("session-{i}");
            assert!(pool.choose(Some(&sid), None).is_ok());
        }
        assert!(pool.session_map_len() <= MAX_SESSION_ENTRIES);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn update_quota_persists_a_parseable_file_that_matches_memory() {
        use super::{AccountPool, AccountQuota};

        let dir = temp_pool_dir("quota");
        let pool = AccountPool::new(&dir);
        pool.save(ready_account("a2")).unwrap();

        pool.update_quota(
            "a2",
            AccountQuota {
                checked_at: 7.0,
                remaining: Some(3.0),
                limit: Some(9.0),
                reset_at: None,
                details: json!({}),
            },
        )
        .unwrap();

        let raw = std::fs::read_to_string(dir.join("a2.json")).unwrap();
        let on_disk: Account = serde_json::from_str(&raw).unwrap();
        assert_eq!(on_disk.quota.as_ref().unwrap()["remaining"], 3.0);
        assert_eq!(
            pool.get("a2").unwrap().quota.as_ref().unwrap()["remaining"],
            3.0
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn updates_legacy_accounts_wrapper_without_creating_duplicate_account_file() {
        use super::{AccountPool, AccountQuota};

        let dir = temp_pool_dir("wrapper-quota");
        let wrapper_path = dir.join("accounts.json");
        std::fs::write(
            &wrapper_path,
            serde_json::to_vec_pretty(&json!({
                "region": "cn",
                "accounts": {"legacy": {
                    "id": "legacy",
                    "refresh_token": "r",
                    "personalToken": "preserve-this-field",
                    "provider_data": {"tag": "keep"}
                }}
            }))
            .unwrap(),
        )
        .unwrap();

        let pool = AccountPool::new(&dir);
        pool.update_quota(
            "legacy",
            AccountQuota {
                checked_at: 9.0,
                remaining: Some(7.0),
                limit: Some(10.0),
                reset_at: None,
                details: json!({}),
            },
        )
        .unwrap();

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&wrapper_path).unwrap()).unwrap();
        assert_eq!(saved["region"], "cn");
        assert_eq!(saved["accounts"]["legacy"]["quota"]["remaining"], 7.0);
        assert_eq!(
            saved["accounts"]["legacy"]["personalToken"],
            "preserve-this-field"
        );
        assert_eq!(saved["accounts"]["legacy"]["provider_data"]["tag"], "keep");
        assert!(!dir.join("legacy.json").exists());

        let reloaded = AccountPool::new(&dir);
        assert_eq!(reloaded.get("legacy").unwrap().remaining(), Some(7.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn per_account_file_wins_over_legacy_wrapper_on_reload() {
        use super::AccountPool;

        let dir = temp_pool_dir("wrapper-precedence");
        std::fs::write(
            dir.join("accounts.json"),
            br#"{"accounts":{"same":{"id":"same","label":"legacy"}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("same.json"), br#"{"id":"same","label":"newer"}"#).unwrap();

        let pool = AccountPool::new(&dir);
        assert_eq!(pool.get("same").unwrap().label, "newer");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn canonical_account_file_wins_over_a_legacy_region_file_with_the_same_id() {
        use super::AccountPool;

        let dir = temp_pool_dir("duplicate-id");
        std::fs::write(
            dir.join("cn.json"),
            br#"{"id":"cn-primary","access_token":"stale","refresh_token":"stale"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("cn-primary.json"),
            br#"{"id":"cn-primary","access_token":"fresh","refresh_token":"fresh"}"#,
        )
        .unwrap();

        let pool = AccountPool::new(&dir);
        let account = pool.get("cn-primary").unwrap();
        assert_eq!(account.access_token, "fresh");
        assert_eq!(account.refresh_token, "fresh");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_refreshes_a_legacy_file_that_shares_the_account_id() {
        use super::AccountPool;

        let dir = temp_pool_dir("duplicate-save");
        std::fs::write(
            dir.join("cn.json"),
            br#"{"id":"cn-primary","access_token":"stale","refresh_token":"stale"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("cn-primary.json"),
            br#"{"id":"cn-primary","access_token":"fresh","refresh_token":"fresh"}"#,
        )
        .unwrap();
        let pool = AccountPool::new(&dir);
        let mut account = pool.get("cn-primary").unwrap();
        account.access_token = "rotated".to_string();
        pool.save(account).unwrap();

        let legacy: Account =
            serde_json::from_slice(&std::fs::read(dir.join("cn.json")).unwrap()).unwrap();
        assert_eq!(legacy.access_token, "rotated");
        assert_eq!(legacy.refresh_token, "fresh");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn choose_skips_cooled_accounts_and_recovers_when_the_cooldown_lapses() {
        use super::AccountPool;

        let dir = temp_pool_dir("cooldown");
        let pool = AccountPool::new(&dir);
        pool.save(ready_account("c1")).unwrap();
        pool.save(ready_account("c2")).unwrap();

        pool.mark_cooldown("c1", 60.0);
        for _ in 0..5 {
            assert_eq!(pool.choose(None, None).unwrap().id, "c2");
        }

        // A sticky session bound to a cooled account falls through to a healthy one.
        assert_eq!(pool.choose(Some("sticky"), None).unwrap().id, "c2");
        // Explicit selection still honors the caller's choice.
        assert_eq!(pool.choose(None, Some("c1")).unwrap().id, "c1");

        // An expired cooldown returns the account to rotation.
        pool.mark_cooldown("c1", -1.0);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..10 {
            seen.insert(pool.choose(None, None).unwrap().id);
        }
        assert!(seen.contains("c1") && seen.contains("c2"), "{seen:?}");

        // Cooling the whole pool yields a clear exhaustion error.
        pool.mark_cooldown("c1", 60.0);
        pool.mark_cooldown("c2", 60.0);
        assert_eq!(pool.choose(None, None).unwrap_err().status_code(), 503);

        let _ = std::fs::remove_dir_all(dir);
    }
}

impl Account {
    pub fn is_ready(&self) -> bool {
        if !self.enabled {
            return false;
        }
        let now = now_secs();
        // If has refresh token, it is renewable
        if !self.refresh_token.is_empty() {
            return true;
        }
        // Otherwise, access token must not be expired
        self.expires_at > now
    }

    pub fn display_name(&self) -> &str {
        if !self.label.is_empty() {
            &self.label
        } else if !self.email.is_empty() {
            &self.email
        } else {
            &self.id
        }
    }

    pub fn remaining(&self) -> Option<f64> {
        if let Some(quota) = &self.quota {
            if quota
                .get("details")
                .and_then(|details| details.get("unlimited"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                return None;
            }
            if let Some(rem) = quota.get("remaining").and_then(|v| v.as_f64()) {
                return Some(rem);
            }
        }
        if let Some(credits) = &self.credits {
            if let Some(rem) = credits.get("remain").and_then(|v| v.as_f64()) {
                return Some(rem);
            }
            if let Some(checkin) = credits.get("checkin") {
                if let Some(rem) = checkin
                    .get("account_credits")
                    .and_then(|c| c.get("remaining"))
                    .and_then(|v| v.as_f64())
                {
                    return Some(rem);
                }
            }
        }
        if let Some(checkin) = &self.checkin {
            if let Some(rem) = checkin
                .get("account_credits")
                .and_then(|c| c.get("remaining"))
                .and_then(|v| v.as_f64())
            {
                return Some(rem);
            }
        }
        None
    }

    pub fn checkin_info(&self) -> serde_json::Value {
        if let Some(c) = &self.checkin {
            if let Some(done) = c
                .get("today_checked_in")
                .and_then(|v| v.as_bool())
                .or_else(|| c.get("checked_in").and_then(|v| v.as_bool()))
            {
                return serde_json::json!({
                    "supported": true,
                    "done": done,
                    "updated_at": null
                });
            }
        }
        if let Some(credits) = &self.credits {
            if let Some(checkin) = credits.get("checkin") {
                if let Some(done) = checkin
                    .get("today_checked_in")
                    .and_then(|v| v.as_bool())
                    .or_else(|| checkin.get("checked_in").and_then(|v| v.as_bool()))
                {
                    return serde_json::json!({
                        "supported": true,
                        "done": done,
                        "updated_at": null
                    });
                }
            }
        }
        serde_json::json!({
            "supported": false,
            "done": null,
            "updated_at": null
        })
    }
}

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn write_atomic(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&tmp_path, contents).map_err(|e| MgError::Io(e.to_string()))?;
    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(MgError::Io(error.to_string()));
    }
    Ok(())
}

fn merge_account_json(existing: serde_json::Value, account: &Account) -> Result<serde_json::Value> {
    let mut merged = existing.as_object().cloned().unwrap_or_default();
    for alias in [
        "id",
        "uid",
        "user_id",
        "label",
        "nickname",
        "name",
        "email",
        "access_token",
        "accessToken",
        "token",
        "refresh_token",
        "refreshToken",
        "expires_at",
        "expiresAt",
        "expired_at",
        "region",
        "realm",
        "edition",
        "enabled",
        "model_enabled",
        "weight",
        "daily_tokens",
        "error",
        "lastError",
        "quota",
        "credits",
        "checkin",
        "extra",
        "provider_specific",
        "providerSpecificData",
        "userType",
        "user_type",
        "organizationId",
        "organization_id",
        "organizationName",
        "organization_name",
    ] {
        merged.remove(alias);
    }
    let serialized =
        serde_json::to_value(account).map_err(|error| MgError::Json(error.to_string()))?;
    if let Some(fields) = serialized.as_object() {
        merged.extend(fields.clone());
    }
    Ok(serde_json::Value::Object(merged))
}

pub fn resolve_pool_dir(data_dir: &std::path::Path, provider: &str) -> PathBuf {
    let native = data_dir.join("native").join(provider).join("accounts");
    if native.exists() {
        return native;
    }
    if provider == "minimax" {
        let minimax_dir = data_dir.join("minimax");
        if minimax_dir.exists() {
            return minimax_dir;
        }
    }
    let std_pool = data_dir.join(provider).join("accounts");
    if std_pool.exists() {
        return std_pool;
    }
    let direct = data_dir.join(provider);
    if direct.exists() {
        return direct;
    }
    std_pool
}

/// Upper bound on remembered session→account stickiness entries. Session ids are
/// client-supplied, so without a cap a client rotating ids grows the map forever.
pub const MAX_SESSION_ENTRIES: usize = 4096;

pub struct AccountPool {
    dir: PathBuf,
    accounts: RwLock<HashMap<String, Account>>,
    active_region: RwLock<String>,
    session_map: RwLock<HashMap<String, String>>,
    /// account id -> epoch seconds until which choose() skips it after a retryable
    /// upstream failure. Kept beside Account (not inside it) so it stays in-memory
    /// only and does not perturb account serialization or struct literals.
    cooldowns: RwLock<HashMap<String, f64>>,
    round_robin_idx: AtomicUsize,
}

#[derive(Deserialize)]
struct AccountsWrapper {
    accounts: HashMap<String, Account>,
}

impl AccountPool {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let pool = Self {
            dir,
            accounts: RwLock::new(HashMap::new()),
            active_region: RwLock::new(String::new()),
            session_map: RwLock::new(HashMap::new()),
            cooldowns: RwLock::new(HashMap::new()),
            round_robin_idx: AtomicUsize::new(0),
        };
        pool.reload();
        pool.load_active_region();
        pool
    }

    fn load_active_region(&self) {
        let saved = fs::read_to_string(self.dir.join("region.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| {
                value
                    .get("region")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .map(|region| region.trim().to_ascii_lowercase())
            .filter(|region| matches!(region.as_str(), "cn" | "intl" | "global"));
        let inferred = if saved.is_none() {
            let mut counts = BTreeMap::<String, usize>::new();
            for account in self.accounts.read().values() {
                let region = account.region.trim().to_ascii_lowercase();
                if !region.is_empty() {
                    *counts.entry(region).or_default() += 1;
                }
            }
            counts
                .into_iter()
                .max_by(|(left_region, left_count), (right_region, right_count)| {
                    left_count
                        .cmp(right_count)
                        .then_with(|| left_region.cmp(right_region))
                })
                .map(|(region, _)| region)
        } else {
            None
        };
        *self.active_region.write() = saved.or(inferred).unwrap_or_else(|| "cn".to_string());
    }

    pub fn active_region(&self) -> String {
        self.active_region.read().clone()
    }

    pub fn set_active_region(&self, region: &str) -> Result<()> {
        let region = region.trim().to_ascii_lowercase();
        let mut active_region = self.active_region.write();
        let _ = fs::create_dir_all(&self.dir);
        let path = self.dir.join("region.json");
        let tmp_path = self
            .dir
            .join(format!("region.{}.tmp", uuid::Uuid::new_v4()));
        let contents = serde_json::to_vec_pretty(&serde_json::json!({ "region": region }))
            .map_err(|e| MgError::Json(e.to_string()))?;
        fs::write(&tmp_path, contents).map_err(|e| MgError::Io(e.to_string()))?;
        if let Err(error) = fs::rename(&tmp_path, &path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(MgError::Io(error.to_string()));
        }
        *active_region = region;
        Ok(())
    }

    pub fn reload(&self) {
        if !self.dir.exists() {
            let _ = fs::create_dir_all(&self.dir);
            return;
        }

        let mut check_dirs = vec![self.dir.clone()];
        let sub_accounts = self.dir.join("accounts");
        if sub_accounts.is_dir() {
            check_dirs.push(sub_accounts);
        }
        let sub_intl = self.dir.join("intl");
        if sub_intl.is_dir() {
            check_dirs.push(sub_intl);
        }

        let mut loaded: HashMap<String, (bool, Account)> = HashMap::new();
        let mut files: Vec<PathBuf> = check_dirs
            .iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            })
            .filter(|path| {
                path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json")
            })
            .collect();
        files.sort();

        // A legacy regional file (for example MiniMax's cn.json) can carry the
        // same id as the canonical per-account file (cn-primary.json). The
        // canonical <id>.json always wins so a stale legacy copy cannot shadow
        // newer credentials after a restart.
        fn insert_loaded(
            loaded: &mut HashMap<String, (bool, Account)>,
            account: Account,
            path: &std::path::Path,
        ) {
            let canonical = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == account.id);
            if let Some((true, _)) = loaded.get(&account.id) {
                return;
            }
            loaded.insert(account.id.clone(), (canonical, account));
        }

        // Load legacy accounts.json wrappers first so a per-account file created
        // by a newer version wins deterministically when both formats coexist.
        for path in files
            .iter()
            .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("accounts.json"))
            .chain(files.iter().filter(|path| {
                path.file_name().and_then(|name| name.to_str()) != Some("accounts.json")
            }))
        {
            let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                filename,
                "model.json"
                    | "settings.json"
                    | "active_realm.json"
                    | "integration-state.json"
                    | "region.json"
                    | "config.json"
                    | "usage_records.json"
                    | "usage_stats.json"
                    | "usage-summary.json"
            ) {
                continue;
            }

            if let Ok(content) = fs::read_to_string(path) {
                if let Ok(acc) = serde_json::from_str::<Account>(&content) {
                    if !acc.id.is_empty() {
                        insert_loaded(&mut loaded, acc, path);
                    }
                } else if let Ok(wrapper) = serde_json::from_str::<AccountsWrapper>(&content) {
                    for (k, mut acc) in wrapper.accounts {
                        if acc.id.is_empty() {
                            acc.id = k;
                        }
                        insert_loaded(&mut loaded, acc, path);
                    }
                }
            }
        }

        info!(dir = %self.dir.display(), count = loaded.len(), "Reloaded accounts");
        *self.accounts.write() = loaded
            .into_iter()
            .map(|(id, (_, account))| (id, account))
            .collect();
    }

    pub fn list(&self) -> Vec<Account> {
        self.accounts.read().values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Account> {
        self.accounts.read().get(id).cloned()
    }

    #[cfg(test)]
    pub fn session_map_len(&self) -> usize {
        self.session_map.read().len()
    }

    pub fn save(&self, account: Account) -> Result<()> {
        self.persist(&account)?;
        self.accounts.write().insert(account.id.clone(), account);
        Ok(())
    }

    /// Writes an account file atomically (tmp + rename) so a crash mid-write can
    /// never leave a truncated JSON that the next startup would fail to parse.
    fn persist(&self, account: &Account) -> Result<()> {
        let _ = fs::create_dir_all(&self.dir);
        let path = self.dir.join(format!("{}.json", account.id));

        if !path.exists() {
            let wrapper_path = self.dir.join("accounts.json");
            if let Ok(content) = fs::read_to_string(&wrapper_path) {
                if let Ok(mut wrapper) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(accounts) = wrapper
                        .get_mut("accounts")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        if let Some(existing) = accounts.get(&account.id).cloned() {
                            accounts
                                .insert(account.id.clone(), merge_account_json(existing, account)?);
                            let json = serde_json::to_vec_pretty(&wrapper)
                                .map_err(|e| MgError::Json(e.to_string()))?;
                            return write_atomic(&wrapper_path, &json);
                        }
                    }
                }
            }
        }

        let account_json = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
                .map(|existing| merge_account_json(existing, account))
                .transpose()?
                .unwrap_or(
                    serde_json::to_value(account)
                        .map_err(|error| MgError::Json(error.to_string()))?,
                )
        } else {
            serde_json::to_value(account).map_err(|error| MgError::Json(error.to_string()))?
        };
        let json =
            serde_json::to_vec_pretty(&account_json).map_err(|e| MgError::Json(e.to_string()))?;
        write_atomic(&path, &json)?;
        // A legacy regional file can share this id (MiniMax cn.json vs
        // cn-primary.json). Leaving it stale makes the next reader refresh
        // with a dead token and invalidate the current grant.
        self.sync_same_id_copies(&path, account, &json)
    }

    fn sync_same_id_copies(
        &self,
        canonical: &std::path::Path,
        account: &Account,
        json: &[u8],
    ) -> Result<()> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == canonical || !path.is_file() {
                continue;
            }
            let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if !filename.ends_with(".json")
                || matches!(
                    filename,
                    "accounts.json"
                        | "model.json"
                        | "settings.json"
                        | "active_realm.json"
                        | "integration-state.json"
                        | "region.json"
                        | "config.json"
                        | "usage_records.json"
                        | "usage_stats.json"
                        | "usage-summary.json"
                )
            {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(other) = serde_json::from_str::<Account>(&content) else {
                continue;
            };
            if other.id == account.id {
                write_atomic(&path, json)?;
            }
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.dir.join(format!("{}.json", id));
        if path.exists() {
            fs::remove_file(path).map_err(|e| MgError::Io(e.to_string()))?;
        }
        let wrapper_path = self.dir.join("accounts.json");
        if let Ok(content) = fs::read_to_string(&wrapper_path) {
            if let Ok(mut wrapper) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(accounts) = wrapper
                    .get_mut("accounts")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    if accounts.remove(id).is_some() {
                        let json = serde_json::to_vec_pretty(&wrapper)
                            .map_err(|e| MgError::Json(e.to_string()))?;
                        write_atomic(&wrapper_path, &json)?;
                    }
                }
            }
        }
        self.accounts.write().remove(id);
        Ok(())
    }

    pub fn choose(&self, session_id: Option<&str>, specific_id: Option<&str>) -> Result<Account> {
        self.choose_in_region(session_id, specific_id, None)
    }

    pub fn choose_in_region(
        &self,
        session_id: Option<&str>,
        specific_id: Option<&str>,
        region: Option<&str>,
    ) -> Result<Account> {
        let accounts = self.accounts.read();
        if accounts.is_empty() {
            return Err(MgError::provider(
                503,
                "No accounts available in pool. Please add an account in the dashboard.",
                "no_accounts",
            ));
        }
        let now = now_secs();

        // 1. If specific account requested
        if let Some(id) = specific_id {
            if let Some(acc) = accounts.get(id) {
                if !region.is_none_or(|region| regions_match(&acc.region, region)) {
                    return Err(MgError::provider(
                        400,
                        format!("Requested account {} is not in the selected region", id),
                        "account_unavailable",
                    ));
                }
                if acc.is_ready() {
                    return Ok(acc.clone());
                }
                return Err(MgError::provider(
                    400,
                    format!("Requested account {} is not ready or disabled", id),
                    "account_unavailable",
                ));
            }
            return Err(MgError::not_found(format!("Account {} not found", id)));
        }

        // 2. Check session stickiness (a cooled-down sticky account falls through to
        // normal selection rather than failing the request).
        let cooldowns = self.cooldowns.read();
        if let Some(sid) = session_id {
            let session_map = self.session_map.read();
            if let Some(mapped_id) = session_map.get(sid) {
                if let Some(acc) = accounts.get(mapped_id) {
                    let cooled = cooldowns.get(mapped_id).copied().unwrap_or(0.0) > now;
                    if acc.is_ready()
                        && !cooled
                        && region.is_none_or(|region| regions_match(&acc.region, region))
                    {
                        return Ok(acc.clone());
                    }
                }
            }
        }

        // 3. Filter ready accounts not in cooldown
        let ready: Vec<&Account> = accounts
            .values()
            .filter(|a| {
                a.is_ready()
                    && cooldowns.get(&a.id).copied().unwrap_or(0.0) <= now
                    && region.is_none_or(|region| regions_match(&a.region, region))
            })
            .collect();
        if ready.is_empty() {
            return Err(MgError::provider(
                503,
                if let Some(region) = region {
                    format!(
                        "No ready accounts are available in the selected region ({})",
                        region
                    )
                } else {
                    "All accounts in the pool are exhausted, disabled, or cooling down after upstream errors".to_string()
                },
                "pool_exhausted",
            ));
        }

        // 4. Round-robin / weighted selection
        let idx = self.round_robin_idx.fetch_add(1, Ordering::Relaxed) % ready.len();
        let selected = ready[idx].clone();

        // 5. Save session mapping if session_id provided
        if let Some(sid) = session_id {
            let mut session_map = self.session_map.write();
            // Session ids are client-supplied and unbounded, so cap the map or a
            // client rotating ids would grow it forever.
            if session_map.len() >= MAX_SESSION_ENTRIES && !session_map.contains_key(sid) {
                if let Some(victim) = session_map.keys().next().cloned() {
                    session_map.remove(&victim);
                }
            }
            session_map.insert(sid.to_string(), selected.id.clone());
        }

        Ok(selected)
    }

    pub fn update_quota(&self, account_id: &str, quota: AccountQuota) -> Result<()> {
        let updated = {
            let mut accounts = self.accounts.write();
            match accounts.get_mut(account_id) {
                Some(acc) => {
                    acc.quota = serde_json::to_value(quota).ok();
                    acc.clone()
                }
                None => return Ok(()),
            }
        };
        // Disk IO outside the lock: holding the write lock across fs::write stalls
        // every concurrent choose() on the request hot path.
        self.persist(&updated)
    }

    pub fn set_error(&self, account_id: &str, err_msg: &str) {
        let updated = {
            let mut accounts = self.accounts.write();
            match accounts.get_mut(account_id) {
                Some(acc) => {
                    acc.error = err_msg.to_string();
                    acc.clone()
                }
                None => return,
            }
        };
        let _ = self.persist(&updated);
    }

    /// Excludes an account from automatic selection for `seconds` after a retryable
    /// upstream failure, so one broken account cannot keep absorbing traffic while
    /// healthy ones sit idle. Explicit `specific_id` selection still honors the
    /// caller's choice.
    pub fn mark_cooldown(&self, account_id: &str, seconds: f64) {
        self.cooldowns
            .write()
            .insert(account_id.to_string(), now_secs() + seconds);
    }

    /// Epoch seconds until which `choose` skips this account (0 when not cooling).
    pub fn cooldown_until(&self, account_id: &str) -> f64 {
        self.cooldowns
            .read()
            .get(account_id)
            .copied()
            .unwrap_or(0.0)
    }
}

pub fn regions_match(account_region: &str, selected_region: &str) -> bool {
    let account_region = account_region.trim();
    let selected_region = selected_region.trim();
    if selected_region.eq_ignore_ascii_case("all") {
        return true;
    }
    let canonical = |region: &str| {
        if region.is_empty()
            || region.eq_ignore_ascii_case("cn")
            || region.eq_ignore_ascii_case("china")
        {
            "cn"
        } else {
            "intl"
        }
    };
    canonical(account_region) == canonical(selected_region)
}
