use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

use mg_providers::trae::TraeUpstream;

/// Provider switches the desktop panel can change while the gateway keeps
/// running.
///
/// They live in their own file because they change how upstream traffic is
/// routed, not how the desktop window behaves: rewriting `desktop-settings.json`
/// would also touch the Windows startup registry value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProviderSettings {
    /// Trae upstream transport: `solo` (call the model directly) or `remote`
    /// (drive a full Trae agent session).
    ///
    /// Empty means "nothing stored yet": the `TRAE_UPSTREAM_MODE` environment
    /// variable decides and the built-in default is `solo`.
    pub trae_upstream_mode: String,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            trae_upstream_mode: String::new(),
        }
    }
}

impl ProviderSettings {
    /// The stored Trae transport, if the panel has ever set one.
    pub fn trae_upstream(&self) -> Option<TraeUpstream> {
        TraeUpstream::parse(&self.trae_upstream_mode)
    }
}

pub fn load(data_dir: &Path) -> ProviderSettings {
    let bytes = match fs::read(data_dir.join("provider_settings.json")) {
        Ok(bytes) => bytes,
        Err(_) => return ProviderSettings::default(),
    };
    serde_json::from_slice::<ProviderSettings>(&bytes)
        .ok()
        .unwrap_or_default()
}

/// Persist the settings, rejecting an unknown Trae transport instead of
/// silently storing something the provider cannot use.
pub fn save(
    data_dir: &Path,
    settings: &ProviderSettings,
) -> io::Result<ProviderSettings> {
    let mode = TraeUpstream::parse(&settings.trae_upstream_mode)
        .ok_or_else(|| io::Error::other("上游通道只能是 solo 或 remote"))?;
    let normalized = ProviderSettings {
        trae_upstream_mode: mode.as_str().to_string(),
    };
    let path = data_dir.join("provider_settings.json");
    let temporary = data_dir.join("provider_settings.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&normalized)?)?;
    fs::rename(&temporary, &path)?;
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::{load, save, ProviderSettings};
    use mg_providers::trae::TraeUpstream;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-rust-provider-settings-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_uses_defaults_and_stores_nothing() {
        let dir = temp_dir("missing");
        let settings = load(&dir);
        assert_eq!(settings, ProviderSettings::default());
        assert!(settings.trae_upstream().is_none());
        assert!(!dir.join("provider_settings.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_normalizes_and_round_trips_the_trae_transport() {
        let dir = temp_dir("save");
        let saved = save(
            &dir,
            &ProviderSettings {
                trae_upstream_mode: "REMOTE".to_string(),
            },
        )
        .unwrap();
        assert_eq!(saved.trae_upstream_mode, "remote");
        assert_eq!(saved.trae_upstream(), Some(TraeUpstream::Remote));
        assert_eq!(load(&dir), saved);

        let saved = save(
            &dir,
            &ProviderSettings {
                trae_upstream_mode: "solo".to_string(),
            },
        )
        .unwrap();
        assert_eq!(saved.trae_upstream(), Some(TraeUpstream::Solo));
        assert_eq!(load(&dir).trae_upstream_mode, "solo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_an_unknown_transport() {
        let dir = temp_dir("reject");
        let error = save(
            &dir,
            &ProviderSettings {
                trae_upstream_mode: "turbo".to_string(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("solo"));
        assert!(!dir.join("provider_settings.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
