use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum MgError {
    #[error("Provider error ({status}): {message}")]
    Provider {
        status: u16,
        code: String,
        message: String,
    },

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("JSON error: {0}")]
    Json(String),
}

impl MgError {
    pub fn provider(status: u16, message: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Provider {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::Provider {
            status: 400,
            code: "invalid_request_error".to_string(),
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::Provider {
            status: 404,
            code: "not_found".to_string(),
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Provider {
            status: 401,
            code: "authentication_error".to_string(),
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Provider {
            status: 403,
            code: "permission_error".to_string(),
            message: message.into(),
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::Provider { status, .. } => *status,
            Self::NotFound(_) => 404,
            Self::Unauthorized(_) => 401,
            Self::BadRequest(_) => 400,
            Self::Internal(_) => 500,
            Self::Io(_) => 500,
            Self::Json(_) => 400,
        }
    }

    pub fn error_type(&self) -> &str {
        match self {
            Self::Provider { code, .. } => code.as_str(),
            Self::NotFound(_) => "not_found",
            Self::Unauthorized(_) => "authentication_error",
            Self::BadRequest(_) => "invalid_request_error",
            Self::Internal(_) => "internal_server_error",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_parse_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Provider { message, .. } => message.clone(),
            Self::NotFound(msg)
            | Self::Unauthorized(msg)
            | Self::BadRequest(msg)
            | Self::Internal(msg)
            | Self::Io(msg)
            | Self::Json(msg) => msg.clone(),
        }
    }

    pub fn to_openai_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "message": self.message(),
                "type": self.error_type(),
                "param": null,
                "code": self.status_code(),
            }
        })
    }

    pub fn to_anthropic_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error": {
                "type": self.error_type(),
                "message": self.message(),
            }
        })
    }
}

pub type Result<T> = std::result::Result<T, MgError>;
