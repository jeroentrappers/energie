//! The one-shot import of Home Assistant's Settings and Memory into the state dir, for
//! `settings_source = "local"`: `energie import-settings --from-ha --config <file>`, or by
//! itself on a first start (empty state dir) with `[ha] seed = true`.
//!
//! One read-only WebSocket session with a single `get_states`; HA is never written to. The
//! helpers become `Settings` and `Memory` through the same bindings the shadow lockstep uses
//! (`ha::mapping`), the utility meters and integrals are seeded from their entities
//! ([`seed_meters`]), and both files are written atomically.

use std::path::Path;
use std::time::Duration;

use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::time::Ts;

use crate::config::{Config, HaConfig, read_secret};
use crate::error::{Error, Result};
use crate::ha::entity::{EntityState, index};
use crate::ha::mapping::{memory_from, seed_meters, settings_from};
use crate::ha::ws::{ClientConfig, fetch_states};
use crate::persist::{MEMORY_FILE, SETTINGS_FILE, write_atomic};

/// Settings and Memory as HA's entities show them at `now` (fields HA lacks keep their
/// defaults).
pub fn from_states(states: Vec<EntityState>, now: Ts) -> (Settings, Memory) {
    let store = index(states);
    let settings = settings_from(&store, Settings::default());
    let mut memory = memory_from(&store, Memory::default());
    memory.meters = seed_meters(&store, now);
    (settings, memory)
}

/// Read HA's states once (within `limit`) and turn them into Settings and Memory.
pub async fn fetch(ha: &HaConfig, now: Ts, limit: Duration) -> Result<(Settings, Memory)> {
    let client = ClientConfig::new(ha.ws_url()?, read_secret(&ha.token_file)?);
    let states = tokio::time::timeout(limit, fetch_states(&client))
        .await
        .map_err(|_| Error::new(format!("no answer from home assistant within {limit:?}")))??;
    tracing::info!(entities = states.len(), "home assistant states read for the import");
    Ok(from_states(states, now))
}

/// Write both files into `dir` (created when missing).
pub fn write(dir: &Path, s: &Settings, m: &Memory) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
    write_atomic(&dir.join(SETTINGS_FILE), &serde_json::to_vec_pretty(s)?)?;
    write_atomic(&dir.join(MEMORY_FILE), &serde_json::to_vec_pretty(m)?)?;
    Ok(())
}

/// `energie import-settings --from-ha`: HA's helpers into `cfg.state_dir`. Refuses to overwrite
/// an existing `settings.json` / `memory.json` unless `force`. Returns what was written.
pub async fn import_settings(cfg: &Config, force: bool, now: Ts) -> Result<String> {
    let ha = cfg.ha.as_ref().ok_or_else(|| Error::new("import-settings --from-ha needs an [ha] section"))?;
    let existing: Vec<_> =
        [SETTINGS_FILE, MEMORY_FILE].into_iter().filter(|f| cfg.state_dir.join(f).exists()).collect();
    if !existing.is_empty() && !force {
        return Err(Error::new(format!(
            "{} already has {}; stop energie and pass --force to replace it",
            cfg.state_dir.display(),
            existing.join(" and ")
        )));
    }
    let (s, m) = fetch(ha, now, Duration::from_secs(ha.start_wait_s.max(1))).await?;
    write(&cfg.state_dir, &s, &m)?;
    Ok(format!(
        "imported the settings and memory from {} into {} (zonprioriteit {}, piekhistoriek {:?})",
        ha.url,
        cfg.state_dir.display(),
        s.zonprioriteit.label(),
        m.capaciteit_piekhistoriek
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::ws::tests::{fake_ha, recv};
    use crate::plug::tests::t0;
    use serde_json::json;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/ha-states-2026-11-07.json")).unwrap()
    }

    fn config(dir: &Path, url: &str, token: &str) -> Config {
        let tok = dir.join("token");
        std::fs::write(&tok, token).unwrap();
        Config::parse(&format!(
            "mode = \"control\"\nstate_dir = \"{}\"\n[ha]\nurl = \"{url}\"\ntoken_file = \"{}\"\nimport_only = true\nstart_wait_s = 5\n",
            dir.join("state").display(),
            tok.display()
        ))
        .unwrap()
    }

    /// The live snapshot of 2026-11-07 becomes the helpers' values: the settings, the latches
    /// and counters, the piekhistoriek and the meters.
    #[test]
    fn states_become_settings_and_memory() {
        let states: Vec<EntityState> = serde_json::from_value(fixture()).unwrap();
        let (s, m) = from_states(states, t0());
        assert_eq!(s.zonprioriteit.label(), "Buzz → Batterij → EV3");
        assert_eq!(s.goedkoop_laden_prijs_drempel, 0.38);
        assert_eq!(m.peak_guard_interventies, 2880);
        assert_eq!(m.batterij_setpoint_bias, 200.0);
        assert!(m.capaciteit_piekhistoriek.starts_with("2025-11:6.572,2025-12:8.011"));
        assert_eq!(m.meters.batterij_ontladen_energie.total, Some(750.288));
        let (s, m) = from_states(Vec::new(), t0());
        assert_eq!((s, m.peak_guard_interventies), (Settings::default(), 0), "nothing from HA: defaults");
    }

    /// Against a fake HA: one get_states, both files written; a second import refuses to
    /// overwrite them without `force`, and replaces them with it.
    #[tokio::test]
    async fn imports_into_the_state_dir_once() {
        let (url, conns) = fake_ha(fixture(), |_, mut ws| async move {
            // Nothing else is ever sent: no subscription, no service call.
            assert!(recv(&mut ws).await.is_none());
        })
        .await;
        let d = tempfile::tempdir().unwrap();
        let cfg = config(d.path(), &url, "good-token");
        let msg = import_settings(&cfg, false, t0()).await.unwrap();
        assert!(msg.contains("zonprioriteit Buzz → Batterij → EV3"), "{msg}");
        let s: Settings =
            serde_json::from_slice(&std::fs::read(d.path().join("state").join(SETTINGS_FILE)).unwrap()).unwrap();
        assert_eq!(s.batterij_reservering_w, 700.0);
        let m: Memory =
            serde_json::from_slice(&std::fs::read(d.path().join("state").join(MEMORY_FILE)).unwrap()).unwrap();
        assert_eq!(m.buzz_laadweigering, 0);
        assert_eq!(m.peak_guard_interventies, 2880);
        let e = import_settings(&cfg, false, t0()).await.unwrap_err().0;
        assert!(e.contains("settings.json and memory.json") && e.contains("--force"), "{e}");
        import_settings(&cfg, true, t0()).await.unwrap();
        assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// A refused token, a failing get_states, no answer, no [ha] and an unwritable state dir
    /// are errors that say what went wrong.
    #[tokio::test]
    async fn import_failures() {
        let d = tempfile::tempdir().unwrap();
        let (url, _) = fake_ha(json!([]), |_, _| async {}).await;
        let bad = config(d.path(), &url, "wrong-token");
        assert!(import_settings(&bad, false, t0()).await.unwrap_err().0.contains("authentication failed"));
        let mut no_ha = bad.clone();
        no_ha.ha = None;
        assert!(import_settings(&no_ha, false, t0()).await.unwrap_err().0.contains("[ha]"));
        // A server that authenticates but answers get_states with an error.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for answer in ["error", "silent"] {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                use crate::ha::ws::tests::push;
                push(&mut ws, json!({"type": "auth_required"})).await;
                recv(&mut ws).await.unwrap();
                push(&mut ws, json!({"type": "auth_ok"})).await;
                let gs = recv(&mut ws).await.unwrap();
                if answer == "error" {
                    push(&mut ws, json!({"type": "event", "id": 9})).await;
                    push(&mut ws, json!({"id": gs["id"], "type": "result", "success": false, "error": "nee"})).await;
                } else {
                    // No answer: wait until the client gives up and hangs up.
                    let _ = recv(&mut ws).await;
                }
            }
        });
        let ha = HaConfig { start_wait_s: 1, ..bad.ha.clone().unwrap() };
        let ha = HaConfig { url: format!("ws://{addr}/api/websocket"), token_file: d.path().join("token"), ..ha };
        let e = fetch(&ha, t0(), Duration::from_secs(5)).await.unwrap_err().0;
        assert!(e.contains("get_states failed") && e.contains("nee"), "{e}");
        let e = fetch(&ha, t0(), Duration::from_millis(100)).await.unwrap_err().0;
        assert!(e.contains("no answer"), "{e}");
        server.await.unwrap();
        let file = d.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(write(&file.join("sub"), &Settings::default(), &Memory::default()).is_err());
    }
}
