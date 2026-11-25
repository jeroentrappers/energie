//! The systemd service protocol (`sd_notify`) without libsystemd: `READY=1` once the web port
//! listens, then `WATCHDOG=1` while the service is healthy.
//!
//! Healthy is what a user sees: the service's own `/healthz`, asked over TCP on the web port,
//! answers `200` in time. That request goes through the accept loop, the router and the tick
//! check, so a stalled accept loop, a starved runtime and a control loop that stopped ticking
//! all stop the pings, and systemd restarts the service after `WatchdogSec`. During the start
//! (the first tick can wait for the devices' first readings) any answer counts, for at most
//! the grace period the caller gives. Without
//! `NOTIFY_SOCKET` (not under systemd, or `Type=exec`) nothing is sent; without `WATCHDOG_USEC`
//! only `READY=1`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// Where to send notifications, from `NOTIFY_SOCKET`: a path, or `@name` for the abstract
/// namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notifier {
    socket: String,
}

impl Notifier {
    pub fn from_env() -> Option<Notifier> {
        std::env::var("NOTIFY_SOCKET").ok().filter(|s| !s.is_empty()).map(|socket| Notifier { socket })
    }

    pub fn new(socket: impl Into<String>) -> Notifier {
        Notifier { socket: socket.into() }
    }

    /// Send one datagram; a failure is logged once by the caller and otherwise ignored.
    pub fn notify(&self, msg: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixDatagram;
            let sock = UnixDatagram::unbound()?;
            if let Some(name) = self.socket.strip_prefix('@') {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    use std::os::linux::net::SocketAddrExt;
                    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
                    sock.send_to_addr(msg.as_bytes(), &addr)?;
                    return Ok(());
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                {
                    let _ = name;
                    return Err(std::io::Error::other("abstract sockets need Linux"));
                }
            }
            sock.send_to(msg.as_bytes(), &self.socket)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = msg;
            Err(std::io::Error::other("sd_notify needs a Unix socket"))
        }
    }
}

/// The watchdog period from `WATCHDOG_USEC` (when `WATCHDOG_PID` is unset or ours).
pub fn watchdog_from_env() -> Option<Duration> {
    if let Ok(pid) = std::env::var("WATCHDOG_PID")
        && pid.trim() != std::process::id().to_string()
    {
        return None;
    }
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.trim().parse().ok()?;
    (usec > 0).then(|| Duration::from_micros(usec))
}

/// The address to probe for a listener on `addr`: a wildcard listener is asked on loopback.
pub fn probe_addr(addr: SocketAddr) -> SocketAddr {
    let ip = match addr {
        SocketAddr::V4(a) if a.ip().is_unspecified() => std::net::Ipv4Addr::LOCALHOST.into(),
        SocketAddr::V6(a) if a.ip().is_unspecified() => std::net::Ipv6Addr::LOCALHOST.into(),
        a => a.ip(),
    };
    SocketAddr::new(ip, addr.port())
}

/// What `GET /healthz` on the web port gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// `200`: ticking.
    Ok,
    /// Another status: the server answers, the control loop is starting or stale.
    Answered,
    /// No answer in time.
    Silent,
}

/// Ask `/healthz` on `addr`, waiting at most `timeout`.
pub async fn probe(addr: SocketAddr, timeout: Duration) -> Probe {
    let ask = async {
        let mut s = tokio::net::TcpStream::connect(addr).await?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n").await?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head).await?;
        std::io::Result::Ok(head)
    };
    match tokio::time::timeout(timeout, ask).await {
        Ok(Ok(head)) if head.starts_with(b"HTTP/1.") && &head[8..] == b" 200" => Probe::Ok,
        Ok(Ok(head)) if head.starts_with(b"HTTP/1.") => Probe::Answered,
        _ => Probe::Silent,
    }
}

/// `GET /healthz` on `addr` answered `200` within `timeout`.
pub async fn healthy(addr: SocketAddr, timeout: Duration) -> bool {
    probe(addr, timeout).await == Probe::Ok
}

/// `READY=1`, then a `WATCHDOG=1` every third of `period` while `/healthz` on `addr` answers
/// `200` (in the first `grace`, while it answers at all). Ends at `stop` with `STOPPING=1`.
pub async fn run(
    n: Notifier,
    period: Option<Duration>,
    grace: Duration,
    addr: SocketAddr,
    mut stop: watch::Receiver<bool>,
) {
    let started = tokio::time::Instant::now();
    if let Err(e) = n.notify("READY=1") {
        tracing::warn!(error = %e, "systemd notify failed");
    }
    let Some(period) = period else {
        let _ = stop.wait_for(|s| *s).await;
        let _ = n.notify("STOPPING=1");
        return;
    };
    let every = (period / 3).max(Duration::from_millis(500));
    let addr = probe_addr(addr);
    tracing::info!(period_s = period.as_secs_f64(), "systemd watchdog on");
    let mut iv = tokio::time::interval(every);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    loop {
        tokio::select! {
            _ = iv.tick() => {}
            _ = stop.wait_for(|s| *s) => break,
        }
        let p = probe(addr, every).await;
        if p == Probe::Ok || (p == Probe::Answered && started.elapsed() < grace) {
            if failing {
                tracing::info!("healthy again; watchdog pings resume");
                failing = false;
            }
            let _ = n.notify("WATCHDOG=1");
        } else if !failing {
            tracing::warn!("/healthz does not answer 200; withholding the watchdog ping");
            failing = true;
        }
    }
    let _ = n.notify("STOPPING=1");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    fn listen(dir: &std::path::Path) -> (UnixDatagram, String) {
        let path = dir.join("notify");
        let sock = UnixDatagram::bind(&path).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (sock, path.display().to_string())
    }

    fn recv(sock: &UnixDatagram) -> String {
        let mut buf = [0u8; 256];
        let n = sock.recv(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    /// A tiny HTTP server answering every request with `status`, switchable.
    async fn server(ok: Arc<std::sync::atomic::AtomicBool>) -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let ok = ok.load(std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    let _ = s.read(&mut buf).await;
                    let reply: &[u8] = if ok { b"HTTP/1.1 200 OK\r\n\r\n" } else { b"HTTP/1.1 503 X\r\n\r\n" };
                    let _ = s.write_all(reply).await;
                });
            }
        });
        addr
    }

    use std::sync::Arc;

    #[test]
    fn notifies_a_path_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, path) = listen(dir.path());
        Notifier::new(path).notify("READY=1").unwrap();
        assert_eq!(recv(&sock), "READY=1");
    }

    #[test]
    fn a_wildcard_listener_is_probed_on_loopback() {
        assert_eq!(probe_addr("0.0.0.0:8080".parse().unwrap()), "127.0.0.1:8080".parse().unwrap());
        assert_eq!(probe_addr("[::]:8080".parse().unwrap()), "[::1]:8080".parse().unwrap());
        assert_eq!(probe_addr("192.168.1.5:80".parse().unwrap()), "192.168.1.5:80".parse().unwrap());
    }

    /// Only a 200 is healthy; a refused port, a 503 and a listener that never answers are not.
    #[tokio::test]
    async fn healthy_needs_a_timely_200() {
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let addr = server(ok.clone()).await;
        assert!(healthy(addr, Duration::from_secs(2)).await);
        ok.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!healthy(addr, Duration::from_secs(2)).await);
        // Bound but never accepted: the backlog takes the connection, nobody answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(!healthy(silent.local_addr().unwrap(), Duration::from_millis(300)).await);
        drop(silent);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(!healthy(closed, Duration::from_millis(300)).await);
    }

    /// READY first, pings while healthy, none while not, STOPPING at the end.
    #[tokio::test]
    async fn pings_only_while_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, path) = listen(dir.path());
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let addr = server(ok.clone()).await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let h =
            tokio::spawn(run(Notifier::new(path), Some(Duration::from_millis(1500)), Duration::ZERO, addr, stop_rx));
        let sock = Arc::new(sock);
        let rx = |s: Arc<UnixDatagram>| tokio::task::spawn_blocking(move || recv(&s));
        assert_eq!(rx(sock.clone()).await.unwrap(), "READY=1");
        assert_eq!(rx(sock.clone()).await.unwrap(), "WATCHDOG=1");
        ok.store(false, std::sync::atomic::Ordering::SeqCst);
        // Let a ping that was already under way land, then nothing for two periods.
        tokio::time::sleep(Duration::from_millis(700)).await;
        sock.set_nonblocking(true).unwrap();
        while sock.recv(&mut [0u8; 64]).is_ok() {}
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(sock.recv(&mut [0u8; 64]).is_err(), "no ping while unhealthy");
        sock.set_nonblocking(false).unwrap();
        ok.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(rx(sock.clone()).await.unwrap(), "WATCHDOG=1");
        stop_tx.send(true).unwrap();
        h.await.unwrap();
        let mut last = String::new();
        sock.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 64];
        while let Ok(n) = sock.recv(&mut buf) {
            last = String::from_utf8_lossy(&buf[..n]).into_owned();
        }
        assert_eq!(last, "STOPPING=1");
    }

    /// While starting, a 503 still pings, until the grace period is over.
    #[tokio::test]
    async fn a_start_gets_a_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, path) = listen(dir.path());
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let addr = server(ok.clone()).await;
        assert_eq!(probe(addr, Duration::from_secs(2)).await, Probe::Answered);
        let (stop_tx, stop_rx) = watch::channel(false);
        let h = tokio::spawn(run(
            Notifier::new(path),
            Some(Duration::from_millis(900)),
            Duration::from_millis(1200),
            addr,
            stop_rx,
        ));
        let sock = Arc::new(sock);
        let rx = |s: Arc<UnixDatagram>| tokio::task::spawn_blocking(move || recv(&s));
        assert_eq!(rx(sock.clone()).await.unwrap(), "READY=1");
        assert_eq!(rx(sock.clone()).await.unwrap(), "WATCHDOG=1", "a 503 pings in the grace period");
        tokio::time::sleep(Duration::from_millis(1300)).await;
        sock.set_nonblocking(true).unwrap();
        while sock.recv(&mut [0u8; 64]).is_ok() {}
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(sock.recv(&mut [0u8; 64]).is_err(), "no ping after the grace period");
        stop_tx.send(true).unwrap();
        h.await.unwrap();
    }

    /// `WATCHDOG_PID` naming another process means the watchdog is not ours.
    #[test]
    fn watchdog_env() {
        // SAFETY: this test is the only one touching these variables.
        unsafe {
            std::env::set_var("WATCHDOG_USEC", "30000000");
            std::env::remove_var("WATCHDOG_PID");
        }
        assert_eq!(watchdog_from_env(), Some(Duration::from_secs(30)));
        unsafe { std::env::set_var("WATCHDOG_PID", "1") };
        assert_eq!(watchdog_from_env(), None);
        unsafe { std::env::set_var("WATCHDOG_PID", std::process::id().to_string()) };
        assert_eq!(watchdog_from_env(), Some(Duration::from_secs(30)));
        unsafe {
            std::env::remove_var("WATCHDOG_USEC");
            std::env::remove_var("WATCHDOG_PID");
        }
        assert_eq!(watchdog_from_env(), None);
    }
}
