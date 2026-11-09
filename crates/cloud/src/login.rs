//! Password logins and the credentials captured at run time.
//!
//! A client that signs in with a password ([`crate::kia`], [`crate::cupra`]) goes through a
//! [`LoginGate`], so a wrong or changed password is never hammered into an account lock: at
//! most one password login per [`LOGIN_GAP`], and after [`MAX_REJECTED`] rejected logins in a
//! row none at all until the credentials change ([`LoginGate::reset`]).
//!
//! Credentials set with `energie credentials set` live in the state dir
//! (`credentials/<device-id>.json`, 0600, [`CredentialStore`]) and win over the read-only
//! credentials file in `/etc`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use energie_drivers::AuthStatus;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::error::{CloudError, Result};
use crate::store::{Secret, StateFile, load_json};

/// At most one password login per this long.
pub const LOGIN_GAP: Duration = Duration::from_secs(30 * 60);
/// Rejected logins in a row after which the gate stays shut until the credentials change.
pub const MAX_REJECTED: u32 = 3;

/// A rejected login: the service answered and refused the credentials (or wants something
/// only a person can do). Transport errors and server faults are not rejections.
pub fn is_rejection(e: &CloudError) -> bool {
    matches!(e, CloudError::Unauthorized(_))
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Rate limit and lock-out guard for one client's password logins.
#[derive(Debug, Clone)]
pub struct LoginGate {
    gap: Duration,
    /// The last attempt (monotonic, wall clock).
    last: Option<(Instant, DateTime<Utc>)>,
    rejected: u32,
    result: Option<String>,
    /// The last use of the session: `None` before the first.
    session: Option<bool>,
}

impl Default for LoginGate {
    fn default() -> Self {
        LoginGate::new(LOGIN_GAP)
    }
}

impl LoginGate {
    pub fn new(gap: Duration) -> Self {
        LoginGate { gap, last: None, rejected: 0, result: None, session: None }
    }

    pub fn blocked(&self) -> bool {
        self.rejected >= MAX_REJECTED
    }

    /// May a password login run at `now`? `Unauthorized` when blocked, `Backoff` inside the gap.
    pub fn check(&self, now: Instant) -> Result<()> {
        if self.blocked() {
            return Err(CloudError::Unauthorized(format!(
                "{} rejected logins in a row; no more attempts until the credentials change \
                 (energie credentials set)",
                self.rejected
            )));
        }
        match self.last {
            Some((at, _)) if now < at + self.gap => Err(CloudError::Backoff(at + self.gap - now)),
            _ => Ok(()),
        }
    }

    /// Record a login attempt made at `now`.
    pub fn record(&mut self, now: Instant, wall: DateTime<Utc>, outcome: &Result<()>) {
        self.last = Some((now, wall));
        match outcome {
            Ok(()) => {
                self.rejected = 0;
                self.result = Some("ok".into());
                self.session = Some(true);
            }
            Err(e) => {
                if is_rejection(e) {
                    self.rejected += 1;
                }
                self.result = Some(e.to_string());
                self.session = Some(false);
            }
        }
    }

    /// Record how the last use of the session went (a token refresh, an API call).
    pub fn session(&mut self, ok: bool) {
        self.session = Some(ok);
    }

    /// New credentials: forget the rejections and the gap.
    pub fn reset(&mut self) {
        self.rejected = 0;
        self.last = None;
    }

    /// The status for the dashboard. `credentials` names the source, `can_login` whether a
    /// password login is possible with them.
    pub fn status(&self, credentials: &str, can_login: bool, now: Instant) -> AuthStatus {
        let session_ok = self.session == Some(true);
        let failed = self.session == Some(false);
        let waiting = self.last.filter(|(at, _)| failed && !self.blocked() && now < *at + self.gap);
        let state = if session_ok {
            "ok"
        } else if failed && self.blocked() {
            "geblokkeerd"
        } else if failed && (!can_login || self.rejected > 0) {
            "aanmelden nodig"
        } else if failed {
            "wacht"
        } else {
            "onbekend"
        };
        AuthStatus {
            state: state.into(),
            credentials: credentials.into(),
            can_login,
            session_ok,
            last_login: self.last.map(|(_, w)| rfc3339(w)),
            last_result: self.result.clone(),
            rejected_logins: self.rejected,
            blocked: self.blocked(),
            next_login: waiting.map(|(_, w)| rfc3339(w + chrono::Duration::from_std(self.gap).unwrap_or_default())),
        }
    }
}

/// Where a client's credentials came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Captured at run time (the state dir).
    State,
    /// The read-only credentials file.
    Etc,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::State => "state",
            Source::Etc => "etc",
        }
    }
}

/// The credentials captured at run time for one device: `<state_dir>/credentials/<id>.json`.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    file: StateFile,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        Ok(CredentialStore { file: StateFile::new(path)? })
    }

    /// The store of device `id` in `state_dir`.
    pub fn in_state_dir(state_dir: &Path, id: &str) -> Result<Self> {
        CredentialStore::new(state_dir.join("credentials").join(format!("{id}.json")))
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.file.load()
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        self.file.save(value)
    }
}

/// The credentials of a client: the store's when it holds any, else the file's.
pub fn load_credentials<T: DeserializeOwned>(store: Option<&CredentialStore>, file: &Path) -> Result<(T, Source)> {
    if let Some(stored) = store.map(CredentialStore::load).transpose()?.flatten() {
        return Ok((stored, Source::State));
    }
    Ok((load_json(file)?, Source::Etc))
}

/// The fields of a credentials request, checked: every key known, the required ones
/// non-empty strings. Errors name the field, never a value.
pub struct Fields<'a>(&'a Map<String, Value>);

impl<'a> Fields<'a> {
    pub fn new(map: &'a Map<String, Value>, required: &[&str], optional: &[&str]) -> Result<Self> {
        if let Some(k) = map.keys().find(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str())) {
            return Err(CloudError::Config(format!("unknown credentials field {k:?}")));
        }
        for k in required.iter().chain(optional) {
            match map.get(*k) {
                None if optional.contains(k) => {}
                Some(Value::String(s)) if !s.is_empty() || optional.contains(k) => {}
                _ => return Err(CloudError::Config(format!("credentials field {k:?} must be a non-empty string"))),
            }
        }
        Ok(Fields(map))
    }

    /// A string field, `None` when absent or empty.
    pub fn get(&self, k: &str) -> Option<&'a str> {
        self.0.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    pub fn secret(&self, k: &str) -> Option<Secret> {
        self.get(k).map(Secret::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S: Duration = Duration::from_secs(1);

    fn wall() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    /// One login per gap; a rejection counts, a server fault does not; three rejections shut
    /// the gate until a reset.
    #[test]
    fn gate_limits_and_blocks() {
        let t0 = Instant::now();
        let mut g = LoginGate::new(10 * S);
        assert_eq!(g.status("etc", true, t0).state, "onbekend");
        g.check(t0).unwrap();
        g.record(t0, wall(), &Err(CloudError::Status(503)));
        assert!(matches!(g.check(t0 + 9 * S), Err(CloudError::Backoff(d)) if d == S));
        let s = g.status("etc", true, t0);
        assert_eq!((s.state.as_str(), s.rejected_logins), ("wacht", 0));
        assert_eq!(s.next_login.as_deref(), Some("2026-11-08T12:00:10Z"));
        assert_eq!(s.last_login.as_deref(), Some("2026-11-08T12:00:00Z"));
        for i in 1..=3 {
            let t = t0 + 10 * S * i;
            g.check(t).unwrap();
            g.record(t, wall(), &Err(CloudError::Unauthorized("bad password".into())));
        }
        let e = g.check(t0 + 1000 * S).unwrap_err();
        assert!(matches!(&e, CloudError::Unauthorized(m) if m.contains("3 rejected")), "{e}");
        let s = g.status("state", true, t0 + 1000 * S);
        assert_eq!((s.state.as_str(), s.blocked, s.next_login), ("geblokkeerd", true, None));
        assert_eq!(s.last_result.as_deref(), Some("unauthorized: bad password"));
        g.reset();
        g.check(t0).unwrap();
        assert_eq!(g.status("state", true, t0).state, "wacht", "new credentials: a login is due");
        g.record(t0, wall(), &Ok(()));
        let s = g.status("state", true, t0);
        assert_eq!((s.state.as_str(), s.session_ok, s.last_result.as_deref()), ("ok", true, Some("ok")));
        assert!(g.check(t0 + S).is_err(), "the gap also follows a good login");
    }

    /// The session state follows its last use; without a way to log in a failure needs a person.
    #[test]
    fn session_states() {
        let t0 = Instant::now();
        let mut g = LoginGate::default();
        g.session(false);
        assert_eq!(g.status("etc", false, t0).state, "aanmelden nodig");
        assert_eq!(g.status("etc", true, t0).state, "wacht");
        g.session(true);
        assert_eq!(g.status("etc", false, t0).state, "ok");
        assert_eq!(Source::State.label(), "state");
        assert_eq!(Source::Etc.label(), "etc");
    }

    /// The store wins over the file; it lives in `credentials/<id>.json`, never under /etc.
    #[test]
    fn store_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cloud.json");
        std::fs::write(&file, r#"{"a":"from-etc"}"#).unwrap();
        let store = CredentialStore::in_state_dir(dir.path(), "ev3").unwrap();
        assert!(store.path().ends_with("credentials/ev3.json"));
        let (v, src): (Value, _) = load_credentials(Some(&store), &file).unwrap();
        assert_eq!((v["a"].as_str(), src), (Some("from-etc"), Source::Etc));
        store.save(&json!({"a": "captured"})).unwrap();
        let (v, src): (Value, _) = load_credentials(Some(&store), &file).unwrap();
        assert_eq!((v["a"].as_str(), src), (Some("captured"), Source::State));
        let (v, _): (Value, _) = load_credentials(None, &file).unwrap();
        assert_eq!(v["a"], "from-etc");
        assert!(CredentialStore::in_state_dir(Path::new("/etc/energie"), "ev3").is_err());
    }

    /// Unknown keys and empty or non-string required fields are refused by name only.
    #[test]
    fn fields_are_checked() {
        let m = |v: Value| v.as_object().unwrap().clone();
        let ok = m(json!({"username": "u", "password": "p", "pin": ""}));
        let f = Fields::new(&ok, &["username", "password"], &["pin"]).unwrap();
        assert_eq!((f.get("username"), f.get("pin"), f.get("nope")), (Some("u"), None, None));
        assert_eq!(f.secret("password").unwrap().expose(), "p");
        for (bad, field) in [
            (json!({"username": "u", "password": "", "pin": "1"}), "password"),
            (json!({"username": "u"}), "password"),
            (json!({"username": "u", "password": 5}), "password"),
            (json!({"username": "u", "password": "p", "pin": 1}), "pin"),
            (json!({"username": "u", "password": "p", "extra": "hunter2"}), "extra"),
        ] {
            let e = Fields::new(&m(bad), &["username", "password"], &["pin"]).err().unwrap().to_string();
            assert!(e.contains(field) && !e.contains("hunter2"), "{e}");
        }
    }
}
