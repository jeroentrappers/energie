//! A negotiated 3.4 session, split into a reading and a writing half so a worker can wait for
//! device frames while it sends.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use super::protocol::{self, Frame, Key, cmd};
use crate::error::{DriverError, Result};

/// Reads frames from the device.
pub struct Reader<S> {
    rd: ReadHalf<S>,
    key: Key,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite> Reader<S> {
    /// Next complete frame. Cancel-safe: bytes read so far stay buffered.
    pub async fn recv(&mut self) -> Result<Frame> {
        loop {
            if let Some((frame, used)) = protocol::unpack(&self.key, &self.buf)? {
                self.buf.drain(..used);
                return Ok(frame);
            }
            if self.rd.read_buf(&mut self.buf).await? == 0 {
                return Err(DriverError::NotConnected);
            }
        }
    }
}

/// Sends frames to the device.
pub struct Writer<S> {
    wr: WriteHalf<S>,
    key: Key,
    seq: u32,
}

impl<S: AsyncRead + AsyncWrite> Writer<S> {
    /// Encrypt `plain` for `command` and send it with the next sequence number.
    pub async fn send(&mut self, command: u32, plain: &[u8]) -> Result<()> {
        self.seq += 1;
        let frame =
            Frame { seq: self.seq, cmd: command, retcode: None, payload: protocol::seal(&self.key, command, plain) };
        self.wr.write_all(&protocol::pack(&self.key, &frame)).await?;
        Ok(())
    }

    /// Decrypt a frame's payload with the session key (version header removed).
    pub fn open(&self, frame: &Frame) -> Result<Vec<u8>> {
        protocol::open(&self.key, &frame.payload)
    }
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S, key: &Key) -> Result<Frame> {
    let mut buf = Vec::new();
    loop {
        if let Some((frame, _)) = protocol::unpack(key, &buf)? {
            return Ok(frame);
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Err(DriverError::NotConnected);
        }
    }
}

/// Run the client side of the session-key handshake on `stream` and split it.
pub async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    local_key: &Key,
    client_nonce: Key,
) -> Result<(Reader<S>, Writer<S>)> {
    let start = Frame {
        seq: 1,
        cmd: cmd::SESS_KEY_NEG_START,
        retcode: None,
        payload: protocol::encrypt(local_key, &client_nonce),
    };
    stream.write_all(&protocol::pack(local_key, &start)).await?;
    let resp = read_frame(&mut stream, local_key).await?;
    if resp.cmd != cmd::SESS_KEY_NEG_RESP {
        return Err(DriverError::Tuya(format!("expected key negotiation response, got command {}", resp.cmd)));
    }
    let plain = protocol::decrypt(local_key, &resp.payload)?;
    if plain.len() < 48 || plain[16..48] != protocol::hmac(local_key, &client_nonce) {
        return Err(DriverError::Tuya("device failed the key negotiation (wrong local key?)".into()));
    }
    let device_nonce: Key = plain[..16].try_into().expect("16 bytes");
    let finish = Frame {
        seq: 2,
        cmd: cmd::SESS_KEY_NEG_FINISH,
        retcode: None,
        payload: protocol::encrypt(local_key, &protocol::hmac(local_key, &device_nonce)),
    };
    stream.write_all(&protocol::pack(local_key, &finish)).await?;
    let key = protocol::session_key(local_key, &client_nonce, &device_nonce);
    let (rd, wr) = tokio::io::split(stream);
    Ok((Reader { rd, key, buf: Vec::new() }, Writer { wr, key, seq: 2 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, duplex};

    const KEY: &Key = b"0123456789abcdef";
    const NONCE: Key = *b"cccccccccccccccc";

    /// A fake device that reads the start frame and answers with `reply`, then hangs up.
    async fn device_answering(reply: Option<Frame>) -> DuplexStream {
        let (client, mut dev) = duplex(4096);
        tokio::spawn(async move {
            let _ = read_frame(&mut dev, KEY).await;
            if let Some(f) = reply {
                dev.write_all(&protocol::pack(KEY, &f)).await.unwrap();
            }
        });
        client
    }

    fn resp(cmd: u32, plain: &[u8]) -> Frame {
        Frame { seq: 1, cmd, retcode: Some(0), payload: protocol::encrypt(KEY, plain) }
    }

    /// The device must answer the start with `SESS_KEY_NEG_RESP`.
    #[tokio::test]
    async fn negotiation_needs_the_response_command() {
        let s = device_answering(Some(resp(cmd::STATUS, b"{}"))).await;
        assert!(matches!(negotiate(s, KEY, NONCE).await, Err(DriverError::Tuya(_))));
    }

    /// The device proves it knows the local key by HMAC-ing our nonce; a wrong proof (or a
    /// short answer) aborts the handshake.
    #[tokio::test]
    async fn negotiation_checks_device_proof() {
        let wrong = [&[0u8; 16][..], &protocol::hmac(KEY, b"other nonce.....")[..]].concat();
        let s = device_answering(Some(resp(cmd::SESS_KEY_NEG_RESP, &wrong))).await;
        assert!(matches!(negotiate(s, KEY, NONCE).await, Err(DriverError::Tuya(_))));
        let s = device_answering(Some(resp(cmd::SESS_KEY_NEG_RESP, b"short"))).await;
        assert!(matches!(negotiate(s, KEY, NONCE).await, Err(DriverError::Tuya(_))));
    }

    /// A device that hangs up mid-handshake is `NotConnected`; one whose frame fails the HMAC
    /// is `BadHmac`.
    #[tokio::test]
    async fn negotiation_eof_and_bad_hmac() {
        let s = device_answering(None).await;
        assert!(matches!(negotiate(s, KEY, NONCE).await, Err(DriverError::NotConnected)));
        let (client, mut dev) = duplex(4096);
        tokio::spawn(async move {
            let _ = read_frame(&mut dev, KEY).await;
            let f = resp(cmd::SESS_KEY_NEG_RESP, b"x");
            dev.write_all(&protocol::pack(b"fedcba9876543210", &f)).await.unwrap();
        });
        assert!(matches!(negotiate(client, KEY, NONCE).await, Err(DriverError::BadHmac)));
    }

    /// After a good handshake a hang-up shows as `NotConnected` on the reader.
    #[tokio::test]
    async fn reader_reports_eof() {
        let (client, mut dev) = duplex(4096);
        tokio::spawn(async move {
            let start = read_frame(&mut dev, KEY).await.unwrap();
            let nonce: Key = protocol::decrypt(KEY, &start.payload).unwrap().try_into().unwrap();
            let body = [&[7u8; 16][..], &protocol::hmac(KEY, &nonce)[..]].concat();
            dev.write_all(&protocol::pack(KEY, &resp(cmd::SESS_KEY_NEG_RESP, &body))).await.unwrap();
            let _ = read_frame(&mut dev, KEY).await;
        });
        let (mut rd, _wr) = negotiate(client, KEY, NONCE).await.unwrap();
        assert!(matches!(rd.recv().await, Err(DriverError::NotConnected)));
    }
}
