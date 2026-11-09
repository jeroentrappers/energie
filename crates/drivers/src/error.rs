//! One error type for every driver.

/// What went wrong talking to a device or a price feed.
///
/// A failed read never invents values: the caller decides whether to keep the last reading or
/// to write the driver's [`crate::Source::unavailable`] reading (all `None`, HA's `unavailable`).
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// Transport-level HTTP failure (connect, TLS, timeout, body).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with a non-success status.
    #[error("http status {0}")]
    Status(u16),
    /// The device rejected our credentials (HomeWizard 401, ENTSO-E 401).
    #[error("unauthorized")]
    Unauthorized,
    /// Socket-level failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The device did not answer in time.
    #[error("timeout")]
    Timeout,
    /// The payload could not be parsed into the expected shape.
    #[error("malformed payload: {0}")]
    Malformed(String),
    /// A Modbus exception response (function code | 0x80) with its exception code.
    #[error("modbus exception code {0}")]
    ModbusException(u8),
    /// Modbus transport or framing failure.
    #[error("modbus: {0}")]
    Modbus(#[from] tokio_modbus::Error),
    /// Tuya frame whose HMAC-SHA256 does not match (wrong key or corrupted frame).
    #[error("tuya: bad hmac")]
    BadHmac,
    /// Tuya protocol violation (bad prefix, unexpected command, failed negotiation, ...).
    #[error("tuya: {0}")]
    Tuya(String),
    /// MQTT client failure.
    #[error("mqtt: {0}")]
    Mqtt(String),
    /// The session to the device is down; the command was not sent.
    #[error("not connected")]
    NotConnected,
    /// A command value outside what the device (or the HA entity) accepts.
    #[error("out of range: {0}")]
    OutOfRange(String),
    /// Invalid configuration (bad key length, bad PEM, ...).
    #[error("config: {0}")]
    Config(String),
    /// A cloud service failed (rejected token, rate limit, backoff, ...), with its message.
    #[error("cloud: {0}")]
    Cloud(String),
}

pub type Result<T> = std::result::Result<T, DriverError>;

impl From<tokio::time::error::Elapsed> for DriverError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        DriverError::Timeout
    }
}

impl From<serde_json::Error> for DriverError {
    fn from(e: serde_json::Error) -> Self {
        DriverError::Malformed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Error messages name the failing layer, so a log line is enough to tell a bad token from a
    /// dead socket.
    #[test]
    fn messages_name_the_layer() {
        assert_eq!(DriverError::Status(503).to_string(), "http status 503");
        assert_eq!(DriverError::Unauthorized.to_string(), "unauthorized");
        assert_eq!(DriverError::ModbusException(2).to_string(), "modbus exception code 2");
        assert_eq!(DriverError::BadHmac.to_string(), "tuya: bad hmac");
        assert_eq!(DriverError::NotConnected.to_string(), "not connected");
        assert_eq!(DriverError::Timeout.to_string(), "timeout");
        assert_eq!(DriverError::Cloud("rate limited: 5091".into()).to_string(), "cloud: rate limited: 5091");
        let json: DriverError = serde_json::from_str::<u8>("x").unwrap_err().into();
        assert!(matches!(json, DriverError::Malformed(_)));
    }

    /// A tokio timeout becomes [`DriverError::Timeout`].
    #[tokio::test]
    async fn elapsed_is_timeout() {
        let e =
            tokio::time::timeout(std::time::Duration::from_millis(1), std::future::pending::<()>()).await.unwrap_err();
        assert!(matches!(DriverError::from(e), DriverError::Timeout));
    }
}
