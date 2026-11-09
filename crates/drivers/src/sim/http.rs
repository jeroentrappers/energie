//! Serving an axum router on `127.0.0.1:0`, plain or over TLS with a throwaway CA.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// A test CA and a server certificate it issued for one DNS name.
pub struct TestPki {
    /// PEM of the CA: what the client trusts.
    pub ca_pem: String,
    cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl TestPki {
    /// A fresh CA and a leaf certificate for `server_name`.
    pub fn new(server_name: &str) -> TestPki {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(DnType::CommonName, "energie simulator CA");
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().expect("key")).expect("ca");
        let leaf_key = KeyPair::generate().expect("key");
        let leaf = CertificateParams::new(vec![server_name.to_string()])
            .expect("params")
            .signed_by(&leaf_key, &ca)
            .expect("leaf");
        TestPki { ca_pem: ca.pem(), cert: leaf.der().clone(), key: leaf_key.serialize_der() }
    }

    fn acceptor(&self) -> TlsAcceptor {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], key)
            .expect("cert");
        TlsAcceptor::from(Arc::new(config))
    }
}

/// A running HTTP(S) server; aborted when dropped.
pub struct HttpServer {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn bind() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// Serve `router` over plain HTTP.
pub async fn serve(router: Router) -> HttpServer {
    let (listener, addr) = bind().await;
    let task = tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    HttpServer { addr, task }
}

/// Serve `router` over HTTPS with the certificate of `pki`.
pub async fn serve_tls(router: Router, pki: &TestPki) -> HttpServer {
    let (listener, addr) = bind().await;
    let acceptor = pki.acceptor();
    let task = tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.expect("accept");
            tokio::spawn(serve_conn(acceptor.clone(), tcp, router.clone()));
        }
    });
    HttpServer { addr, task }
}

async fn serve_conn(acceptor: TlsAcceptor, tcp: tokio::net::TcpStream, router: Router) -> Option<()> {
    let tls = acceptor.accept(tcp).await.ok()?;
    let svc = TowerToHyperService::new(router);
    hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await.ok()
}
