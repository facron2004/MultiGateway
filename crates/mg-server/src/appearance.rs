use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// Local UI preferences. Kept apart from desktop window settings so changing
/// the theme never rewrites the Windows startup registry value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppearanceSettings {
    pub theme: String,
    pub language: String,
    pub accent: String,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            language: "system".to_string(),
            accent: "#fa751b".to_string(),
        }
    }
}

pub fn load(data_dir: &Path) -> AppearanceSettings {
    let bytes = match fs::read(data_dir.join("appearance.json")) {
        Ok(bytes) => bytes,
        Err(_) => return AppearanceSettings::default(),
    };
    serde_json::from_slice::<AppearanceSettings>(&bytes)
        .ok()
        .and_then(|settings| normalize(settings).ok())
        .unwrap_or_default()
}

pub fn normalize(settings: AppearanceSettings) -> Result<AppearanceSettings, &'static str> {
    let theme = match settings.theme.as_str() {
        "system" | "light" | "dark" => settings.theme,
        _ => return Err("主题只能是跟随系统、亮色或暗色"),
    };
    let language = match settings.language.as_str() {
        "system" | "zh-CN" | "en" => settings.language,
        _ => return Err("语言只能是跟随系统、简体中文或 English"),
    };
    let accent = settings.accent.trim().to_ascii_lowercase();
    if !is_accent(&accent) {
        return Err("主题色需要是 #RRGGBB 颜色");
    }
    Ok(AppearanceSettings {
        theme,
        language,
        accent,
    })
}

fn is_accent(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next() == Some('#')
        && chars.clone().count() == 6
        && chars.all(|char| char.is_ascii_hexdigit())
}

pub fn save(data_dir: &Path, settings: &AppearanceSettings) -> io::Result<AppearanceSettings> {
    let settings = normalize(settings.clone()).map_err(io::Error::other)?;
    let path = data_dir.join("appearance.json");
    let temporary = data_dir.join("appearance.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&settings)?)?;
    fs::rename(&temporary, &path)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::{load, normalize, save, AppearanceSettings};

    #[test]
    fn missing_file_and_partial_json_use_defaults() {
        let dir = std::env::temp_dir().join(format!("mg-rust-appearance-{}", uuid::Uuid::new_v4()));
        let settings = load(&dir);
        assert_eq!(settings, AppearanceSettings::default());
        assert!(!dir.join("appearance.json").exists());

        let parsed: AppearanceSettings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(parsed.language, "system");
        assert_eq!(parsed.accent, "#fa751b");
        assert_eq!(parsed.theme, "dark");
    }

    #[test]
    fn normalize_rejects_unknown_values_and_lowercases_accent() {
        let bad_theme = AppearanceSettings {
            theme: "blue".to_string(),
            ..AppearanceSettings::default()
        };
        assert!(normalize(bad_theme).is_err());

        let bad_language = AppearanceSettings {
            language: "ja".to_string(),
            ..AppearanceSettings::default()
        };
        assert!(normalize(bad_language).is_err());

        let bad_accent = AppearanceSettings {
            accent: "orange".to_string(),
            ..AppearanceSettings::default()
        };
        assert!(normalize(bad_accent).is_err());

        let accent = normalize(AppearanceSettings {
            accent: "  #3B82F6 ".to_string(),
            ..AppearanceSettings::default()
        })
        .unwrap();
        assert_eq!(accent.accent, "#3b82f6");
    }

    #[test]
    fn save_round_trips_normalized_settings() {
        let dir = std::env::temp_dir().join(format!("mg-rust-appearance-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = save(
            &dir,
            &AppearanceSettings {
                theme: "light".to_string(),
                language: "en".to_string(),
                accent: "#7C3AED".to_string(),
            },
        )
        .unwrap();
        assert_eq!(saved.accent, "#7c3aed");
        assert_eq!(load(&dir), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
