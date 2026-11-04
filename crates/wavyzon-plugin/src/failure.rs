//! Typed failures with severities, reported to clients in `TrackExceptionEvent`
//! and `loadType: "error"` results.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Expected in normal operation (not found, timed out).
    Common,
    /// The source refused us (401/403/429) — possibly rate limited or banned.
    Suspicious,
    /// A bug or an unrecoverable node-side problem.
    Fault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cause: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

impl Failure {
    pub fn new(
        severity: Severity,
        source: impl Into<String>,
        message: impl Into<String>,
        cause: Option<String>,
    ) -> Self {
        Self {
            severity,
            message: message.into(),
            cause: cause.unwrap_or_default(),
            source: source.into(),
        }
    }
    pub fn common(source: impl Into<String>, message: impl Into<String>, cause: Option<String>) -> Self {
        Self::new(Severity::Common, source, message, cause)
    }
    pub fn suspicious(source: impl Into<String>, message: impl Into<String>, cause: Option<String>) -> Self {
        Self::new(Severity::Suspicious, source, message, cause)
    }
    pub fn fault(source: impl Into<String>, message: impl Into<String>, cause: Option<String>) -> Self {
        Self::new(Severity::Fault, source, message, cause)
    }

    /// Classify an HTTP status code returned by a source.
    pub fn from_status(source: &str, status: u16, cause: impl Into<String>) -> Self {
        let cause = Some(cause.into());
        match status {
            401 | 403 | 429 => Self::suspicious(source, "refused by source", cause),
            404 => Self::common(source, "not found at source", cause),
            _ => Self::common(source, format!("{source} failed"), cause),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cause.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.message, self.cause)
        }
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert_eq!(Failure::from_status("x", 429, "").severity, Severity::Suspicious);
        assert_eq!(Failure::from_status("x", 404, "").message, "not found at source");
        assert_eq!(
            Failure::common("x", "boom", Some("why".into())).to_string(),
            "boom: why"
        );
    }
}
