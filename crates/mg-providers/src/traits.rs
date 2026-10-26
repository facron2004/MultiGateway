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

    /// Run one chat turn.
    ///
    /// The third element names the upstream route when the provider has more
    /// than one. The Trae provider serves a turn either from a direct model
    /// call or from a full agent session, and the two differ so much in prompt
    /// size and latency that the request log has to say which one ran - a
    /// silent switch between them is indistinguishable from a broken gateway.
    async fn chat(
        &self,
        req: ChatCompletionRequest,
        session_id: Option<&str>,
    ) -> Result<(BoxStream<ChatCompletionChunk>, String, Option<String>)>;

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
