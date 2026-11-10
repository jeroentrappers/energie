use std::time::Duration;

use tokio_modbus::ExceptionCode;

use super::*;
use crate::sim::proxy::TcpProxy;
use crate::sim::solis::SolisSim;

/// Every block on every poll, no frame gap: the register-level tests see each change at once.
fn cfg(addr: SocketAddr) -> SolisConfig {
    SolisConfig {
        timeout: Duration::from_millis(500),
        frame_gap: Duration::ZERO,
        settings_every: 1,
        ..SolisConfig::new(addr, 1)
    }
}

async fn driver(sim: &SolisSim) -> Solis {
    Solis::tcp(cfg(sim.addr))
}

/// The live registers of 2026-11-07 decode to the HA states of that moment: SoC 100 %, PV 227 W,
/// RC force charge with 0 W, timeout 10 min, feed-in limit on (43073 = 16), grid charging off
/// (43110 = 17: self-use + reserve), backflow 5500 W (43074 = 55).
#[tokio::test]
async fn live_registers_decode_to_ha_states() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    assert_eq!(s.name(), "solis");
    let Reading::Solis(r) = s.read().await.unwrap() else { panic!() };
    assert_eq!(
        r,
        SolisReading {
            soc: Some(100.0),
            battery_power_w: Some(0.0),
            pv_power_w: Some(227.0),
            rc_mode: Some(RcMode::ForceCharge),
            rc_charge_w: Some(0.0),
            rc_discharge_w: Some(0.0),
            rc_timeout_min: Some(10.0),
            allow_grid_charge: Some(false),
            feed_in_limit_on: Some(true),
            backflow_w: Some(5500.0),
        }
    );
    assert_eq!(s.unavailable(), Reading::Solis(SolisReading::default()));
}

/// `battery_power_net`: 33149-33150 as S32, negated when 33135 (direction) is 0 = charging.
#[tokio::test]
async fn battery_power_sign_follows_direction_register() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.set_battery(-1500);
    assert_eq!(s.poll().await.unwrap().battery_power_w, Some(-1500.0));
    sim.set_battery(2400);
    assert_eq!(s.poll().await.unwrap().battery_power_w, Some(2400.0));
    sim.set_battery(70_000);
    assert_eq!(s.poll().await.unwrap().battery_power_w, Some(70_000.0));
}

/// Total PV power is a signed 32-bit pair, high word first.
#[tokio::test]
async fn pv_power_is_s32_high_word_first() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.set_pv(70_000);
    assert_eq!(s.poll().await.unwrap().pv_power_w, Some(70_000.0));
    assert_eq!(s32(0xFFFF, 0xFFFE), -2);
}

/// Spike filter on 33139: a 0 or 100 SoC is only believed on the third poll in a row; until
/// then the previous value is reported. Other values pass at once.
#[tokio::test]
async fn soc_spikes_need_three_polls() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.set_input(reg::SOC, 57);
    assert_eq!(s.poll().await.unwrap().soc, Some(57.0));
    sim.set_input(reg::SOC, 0);
    assert_eq!(s.poll().await.unwrap().soc, Some(57.0));
    assert_eq!(s.poll().await.unwrap().soc, Some(57.0));
    assert_eq!(s.poll().await.unwrap().soc, Some(0.0));
    assert_eq!(s.poll().await.unwrap().soc, Some(0.0));
    sim.set_input(reg::SOC, 58);
    assert_eq!(s.poll().await.unwrap().soc, Some(58.0));
}

/// An RC mode value outside 0/1/2 matches no select option: `None` (HA shows unknown).
#[tokio::test]
async fn unknown_rc_mode_is_none() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.set_holding(reg::RC_MODE, 7);
    assert_eq!(s.poll().await.unwrap().rc_mode, None);
    sim.set_holding(reg::RC_MODE, 0);
    assert_eq!(s.poll().await.unwrap().rc_mode, Some(RcMode::None));
    sim.set_holding(reg::RC_MODE, 2);
    assert_eq!(s.poll().await.unwrap().rc_mode, Some(RcMode::ForceDischarge));
}

/// `select_sensors.py` companion writes: selecting a mode writes 43135, then re-writes 43136,
/// 43129 and 43282 from cache so the setpoints and timeout latch. The simulator's battery then
/// follows the mode.
#[tokio::test]
async fn rc_mode_select_writes_companions_from_cache() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::RC_DISCHARGE, 250);
    let mut s = driver(&sim).await;
    s.poll().await.unwrap();
    assert_eq!(s.apply(&Command::SolisRcMode(RcMode::ForceDischarge)).await.unwrap(), Applied::Done);
    assert_eq!(
        sim.writes(),
        vec![(reg::RC_MODE, 2), (reg::RC_CHARGE, 0), (reg::RC_DISCHARGE, 250), (reg::RC_TIMEOUT, 10)]
    );
    let r = s.poll().await.unwrap();
    assert_eq!(r.rc_mode, Some(RcMode::ForceDischarge));
    assert_eq!(r.battery_power_w, Some(2500.0));
}

/// Without a cached value a power companion is skipped (`Skipping companion write ... no cached
/// value`); the RC timeout is not: with nothing cached the fallback (10 min) is written, so the
/// forced mode times out. The select itself writes even when the mode is already set.
#[tokio::test]
async fn rc_mode_without_cache_writes_the_mode_and_a_timeout() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    s.apply(&Command::SolisRcMode(RcMode::ForceCharge)).await.unwrap();
    s.apply(&Command::SolisRcMode(RcMode::None)).await.unwrap();
    assert_eq!(sim.writes(), vec![(reg::RC_MODE, 1), (reg::RC_TIMEOUT, 10), (reg::RC_MODE, 0), (reg::RC_TIMEOUT, 10)]);
}

/// The inverter already holds the mode: a forced mode is re-written alone (43135, the keepalive
/// its RC timeout needs), once per `rc_keepalive`; `None` writes nothing. A change of mode writes
/// the companions as HA does.
#[tokio::test]
async fn an_rc_mode_already_held_is_only_kept_alive() {
    let sim = SolisSim::start().await;
    let mut s = Solis::tcp(SolisConfig { rc_keepalive: Some(Duration::from_secs(300)), ..cfg(sim.addr) });
    s.poll().await.unwrap();
    let mut apply = async |m| s.apply(&Command::SolisRcMode(m)).await.unwrap();
    assert_eq!(apply(RcMode::ForceCharge).await, Applied::Done, "no write of ours yet: keep it alive");
    assert_eq!(apply(RcMode::ForceCharge).await, Applied::Unchanged, "kept alive 0 s ago");
    assert_eq!(apply(RcMode::None).await, Applied::Done);
    assert_eq!(apply(RcMode::None).await, Applied::Unchanged, "nothing to keep alive");
    assert_eq!(
        sim.writes(),
        vec![(reg::RC_MODE, 1), (reg::RC_MODE, 0), (reg::RC_CHARGE, 0), (reg::RC_DISCHARGE, 0), (reg::RC_TIMEOUT, 10)]
    );

    let sim = SolisSim::start().await;
    let mut s = Solis::tcp(SolisConfig { rc_keepalive: Some(Duration::ZERO), ..cfg(sim.addr) });
    s.poll().await.unwrap();
    for _ in 0..2 {
        s.apply(&Command::SolisRcMode(RcMode::ForceCharge)).await.unwrap();
    }
    assert_eq!(sim.writes(), vec![(reg::RC_MODE, 1), (reg::RC_MODE, 1)], "every keepalive is due");
}

/// Held, but with an invalid RC timeout: the full select, which writes the fallback timeout.
/// Without `rc_keepalive` (the default) every select writes the mode and its companions, as HA
/// does.
#[tokio::test]
async fn an_rc_mode_is_selected_again_for_a_bad_timeout_or_as_ha() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::RC_TIMEOUT, 0);
    let mut s = Solis::tcp(SolisConfig { rc_keepalive: Some(Duration::from_secs(300)), ..cfg(sim.addr) });
    s.poll().await.unwrap();
    s.apply(&Command::SolisRcMode(RcMode::ForceCharge)).await.unwrap();
    assert_eq!(
        sim.writes(),
        vec![(reg::RC_MODE, 1), (reg::RC_CHARGE, 0), (reg::RC_DISCHARGE, 0), (reg::RC_TIMEOUT, 10)]
    );

    let sim = SolisSim::start().await;
    let mut s = Solis::tcp(SolisConfig { rc_keepalive: None, ..cfg(sim.addr) });
    s.poll().await.unwrap();
    for m in [RcMode::None, RcMode::None] {
        s.apply(&Command::SolisRcMode(m)).await.unwrap();
    }
    assert_eq!(sim.writes().len(), 8);
}

/// Fail-safe: an RC timeout outside 1..30 (0 would never time out) is replaced by the fallback
/// when a mode is selected; a valid one is re-written as cached.
#[tokio::test]
async fn rc_mode_replaces_an_invalid_timeout() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::RC_TIMEOUT, 0);
    let mut s = Solis::tcp(SolisConfig { rc_timeout_fallback_min: 7, ..cfg(sim.addr) });
    s.poll().await.unwrap();
    s.apply(&Command::SolisRcMode(RcMode::ForceDischarge)).await.unwrap();
    assert_eq!(sim.holding(reg::RC_TIMEOUT), Some(7));
    sim.set_holding(reg::RC_TIMEOUT, 31);
    s.poll().await.unwrap();
    s.apply(&Command::SolisRcMode(RcMode::ForceDischarge)).await.unwrap();
    assert_eq!(sim.holding(reg::RC_TIMEOUT), Some(7));
    sim.set_holding(reg::RC_TIMEOUT, 30);
    s.poll().await.unwrap();
    s.apply(&Command::SolisRcMode(RcMode::ForceDischarge)).await.unwrap();
    assert_eq!(sim.holding(reg::RC_TIMEOUT), Some(30));
}

/// The pacing HA's polling sets (fast 15 s for the measurements, slow 300 s for the settings):
/// the holding blocks are read on the first poll, then every `settings_every` polls, and on the
/// poll after a write (the read-back); the input blocks every poll. Between settings reads the
/// cache answers, so 3 polls of 4 cost 2 requests instead of 6.
#[tokio::test]
async fn settings_are_read_every_n_polls_and_after_a_write() {
    let sim = SolisSim::start().await;
    let mut s = Solis::tcp(SolisConfig { settings_every: 4, ..cfg(sim.addr) });
    s.poll().await.unwrap();
    sim.set_holding(reg::BACKFLOW, 10);
    sim.set_pv(1234);
    let r = s.poll().await.unwrap();
    assert_eq!((r.pv_power_w, r.backflow_w), (Some(1234.0), Some(5500.0)), "inputs fresh, settings cached");
    s.poll().await.unwrap();
    s.poll().await.unwrap();
    assert_eq!(s.poll().await.unwrap().backflow_w, Some(1000.0), "fifth poll: settings due again");
    sim.set_holding(reg::RC_CHARGE, 7);
    assert_eq!(s.poll().await.unwrap().rc_charge_w, Some(0.0));
    s.apply(&Command::SolisRcTimeoutMin(12)).await.unwrap();
    assert_eq!(s.poll().await.unwrap().rc_charge_w, Some(70.0), "the read-back after a write");
    assert_eq!(s.poll().await.unwrap().rc_timeout_min, Some(12.0));
}

/// `execute` reports every register write with the cached value it replaced; `plan` (the dry
/// run) reports the same writes and leaves the inverter and the cache alone.
#[tokio::test]
async fn plan_reports_the_writes_without_writing() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::RC_DISCHARGE, 250);
    let mut s = driver(&sim).await;
    s.poll().await.unwrap();
    let cmd = Command::SolisRcMode(RcMode::ForceDischarge);
    let plan = s.plan(&cmd).await.unwrap();
    assert!(sim.writes().is_empty());
    assert_eq!(s.cached(reg::RC_MODE), Some(1));
    let w = |r: u16, v: u16, b: u16| Write::new(format!("holding {r}"), v).before(Some(b));
    let want = vec![w(43135, 2, 1), w(43136, 0, 0), w(43129, 250, 250), w(43282, 10, 10)];
    assert_eq!(plan, Outcome { applied: Applied::Done, writes: want.clone() });
    assert_eq!(
        serde_json::to_value(&plan.writes[0]).unwrap(),
        serde_json::json!({"target": "holding 43135", "value": 2, "before": 1})
    );
    assert_eq!(s.execute(&cmd).await.unwrap(), plan);
    assert_eq!(sim.writes().len(), 4);
    // A number already in place, a bit already set: nothing to write.
    let same = s.plan(&Command::SolisRcTimeoutMin(10)).await.unwrap();
    assert_eq!(same, Outcome { applied: Applied::Unchanged, writes: vec![] });
    assert_eq!(s.plan(&Command::SolisFeedInLimit(true)).await.unwrap().applied, Applied::Unchanged);
    let grid = s.plan(&Command::SolisAllowGridCharge(true)).await.unwrap();
    assert_eq!(grid.writes, vec![w(43110, 49, 17)]);
    assert!(matches!(s.plan(&Command::SolisBackflowW(30_000)).await, Err(DriverError::OutOfRange(_))));
    assert_eq!(s.plan(&Command::CableStop).await.unwrap().applied, Applied::NotMine);
    assert_eq!(s.execute(&Command::CableStop).await.unwrap().applied, Applied::NotMine);
    let rc = s.plan(&Command::SolisReconnect).await.unwrap();
    assert_eq!(rc.writes, vec![Write::new("modbus session", "reconnect")]);
    assert_eq!(sim.writes().len(), 4, "the dry run never writes");
}

/// A dry run of a switch with nothing cached reads the register (a read, never a write).
#[tokio::test]
async fn plan_of_an_uncached_switch_reads_live() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::FEED_IN_SWITCH, 0);
    let mut s = driver(&sim).await;
    let p = s.plan(&Command::SolisFeedInLimit(true)).await.unwrap();
    assert_eq!(p.writes, vec![Write::new("holding 43073", 16).before(Some(0))]);
    assert!(sim.writes().is_empty());
}

/// Numbers write `round(value / step)` with Python's ties-to-even: 1505 W → 150, 1515 W → 152,
/// backflow 5550 W → 56, 5450 W → 54, 5449 W → 54; the timeout is written as-is.
#[tokio::test]
async fn number_writes_round_half_to_even() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    for (cmd, reg, raw) in [
        (Command::SolisRcChargeW(1505), reg::RC_CHARGE, 150),
        (Command::SolisRcChargeW(1515), reg::RC_CHARGE, 152),
        (Command::SolisRcDischargeW(3000), reg::RC_DISCHARGE, 300),
        (Command::SolisBackflowW(5550), reg::BACKFLOW, 56),
        (Command::SolisBackflowW(5450), reg::BACKFLOW, 54),
        (Command::SolisBackflowW(5449), reg::BACKFLOW, 54),
        (Command::SolisRcTimeoutMin(15), reg::RC_TIMEOUT, 15),
    ] {
        assert_eq!(s.apply(&cmd).await.unwrap(), Applied::Done, "{cmd:?}");
        assert_eq!(sim.holding(reg), Some(raw), "{cmd:?}");
    }
    assert_eq!(s.cached(reg::RC_TIMEOUT), Some(15));
}

/// A number already showing the requested value is not written (`if native_value == value: return`).
#[tokio::test]
async fn unchanged_number_is_not_written() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    s.poll().await.unwrap();
    assert_eq!(s.apply(&Command::SolisBackflowW(5500)).await.unwrap(), Applied::Unchanged);
    assert_eq!(s.apply(&Command::SolisRcTimeoutMin(10)).await.unwrap(), Applied::Unchanged);
    assert!(sim.writes().is_empty());
}

/// Values outside the HA entity's min/max are refused before anything is written.
#[tokio::test]
async fn out_of_range_numbers_are_refused() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    for cmd in [Command::SolisRcTimeoutMin(0), Command::SolisRcTimeoutMin(31), Command::SolisBackflowW(20_100)] {
        assert!(matches!(s.apply(&cmd).await, Err(DriverError::OutOfRange(_))), "{cmd:?}");
    }
    assert!(sim.writes().is_empty());
}

/// Switches flip one bit of a shared register and keep the others: grid charge on turns
/// 43110 = 17 into 49 and back; feed-in limit off turns 43073 = 16 into 0. A bit already in
/// place is not written.
#[tokio::test]
async fn switches_read_modify_write_one_bit() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    s.poll().await.unwrap();
    assert_eq!(s.apply(&Command::SolisAllowGridCharge(true)).await.unwrap(), Applied::Done);
    assert_eq!(sim.holding(reg::STORAGE_CONTROL), Some(49));
    assert_eq!(s.apply(&Command::SolisAllowGridCharge(true)).await.unwrap(), Applied::Unchanged);
    s.apply(&Command::SolisAllowGridCharge(false)).await.unwrap();
    assert_eq!(sim.holding(reg::STORAGE_CONTROL), Some(17));
    s.apply(&Command::SolisFeedInLimit(false)).await.unwrap();
    assert_eq!(sim.holding(reg::FEED_IN_SWITCH), Some(0));
    assert_eq!(s.poll().await.unwrap().feed_in_limit_on, Some(false));
}

/// With an empty cache the switch reads the register live first (issue #402: never start the
/// read-modify-write from 0).
#[tokio::test]
async fn switch_without_cache_reads_live_value_first() {
    let sim = SolisSim::start().await;
    sim.set_holding(reg::STORAGE_CONTROL, 0b1000_0001);
    let mut s = driver(&sim).await;
    s.apply(&Command::SolisAllowGridCharge(true)).await.unwrap();
    assert_eq!(sim.holding(reg::STORAGE_CONTROL), Some(0b1010_0001));
    assert_eq!(sim.writes().len(), 1);
}

/// An exception response fails the read with its code and keeps the session usable.
#[tokio::test]
async fn modbus_exception_fails_the_read() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.fail_next(ExceptionCode::ServerDeviceBusy);
    assert!(matches!(s.poll().await, Err(DriverError::ModbusException(6))));
    assert!(s.poll().await.is_ok());
    sim.remove_holding(reg::RC_TIMEOUT);
    assert!(matches!(s.poll().await, Err(DriverError::ModbusException(2))));
}

/// A rejected write leaves the cache alone, so the next attempt is not skipped as unchanged.
#[tokio::test]
async fn failed_write_does_not_update_cache() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.fail_next(ExceptionCode::IllegalDataValue);
    assert!(s.apply(&Command::SolisRcTimeoutMin(20)).await.is_err());
    assert_eq!(s.cached(reg::RC_TIMEOUT), None);
    assert_eq!(s.apply(&Command::SolisRcTimeoutMin(20)).await.unwrap(), Applied::Done);
}

/// When the gateway drops the TCP session, the next poll reconnects transparently (one retry on
/// a reused connection).
#[tokio::test]
async fn reconnects_after_gateway_drops_connection() {
    let sim = SolisSim::start().await;
    let proxy = TcpProxy::start(sim.addr).await;
    let mut s = Solis::tcp(cfg(proxy.addr));
    s.poll().await.unwrap();
    proxy.drop_all();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(s.poll().await.is_ok());
    assert_eq!(proxy.open(), 1);
}

/// `SolisReconnect` drops the session; the next poll opens a fresh one.
#[tokio::test]
async fn reconnect_command_opens_a_new_session() {
    let sim = SolisSim::start().await;
    let proxy = TcpProxy::start(sim.addr).await;
    let mut s = Solis::tcp(cfg(proxy.addr));
    s.poll().await.unwrap();
    assert_eq!(s.apply(&Command::SolisReconnect).await.unwrap(), Applied::Done);
    s.poll().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(proxy.open(), 1);
}

/// A gateway that accepts but never answers times out; once it answers again the driver
/// recovers on a new connection.
#[tokio::test]
async fn silent_gateway_times_out_then_recovers() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    sim.set_stall(true);
    assert!(matches!(s.poll().await, Err(DriverError::Timeout)));
    sim.set_stall(false);
    assert!(s.poll().await.is_ok());
}

/// Nothing listening: the connect fails with an I/O error.
#[cfg(unix)] // Windows retries a refused connect until the timeout
#[tokio::test]
async fn unreachable_gateway() {
    let mut s = Solis::tcp(cfg("127.0.0.1:9".parse().unwrap()));
    assert!(matches!(s.poll().await, Err(DriverError::Io(_))));
}

/// The configured gap between frames is respected (the Solis wants > 300 ms).
#[tokio::test]
async fn frames_are_spaced() {
    let sim = SolisSim::start().await;
    let mut s = Solis::tcp(SolisConfig { frame_gap: Duration::from_millis(30), ..cfg(sim.addr) });
    let t0 = Instant::now();
    s.poll().await.unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(150), "6 blocks, 5 gaps");
    let d = SolisConfig::new(sim.addr, 1);
    assert_eq!((d.frame_gap, d.timeout), (Duration::from_millis(310), Duration::from_secs(5)));
    assert_eq!((d.settings_every, d.rc_timeout_fallback_min), (4, 10));
}

/// Commands for other devices are ignored.
#[tokio::test]
async fn other_commands_are_not_mine() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    assert_eq!(s.apply(&Command::CableStop).await.unwrap(), Applied::NotMine);
}

/// A transport that returns fewer registers than asked, or nothing at all.
struct Short;

#[async_trait]
impl ModbusTransport for Short {
    async fn read_input(&mut self, _: u16, _: u16) -> Result<Vec<u16>> {
        Ok(vec![1])
    }
    async fn read_holding(&mut self, _: u16, _: u16) -> Result<Vec<u16>> {
        Ok(vec![])
    }
    async fn write_single(&mut self, _: u16, _: u16) -> Result<()> {
        Ok(())
    }
    async fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A short block is refused rather than mis-assigned; an empty live read never leads to a
/// write that would clear the other bits.
#[tokio::test]
async fn short_reads_are_refused() {
    let mut s = Solis::new(Short);
    assert!(matches!(s.poll().await, Err(DriverError::Malformed(_))));
    assert!(matches!(s.apply(&Command::SolisFeedInLimit(true)).await, Err(DriverError::Malformed(_))));
    assert_eq!(s.apply(&Command::SolisReconnect).await.unwrap(), Applied::Done);
    assert!(s.write_single_for_test().await.is_ok());
}

impl Solis<Short> {
    async fn write_single_for_test(&mut self) -> Result<()> {
        self.transport.write_single(1, 1).await
    }
}

fn value(es: &[Entity], key: &str) -> crate::entity::Value {
    es.iter().find(|e| e.key == key).map(|e| e.value.clone()).expect(key)
}

/// The values HA's `solis_modbus` entities show beyond the reading, decoded as the component
/// does (U32 kWh totals, ×0.1 daily kWh, S16 ×0.1 A battery current, ×100 W peak power), with
/// the HA entity each replaces.
#[tokio::test]
async fn extra_entities_decode_like_the_component() {
    use crate::entity::Value::{Bool, Null, Number, Text};
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    assert!(s.entities().iter().all(|e| e.value == Null), "nothing read yet");
    s.poll().await.unwrap();
    let es = s.entities();
    for (key, want) in [
        ("pv_total_energy_kwh", Number(9120.0)),
        ("pv_today_energy_kwh", Number(87.0 * 0.1)),
        ("battery_voltage_v", Number(528.0 * 0.1)),
        ("battery_current_a", Number(-25.0 * 0.1)),
        ("battery_soh_pct", Number(99.0)),
        ("battery_charge_total_kwh", Number(3100.0)),
        ("battery_charge_today_kwh", Number(42.0 * 0.1)),
        ("battery_discharge_total_kwh", Number(2900.0)),
        ("battery_discharge_today_kwh", Number(31.0 * 0.1)),
        ("grid_import_total_kwh", Number(4200.0)),
        ("grid_import_today_kwh", Number(55.0 * 0.1)),
        ("grid_export_total_kwh", Number(7000.0)),
        ("grid_export_today_kwh", Number(12.0 * 0.1)),
        ("overdischarge_soc_pct", Number(20.0)),
        ("force_charge_soc_pct", Number(10.0)),
        ("backup_soc_pct", Number(30.0)),
        ("work_mode", Text("Self-Use + Reserve/Backup".into())),
        ("storage_control_raw", Number(17.0)),
        ("self_use_mode", Bool(true)),
        ("reserve_battery_mode", Bool(true)),
        ("battery_forcecharge_peakshaving", Bool(false)),
        ("peak_shaving_mode", Bool(false)),
        ("grid_peak_shaving_power_enable", Bool(true)),
        ("peak_baseline_soc_pct", Number(50.0)),
        ("peak_max_grid_power_w", Number(4000.0)),
    ] {
        assert_eq!(value(&es, key), want, "{key}");
    }
    let keys: Vec<_> = es.iter().map(|e| e.key.as_str()).collect();
    let mapped: Vec<_> = extra::HA_ENTITIES.iter().map(|(k, _)| *k).collect();
    assert_eq!(keys, mapped, "every extra entity has its HA entity id");
    assert!(es.iter().zip(extra::HA_ENTITIES).all(|(e, (_, ha))| e.ha_entity.as_deref() == Some(ha)));
    assert!(extra::HA_ENTITIES.iter().all(|(_, ha)| ha.ends_with("_2") && ha.contains(".solis_s6_eh1p_")));
    let e = es.iter().find(|e| e.key == "pv_total_energy_kwh").unwrap();
    assert_eq!((e.class.as_deref(), e.unit.as_deref()), (Some("energy"), Some("kWh")));
}

/// `current_option` of the Storage Mode select: the more specific option first, a strict pass
/// (no other mode bit set), then a lenient one; nothing set is no option.
#[test]
fn work_mode_follows_the_select() {
    for (raw, want) in [
        (17, Some("Self-Use + Reserve/Backup")),
        (1, Some("Self-Use")),
        (33, Some("Self-Use")),
        (35, Some("Self-Use + TOU")),
        (19, Some("Self-Use + TOU + Reserve/Backup")),
        (64, Some("Feed-in Priority")),
        (80, Some("Feed-in Priority + Reserve/Backup")),
        (4, Some("Off-Grid Operation")),
        (2080, Some("Peak Shaving")),
        (2049, Some("Self-Use")),
        (32, None),
    ] {
        assert_eq!(extra::work_mode(raw), want, "{raw}");
    }
}

/// The slow blocks (energies) are read with every fifth settings read; the measurement and
/// settings blocks keep their cadence.
#[tokio::test]
async fn energies_are_read_on_the_slow_cycle() {
    let sim = SolisSim::start().await;
    let mut s = driver(&sim).await;
    s.poll().await.unwrap();
    sim.set_input(33035, 90);
    sim.set_holding(43487, 60);
    for _ in 0..4 {
        s.poll().await.unwrap();
    }
    let es = s.entities();
    assert_eq!(value(&es, "pv_today_energy_kwh"), crate::entity::Value::Number(87.0 * 0.1), "not due yet");
    assert_eq!(value(&es, "peak_baseline_soc_pct"), crate::entity::Value::Number(60.0));
    s.poll().await.unwrap();
    assert_eq!(value(&s.entities(), "pv_today_energy_kwh"), crate::entity::Value::Number(90.0 * 0.1));
}

/// A block the inverter refuses blanks only its own entities and is tried again; the reading
/// the logic needs is unaffected.
#[tokio::test]
async fn a_refused_extra_block_blanks_only_itself() {
    let sim = SolisSim::start().await;
    sim.remove_holding(43488);
    let mut s = driver(&sim).await;
    let r = s.poll().await.unwrap();
    assert_eq!(r.soc, Some(100.0));
    let es = s.entities();
    assert_eq!(value(&es, "peak_baseline_soc_pct"), crate::entity::Value::Null);
    assert_eq!(value(&es, "backup_soc_pct"), crate::entity::Value::Number(30.0));
    assert_eq!(value(&es, "pv_total_energy_kwh"), crate::entity::Value::Number(9120.0));
    sim.set_holding(43488, 40);
    s.poll().await.unwrap();
    assert_eq!(value(&s.entities(), "peak_max_grid_power_w"), crate::entity::Value::Number(4000.0));
}

/// A transport that answers the core blocks and fails the extras with `fail` (and counts).
struct ExtraFail(DriverError, usize);

#[async_trait]
impl ModbusTransport for ExtraFail {
    async fn read_input(&mut self, addr: u16, count: u16) -> Result<Vec<u16>> {
        if addr == 33029 {
            self.1 += 1;
            return Err(std::mem::replace(&mut self.0, DriverError::Timeout));
        }
        Ok(vec![0; usize::from(count)])
    }
    async fn read_holding(&mut self, addr: u16, count: u16) -> Result<Vec<u16>> {
        if addr == 43011 {
            self.1 += 1;
            return Ok(vec![1]);
        }
        Ok(vec![0; usize::from(count)])
    }
    async fn write_single(&mut self, _: u16, _: u16) -> Result<()> {
        Ok(())
    }
    async fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A short extra block counts as refused; a transport error stops the extras for this poll
/// (the last slow block is not tried) without failing the reading.
#[tokio::test]
async fn extra_transport_errors_end_the_extras() {
    let mut s = Solis::new(ExtraFail(DriverError::Timeout, 0));
    assert!(s.poll().await.is_ok());
    assert_eq!(s.transport.1, 2, "43011 short, 33029 timed out, 33161 skipped");
    assert_eq!(value(&s.entities(), "backup_soc_pct"), crate::entity::Value::Null);
    assert_eq!(value(&s.entities(), "battery_charge_total_kwh"), crate::entity::Value::Null);
}
