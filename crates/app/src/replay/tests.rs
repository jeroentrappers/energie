//! End-to-end replays of small synthetic recorder exports.

use super::*;
use crate::replay::acts::ActReport;
use history::read_all;

/// 2026-11-07 13:00:00 Brussels.
const T0: f64 = 1_794_052_800.0;

/// A CSV row at `T0 + dt`.
fn row(entity: &str, state: &str, dt: f64) -> String {
    format!("{entity},{state},{},\n", T0 + dt)
}

/// A steady house: 1500 W PV, 500 W import, battery idle; HA's `PV vermogen totaal` shows
/// 1500 until `wrong_from`, then 9999 (a disagreement); the Buzz override goes 6 → 10 at 30 s.
fn house(secs: u32, wrong_from: u32) -> String {
    let mut s = String::new();
    s += &row("sun.sun", "above_horizon", -1.0);
    s += &row("sensor.solis_s6_eh1p_total_pv_power_2", "1500", -1.0);
    s += &row("sensor.solis_s6_eh1p_battery_power_net_2", "0", -1.0);
    s += &row("sensor.solis_s6_eh1p_battery_soc_2", "50", -1.0);
    s += &row("sensor.pv_energie_totaal", "100.0", -1.0);
    s += &row("number.smartevse_1234_charge_current_override", "6", -1.0);
    s += &row("input_boolean.batterij_net_laden", "off", -1.0);
    s += "number.smartevse_1234_charge_current_override,6,1794052799.5,\"{\"\"max\"\": 29}\"\n";
    s += &row("sensor.jeroen_average_electricity_price", "0.3", -1.0);
    s += "sensor.jeroen_average_electricity_price,0.3,1794052799.6,{}\n";
    for t in 0..secs {
        let dt = f64::from(t) + 0.2;
        s += &row("sensor.p1_meter_vermogen", if t % 2 == 0 { "500" } else { "501" }, dt);
        let pv = if t >= wrong_from { "9999" } else { "1500" };
        s += &row("sensor.pv_vermogen_totaal", pv, dt);
        if t == 30 {
            s += &row("number.smartevse_1234_charge_current_override", "10", dt);
        }
    }
    s
}

fn replay(csv: &str, cfg: ReplayConfig) -> Report {
    Replay::new(cfg).run(read_all(csv.as_bytes()).unwrap().into_iter().map(Ok)).unwrap()
}

fn field<'a>(r: &'a Report, f: &str) -> &'a fields::FieldReport {
    r.fields.iter().find(|x| x.field == f).unwrap()
}

fn act<'a>(r: &'a Report, k: &str) -> &'a ActReport {
    r.actuators.iter().find(|x| x.key == k).unwrap()
}

/// One step per second from the first row; nothing is compared while settling; a field
/// HA shows the same value for agrees, and the wrong value is a mismatch episode.
#[test]
fn replays_a_steady_house() {
    let cfg = ReplayConfig { settle_s: 5.0, trace: Some("pv_vermogen_totaal_w".into()), ..Default::default() };
    let r = replay(&house(60, 40), cfg);
    assert_eq!(r.trace.len(), 14);
    assert_eq!(r.trace[0].at, "2026-11-07 13:00:46");
    assert_eq!(r.trace[0].ha, vec![Val::Num(9999.0)]);
    // First row at T0 − 1 → the clock starts at T0 − 1 and stops after the last row.
    assert_eq!(r.steps, 61);
    assert_eq!(r.from, "2026-11-07 12:59:59");
    assert!(r.gaps.is_empty());
    let pv = field(&r, "pv_vermogen_totaal_w");
    assert_eq!(pv.skipped, 6, "T0−1 ..= T0+4 settle");
    assert_eq!(pv.samples, 55);
    // HA shows 9999 from T0+40.2; within ±5 s, T0+46 ..= T0+59 no longer see 1500.
    assert_eq!(pv.matches, 55 - 14);
    assert_eq!(pv.mismatch_episodes, 1);
    assert_eq!(pv.worst[0].ha, Val::Num(9999.0));
    assert_eq!(pv.worst[0].rust, Val::Num(1500.0));
    assert!(r.field_agreement_pct.is_some());
    // The override change is an HA-only actuation; the timeline is HA's adopted value.
    let o = act(&r, "evse.override_a");
    assert_eq!((o.ha_changes, o.ha_changes_matched), (1, 0));
    assert_eq!(o.ha_only[0].val, Val::Num(10.0));
    // The Buzz probe: HA's 6 → 10 against Rust's target (no plug: parked at 6 A).
    assert_eq!(r.buzz_target.samples, 1);
    assert_eq!(r.buzz_target.worst[0].rust, 6.0);
    // The latch is compared every second.
    assert!(act(&r, "latch.batterij_net_laden").timeline_s > 0);
}

/// `--from`/`--to` window the replay; rows before `from` only fill the table.
#[test]
fn from_and_to_window_the_replay() {
    let cfg = ReplayConfig { from: Some(ts(T0 + 10.0)), to: Some(ts(T0 + 20.0)), settle_s: 0.0, ..Default::default() };
    let r = replay(&house(60, 1000), cfg);
    assert_eq!(r.steps, 11);
    assert_eq!(r.from, "2026-11-07 13:00:10");
    assert_eq!(r.to, "2026-11-07 13:00:20");
    assert_eq!(field(&r, "pv_vermogen_totaal_w").agreement_pct, Some(100.0));
}

/// No row for longer than `gap_s` is HA down: the clock jumps, the engine restarts with
/// re-seeded meters, and the first `settle_s` after the gap are not compared.
#[test]
fn gaps_restart_the_engine() {
    let mut csv = house(20, 1000);
    csv += &row("sensor.pv_energie_totaal", "250.0", 200.0);
    for t in 200..230 {
        csv += &row("sensor.p1_meter_vermogen", "700", f64::from(t) + 0.5);
        csv += &row("sensor.pv_vermogen_totaal", "1500", f64::from(t) + 0.5);
    }
    let cfg = ReplayConfig { settle_s: 10.0, ..Default::default() };
    let r = replay(&csv, cfg);
    assert_eq!(r.gaps.len(), 1);
    assert_eq!(r.gaps[0].from, "2026-11-07 13:00:19");
    assert!((r.gaps[0].seconds - 180.8).abs() < 1e-6);
    // 21 steps before (T0−1 ..= T0+19), 30 after (T0+201 ..= T0+230).
    assert_eq!(r.steps, 21 + 30);
    let pv = field(&r, "pv_vermogen_totaal_w");
    assert_eq!(pv.skipped, 11 + 11);
    // The meter restarted from HA's restored 250 kWh: no adoption needed.
    assert_eq!(r.meter_adoptions, 0);
    let e = field(&r, "pv_energie_totaal_kwh");
    assert_eq!(e.agreement_pct, Some(100.0));
}

/// In lockstep a meter that HA moves away from Rust's (a manual reset) is re-adopted after
/// the hold time and counted; without lockstep it is not.
#[test]
fn lockstep_adopts_drifted_meters() {
    let mut csv = house(200, 1000);
    csv += &row("sensor.pv_energie_totaal", "50.0", 200.5);
    for t in 200..400 {
        csv += &row("sensor.p1_meter_vermogen", "500", f64::from(t) + 0.2);
    }
    let r = replay(&csv, ReplayConfig { settle_s: 0.0, ..Default::default() });
    assert_eq!(r.meter_adoptions, 1);
    assert_eq!(r.meter_adoptions_by_entity.get("sensor.pv_energie_totaal"), Some(&1));
    assert_eq!(r.meter_adoption_events[0].ha, 50.0);
    assert!(report::markdown(&r).contains("`sensor.pv_energie_totaal`: Rust 100."));
    let r = replay(&csv, ReplayConfig { settle_s: 0.0, lockstep: false, ..Default::default() });
    assert_eq!(r.meter_adoptions, 0);
    assert!(!r.lockstep);
}

/// Rust's commands are compared too: the default settings at 100 % SoC have the dispatcher
/// keep the RC mode; whatever it sends is counted as Rust acts.
#[test]
fn rust_commands_are_counted() {
    let mut csv = house(120, 1000);
    csv += &row("select.solis_s6_eh1p_rc_force_charge_discharge_2", "None", 0.1);
    let r = replay(&csv, ReplayConfig { settle_s: 0.0, ..Default::default() });
    let total: u64 = r.actuators.iter().map(|a| a.rust_acts).sum();
    assert!(total > 0, "{:?}", r.actuators);
}

/// An empty file and a malformed row are errors.
#[test]
fn errors() {
    let empty = Replay::new(ReplayConfig::default()).run(std::iter::empty());
    assert_eq!(empty.unwrap_err().0, "the history is empty");
    let bad = Replay::new(ReplayConfig::default()).run(std::iter::once(Err(Error::new("line 1: bad"))));
    assert_eq!(bad.unwrap_err().0, "line 1: bad");
    let mut rows = read_all(house(3, 1000).as_bytes()).unwrap().into_iter().map(Ok).collect::<Vec<_>>();
    rows.insert(5, Err(Error::new("line 6: bad")));
    let mid = Replay::new(ReplayConfig::default()).run(rows.into_iter());
    assert_eq!(mid.unwrap_err().0, "line 6: bad");
}

/// `run` reads the file and writes the JSON report and the markdown summary.
#[test]
fn run_writes_report_and_summary() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path().join("h.csv");
    std::fs::write(&h, house(30, 20)).unwrap();
    let cfg = ReplayConfig {
        history: h.clone(),
        report: Some(d.path().join("r.json")),
        summary: Some(d.path().join("r.md")),
        settle_s: 0.0,
        ..Default::default()
    };
    let r = run(cfg.clone()).unwrap();
    let json: Value = serde_json::from_str(&std::fs::read_to_string(d.path().join("r.json")).unwrap()).unwrap();
    assert_eq!(json["steps"], r.steps);
    assert!(std::fs::read_to_string(d.path().join("r.md")).unwrap().starts_with("# Replay 2026-11-07 12:59:59"));
    let bad = ReplayConfig { report: Some(d.path().join("no/such/dir.json")), ..cfg.clone() };
    assert!(run(bad).unwrap_err().0.contains("no/such/dir.json"));
    let bad = ReplayConfig { summary: Some(d.path().join("no/such/dir.md")), ..cfg.clone() };
    assert!(run(bad).unwrap_err().0.contains("no/such/dir.md"));
    assert!(run(ReplayConfig { history: d.path().join("nope"), ..cfg }).is_err());
}

/// Episodes keep their worst sample; spans are inclusive; times print in Brussels time.
#[test]
fn helpers() {
    let mut e = Episode::new(T0, Val::Num(1.0), Val::Num(2.0), Some(1.0));
    e.extend(T0 + 1.0, Val::Num(5.0), Val::Num(2.0), Some(3.0));
    e.extend(T0 + 2.0, Val::Num(3.0), Val::Num(2.0), Some(1.0));
    e.extend(T0 + 3.0, Val::Null, Val::Num(2.0), None);
    assert_eq!((e.duration_s, e.err, e.rust.clone()), (3.0, Some(3.0), Val::Num(5.0)));
    assert_eq!(e.worst_at, "2026-11-07 13:00:01");
    assert_eq!(e.end, "2026-11-07 13:00:03");
    assert_eq!(e.score(), 3.0);
    let mut n = Episode::new(T0, Val::Null, Val::Num(2.0), None);
    n.extend(T0 + 4.0, Val::Null, Val::Num(2.0), None);
    assert_eq!(n.score(), 4.0);
    let mut s = Spans::default();
    s.push(1.0, 2.0);
    assert!(s.contains(1.0) && s.contains(2.0) && !s.contains(2.5));
    assert_eq!(fmt_ts(T0), "2026-11-07 13:00:00");
    assert_eq!(epoch(ts(T0 + 0.25)), T0 + 0.25);
    assert_eq!(latch_values(&Memory::default()).len(), 6);
}

/// The summary lists gaps, adoptions, actuator examples, fields and their worst episodes.
#[test]
fn markdown_summary() {
    let mut csv = house(100, 60);
    csv += &row("sensor.pv_energie_totaal", "50.0", 100.5);
    for t in 100..300 {
        csv += &row("sensor.p1_meter_vermogen", "500", f64::from(t) + 0.2);
        if t % 10 == 0 {
            let v = format!("{}", 7 + t / 10);
            csv += &row("number.smartevse_1234_charge_current_override", &v, f64::from(t) + 0.3);
        }
    }
    csv += &row("sensor.p1_meter_vermogen", "500", 500.0);
    let r = replay(&csv, ReplayConfig { settle_s: 0.0, ..Default::default() });
    let md = report::markdown(&r);
    for want in [
        "Gaps (HA down",
        "adopted `sensor.pv_energie_totaal` ×1",
        "## Actuators",
        "### `evse.override_a`",
        "- HA only: 2026-11-07 13:00:30: 10",
        "HA-only changes in all",
        "## Fields",
        "`pv_vermogen_totaal_w`",
        "Never compared",
        "### `pv_vermogen_totaal_w`",
        "Rust 1500 vs HA 9999",
    ] {
        assert!(md.contains(want), "missing {want:?} in\n{md}");
    }
}

/// Rust-only acts beyond the examples are summarised; values of every kind print.
#[test]
fn markdown_rust_only_and_values() {
    use acts::ActComparator;
    let mut c = ActComparator::new(1.0, 0.0);
    for i in 0..20 {
        c.rust_act(
            T0 + f64::from(i * 10),
            crate::shadow::commands::Act { key: "evse.mode", val: Val::Text(format!("m{i}")) },
        );
    }
    c.rust_act(T0, crate::shadow::commands::Act { key: "airco.zolder", val: Val::Bool(true) });
    let r = Report {
        history: "h".into(),
        from: "a".into(),
        to: "b".into(),
        step_ms: 1000,
        lockstep: false,
        steps: 0,
        rows: 0,
        elapsed_s: 0.0,
        gaps: vec![],
        meter_adoptions: 0,
        meter_adoptions_by_entity: Default::default(),
        meter_adoption_events: vec![],
        field_agreement_pct: None,
        fields: vec![],
        actuators: c.report(T0, T0, &Spans::default()),
        buzz_target: Default::default(),
        buzz_mode: Default::default(),
        trace: vec![],
    };
    let md = report::markdown(&r);
    assert!(md.contains("lockstep off"));
    assert!(md.contains("Rust-only acts in all"), "{md}");
    assert!(md.contains("Rust only: 2026-11-07 13:00:00: true (HA unavailable)"), "{md}");
    assert!(md.contains("Rust only: 2026-11-07 13:00:00: m0"), "{md}");
}

/// The Buzz plugged and charging at night under `Max tot piek` with cheap power.
fn buzz_night(secs: u32) -> String {
    let mut s = String::new();
    for (e, v) in [
        ("sun.sun", "below_horizon"),
        ("input_select.ev_net_niveau", "Max tot piek"),
        ("input_select.ev_batterij_niveau", "Uit"),
        ("input_select.zonprioriteit", "Buzz → Batterij → EV3"),
        ("input_number.goedkoop_laden_prijs_drempel", "0.5"),
        ("sensor.jeroen_current_electricity_market_price", "0.3"),
        ("input_number.prijs_backup_nu", "-1"),
        ("input_boolean.pv_overschot_laden_actief", "on"),
        ("input_boolean.batterij_reservering_actief", "off"),
        ("sensor.solis_s6_eh1p_battery_soc_2", "100"),
        ("sensor.solis_s6_eh1p_battery_power_net_2", "0"),
        ("sensor.smartevse_1234_ev_plug_state", "Connected"),
        ("sensor.smartevse_1234_state", "Charging"),
        ("sensor.smartevse_1234_stateid", "C"),
        ("select.smartevse_1234_mode", "Normal"),
        ("sensor.smartevse_1234_charge_current", "10"),
        ("sensor.p1_meter_piekverbruik_huidige_maand", "3900"),
        ("sensor.p1_meter_stroom_fase_1", "10"),
        ("sensor.p1_meter_stroom_fase_2", "10"),
        ("sensor.p1_meter_stroom_fase_3", "10"),
    ] {
        s += &row(e, v, -2.0);
    }
    s += "number.smartevse_1234_charge_current_override,10,1794052798,\"{\"\"max\"\": 29}\"\n";
    s += "sensor.p1_meter_kwartier_import,0.1,1794052798,\"{\"\"last_reset\"\": \"\"2026-11-07T12:00:00+00:00\"\"}\"\n";
    for t in 0..secs {
        s += &row("sensor.p1_meter_vermogen", if t % 2 == 0 { "2500" } else { "2501" }, f64::from(t) + 0.1);
    }
    s
}

/// HA's override and mode changes are compared with what Rust's `ev_laden_buzz` would set on
/// the state just before them; the charger's own answer (its charge current, a few ms before
/// the override echo) is rolled back for that.
#[test]
fn probes_the_buzz_at_has_decisions() {
    let mut csv = buzz_night(40);
    // At 20.5 s HA commands 6 A: the charger answers at 20.6, the echo follows at 20.7.
    csv += &row("sensor.smartevse_1234_charge_current", "6", 20.6);
    csv += &row("sensor.ev_laadvermogen_smartevse_w", "1380", 20.65);
    csv += &row("sensor.huisverbruik_excl_stuurbaar", "100", 20.66);
    csv += &row("number.smartevse_1234_charge_current_override", "6", 20.7);
    csv += &row("number.smartevse_1234_charge_current_override", "6", 21.0);
    csv += &row("select.smartevse_1234_mode", "Pause", 30.5);
    let mut rows = read_all(csv.as_bytes()).unwrap();
    rows.sort_by(|a, b| a.ts.total_cmp(&b.ts));
    let r = Replay::new(ReplayConfig { settle_s: 0.0, ..Default::default() }).run(rows.into_iter().map(Ok)).unwrap();
    assert_eq!(r.buzz_target.samples, 1, "6 → 6 is no change");
    assert_eq!(r.buzz_target.worst.len(), 1);
    assert!(r.buzz_target.worst[0].rust > 6.0, "{:?}", r.buzz_target);
    assert_eq!(r.buzz_mode.samples, 1);
    assert_eq!(r.buzz_mode.examples[0].ha, "Pause");
    // The recorder has no `next_reset`: never compared.
    assert_eq!(field(&r, "kwartier_next_reset").samples, 0);
    let md = report::markdown(&r);
    assert!(md.contains("1 HA mode changes"), "{md}");
    assert!(md.contains("HA Pause vs Rust"), "{md}");
}

/// Rolled-back entities hide the table's; an entity absent before shows as absent.
#[test]
fn overlay_rolls_back() {
    let mut base = HashMap::new();
    base.insert("a".to_string(), EntityState::new("a", "new"));
    base.insert("b".to_string(), EntityState::new("b", "1"));
    let mut over = HashMap::new();
    over.insert("a".to_string(), Some(EntityState::new("a", "old")));
    over.insert("b".to_string(), None);
    let o = Overlay { base: &base, over: &over };
    assert_eq!(o.get("a").unwrap().state, "old");
    assert!(o.get("b").is_none());
    assert!(o.get("c").is_none());
    assert_eq!(o.all().count(), 1);
    assert!(is_readback("solis.rc_charge_w", "3330") && !is_readback("solis.rc_charge_w", "3331.0"));
    assert!(!is_readback("evse.override_a", "15"));
    assert!(is_evse("sensor.ev_laadvermogen_smartevse_w") && !is_evse("sensor.p1_meter_vermogen"));
    assert_eq!(mode_of(Command::EvseMode(energie_core::world::EvseMode::Pause)), Some("Pause"));
    assert_eq!(mode_of(Command::EvseCurrentDeciA(60)), None);
}
