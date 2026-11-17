use serde::{Deserialize, Serialize};

use super::ErrorSchema;

/// Machine-readable error codes of `cardinal.error.v1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    StateUnavailable,
    ConfigInvalid,
    NotFound,
    AmbiguousId,
    InvalidArgument,
    NotImplemented,
    Internal,
}

impl ErrorCode {
    /// The code as it appears in JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StateUnavailable => "state_unavailable",
            Self::ConfigInvalid => "config_invalid",
            Self::NotFound => "not_found",
            Self::AmbiguousId => "ambiguous_id",
            Self::InvalidArgument => "invalid_argument",
            Self::NotImplemented => "not_implemented",
            Self::Internal => "internal",
        }
    }
}

/// The error every command handler returns; the CLI prints it as `cardinal.error.v1`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error of a declared function whose body is not written yet.
    #[must_use]
    pub fn not_implemented(what: &str) -> Self {
        Self::new(
            ErrorCode::NotImplemented,
            format!("{what} is not implemented"),
        )
    }

    /// The `cardinal.error.v1` document for this error.
    #[must_use]
    pub fn to_doc(&self) -> ErrorDoc {
        ErrorDoc {
            schema: ErrorSchema,
            error: ErrorBody {
                code: self.code,
                message: self.message.clone(),
            },
        }
    }
}

/// `cardinal.error.v1`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDoc {
    pub schema: ErrorSchema,
    pub error: ErrorBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCode};

    #[test]
    fn error_doc_matches_the_contract() {
        let doc = Error::not_implemented("inventory").to_doc();
        assert_eq!(
            serde_json::to_value(doc).unwrap(),
            serde_json::json!({
                "schema": "cardinal.error.v1",
                "error": {"code": "not_implemented", "message": "inventory is not implemented"}
            })
        );
    }

    #[test]
    fn as_str_matches_serde() {
        for code in [
            ErrorCode::StateUnavailable,
            ErrorCode::ConfigInvalid,
            ErrorCode::NotFound,
            ErrorCode::AmbiguousId,
            ErrorCode::InvalidArgument,
            ErrorCode::NotImplemented,
            ErrorCode::Internal,
        ] {
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                serde_json::Value::from(code.as_str())
            );
        }
    }
}
