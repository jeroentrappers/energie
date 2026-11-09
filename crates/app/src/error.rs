//! One error type for the application layer: a message, with `From` impls for the I/O,
//! (de)serialisation and protocol errors the runtime can hit.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

macro_rules! from_err {
    ($($t:ty),*) => {$(
        impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error(e.to_string())
            }
        }
    )*};
}

from_err!(
    std::io::Error,
    serde_json::Error,
    toml::de::Error,
    tokio_tungstenite::tungstenite::Error,
    reqwest::Error,
    std::net::AddrParseError
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Errors carry their message through `Display` and convert from the library errors.
    #[test]
    fn errors_display_their_message_and_convert() {
        let e = Error::new("kapot");
        assert_eq!(e.to_string(), "kapot");
        let io: Error = std::io::Error::other("schijf vol").into();
        assert_eq!(io.0, "schijf vol");
        let json: Error = serde_json::from_str::<u8>("x").unwrap_err().into();
        assert!(json.0.contains("expected"));
        let addr: Error = "nope".parse::<std::net::SocketAddr>().unwrap_err().into();
        assert!(!addr.0.is_empty());
    }
}
