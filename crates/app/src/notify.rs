//! Notifications: the `Notify` / `Persistent` commands. HA's `script.notify_jeroen` becomes a
//! [`Notifier`]; persistent notes only appear on the dashboard.

use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Notification {
    pub at: String,
    pub title: String,
    pub message: String,
    pub high_priority: bool,
    /// A dashboard note (`persistent_notification.create`), not a push message.
    pub persistent: bool,
    /// The note's `notification_id`: a newer note with the same id replaces it on the dashboard.
    pub id: Option<String>,
}

pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    /// Deliver `n`. Must not block the tick.
    fn notify(&self, n: &Notification);
}

/// Logs only (shadow mode: HA still sends the real ones).
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn name(&self) -> &str {
        "log"
    }
    fn notify(&self, n: &Notification) {
        tracing::info!(title = %n.title, message = %n.message, high = n.high_priority, "notification (log only)");
    }
}

/// Push messages through ntfy's JSON publish API.
pub struct NtfyNotifier {
    base: String,
    topic: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl NtfyNotifier {
    /// `url` is the topic URL, e.g. `https://ntfy.sh/mijn-topic`.
    pub fn new(url: &str, token: Option<String>) -> Result<Self> {
        let (base, topic) = url
            .trim_end_matches('/')
            .rsplit_once('/')
            .filter(|(b, t)| b.contains("://") && !b.ends_with('/') && !t.is_empty())
            .ok_or_else(|| Error::new(format!("ntfy url needs a topic: {url}")))?;
        Ok(NtfyNotifier { base: format!("{base}/"), topic: topic.into(), token, client: reqwest::Client::new() })
    }

    /// The JSON body ntfy expects.
    pub fn body(&self, n: &Notification) -> serde_json::Value {
        json!({
            "topic": self.topic,
            "title": n.title,
            "message": n.message,
            "priority": if n.high_priority { 5 } else { 3 },
            "tags": ["zap"],
        })
    }

    pub async fn send(&self, n: &Notification) -> Result<()> {
        let mut req = self.client.post(&self.base).json(&self.body(n));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await?.error_for_status()?;
        Ok(())
    }
}

impl Notifier for std::sync::Arc<NtfyNotifier> {
    fn name(&self) -> &str {
        "ntfy"
    }
    fn notify(&self, n: &Notification) {
        if n.persistent {
            return;
        }
        let me = self.clone();
        let n = n.clone();
        tokio::spawn(async move {
            if let Err(e) = me.send(&n).await {
                tracing::warn!(error = %e, title = %n.title, "ntfy delivery failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::http::HeaderMap;
    use std::sync::{Arc, Mutex};

    fn n(persistent: bool) -> Notification {
        Notification {
            at: "2026-11-07T13:46:00+01:00".into(),
            title: "⚡ Peak guard".into(),
            message: "Projectie 3.9 kW — afschakelen gestart.".into(),
            high_priority: true,
            persistent,
            id: None,
        }
    }

    /// The topic URL splits into the publish endpoint and the topic; a URL without a topic is
    /// refused.
    #[test]
    fn topic_url_parsing() {
        let ntfy = NtfyNotifier::new("https://ntfy.sh/energie-thuis/", None).unwrap();
        assert_eq!((ntfy.base.as_str(), ntfy.topic.as_str()), ("https://ntfy.sh/", "energie-thuis"));
        assert!(NtfyNotifier::new("https://ntfy.sh", None).is_err());
        assert!(NtfyNotifier::new("ntfy", None).is_err());
        let b = ntfy.body(&n(false));
        assert_eq!(b["priority"], 5);
        assert_eq!(b["title"], "⚡ Peak guard");
        let low = Notification { high_priority: false, ..n(false) };
        assert_eq!(ntfy.body(&low)["priority"], 3);
        assert_eq!(LogNotifier.name(), "log");
        LogNotifier.notify(&low);
    }

    type Seen = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

    async fn fake_ntfy(status: u16) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |h: HeaderMap, Json(v): Json<serde_json::Value>| {
                let s = s2.clone();
                async move {
                    let auth = h.get("authorization").map(|v| v.to_str().unwrap().to_string());
                    s.lock().unwrap().push((auth, v));
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/energie", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, seen)
    }

    /// A push message is POSTed as JSON with the bearer token; persistent notes are not pushed.
    #[tokio::test]
    async fn posts_json_with_token_and_skips_persistent_notes() {
        let (url, seen) = fake_ntfy(200).await;
        let ntfy = Arc::new(NtfyNotifier::new(&url, Some("tk".into())).unwrap());
        assert_eq!(ntfy.name(), "ntfy");
        ntfy.notify(&n(true));
        ntfy.notify(&n(false));
        crate::ha::ws::tests::until(|| !seen.lock().unwrap().is_empty()).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.as_deref(), Some("Bearer tk"));
        assert_eq!(got[0].1["topic"], "energie");
        assert_eq!(got[0].1["message"], "Projectie 3.9 kW — afschakelen gestart.");
    }

    /// A server error is reported by `send` and only logged by `notify`.
    #[tokio::test]
    async fn server_errors_are_reported() {
        let (url, seen) = fake_ntfy(500).await;
        let ntfy = Arc::new(NtfyNotifier::new(&url, None).unwrap());
        assert!(ntfy.send(&n(false)).await.is_err());
        ntfy.notify(&n(false));
        crate::ha::ws::tests::until(|| seen.lock().unwrap().len() >= 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap()[0].0, None);
    }
}
