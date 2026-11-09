//! HTTP plumbing shared by the clients: one rustls `reqwest` client per service, optional
//! cookie jar (the browser-style logins of VW identity and Fluvius B2C need one), and URL
//! helpers.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use reqwest::cookie::Jar;
use reqwest::redirect::Policy;

use crate::error::{CloudError, Result};

/// Default request timeout (HA's Kia and Fluvius clients use 30 s).
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// A client with the given timeout. `jar` shares cookies between clients (a redirecting and a
/// non-redirecting one); `follow` turns redirect following on or off.
pub fn client(timeout: Duration, jar: Option<Arc<Jar>>, follow: bool) -> Result<reqwest::Client> {
    // Send `Accept: */*` like aiohttp, which the reference clients use.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::ACCEPT, reqwest::header::HeaderValue::from_static("*/*"));
    let mut b = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(timeout)
        .default_headers(headers)
        .redirect(if follow { Policy::limited(10) } else { Policy::none() });
    if let Some(jar) = jar {
        b = b.cookie_provider(jar);
    }
    Ok(b.build()?)
}

/// Parse a base URL from config.
pub fn parse_url(s: &str) -> Result<Url> {
    Url::parse(s).map_err(|e| CloudError::Config(format!("bad url {s:?}: {e}")))
}

/// `scheme://host[:port]` of a URL.
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The first value of query parameter `key`.
pub fn query_param(url: &Url, key: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// At most `n` characters of a response body, for error messages.
pub fn snippet(body: &str, n: usize) -> String {
    body.chars().take(n).collect()
}

/// Random URL-safe base64 of `n` random bytes, without padding.
pub fn random_urlsafe(n: usize) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Random lowercase hex of `n` random bytes.
pub fn random_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

/// A random (version 4) UUID, hyphenated lowercase.
pub fn uuid4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

#[cfg(test)]
pub(crate) mod mock {
    //! A plain-HTTP axum server on `127.0.0.1:0` for client tests.

    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::HeaderMap;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// A running mock service; aborted when dropped.
    pub struct Mock {
        pub base: String,
        task: JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Mock {
        pub async fn start(router: Router) -> Mock {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Mock { base, task }
        }

        pub fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
    }

    /// One canned response.
    #[derive(Clone, Debug)]
    pub struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Reply {
        pub fn json(status: u16, body: impl Into<String>) -> Reply {
            Reply {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.into().into_bytes(),
            }
        }

        pub fn html(body: impl Into<String>) -> Reply {
            Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: body.into().into_bytes(),
            }
        }

        pub fn bytes(body: Vec<u8>) -> Reply {
            Reply { status: 200, headers: vec![], body }
        }

        pub fn redirect(location: impl Into<String>) -> Reply {
            Reply { status: 302, headers: vec![("location".into(), location.into())], body: vec![] }
        }

        pub fn header(mut self, k: &str, v: impl Into<String>) -> Reply {
            self.headers.push((k.into(), v.into()));
            self
        }
    }

    /// Canned responses per `"METHOD /path"`: queued one-shot replies first, then the default.
    /// Unscripted requests get 404 `{}`.
    #[derive(Clone, Default)]
    pub struct Script {
        once: Arc<Mutex<std::collections::HashMap<String, std::collections::VecDeque<Reply>>>>,
        default: Arc<Mutex<std::collections::HashMap<String, Reply>>>,
    }

    impl Script {
        pub fn set(&self, key: &str, r: Reply) {
            self.default.lock().unwrap().insert(key.into(), r);
        }

        pub fn once(&self, key: &str, r: Reply) {
            self.once.lock().unwrap().entry(key.into()).or_default().push_back(r);
        }

        fn next(&self, key: &str) -> Reply {
            let queued = self.once.lock().unwrap().get_mut(key).and_then(|q| q.pop_front());
            queued.or_else(|| self.default.lock().unwrap().get(key).cloned()).unwrap_or_else(|| Reply::json(404, "{}"))
        }
    }

    async fn scripted_handler(
        axum::extract::State((script, log)): axum::extract::State<(Script, Log)>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: String,
    ) -> axum::response::Response {
        log.push(format!("{method} {uri}"), &headers, body);
        let r = script.next(&format!("{method} {}", uri.path()));
        let mut resp = axum::response::Response::new(axum::body::Body::from(r.body));
        *resp.status_mut() = axum::http::StatusCode::from_u16(r.status).unwrap();
        for (k, v) in r.headers {
            let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            resp.headers_mut().append(name, v.parse().unwrap());
        }
        resp
    }

    impl Mock {
        /// A mock that answers every request from a [`Script`] and logs it.
        pub async fn scripted() -> (Mock, Script, Log) {
            let (script, log) = (Script::default(), Log::default());
            let app = Router::new().fallback(scripted_handler).with_state((script.clone(), log.clone()));
            (Mock::start(app).await, script, log)
        }
    }

    /// Requests a mock saw: `(method path?query, headers, body)`.
    #[derive(Clone, Default)]
    pub struct Log(pub Arc<Mutex<Vec<(String, HeaderMap, String)>>>);

    impl Log {
        pub fn push(&self, what: impl Into<String>, headers: &HeaderMap, body: impl Into<String>) {
            self.0.lock().unwrap().push((what.into(), headers.clone(), body.into()));
        }

        pub fn paths(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().map(|(p, _, _)| p.clone()).collect()
        }

        pub fn count(&self, prefix: &str) -> usize {
            self.paths().iter().filter(|p| p.starts_with(prefix)).count()
        }

        /// Headers and body of the last request whose line starts with `prefix`.
        pub fn last(&self, prefix: &str) -> (HeaderMap, String) {
            let log = self.0.lock().unwrap();
            let (_, h, b) = log.iter().rev().find(|(p, _, _)| p.starts_with(prefix)).expect("request");
            (h.clone(), b.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_helpers() {
        let u = parse_url("http://127.0.0.1:8080/a/b?code=xyz&state=s&code=2").unwrap();
        assert_eq!(origin(&u), "http://127.0.0.1:8080");
        assert_eq!(query_param(&u, "code").as_deref(), Some("xyz"));
        assert_eq!(query_param(&u, "nope"), None);
        assert!(matches!(parse_url("::"), Err(CloudError::Config(_))));
        assert_eq!(snippet("héllo world", 5), "héllo");
        assert_eq!(random_urlsafe(32).len(), 43);
        assert_ne!(random_urlsafe(8), random_urlsafe(8));
        assert_eq!(random_hex(4).len(), 8);
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn builds_clients() {
        assert!(client(TIMEOUT, None, true).is_ok());
        assert!(client(TIMEOUT, Some(Arc::new(Jar::default())), false).is_ok());
    }
}
