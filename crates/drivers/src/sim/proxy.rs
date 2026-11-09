//! A TCP pass-through that can cut every connection on demand: how tests make a device "drop"
//! the link without restarting the simulator behind it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A running proxy from `addr` to an upstream address.
pub struct TcpProxy {
    pub addr: SocketAddr,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
    task: JoinHandle<()>,
}

impl TcpProxy {
    pub async fn start(upstream: SocketAddr) -> TcpProxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let c = conns.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut down, _) = listener.accept().await.expect("accept");
                let handle = tokio::spawn(async move {
                    if let Ok(mut up) = TcpStream::connect(upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                    }
                });
                c.lock().unwrap().push(handle);
            }
        });
        TcpProxy { addr, conns, task }
    }

    /// Cut every open connection (both directions see EOF/reset). New connections still work.
    pub fn drop_all(&self) {
        for h in self.conns.lock().unwrap().drain(..) {
            h.abort();
        }
    }

    /// Number of connections accepted and not yet cut.
    pub fn open(&self) -> usize {
        self.conns.lock().unwrap().iter().filter(|h| !h.is_finished()).count()
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        self.drop_all();
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Bytes pass through both ways; `drop_all` cuts the link and a new connection works again.
    #[tokio::test]
    async fn passes_bytes_and_cuts_on_demand() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = echo.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 16];
                    while let Ok(n @ 1..) = s.read(&mut buf).await {
                        let _ = s.write_all(&buf[..n]).await;
                    }
                });
            }
        });
        let proxy = TcpProxy::start(upstream).await;
        let mut c = TcpStream::connect(proxy.addr).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        assert_eq!(proxy.open(), 1);
        proxy.drop_all();
        assert_eq!(c.read(&mut buf).await.unwrap_or(0), 0);
        let mut c2 = TcpStream::connect(proxy.addr).await.unwrap();
        c2.write_all(b"pong").await.unwrap();
        c2.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    /// A dead upstream closes the downstream connection right away.
    #[tokio::test]
    async fn dead_upstream_closes_downstream() {
        let proxy = TcpProxy::start("127.0.0.1:9".parse().unwrap()).await;
        let mut c = TcpStream::connect(proxy.addr).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(c.read(&mut buf).await.unwrap_or(0), 0);
    }
}
