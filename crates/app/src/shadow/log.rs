//! The mismatch log: one JSON object per line, appended, rotated to `<file>.1` past a size
//! limit. Only mismatch *onsets* are written, so on an SD card it stays small and quiet.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

pub struct MismatchLog {
    path: Option<PathBuf>,
    max_bytes: u64,
    /// Lines written since start.
    pub lines: u64,
    pub errors: u64,
}

impl MismatchLog {
    /// `None` disables writing (lines are still counted).
    pub fn new(path: Option<PathBuf>, max_bytes: u64) -> Self {
        MismatchLog { path, max_bytes, lines: 0, errors: 0 }
    }

    pub fn write(&mut self, rec: &serde_json::Value) {
        self.lines += 1;
        let Some(path) = &self.path else { return };
        let line = format!("{}\n", serde_json::to_string(rec).expect("log records serialise"));
        if let Err(e) = append(path, &line, self.max_bytes) {
            self.errors += 1;
            tracing::warn!(path = %path.display(), error = %e, "shadow log write failed");
        }
    }
}

fn append(path: &PathBuf, line: &str, max_bytes: u64) -> std::io::Result<()> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size + line.len() as u64 > max_bytes {
        let mut old = path.clone().into_os_string();
        old.push(".1");
        std::fs::rename(path, old)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines are appended as JSON and the file rotates to `.1` when it would grow too large.
    #[test]
    fn appends_jsonl_and_rotates() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.jsonl");
        let mut l = MismatchLog::new(Some(p.clone()), 40);
        l.write(&serde_json::json!({"a": 1}));
        l.write(&serde_json::json!({"b": 2}));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        l.write(&serde_json::json!({"c": "a fairly long value to force rotation"}));
        assert_eq!(std::fs::read_to_string(d.path().join("m.jsonl.1")).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        assert!(std::fs::read_to_string(&p).unwrap().starts_with("{\"c\""));
        assert_eq!(l.lines, 3);
        assert_eq!(l.errors, 0);
    }

    /// A disabled log only counts; an unwritable path counts an error and does not panic.
    #[test]
    fn disabled_and_failing_logs() {
        let mut off = MismatchLog::new(None, 10);
        off.write(&serde_json::json!(1));
        assert_eq!(off.lines, 1);
        let d = tempfile::tempdir().unwrap();
        let mut bad = MismatchLog::new(Some(d.path().join("missing-dir/m.jsonl")), 10);
        bad.write(&serde_json::json!(1));
        assert_eq!(bad.errors, 1);
        // Rotation onto a non-empty directory fails too.
        let p = d.path().join("r.jsonl");
        std::fs::write(&p, "{}\n").unwrap();
        std::fs::create_dir_all(d.path().join("r.jsonl.1/x")).unwrap();
        let mut rot = MismatchLog::new(Some(p), 4);
        rot.write(&serde_json::json!("lang genoeg"));
        assert_eq!(rot.errors, 1);
    }
}
