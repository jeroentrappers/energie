//! One error type for every cloud client.

use std::time::Duration;

/// What went wrong talking to a cloud service.
///
/// A failed read never invents values: the caller writes the client's
/// [`crate::CloudSource::unavailable`] reading (HA's `unavailable`) or keeps the last one.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    /// Transport-level HTTP failure (connect, TLS, timeout, body).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with a non-success status the client has no better name for.
    #[error("http status {0}")]
    Status(u16),
    /// Credentials or tokens were rejected; retrying soon will not help.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// The service answered with one of its own error codes.
    #[error("api error {code}: {message}")]
    Api { code: String, message: String },
    /// The service says we asked too often.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// A previous failure put the client in backoff; no request was sent.
    #[error("backing off, next attempt in {}s", .0.as_secs())]
    Backoff(Duration),
    /// The payload could not be parsed into the expected shape.
    #[error("malformed payload: {0}")]
    Malformed(String),
    /// Invalid configuration or credentials file.
    #[error("config: {0}")]
    Config(String),
    /// Reading or writing a credentials or state file failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A command value outside what the device (or the HA entity) accepts.
    #[error("out of range: {0}")]
    OutOfRange(String),
    /// The service has no data for us yet (empty listing, unknown device).
    #[error("not ready: {0}")]
    NotReady(String),
}

pub type Result<T> = std::result::Result<T, CloudError>;

impl From<serde_json::Error> for CloudError {
    fn from(e: serde_json::Error) -> Self {
        CloudError::Malformed(e.to_string())
    }
}

impl CloudError {
    /// Errors that a retry in a few minutes will not fix: they back off to the maximum at once.
    pub fn is_permanent(&self) -> bool {
        matches!(self, CloudError::Unauthorized(_) | CloudError::Config(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Messages name the failing layer and never carry a payload.
    #[test]
    fn messages_name_the_layer() {
        assert_eq!(CloudError::Status(503).to_string(), "http status 503");
        assert_eq!(CloudError::Unauthorized("token".into()).to_string(), "unauthorized: token");
        let api = CloudError::Api { code: "1103".into(), message: "invalid token".into() };
        assert_eq!(api.to_string(), "api error 1103: invalid token");
        assert_eq!(CloudError::RateLimited("5091".into()).to_string(), "rate limited: 5091");
        assert_eq!(CloudError::Backoff(Duration::from_secs(90)).to_string(), "backing off, next attempt in 90s");
        assert_eq!(CloudError::Config("x".into()).to_string(), "config: x");
        assert_eq!(CloudError::OutOfRange("x".into()).to_string(), "out of range: x");
        assert_eq!(CloudError::NotReady("x".into()).to_string(), "not ready: x");
        let json: CloudError = serde_json::from_str::<u8>("x").unwrap_err().into();
        assert!(matches!(json, CloudError::Malformed(_)));
        assert!(json.to_string().starts_with("malformed payload: "));
        let io: CloudError = std::io::Error::other("disk").into();
        assert_eq!(io.to_string(), "io: disk");
    }

    /// Only credential and configuration problems skip the gradual backoff.
    #[test]
    fn permanent_errors() {
        assert!(CloudError::Unauthorized("x".into()).is_permanent());
        assert!(CloudError::Config("x".into()).is_permanent());
        assert!(!CloudError::Status(500).is_permanent());
        assert!(!CloudError::RateLimited("x".into()).is_permanent());
    }
}
