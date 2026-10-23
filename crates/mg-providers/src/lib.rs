pub mod antigravity;
pub mod catalog;
pub mod copilot;
pub mod grok;
pub mod minimax;
mod model_limits;
mod model_metadata;
pub mod qoder;
pub mod trae;
pub mod traits;
pub mod workbuddy;

pub use antigravity::AntigravityProvider;
pub use copilot::CopilotProvider;
pub use grok::GrokProvider;
pub use minimax::MiniMaxProvider;
pub use model_metadata::{
    with_agent_metadata, with_provider_metadata, ProviderDefaults, EFFORT_LEVELS,
};
pub use qoder::QoderProvider;
pub use trae::TraeProvider;
pub use traits::*;
pub use workbuddy::WorkBuddyProvider;

/// How long an account is benched from automatic selection after an
/// account-level upstream failure (see [`is_account_level_failure`]). Bounded so
/// a healed account returns to rotation quickly, while a broken one cannot keep
/// absorbing and failing ~1/N of all traffic.
pub const ACCOUNT_COOLDOWN_SECS: f64 = 60.0;

/// Returns true when an upstream HTTP `status` means the *account* is unusable
/// (dead credentials, rate-limited, or an upstream server fault) rather than the
/// *request* being malformed. 400/404 are request-level problems — the caller
/// sent something the provider rejected — and must NOT cool an account down,
/// because the next well-formed request on that same account may succeed.
pub fn is_account_level_failure(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 500..=599)
}

/// Whether an upstream check-in error body actually means "already checked in
/// today". Several providers answer the claim endpoint with HTTP 400 and this
/// business message instead of a success code.
pub fn is_already_checked_in_message(body: &str) -> bool {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if value.get("code").and_then(serde_json::Value::as_i64) == Some(10001) {
            return true;
        }
        for key in ["msg", "message", "error"] {
            if let Some(text) = value.get(key).and_then(serde_json::Value::as_str) {
                if text.contains("今日已签到")
                    || text.contains("已完成签到")
                    || text.contains("今天已签到")
                    || text.to_ascii_lowercase().contains("already checked")
                    || text.to_ascii_lowercase().contains("already claimed")
                {
                    return true;
                }
            }
        }
    }
    let lower = body.to_ascii_lowercase();
    body.contains("今日已签到")
        || body.contains("已完成签到")
        || body.contains("今天已签到")
        || lower.contains("already checked")
        || lower.contains("already_checked")
        || lower.contains("already claimed")
        || lower.contains("already_claimed")
}

/// Maximum number of characters kept from a cleaned upstream error body.
pub const UPSTREAM_ERROR_BODY_MAX_CHARS: usize = 200;

/// Builds a short, single-line, client-safe description of an upstream error
/// response body for use inside [`mg_core::error::MgError`] messages.
///
/// The raw body is never shown to clients: HTML tags are stripped, common HTML
/// entities are decoded, all whitespace and control characters (including
/// `\r\n` and tabs) are collapsed to single spaces, and the result is truncated
/// to at most [`UPSTREAM_ERROR_BODY_MAX_CHARS`] characters on a char boundary
/// (never slicing UTF-8 bytes). Empty or fully-stripped bodies fall back to a
/// generic phrase. Returns e.g. `upstream HTTP 404: 404 Not Found`.
pub fn upstream_error_body(status: u16, body: &str) -> String {
    fn clean(body: &str) -> String {
        // 1. Strip HTML tags (everything between '<' and the next '>').
        let mut no_tags = String::with_capacity(body.len().min(512));
        let mut in_tag = false;
        for ch in body.chars() {
            if in_tag {
                if ch == '>' {
                    in_tag = false;
                }
                continue;
            }
            if ch == '<' {
                in_tag = true;
            } else {
                no_tags.push(ch);
            }
        }

        // 2. Decode the most common HTML entities.
        let decoded = no_tags
            .replace("&nbsp;", " ")
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'");

        // 3. Collapse whitespace and control characters to single spaces.
        let mut collapsed = String::with_capacity(decoded.len().min(512));
        let mut last_was_space = false;
        for ch in decoded.chars() {
            if ch.is_whitespace() || (ch as u32) < 0x20 || ch == '\u{7f}' {
                if !last_was_space && !collapsed.is_empty() {
                    collapsed.push(' ');
                    last_was_space = true;
                }
            } else {
                collapsed.push(ch);
                last_was_space = false;
            }
        }
        collapsed.trim().to_string()
    }

    let cleaned = clean(body);
    if cleaned.is_empty() {
        return format!("upstream HTTP {}: (empty response body)", status);
    }

    // Truncate on a char boundary (count chars, never slice bytes).
    let total_chars = cleaned.chars().count();
    if total_chars > UPSTREAM_ERROR_BODY_MAX_CHARS {
        let head: String = cleaned
            .chars()
            .take(UPSTREAM_ERROR_BODY_MAX_CHARS)
            .collect();
        format!("upstream HTTP {}: {}…", status, head)
    } else {
        format!("upstream HTTP {}: {}", status, cleaned)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        upstream_error_body, AntigravityProvider, CopilotProvider, GrokProvider, MiniMaxProvider,
        Provider, QoderProvider, TraeProvider, WorkBuddyProvider, UPSTREAM_ERROR_BODY_MAX_CHARS,
    };
    use mg_core::AccountPool;

    #[test]
    fn detects_already_checked_in_business_messages() {
        assert!(super::is_already_checked_in_message(
            "{\"code\":400,\"msg\":\"今日已签到\"}"
        ));
        assert!(super::is_already_checked_in_message("ALREADY_CLAIMED"));
        assert!(super::is_already_checked_in_message(
            "The user has already checked in today"
        ));
        assert!(!super::is_already_checked_in_message(
            "refreshing the access token failed"
        ));
    }

    #[test]
    fn upstream_error_body_strips_html_and_collapses_whitespace() {
        let raw = "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>TLB</center>\t</body></html>";
        let msg = upstream_error_body(404, raw);
        assert_eq!(msg, "upstream HTTP 404: 404 Not Found 404 Not Found TLB");
        assert!(!msg.contains('<'));
        assert!(!msg.contains('>'));
        assert!(!msg.contains('\r'));
        assert!(!msg.contains('\n'));
        assert!(!msg.contains('\t'));
        assert!(!msg.contains("  "));
    }

    #[test]
    fn upstream_error_body_truncates_multibyte_safely() {
        // 3-byte UTF-8 chars: a byte-slice truncation would panic or corrupt.
        let long = "错".repeat(500);
        let msg = upstream_error_body(500, &long);
        // Still valid UTF-8 (it is a String) and ends with the ellipsis marker.
        assert!(msg.ends_with('…'));
        let body_part = msg.strip_prefix("upstream HTTP 500: ").unwrap();
        let kept = body_part.chars().count() - 1; // minus the ellipsis
        assert_eq!(kept, UPSTREAM_ERROR_BODY_MAX_CHARS);
        assert!(body_part.chars().all(|c| c == '错' || c == '…'));
    }

    #[test]
    fn upstream_error_body_keeps_short_text_unchanged() {
        let msg = upstream_error_body(429, "{\"error\":\"rate limited\"}");
        assert_eq!(msg, "upstream HTTP 429: {\"error\":\"rate limited\"}");
    }

    #[test]
    fn upstream_error_body_falls_back_when_empty() {
        assert_eq!(
            upstream_error_body(502, ""),
            "upstream HTTP 502: (empty response body)"
        );
        assert_eq!(
            upstream_error_body(502, "\r\n\t  \n"),
            "upstream HTTP 502: (empty response body)"
        );
        // Body that is nothing but HTML tags strips down to the fallback.
        assert_eq!(
            upstream_error_body(404, "<html><body><hr></body></html>"),
            "upstream HTTP 404: (empty response body)"
        );
    }

    #[tokio::test]
    async fn every_provider_has_an_offline_catalog_without_account_files() {
        let root =
            std::env::temp_dir().join(format!("mg-rust-provider-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = |provider: &str| Arc::new(AccountPool::new(root.join(provider)));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AntigravityProvider::new(
                pool("antigravity"),
                String::new(),
                String::new(),
            )),
            Arc::new(CopilotProvider::new(pool("copilot"))),
            Arc::new(GrokProvider::new(pool("grok"))),
            Arc::new(MiniMaxProvider::new(pool("minimax"))),
            Arc::new(QoderProvider::new(pool("qoder"))),
            Arc::new(TraeProvider::new(pool("trae"))),
            Arc::new(WorkBuddyProvider::new(pool("workbuddy"), None, None, None)),
        ];

        for provider in providers {
            let models = provider.models().await.unwrap();
            assert!(
                !models.is_empty(),
                "{} returned no offline models",
                provider.id()
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }
}
