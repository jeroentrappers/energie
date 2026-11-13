//! The protocol versions side by side: one encode, decode and open for 3.4 ([`protocol`]) and
//! 3.5 ([`v35`]), used by the client session and by the simulator.

use std::fmt;

use super::protocol::{self, Frame, Key};
use super::v35;
use crate::error::{DriverError, Result};

/// A Tuya local protocol version this crate speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Version {
    /// 3.4: `55AA` frames, AES-ECB and HMAC-SHA256.
    #[default]
    V34,
    /// 3.5: `6699` frames, AES-GCM.
    V35,
}

impl Version {
    /// `"3.4"` or `"3.5"` (tinytuya's and `tuya_local`'s spelling).
    pub fn parse(s: &str) -> Result<Version> {
        match s {
            "3.4" => Ok(Version::V34),
            "3.5" => Ok(Version::V35),
            other => Err(DriverError::Config(format!("tuya protocol {other:?} is not supported (3.4 or 3.5)"))),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Version::V34 => "3.4",
            Version::V35 => "3.5",
        })
    }
}

/// One frame on the wire. `plain` is the payload before encryption (`None`: an empty payload,
/// as in a bare ack); the version header is added where the protocol wants one. Devices set a
/// `retcode`, clients do not.
pub fn encode(v: Version, key: &Key, seq: u32, cmd: u32, retcode: Option<u32>, plain: Option<&[u8]>) -> Vec<u8> {
    match v {
        Version::V34 => {
            let payload = plain.map(|p| protocol::seal(key, cmd, p)).unwrap_or_default();
            protocol::pack(key, &Frame { seq, cmd, retcode, payload })
        }
        Version::V35 => {
            let payload = match plain {
                Some(p) if protocol::has_version_header(cmd) => [&v35::VERSION_HEADER[..], p].concat(),
                Some(p) => p.to_vec(),
                None => Vec::new(),
            };
            v35::pack(key, &Frame { seq, cmd, retcode, payload }, rand::random())
        }
    }
}

/// Parse one frame from the front of `buf` (see [`protocol::unpack`]). A 3.5 frame comes back
/// decrypted; `from_device` takes its return code out of the plaintext.
pub fn decode(v: Version, key: &Key, buf: &[u8], from_device: bool) -> Result<Option<(Frame, usize)>> {
    match v {
        Version::V34 => protocol::unpack(key, buf),
        Version::V35 => {
            Ok(v35::unpack(key, buf)?.map(|(f, n)| (if from_device { v35::split_retcode(f) } else { f }, n)))
        }
    }
}

/// The payload of a decoded frame as plaintext, version header removed.
pub fn open(v: Version, key: &Key, frame: &Frame) -> Result<Vec<u8>> {
    match v {
        Version::V34 => protocol::open(key, &frame.payload),
        Version::V35 => Ok(v35::strip_version(&frame.payload).to_vec()),
    }
}

/// The payload of a handshake frame (nonces and HMACs: no version header to look for).
pub fn open_handshake(v: Version, key: &Key, frame: &Frame) -> Result<Vec<u8>> {
    match v {
        Version::V34 => protocol::decrypt(key, &frame.payload),
        Version::V35 => Ok(frame.payload.clone()),
    }
}

/// The session key both sides derive from the two nonces.
pub fn session_key(v: Version, local_key: &Key, client_nonce: &Key, device_nonce: &Key) -> Key {
    match v {
        Version::V34 => protocol::session_key(local_key, client_nonce, device_nonce),
        Version::V35 => v35::session_key(local_key, client_nonce, device_nonce),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuya::protocol::cmd;

    const KEY: &Key = b"0123456789abcdef";

    /// Both versions round-trip a control command (version header added and removed) and a
    /// device's bare ack; a 3.5 device frame gives its return code back.
    #[test]
    fn both_versions_round_trip() {
        for v in [Version::V34, Version::V35] {
            let wire = encode(v, KEY, 5, cmd::CONTROL_NEW, None, Some(b"{\"a\":1}"));
            let (f, used) = decode(v, KEY, &wire, false).unwrap().unwrap();
            assert_eq!((f.seq, f.cmd, used), (5, cmd::CONTROL_NEW, wire.len()), "{v}");
            assert_eq!(open(v, KEY, &f).unwrap(), b"{\"a\":1}", "{v}");
            let ack = encode(v, KEY, 6, cmd::CONTROL_NEW, Some(0), None);
            let (f, _) = decode(v, KEY, &ack, true).unwrap().unwrap();
            assert_eq!((f.retcode, open(v, KEY, &f).unwrap()), (Some(0), vec![]), "{v}");
        }
    }

    /// Queries and heartbeats carry no version header in either version.
    #[test]
    fn no_header_on_queries() {
        let wire = encode(Version::V35, KEY, 1, cmd::DP_QUERY_NEW, None, Some(b"{}"));
        assert_eq!(decode(Version::V35, KEY, &wire, false).unwrap().unwrap().0.payload, b"{}");
        let wire = encode(Version::V35, KEY, 1, cmd::CONTROL_NEW, None, Some(b"{}"));
        assert!(decode(Version::V35, KEY, &wire, false).unwrap().unwrap().0.payload.starts_with(b"3.5"));
    }

    /// A frame of one version does not parse as the other.
    #[test]
    fn versions_do_not_mix() {
        let wire = encode(Version::V34, KEY, 1, cmd::HEART_BEAT, None, Some(b"{}"));
        assert!(decode(Version::V35, KEY, &wire, true).is_err());
        let wire = encode(Version::V35, KEY, 1, cmd::HEART_BEAT, None, Some(b"{}"));
        assert!(decode(Version::V34, KEY, &wire, true).is_err());
    }

    #[test]
    fn parse_and_display() {
        assert_eq!(Version::parse("3.5").unwrap(), Version::V35);
        assert_eq!(Version::parse("3.4").unwrap().to_string(), "3.4");
        assert!(matches!(Version::parse("3.3"), Err(DriverError::Config(_))));
        assert_eq!(Version::default(), Version::V34);
    }
}
