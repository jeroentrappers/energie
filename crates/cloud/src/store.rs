//! Credentials files (read-only, HA config-entry `data` shape) and state files (tokens the
//! clients rotate).
//!
//! Credentials live in `/etc/energie/cloud-*.json` and are never written. A token that a
//! service rotates goes to a state file in the state dir (`/var/lib/energie/...`), written
//! atomically with mode 0600; a state path under `/etc` is refused.

use std::fmt;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{CloudError, Result};

/// A secret string: `Debug` and `Display` never show it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// The secret itself, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() { f.write_str("\"\"") } else { f.write_str("<redacted>") }
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret(s.to_string())
    }
}

/// Read a JSON credentials file. Errors name the file, never its content.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CloudError::Config(format!("cannot read credentials {}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| {
        // serde's message carries line/column and the expected type, not the value.
        CloudError::Config(format!("credentials {}: {}", path.display(), e))
    })
}

/// One JSON state file a client owns.
#[derive(Debug, Clone)]
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// A state file at `path`. Paths under `/etc` are refused: the credentials there stay as
    /// the owner wrote them.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if path.starts_with("/etc") {
            return Err(CloudError::Config(format!("state file {} must not be under /etc", path.display())));
        }
        Ok(StateFile { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored value; `None` when the file does not exist yet.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Replace the stored value: write a 0600 temp file next to it, then rename.
    pub fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        std::fs::create_dir_all(self.path.parent().unwrap_or(Path::new(".")))?;
        let tmp = self.path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut f = options.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Creds {
        user: String,
        password: Secret,
    }

    /// Secrets never reach a log line.
    #[test]
    fn secrets_are_redacted() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
        assert!(!s.is_empty());
        assert_eq!(format!("{:?}", Secret::default()), "\"\"");
        assert!(Secret::from("").is_empty());
        let c = Creds { user: "u".into(), password: "p".into() };
        assert!(!format!("{c:?}").contains("\"p\""));
    }

    /// A credentials file parses; a missing or broken one names the path, not the content.
    #[test]
    fn loads_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.json");
        std::fs::write(&p, r#"{"user":"u","password":"topsecret"}"#).unwrap();
        let c: Creds = load_json(&p).unwrap();
        assert_eq!(c.password.expose(), "topsecret");
        let e = load_json::<Creds>(&dir.path().join("missing.json")).unwrap_err();
        assert!(e.to_string().contains("missing.json"), "{e}");
        std::fs::write(&p, r#"{"user":"u","password":5}"#).unwrap();
        let e = load_json::<Creds>(&p).unwrap_err();
        assert!(matches!(e, CloudError::Config(_)) && !e.to_string().contains("topsecret"), "{e}");
    }

    /// State round-trips through a 0600 file; a missing file is `None`.
    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let st = StateFile::new(dir.path().join("sub/token.json")).unwrap();
        assert!(st.path().ends_with("sub/token.json"));
        assert_eq!(st.load::<Creds>().unwrap(), None);
        let c = Creds { user: "u".into(), password: "p".into() };
        st.save(&c).unwrap();
        assert_eq!(st.load::<Creds>().unwrap(), Some(c));
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(st.path()).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(st.path(), "nope").unwrap();
        assert!(matches!(st.load::<Creds>(), Err(CloudError::Malformed(_))));
    }

    /// Nothing is ever written under /etc; unreadable state is an I/O error.
    #[test]
    fn refuses_etc_and_reports_io() {
        assert!(matches!(StateFile::new("/etc/energie/kia-token.json"), Err(CloudError::Config(_))));
        let dir = tempfile::tempdir().unwrap();
        let st = StateFile::new(dir.path()).unwrap();
        assert!(matches!(st.load::<Creds>(), Err(CloudError::Io(_))));
        let file = dir.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let st = StateFile::new(file.join("under-a-file.json")).unwrap();
        assert!(matches!(st.save(&1), Err(CloudError::Io(_))));
    }
}
