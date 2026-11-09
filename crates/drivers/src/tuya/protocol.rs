//! Tuya local protocol 3.4: framing, AES-128-ECB, HMAC-SHA256 and the session-key handshake.
//!
//! Frame (all integers big-endian):
//!
//! ```text
//! 000055AA | seq u32 | cmd u32 | len u32 | [retcode u32] | payload | HMAC-SHA256 (32) | 0000AA55
//! ```
//!
//! `len` counts everything after itself. The HMAC covers the frame from the prefix to the end of
//! the payload. Frames from the device carry a 4-byte return code before the payload; since the
//! payload is AES ciphertext (a multiple of 16 bytes), its presence shows as `body % 16 == 4`.
//! Payloads are AES-128-ECB with PKCS#7 padding. Control commands carry a 15-byte version
//! header (`"3.4"` + 12 zero bytes) inside the ciphertext; queries, heartbeats and the
//! handshake do not. During the handshake the device's local key is both AES and HMAC key;
//! afterwards the negotiated session key is.
//!
//! Handshake: the client sends `SESS_KEY_NEG_START` with a 16-byte nonce; the device answers
//! `SESS_KEY_NEG_RESP` with its own nonce followed by `HMAC(local_key, client_nonce)`; the
//! client answers `SESS_KEY_NEG_FINISH` with `HMAC(local_key, device_nonce)`. The session key is
//! `AES-ECB(local_key, client_nonce XOR device_nonce)` without padding (as tinytuya does).

use aes::Aes128;
use aes::cipher::{BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{DriverError, Result};

pub const PREFIX: u32 = 0x0000_55AA;
pub const SUFFIX: u32 = 0x0000_AA55;
/// Largest frame accepted; real DP payloads are a few hundred bytes.
pub const MAX_FRAME: usize = 64 * 1024;

/// Command codes used by protocol 3.4.
pub mod cmd {
    pub const SESS_KEY_NEG_START: u32 = 0x03;
    pub const SESS_KEY_NEG_RESP: u32 = 0x04;
    pub const SESS_KEY_NEG_FINISH: u32 = 0x05;
    pub const STATUS: u32 = 0x08;
    pub const HEART_BEAT: u32 = 0x09;
    pub const CONTROL_NEW: u32 = 0x0d;
    pub const DP_QUERY_NEW: u32 = 0x10;
}

/// `"3.4"` followed by 12 zero bytes.
pub const VERSION_HEADER: [u8; 15] = *b"3.4\0\0\0\0\0\0\0\0\0\0\0\0";

/// A 16-byte AES key.
pub type Key = [u8; 16];

/// One frame. `payload` is the (encrypted) payload as on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub cmd: u32,
    pub retcode: Option<u32>,
    pub payload: Vec<u8>,
}

/// AES-128-ECB with PKCS#7 padding.
pub fn encrypt(key: &Key, plain: &[u8]) -> Vec<u8> {
    ecb::Encryptor::<Aes128>::new(key.into()).encrypt_padded_vec_mut::<Pkcs7>(plain)
}

/// Inverse of [`encrypt`]. Bad length or padding means a wrong key.
pub fn decrypt(key: &Key, data: &[u8]) -> Result<Vec<u8>> {
    ecb::Decryptor::<Aes128>::new(key.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| DriverError::Tuya("cannot decrypt payload (wrong key?)".into()))
}

/// One raw AES block, no padding (session-key derivation).
pub fn encrypt_block(key: &Key, block: &Key) -> Key {
    let mut b = (*block).into();
    Aes128::new(key.into()).encrypt_block(&mut b);
    b.into()
}

fn mac(key: &Key) -> Hmac<Sha256> {
    <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac takes any key length")
}

/// HMAC-SHA256.
pub fn hmac(key: &Key, data: &[u8]) -> [u8; 32] {
    let mut m = mac(key);
    m.update(data);
    m.finalize().into_bytes().into()
}

/// Serialize a frame, signing it with `key`.
pub fn pack(key: &Key, f: &Frame) -> Vec<u8> {
    let ret_len = if f.retcode.is_some() { 4 } else { 0 };
    let len = (ret_len + f.payload.len() + 32 + 4) as u32;
    let mut buf = Vec::with_capacity(16 + len as usize);
    for word in [PREFIX, f.seq, f.cmd, len] {
        buf.extend_from_slice(&word.to_be_bytes());
    }
    if let Some(rc) = f.retcode {
        buf.extend_from_slice(&rc.to_be_bytes());
    }
    buf.extend_from_slice(&f.payload);
    let tag = hmac(key, &buf);
    buf.extend_from_slice(&tag);
    buf.extend_from_slice(&SUFFIX.to_be_bytes());
    buf
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

/// Parse one frame from the front of `buf`. `Ok(None)` means more bytes are needed; on success
/// the frame and the number of bytes it used are returned.
pub fn unpack(key: &Key, buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < 16 {
        return Ok(None);
    }
    if word(buf, 0) != PREFIX {
        return Err(DriverError::Tuya("bad frame prefix".into()));
    }
    let len = word(buf, 12) as usize;
    if !(36..=MAX_FRAME).contains(&len) {
        return Err(DriverError::Tuya(format!("bad frame length {len}")));
    }
    let total = 16 + len;
    if buf.len() < total {
        return Ok(None);
    }
    if word(buf, total - 4) != SUFFIX {
        return Err(DriverError::Tuya("bad frame suffix".into()));
    }
    let signed = &buf[..total - 36];
    let mut m = mac(key);
    m.update(signed);
    m.verify_slice(&buf[total - 36..total - 4]).map_err(|_| DriverError::BadHmac)?;
    let body = &signed[16..];
    let (retcode, payload) = if body.len() % 16 == 4 { (Some(word(body, 0)), &body[4..]) } else { (None, body) };
    Ok(Some((Frame { seq: word(buf, 4), cmd: word(buf, 8), retcode, payload: payload.to_vec() }, total)))
}

/// Whether `cmd` carries the version header inside its ciphertext.
pub fn has_version_header(cmd: u32) -> bool {
    !matches!(
        cmd,
        cmd::DP_QUERY_NEW
            | cmd::HEART_BEAT
            | cmd::SESS_KEY_NEG_START
            | cmd::SESS_KEY_NEG_RESP
            | cmd::SESS_KEY_NEG_FINISH
    )
}

/// Encrypt a payload for `cmd`, adding the version header where the protocol wants one.
pub fn seal(key: &Key, cmd: u32, plain: &[u8]) -> Vec<u8> {
    if has_version_header(cmd) { encrypt(key, &[&VERSION_HEADER[..], plain].concat()) } else { encrypt(key, plain) }
}

/// Decrypt a payload and drop a leading version header. An empty payload (a bare ack) stays
/// empty.
pub fn open(key: &Key, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    let plain = decrypt(key, payload)?;
    if plain.len() >= VERSION_HEADER.len() && plain.starts_with(b"3.4") {
        Ok(plain[VERSION_HEADER.len()..].to_vec())
    } else {
        Ok(plain)
    }
}

/// `AES-ECB(local_key, a XOR b)`, the 3.4 session key.
pub fn session_key(local_key: &Key, client_nonce: &Key, device_nonce: &Key) -> Key {
    let mut x = [0u8; 16];
    for (i, b) in x.iter_mut().enumerate() {
        *b = client_nonce[i] ^ device_nonce[i];
    }
    encrypt_block(local_key, &x)
}

/// Parse a 16-character ASCII local key.
pub fn parse_key(key: &str) -> Result<Key> {
    key.as_bytes()
        .try_into()
        .map_err(|_| DriverError::Config(format!("tuya local key must be 16 bytes, got {}", key.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &Key = b"0123456789abcdef";

    /// Encrypting 16 bytes yields 32 (a full padding block), and decrypting gives them back.
    #[test]
    fn ecb_round_trip_with_pkcs7() {
        let c = encrypt(KEY, b"sixteen byte msg");
        assert_eq!(c.len(), 32);
        assert_eq!(decrypt(KEY, &c).unwrap(), b"sixteen byte msg");
        assert!(decrypt(b"fedcba9876543210", &c).is_err());
        assert!(decrypt(KEY, b"short").is_err());
    }

    /// Known-answer test: AES-128 of the FIPS-197 vector, and HMAC-SHA256 of RFC 4231 case 2.
    #[test]
    fn primitives_match_reference_vectors() {
        let key: Key = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let pt: Key = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let ct: Key = [0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a];
        assert_eq!(encrypt_block(&key, &pt), ct);
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(b"Jefe").unwrap();
        m.update(b"what do ya want for nothing?");
        let tag: [u8; 32] = m.finalize().into_bytes().into();
        assert_eq!(tag[..4], [0x5b, 0xdc, 0xc1, 0x46]);
        assert_eq!(hmac(KEY, b"x").len(), 32);
    }

    /// A packed frame unpacks to itself, with and without a return code, and reports its size.
    #[test]
    fn frames_round_trip() {
        for retcode in [None, Some(0)] {
            let f = Frame { seq: 7, cmd: cmd::STATUS, retcode, payload: encrypt(KEY, b"{}") };
            let wire = pack(KEY, &f);
            assert_eq!(&wire[..4], &[0, 0, 0x55, 0xAA]);
            assert_eq!(&wire[wire.len() - 4..], &[0, 0, 0xAA, 0x55]);
            assert_eq!(unpack(KEY, &wire).unwrap(), Some((f, wire.len())));
        }
    }

    /// Partial input asks for more bytes instead of failing.
    #[test]
    fn partial_frames_need_more_bytes() {
        let wire = pack(KEY, &Frame { seq: 1, cmd: 9, retcode: None, payload: vec![] });
        assert_eq!(unpack(KEY, &wire[..10]).unwrap(), None);
        assert_eq!(unpack(KEY, &wire[..wire.len() - 1]).unwrap(), None);
    }

    /// A flipped payload bit or the wrong key fails the HMAC; broken framing is a protocol error.
    #[test]
    fn tampering_is_detected() {
        let mut wire = pack(KEY, &Frame { seq: 1, cmd: 8, retcode: Some(0), payload: encrypt(KEY, b"{}") });
        assert!(matches!(unpack(b"fedcba9876543210", &wire), Err(DriverError::BadHmac)));
        wire[21] ^= 1;
        assert!(matches!(unpack(KEY, &wire), Err(DriverError::BadHmac)));
        let mut bad = wire.clone();
        bad[0] = 1;
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
        let mut bad = wire.clone();
        let n = bad.len();
        bad[n - 1] = 0;
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
        let mut bad = wire;
        bad[12..16].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
    }

    /// Control commands carry the `3.4` version header inside the ciphertext; queries and
    /// heartbeats do not. `open` strips it.
    #[test]
    fn version_header_only_on_control_commands() {
        let sealed = seal(KEY, cmd::CONTROL_NEW, b"{\"a\":1}");
        assert!(decrypt(KEY, &sealed).unwrap().starts_with(&VERSION_HEADER));
        assert_eq!(open(KEY, &sealed).unwrap(), b"{\"a\":1}");
        let sealed = seal(KEY, cmd::DP_QUERY_NEW, b"{}");
        assert_eq!(decrypt(KEY, &sealed).unwrap(), b"{}");
        assert_eq!(open(KEY, &sealed).unwrap(), b"{}");
        assert_eq!(open(KEY, &[]).unwrap(), b"");
        assert!(!has_version_header(cmd::HEART_BEAT));
        assert!(has_version_header(cmd::STATUS));
    }

    /// The session key is AES(local_key, nonce XOR nonce), symmetric in the two nonces.
    #[test]
    fn session_key_is_symmetric() {
        let a = *b"aaaaaaaaaaaaaaaa";
        let b = *b"bbbbbbbbbbbbbbbb";
        assert_eq!(session_key(KEY, &a, &b), session_key(KEY, &b, &a));
        assert_eq!(session_key(KEY, &a, &b), encrypt_block(KEY, &[3u8; 16]));
    }

    /// Local keys are 16 ASCII bytes.
    #[test]
    fn key_parsing() {
        assert_eq!(&parse_key("0123456789abcdef").unwrap(), KEY);
        assert!(matches!(parse_key("short"), Err(DriverError::Config(_))));
    }
}
