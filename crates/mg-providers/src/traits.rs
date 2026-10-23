use async_trait::async_trait;
use futures::Stream;
use mg_core::account::AccountQuota;
use mg_core::error::{MgError, Result};
use mg_protocols::openai::{ChatCompletionChunk, ChatCompletionRequest, ModelEntry};
use std::pin::Pin;

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> serde_json::Value;

    fn active_region(&self) -> Option<String> {
        None
    }

    async fn models(&self) -> Result<Vec<ModelEntry>>;

    async fn models_for_region(&self, _region: Option<&str>) -> Result<Vec<ModelEntry>> {
        self.models().await
    }

    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String)>;

    async fn refresh_quota(&self, account_id: &str) -> Result<AccountQuota>;

    async fn refresh_credentials(&self, _account_id: &str) -> Result<serde_json::Value> {
        Err(MgError::bad_request("Credential refresh is not supported"))
    }

    async fn checkin(&self, _account_id: &str) -> Result<serde_json::Value> {
        Err(MgError::bad_request("Check-in is not supported"))
    }

    async fn set_region(&self, _region: &str) -> Result<serde_json::Value> {
        Err(MgError::bad_request("Region switching is not supported"))
    }

    async fn import_accounts(&self, _data: serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "ok": true, "count": 0 }))
    }

    async fn start_login(&self, redirect_url: Option<&str>) -> Result<serde_json::Value>;
    /// Region-aware variant used by providers whose login endpoint differs per
    /// realm. The default keeps the region-less behavior.
    async fn start_login_region(
        &self,
        _region: Option<&str>,
        redirect_url: Option<&str>,
    ) -> Result<serde_json::Value> {
        self.start_login(redirect_url).await
    }
    async fn poll_login(&self, login_id: &str) -> Result<serde_json::Value>;
}
