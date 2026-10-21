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
            } else if PathBuf::from(r"E:\Program\MultiGateway\gateway-data").exists() {
                PathBuf::from(r"E:\Program\MultiGateway\gateway-data")
            } else if PathBuf::from(r"..\MultiGateway\gateway-data").exists() {
                PathBuf::from(r"..\MultiGateway\gateway-data")
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

        let api_key = api_key.or_else(|| std::env::var("MG_API_KEY").ok());

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
