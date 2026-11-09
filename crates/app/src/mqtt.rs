//! A passive MQTT 3.1.1 subscriber: CONNECT, SUBSCRIBE (QoS 0), PINGREQ, and reading
//! PUBLISH packets. It never sends a PUBLISH — by construction there is no code for it — so
//! listening to `SmartEVSE/1234/Set/#` cannot disturb the charger.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use energie_core::time::{TZ, Ts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct MqttMessage {
    pub at: Ts,
    pub topic: String,
    pub payload: String,
}

/// Received messages, drained by the runtime each tick.
#[derive(Debug, Default)]
pub struct Inbox {
    pub messages: VecDeque<MqttMessage>,
    pub connected: bool,
    pub received: u64,
}

pub type SharedInbox = Arc<Mutex<Inbox>>;

const MAX_QUEUED: usize = 1000;

impl Inbox {
    pub fn push(&mut self, m: MqttMessage) {
        if self.messages.len() >= MAX_QUEUED {
            self.messages.pop_front();
        }
        self.received += 1;
        self.messages.push_back(m);
    }

    pub fn drain(&mut self) -> Vec<MqttMessage> {
        self.messages.drain(..).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Subscriber {
    pub addr: String,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic: String,
    pub keepalive: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u16).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    let mut n = body.len();
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

/// The CONNECT packet (clean session, optional username/password).
pub fn connect_packet(s: &Subscriber) -> Vec<u8> {
    let mut flags = 0x02u8;
    if s.username.is_some() {
        flags |= 0x80;
    }
    if s.password.is_some() {
        flags |= 0x40;
    }
    let mut body = Vec::new();
    put_str(&mut body, "MQTT");
    body.push(4);
    body.push(flags);
    body.extend_from_slice(&(s.keepalive.as_secs() as u16).to_be_bytes());
    put_str(&mut body, &s.client_id);
    for v in [&s.username, &s.password].into_iter().flatten() {
        put_str(&mut body, v);
    }
    packet(0x10, &body)
}

/// SUBSCRIBE to one topic filter at QoS 0.
pub fn subscribe_packet(topic: &str) -> Vec<u8> {
    let mut body = vec![0, 1];
    put_str(&mut body, topic);
    body.push(0);
    packet(0x82, &body)
}

pub const PINGREQ: [u8; 2] = [0xC0, 0];

/// Read one packet: (first byte, body).
pub async fn read_packet(r: &mut (dyn tokio::io::AsyncRead + Unpin + Send)) -> Result<(u8, Vec<u8>)> {
    let kind = r.read_u8().await?;
    let mut len = 0usize;
    let mut shift = 0;
    loop {
        let b = r.read_u8().await?;
        len |= ((b & 0x7F) as usize) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return Err(Error::new("mqtt: bad remaining length"));
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok((kind, body))
}

/// Topic and payload of a PUBLISH body.
pub fn parse_publish(kind: u8, body: &[u8]) -> Result<(String, String)> {
    let short = || Error::new("mqtt: short publish");
    let n = u16::from_be_bytes([*body.first().ok_or_else(short)?, *body.get(1).ok_or_else(short)?]) as usize;
    let topic = body.get(2..2 + n).ok_or_else(short)?;
    let qos = (kind >> 1) & 0x03;
    let start = 2 + n + if qos > 0 { 2 } else { 0 };
    let payload = body.get(start..).ok_or_else(short)?;
    Ok((String::from_utf8_lossy(topic).into_owned(), String::from_utf8_lossy(payload).into_owned()))
}

/// Subscribe and collect until `shutdown`; reconnect with backoff.
pub async fn run(s: Subscriber, inbox: SharedInbox, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = s.backoff_min;
    loop {
        let (subscribed, err) = tokio::select! {
            r = session(&s, &inbox) => r,
            _ = shutdown.wait_for(|v| *v) => return,
        };
        inbox.lock().expect("inbox lock").connected = false;
        if subscribed {
            backoff = s.backoff_min;
        }
        tracing::warn!(error = %err, retry_in = ?backoff, "mqtt connection lost");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.wait_for(|v| *v) => return,
        }
        backoff = (backoff * 2).min(s.backoff_max);
    }
}

/// One connection. Returns whether the subscription was acknowledged, and why it ended.
async fn session(s: &Subscriber, inbox: &SharedInbox) -> (bool, Error) {
    let mut tcp = match handshake(s).await {
        Ok(t) => t,
        Err(e) => return (false, e),
    };
    inbox.lock().expect("inbox lock").connected = true;
    tracing::info!(addr = %s.addr, topic = %s.topic, "mqtt subscribed");
    let Err(e) = listen(s, &mut tcp, inbox).await;
    (true, e)
}

async fn handshake(s: &Subscriber) -> Result<TcpStream> {
    let mut tcp = TcpStream::connect(&s.addr).await?;
    tcp.write_all(&connect_packet(s)).await?;
    let (kind, body) = read_packet(&mut tcp).await?;
    if kind != 0x20 || body.get(1) != Some(&0) {
        return Err(Error::new(format!("mqtt: connection refused (code {:?})", body.get(1))));
    }
    tcp.write_all(&subscribe_packet(&s.topic)).await?;
    let (kind, body) = read_packet(&mut tcp).await?;
    if kind != 0x90 || body.last().is_none_or(|rc| *rc >= 0x80) {
        return Err(Error::new("mqtt: subscription refused"));
    }
    Ok(tcp)
}

async fn listen(s: &Subscriber, tcp: &mut TcpStream, inbox: &SharedInbox) -> Result<std::convert::Infallible> {
    let every = (s.keepalive / 2).max(Duration::from_millis(10));
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tokio::select! {
            p = read_packet(tcp) => {
                let (kind, body) = p?;
                if kind >> 4 == 3 {
                    let (topic, payload) = parse_publish(kind, &body)?;
                    let at = chrono::Utc::now().with_timezone(&TZ);
                    inbox.lock().expect("inbox lock").push(MqttMessage { at, topic, payload });
                }
            }
            _ = ping.tick() => tcp.write_all(&PINGREQ).await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn sub(addr: String) -> Subscriber {
        Subscriber {
            addr,
            client_id: "energie-test".into(),
            username: Some("u".into()),
            password: Some("p".into()),
            topic: "SmartEVSE/1234/Set/#".into(),
            keepalive: Duration::from_millis(100),
            backoff_min: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
        }
    }

    fn publish(topic: &str, payload: &str) -> Vec<u8> {
        let mut body = Vec::new();
        put_str(&mut body, topic);
        body.extend_from_slice(payload.as_bytes());
        packet(0x30, &body)
    }

    /// The packets follow MQTT 3.1.1 byte for byte.
    #[test]
    fn packet_encoding() {
        let s = sub("x".into());
        let c = connect_packet(&s);
        assert_eq!(c[0], 0x10);
        assert_eq!(&c[2..8], b"\0\x04MQTT");
        assert_eq!(c[8], 4);
        assert_eq!(c[9], 0xC2);
        let anon = Subscriber { username: None, password: None, ..s };
        assert_eq!(connect_packet(&anon)[9], 0x02);
        assert_eq!(subscribe_packet("a/#"), vec![0x82, 8, 0, 1, 0, 3, b'a', b'/', b'#', 0]);
        // Remaining length over 127 uses two bytes.
        let big = packet(0x30, &[0u8; 200]);
        assert_eq!(&big[..3], &[0x30, 0xC8, 0x01]);
    }

    /// PUBLISH bodies parse at QoS 0 and 1; truncated ones are rejected.
    #[test]
    fn publish_parsing() {
        let p = publish("SmartEVSE/1234/Set/CurrentOverride", "145");
        assert_eq!(
            parse_publish(p[0], &p[2..]).unwrap(),
            ("SmartEVSE/1234/Set/CurrentOverride".to_string(), "145".to_string())
        );
        let mut q1 = Vec::new();
        put_str(&mut q1, "t");
        q1.extend_from_slice(&[0, 7]);
        q1.extend_from_slice(b"x");
        assert_eq!(parse_publish(0x32, &q1).unwrap().1, "x");
        assert!(parse_publish(0x30, &[0]).is_err());
        assert!(parse_publish(0x30, &[0, 5, b'a']).is_err());
        assert!(parse_publish(0x32, &[0, 1, b'a']).is_err());
    }

    /// A malformed remaining-length field is an error, not a hang.
    #[tokio::test]
    async fn bad_remaining_length() {
        let mut r: &[u8] = &[0x30, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert!(read_packet(&mut r).await.is_err());
    }

    /// The inbox is bounded and drains in order.
    #[test]
    fn inbox_is_bounded() {
        let mut i = Inbox::default();
        let at = crate::plug::tests::t0();
        for n in 0..(MAX_QUEUED + 3) {
            i.push(MqttMessage { at, topic: "t".into(), payload: n.to_string() });
        }
        let d = i.drain();
        assert_eq!(d.len(), MAX_QUEUED);
        assert_eq!(d[0].payload, "3");
        assert_eq!(i.received as usize, MAX_QUEUED + 3);
    }

    /// Against a fake broker: connect, subscribe, receive messages, ping, and reconnect after
    /// the broker hangs up; a refused connection is retried.
    #[tokio::test]
    async fn subscribes_receives_pings_and_reconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let pings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p2 = pings.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            for n in 1..=3 {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let (kind, _) = read_packet(&mut tcp).await.unwrap();
                assert_eq!(kind, 0x10);
                if n == 1 {
                    // Bad credentials.
                    tcp.write_all(&[0x20, 2, 0, 5]).await.unwrap();
                    continue;
                }
                tcp.write_all(&[0x20, 2, 0, 0]).await.unwrap();
                let (kind, body) = read_packet(&mut tcp).await.unwrap();
                assert_eq!(kind, 0x82);
                assert!(String::from_utf8_lossy(&body).contains("SmartEVSE/1234/Set/#"));
                tcp.write_all(&[0x90, 3, 0, 1, 0]).await.unwrap();
                tcp.write_all(&publish("SmartEVSE/1234/Set/CurrentOverride", &format!("{}", 140 + n))).await.unwrap();
                tcp.write_all(&[0xD0, 0]).await.unwrap();
                if n == 2 {
                    continue; // hang up
                }
                // Only pings come from a subscriber that never publishes.
                while let Ok((k, _)) = read_packet(&mut tcp).await {
                    assert_eq!(k, 0xC0);
                    p2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tcp.write_all(&[0xD0, 0]).await.unwrap();
                }
            }
            done_tx.send(()).unwrap();
        });
        let inbox: SharedInbox = Arc::default();
        let (stop, rx) = watch::channel(false);
        let h = tokio::spawn(run(sub(addr), inbox.clone(), rx));
        crate::ha::ws::tests::until(|| inbox.lock().unwrap().received >= 2).await;
        crate::ha::ws::tests::until(|| pings.load(std::sync::atomic::Ordering::SeqCst) >= 2).await;
        let msgs = inbox.lock().unwrap().drain();
        assert_eq!(msgs[0].payload, "142");
        assert_eq!(msgs[1].payload, "143");
        assert!(inbox.lock().unwrap().connected);
        stop.send(true).unwrap();
        h.await.unwrap();
        done_rx.await.unwrap();
    }

    /// Stopping during the reconnect backoff ends the subscriber at once.
    #[tokio::test]
    async fn stop_during_backoff_returns_promptly() {
        let s = Subscriber { backoff_min: Duration::from_secs(30), ..sub("127.0.0.1:1".into()) };
        let (stop, rx) = watch::channel(false);
        let h = tokio::spawn(run(s, Arc::default(), rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap();
    }

    /// A broker that refuses the subscription, or is not there, ends the session.
    #[tokio::test]
    async fn refused_subscription_and_unreachable_broker() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            read_packet(&mut tcp).await.unwrap();
            tcp.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            read_packet(&mut tcp).await.unwrap();
            tcp.write_all(&[0x90, 3, 0, 1, 0x80]).await.unwrap();
        });
        let inbox: SharedInbox = Arc::default();
        let (ok, e) = session(&sub(addr), &inbox).await;
        assert!(!ok);
        assert!(e.0.contains("subscription refused"));
        let (ok, _) = session(&sub("127.0.0.1:1".into()), &inbox).await;
        assert!(!ok);
    }
}
