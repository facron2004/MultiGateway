use std::collections::HashMap;
use std::sync::Arc;
use mg_core::{AccountPool, Activity, Config, KeyManager};
use mg_providers::Provider;
use parking_lot::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub activity: Arc<Activity>,
    pub key_manager: Arc<KeyManager>,
    pub providers: Arc<RwLock<HashMap<String, Arc<dyn Provider>>>>,
    /// The live pools the providers route through, shared with the management
    /// handlers so that enable/disable/delete/import take effect immediately.
    pub pools: Arc<HashMap<String, Arc<AccountPool>>>,
}

impl AppState {
    pub fn get_provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.read().get(name).cloned()
    }

    pub fn pool(&self, name: &str) -> Option<Arc<AccountPool>> {
        self.pools.get(name).cloned()
    }
}
