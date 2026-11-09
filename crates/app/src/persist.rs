//! Settings and Memory on disk, SD-card friendly: JSON files written atomically (temp file,
//! fsync, rename, fsync of the directory), at most once per interval, only when the content
//! changed, and at once when a setting changes.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::time::{Ts, secs};
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

pub const SETTINGS_FILE: &str = "settings.json";
pub const MEMORY_FILE: &str = "memory.json";

/// Write `bytes` to `path` so that a crash leaves either the old or the new file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| Error::new(format!("{}: no parent directory", path.display())))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // Windows cannot open a directory as a file; its rename is already durable enough here.
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Read a JSON file; `Ok(None)` when it does not exist.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let named = |e: serde_json::Error| Error::new(format!("{}: {e}", path.display()));
    read_file(path)?.map(|b| serde_json::from_slice(&b).map_err(named)).transpose()
}

/// The file's bytes; `Ok(None)` when it does not exist.
fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::new(format!("{}: {e}", path.display()))),
    }
}

pub struct Persister {
    dir: PathBuf,
    interval_s: f64,
    last_write: Option<Ts>,
    last_settings: Vec<u8>,
    last_memory: Vec<u8>,
    /// Files written since start.
    pub writes: u64,
}

impl Persister {
    pub fn new(dir: PathBuf, interval_s: u64) -> Self {
        Persister {
            dir,
            interval_s: interval_s as f64,
            last_write: None,
            last_settings: vec![],
            last_memory: vec![],
            writes: 0,
        }
    }

    /// Load both files (`None` for a file that is not there yet).
    pub fn load(&mut self) -> Result<(Option<Settings>, Option<Memory>)> {
        let s: Option<Settings> = load_json(&self.dir.join(SETTINGS_FILE))?;
        let m: Option<Memory> = load_json(&self.dir.join(MEMORY_FILE))?;
        // What is on disk now counts as written, so an unchanged state is not rewritten.
        if let Some(s) = &s {
            self.last_settings = serde_json::to_vec_pretty(s)?;
        }
        if let Some(m) = &m {
            self.last_memory = serde_json::to_vec_pretty(m)?;
        }
        Ok((s, m))
    }

    /// Save when the interval has passed (or `force`), writing only files whose content
    /// changed. Returns how many files were written.
    pub fn save(&mut self, now: Ts, s: &Settings, m: &Memory, force: bool) -> Result<u32> {
        if !force && self.last_write.is_some_and(|t| secs(t, now) < self.interval_s) {
            return Ok(0);
        }
        self.last_write = Some(now);
        let mut n = 0;
        let sb = serde_json::to_vec_pretty(s)?;
        if sb != self.last_settings {
            write_atomic(&self.dir.join(SETTINGS_FILE), &sb)?;
            self.last_settings = sb;
            n += 1;
        }
        let mb = serde_json::to_vec_pretty(m)?;
        if mb != self.last_memory {
            write_atomic(&self.dir.join(MEMORY_FILE), &mb)?;
            self.last_memory = mb;
            n += 1;
        }
        self.writes += u64::from(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plug::tests::t0;
    use chrono::Duration;

    /// The atomic write replaces the file and leaves no temp file behind.
    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.json");
        write_atomic(&p, b"1").unwrap();
        write_atomic(&p, b"2").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"2");
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("x.json")]);
        assert!(write_atomic(Path::new("/"), b"x").is_err());
        assert!(write_atomic(&d.path().join("no/such/dir.json"), b"x").is_err());
    }

    /// Missing files load as `None`; corrupt files are an error naming the file.
    #[test]
    fn loading_missing_and_corrupt_files() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Persister::new(d.path().into(), 60);
        assert_eq!(p.load().unwrap(), (None, None));
        std::fs::write(d.path().join(SETTINGS_FILE), "{kapot").unwrap();
        assert!(p.load().unwrap_err().0.contains("settings.json"));
        let unreadable = d.path().join("dir.json");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(load_json::<Settings>(&unreadable).is_err());
    }

    /// Writes are throttled to the interval, skipped when nothing changed, and forced on a
    /// settings change.
    #[test]
    fn throttled_change_only_and_forced_writes() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Persister::new(d.path().into(), 60);
        let s = Settings::default();
        let mut m = Memory::default();
        assert_eq!(p.save(t0(), &s, &m, false).unwrap(), 2);
        m.peak_guard_interventies = 1;
        assert_eq!(p.save(t0() + Duration::seconds(30), &s, &m, false).unwrap(), 0, "throttled");
        assert_eq!(p.save(t0() + Duration::seconds(61), &s, &m, false).unwrap(), 1, "memory only");
        assert_eq!(p.save(t0() + Duration::seconds(200), &s, &m, false).unwrap(), 0, "unchanged");
        let mut s2 = s.clone();
        s2.peak_guard_actief = false;
        assert_eq!(p.save(t0() + Duration::seconds(201), &s2, &m, true).unwrap(), 1, "forced");
        assert_eq!(p.writes, 4);
        let mut fresh = Persister::new(d.path().into(), 60);
        let (ls, lm) = fresh.load().unwrap();
        assert_eq!((ls.unwrap(), lm.unwrap()), (s2.clone(), m.clone()));
        assert_eq!(fresh.save(t0(), &s2, &m, false).unwrap(), 0, "what was loaded is not rewritten");
    }

    /// A failing write is reported.
    #[test]
    fn write_errors_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let mut p = Persister::new(d.path().join("missing"), 60);
        assert!(p.save(t0(), &Settings::default(), &Memory::default(), true).is_err());
        let mut p = Persister::new(d.path().into(), 60);
        std::fs::create_dir(d.path().join(format!("{MEMORY_FILE}.tmp"))).unwrap();
        assert!(p.save(t0(), &Settings::default(), &Memory::default(), true).is_err());
    }
}
