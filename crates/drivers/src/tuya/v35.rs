//! Tuya local protocol 3.5: the `6699` frame with AES-128-GCM.
//!
//! Frame (all integers big-endian):
//!
//! ```text
//! 00006699 | 0000 u16 | seq u32 | cmd u32 | len u32 | IV (12) | ciphertext | GCM tag (16) | 00009966
//! ```
//!
//! `len` counts IV, ciphertext and tag. The 14 header bytes after the prefix are the GCM
//! additional data, so the tag covers the header as well as the payload. A sender picks a fresh
//! random IV per frame. Frames from the device carry a 4-byte return code at the start of the
//! *plaintext* (3.4 puts it before the ciphertext). Control commands carry the version header
//! (`"3.5"` + 12 zero bytes) at the start of the plaintext, as in 3.4; queries, heartbeats and
//! the handshake do not.
//!
//! The handshake is the 3.4 one (nonce, `HMAC(local_key, nonce)` both ways) with every frame in
//! this format under the local key. The session key is the GCM encryption of `client_nonce XOR
//! device_nonce` under the local key with the first 12 bytes of the client nonce as IV, tag
//! dropped (as tinytuya does).

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};

use super::protocol::{Frame, Key, MAX_FRAME};
use crate::error::{DriverError, Result};

pub const PREFIX: u32 = 0x0000_6699;
pub const SUFFIX: u32 = 0x0000_9966;
/// Prefix, the reserved u16, seq, cmd and len.
const HEADER: usize = 18;
const IV: usize = 12;
const TAG: usize = 16;

/// `"3.5"` followed by 12 zero bytes.
pub const VERSION_HEADER: [u8; 15] = *b"3.5\0\0\0\0\0\0\0\0\0\0\0\0";

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

/// Serialize a frame: `retcode` (when set) and `payload` are the plaintext, sealed under `key`
/// with `iv`.
pub fn pack(key: &Key, f: &Frame, iv: [u8; IV]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(4 + f.payload.len());
    if let Some(rc) = f.retcode {
        plain.extend_from_slice(&rc.to_be_bytes());
    }
    plain.extend_from_slice(&f.payload);
    let len = (IV + plain.len() + TAG) as u32;
    let mut buf = Vec::with_capacity(HEADER + len as usize + 4);
    buf.extend_from_slice(&PREFIX.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    for w in [f.seq, f.cmd, len] {
        buf.extend_from_slice(&w.to_be_bytes());
    }
    let sealed = Aes128Gcm::new(key.into())
        .encrypt(Nonce::from_slice(&iv), Payload { msg: &plain, aad: &buf[4..HEADER] })
        .expect("GCM encryption of a bounded buffer");
    buf.extend_from_slice(&iv);
    buf.extend_from_slice(&sealed);
    buf.extend_from_slice(&SUFFIX.to_be_bytes());
    buf
}

/// Parse and decrypt one frame from the front of `buf`. `Ok(None)` means more bytes are needed;
/// on success the frame (its whole plaintext in `payload`, `retcode` unset: see
/// [`split_retcode`]) and the number of bytes it used are returned. A failed GCM tag (wrong key
/// or a corrupted frame) is [`DriverError::BadHmac`], as the 3.4 HMAC.
pub fn unpack(key: &Key, buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < HEADER {
        return Ok(None);
    }
    if word(buf, 0) != PREFIX {
        return Err(DriverError::Tuya("bad frame prefix".into()));
    }
    let len = word(buf, 14) as usize;
    if !(IV + TAG..=MAX_FRAME).contains(&len) {
        return Err(DriverError::Tuya(format!("bad frame length {len}")));
    }
    let total = HEADER + len + 4;
    if buf.len() < total {
        return Ok(None);
    }
    if word(buf, total - 4) != SUFFIX {
        return Err(DriverError::Tuya("bad frame suffix".into()));
    }
    let iv = &buf[HEADER..HEADER + IV];
    let plain = Aes128Gcm::new(key.into())
        .decrypt(Nonce::from_slice(iv), Payload { msg: &buf[HEADER + IV..total - 4], aad: &buf[4..HEADER] })
        .map_err(|_| DriverError::BadHmac)?;
    Ok(Some((Frame { seq: word(buf, 6), cmd: word(buf, 10), retcode: None, payload: plain }, total)))
}

/// A frame from the device: the first four plaintext bytes are its return code.
pub fn split_retcode(mut f: Frame) -> Frame {
    if f.payload.len() >= 4 {
        f.retcode = Some(word(&f.payload, 0));
        f.payload.drain(..4);
    }
    f
}

/// The plaintext with a leading version header removed.
pub fn strip_version(plain: &[u8]) -> &[u8] {
    if plain.len() >= VERSION_HEADER.len() && plain.starts_with(b"3.5") {
        &plain[VERSION_HEADER.len()..]
    } else {
        plain
    }
}

/// The 3.5 session key (see the module docs).
pub fn session_key(local_key: &Key, client_nonce: &Key, device_nonce: &Key) -> Key {
    let mut x = [0u8; 16];
    for (i, b) in x.iter_mut().enumerate() {
        *b = client_nonce[i] ^ device_nonce[i];
    }
    let sealed = Aes128Gcm::new(local_key.into())
        .encrypt(Nonce::from_slice(&client_nonce[..IV]), &x[..])
        .expect("GCM encryption of 16 bytes");
    sealed[..16].try_into().expect("16 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuya::protocol::cmd;

    const KEY: &Key = b"0123456789abcdef";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Known answers from tinytuya 1.21 (`pack_message` with a fixed IV): a control frame as
    /// we send it, a query answer as the device sends it, and the session key.
    #[test]
    fn matches_tinytuya() {
        let control = Frame {
            seq: 7,
            cmd: cmd::CONTROL_NEW,
            retcode: None,
            payload: [&VERSION_HEADER[..], br#"{"protocol":5}"#].concat(),
        };
        assert_eq!(
            pack(KEY, &control, *b"abcdefghijkl"),
            hex(
                "000066990000000000070000000d000000396162636465666768696a6b6cd6fc49b553a559a55930b43af63edf7228659e884c94503de8def74d2f9bc39dd8ac81b4b5bef81ecce400dcb500009966"
            )
        );
        let wire = hex(
            "00006699000000000003000000100000002f4142434445464748494a4b4cb3f7da4f077e0f8f9b307455bf0a095259094b26139f2bc6008dc257517659c3c4023a00009966",
        );
        let (f, used) = unpack(KEY, &wire).unwrap().unwrap();
        assert_eq!(used, wire.len());
        let f = split_retcode(f);
        assert_eq!((f.seq, f.cmd, f.retcode), (3, cmd::DP_QUERY_NEW, Some(0)));
        assert_eq!(f.payload, br#"{"dps":{"1":2}}"#);
        assert_eq!(
            session_key(KEY, b"cccccccccccccccc", b"dddddddddddddddd").to_vec(),
            hex("b8e8238f52bbf334fda8305d5b734e76")
        );
    }

    /// A packed frame unpacks to its plaintext, with the return code where the device puts it.
    #[test]
    fn frames_round_trip() {
        let f = Frame { seq: 9, cmd: cmd::STATUS, retcode: Some(1), payload: b"{}".to_vec() };
        let wire = pack(KEY, &f, [7; 12]);
        let (back, used) = unpack(KEY, &wire).unwrap().unwrap();
        assert_eq!(used, wire.len());
        assert_eq!(split_retcode(back), f);
        let bare = Frame { seq: 1, cmd: cmd::HEART_BEAT, retcode: None, payload: vec![] };
        let (back, _) = unpack(KEY, &pack(KEY, &bare, [0; 12])).unwrap().unwrap();
        assert_eq!(back, bare);
        assert_eq!(split_retcode(back).retcode, None, "nothing to split");
    }

    /// Partial input asks for more bytes; two frames in one buffer are read one by one.
    #[test]
    fn partial_and_back_to_back_frames() {
        let a = pack(KEY, &Frame { seq: 1, cmd: 9, retcode: None, payload: vec![] }, [1; 12]);
        let b = pack(KEY, &Frame { seq: 2, cmd: 8, retcode: None, payload: b"x".to_vec() }, [2; 12]);
        assert_eq!(unpack(KEY, &a[..10]).unwrap(), None);
        assert_eq!(unpack(KEY, &a[..a.len() - 1]).unwrap(), None);
        let both = [a.clone(), b].concat();
        let (f, used) = unpack(KEY, &both).unwrap().unwrap();
        assert_eq!((f.seq, used), (1, a.len()));
        assert_eq!(unpack(KEY, &both[used..]).unwrap().unwrap().0.seq, 2);
    }

    /// The tag covers payload and header: a flipped bit in either, or the wrong key, fails it;
    /// broken framing is a protocol error.
    #[test]
    fn tampering_is_detected() {
        let wire = pack(KEY, &Frame { seq: 1, cmd: 8, retcode: None, payload: b"{}".to_vec() }, [3; 12]);
        assert!(matches!(unpack(b"fedcba9876543210", &wire), Err(DriverError::BadHmac)));
        for at in [8, 11, 20, 32] {
            let mut bad = wire.clone();
            bad[at] ^= 1;
            assert!(matches!(unpack(KEY, &bad), Err(DriverError::BadHmac)), "byte {at}");
        }
        let mut bad = wire.clone();
        bad[0] = 1;
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
        let mut bad = wire.clone();
        let n = bad.len();
        bad[n - 1] = 0;
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
        let mut bad = wire;
        bad[14..18].copy_from_slice(&3u32.to_be_bytes());
        assert!(matches!(unpack(KEY, &bad), Err(DriverError::Tuya(_))));
    }

    /// Only a leading `3.5` header is stripped.
    #[test]
    fn version_header_is_stripped() {
        assert_eq!(strip_version(&[&VERSION_HEADER[..], b"{}"].concat()), b"{}");
        assert_eq!(strip_version(b"{}"), b"{}");
        assert_eq!(strip_version(b"3.5"), b"3.5");
    }
}
