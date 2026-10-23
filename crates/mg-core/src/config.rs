use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub api_key: Option<String>,
    pub headless: bool,

    // Antigravity OAuth config
    pub antigravity_client_id: String,
    pub antigravity_client_secret: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8791,
            data_dir: PathBuf::from("gateway-data"),
            api_key: None,
            headless: true,
            antigravity_client_id: String::new(),
            antigravity_client_secret: String::new(),
        }
    }
}

impl Config {
    pub fn load_from_env_and_args(
        host: Option<String>,
        port: Option<u16>,
        data_dir: Option<PathBuf>,
        api_key: Option<String>,
        headless: bool,
    ) -> Self {
        // Load .env if exists
        let _ = dotenvy::dotenv();
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let _ = dotenvy::from_path(exe_dir.join(".env"));
            }
        }

        let base_data_dir = data_dir.unwrap_or_else(|| {
            if let Ok(dir) = std::env::var("MG_DATA_DIR") {
                PathBuf::from(dir)
            } else if let Ok(exe_path) = std::env::current_exe() {
                let beside_exe = exe_path.parent().map(|p| p.join("gateway-data"));
                if let Some(dir) = beside_exe.filter(|p| p.exists()) {
                    dir
                } else if let Some(dir) = macos_bundle_data_dir() {
                    dir
                } else {
                    PathBuf::from("gateway-data")
                }
            } else if let Some(dir) = macos_bundle_data_dir() {
                dir
            } else {
                PathBuf::from("gateway-data")
            }
        });

        let host = host.unwrap_or_else(|| {
            std::env::var("MG_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
        });

        let port = port.unwrap_or_else(|| {
            std::env::var("MG_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(8791)
        });

        let api_key = api_key
            .or_else(|| std::env::var("MG_API_KEY").ok())
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());

        let antigravity_client_id = std::env::var("ANTIGRAVITY_OAUTH_CLIENT_ID")
            .unwrap_or_default();
        let antigravity_client_secret = std::env::var("ANTIGRAVITY_OAUTH_CLIENT_SECRET")
            .unwrap_or_default();

        Self {
            host,
            port,
            data_dir: base_data_dir,
            api_key,
            headless,
            antigravity_client_id,
            antigravity_client_secret,
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.data_dir.join("antigravity/accounts"))?;
        std::fs::create_dir_all(self.data_dir.join("copilot/accounts"))?;
        std::fs::create_dir_all(self.data_dir.join("grok/accounts"))?;
        std::fs::create_dir_all(self.data_dir.join("minimax/accounts"))?;
        std::fs::create_dir_all(self.data_dir.join("trae"))?;
        std::fs::create_dir_all(self.data_dir.join("workbuddy/accounts"))?;
        std::fs::create_dir_all(self.data_dir.join("qoder/accounts"))?;
        Ok(())
    }
}

/// Data directory for a `.app` bundle. A `gateway-data` folder beside the app
/// wins, so a portable copy keeps its files. Otherwise accounts live in
/// Application Support. Finder launches the bundle with a useless working
/// directory, so a relative `gateway-data` would not be writable.
fn macos_bundle_data_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    bundle_data_dir(&executable, home.as_deref(), |path| path.is_dir())
}

fn bundle_data_dir(
    executable: &std::path::Path,
    home: Option<&std::path::Path>,
    portable_exists: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let macos_dir = executable.parent()?;
    let contents_dir = macos_dir.parent()?;
    let app_dir = contents_dir.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS"
        || contents_dir.file_name()?.to_str()? != "Contents"
        || app_dir.extension()?.to_str()? != "app"
    {
        return None;
    }
    if let Some(app_parent) = app_dir.parent() {
        let portable = app_parent.join("gateway-data");
        if portable_exists(&portable) {
            return Some(portable);
        }
    }
    Some(
        home?.join("Library")
            .join("Application Support")
            .join("MultiGateway"),
    )
}

#[cfg(test)]
mod tests {
    use super::bundle_data_dir;
    use std::path::{Path, PathBuf};

    #[test]
    fn app_bundle_uses_application_support_without_portable_data() {
        let executable = Path::new("/Applications/MultiGateway.app/Contents/MacOS/MultiGateway");
        let dir = bundle_data_dir(executable, Some(Path::new("/Users/me")), |_| false).unwrap();
        assert_eq!(
            dir,
            PathBuf::from("/Users/me/Library/Application Support/MultiGateway")
        );
    }

    #[test]
    fn app_bundle_prefers_gateway_data_beside_the_app() {
        let executable =
            Path::new("/Volumes/MultiGateway/MultiGateway.app/Contents/MacOS/MultiGateway");
        let dir = bundle_data_dir(executable, None, |path| path.ends_with("gateway-data")).unwrap();
        assert_eq!(dir, PathBuf::from("/Volumes/MultiGateway/gateway-data"));
    }

    #[test]
    fn loose_binary_is_not_treated_as_a_bundle() {
        let executable = Path::new("/usr/local/bin/MultiGateway");
        assert!(bundle_data_dir(executable, Some(Path::new("/Users/me")), |_| true).is_none());
    }
}
