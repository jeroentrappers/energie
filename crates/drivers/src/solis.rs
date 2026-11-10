//! Solis S6-EH1P hybrid inverter over Modbus TCP through the Waveshare RS485 gateway
//! (`solis_modbus` custom component, entities `*_solis_s6_eh1p_*_2`).
//!
//! Register numbers are used as-is as Modbus addresses: 3xxxx are input registers (function 4),
//! 4xxxx holding registers (function 3, written with function 6), exactly as the component does
//! (`is_holding = start_register >= 40000`). Map and encodings, from
//! `sensor_data/hybrid_sensors.py`, `switch_sensors.py`, `select_sensors.py` and the entity
//! classes under `sensors/`:
//!
//! | Entity | Register | Encoding |
//! | --- | --- | --- |
//! | `sensor.…_total_pv_power_2` | in 33057-33058 | S32 big-endian, W |
//! | `sensor.…_battery_soc_2` | in 33139 | U16 %, spike filter on 0/100 |
//! | `sensor.…_battery_power_net_2` | in 33149-33150 + 33135 | S32 W; 33135 = 0 (charging) negates |
//! | `select.…_rc_force_charge_discharge_2` | hold 43135 | 0 None / 1 Charge / 2 Discharge |
//! | `number.…_rc_force_battery_discharge_power_2` | hold 43129 | ×10 W |
//! | `number.…_rc_force_battery_charge_power_2` | hold 43136 | ×10 W |
//! | `number.…_rc_timeout_2` | hold 43282 | minutes, 1..30 |
//! | `switch.…_grid_feed_in_power_limit_switch_2` | hold 43073 bit 4 | read-modify-write |
//! | `number.…_backflow_power_2` | hold 43074 | ×100 W, 0..20000 |
//! | `switch.…_allow_grid_to_charge_the_battery_2` | hold 43110 bit 5 | read-modify-write |
//!
//! Write semantics copied from the component: selecting an RC mode writes 43135 and then
//! re-writes 43136, 43129 and 43282 from cache (the "companion writes" that make the setpoints
//! and timeout latch); a number write is skipped when the entity already shows that value; a
//! switch reads the register (cache, else a live read), flips one bit and writes only if the
//! register changed. Two deviations: a forced mode never goes without an RC timeout (a missing or
//! out-of-range 43282 is written as `rc_timeout_fallback_min`), and an RC mode the inverter
//! already holds is not selected again (see [`SolisConfig::rc_keepalive`]).
//!
//! Pacing as HA polls (`client_manager.py`, `data_retrieval.py`): 310 ms between frames, the
//! measurements every poll (15 s), the settings every `settings_every` polls (60 s) and after a
//! write, the [`extra`] energies every fifth settings read (300 s). `docs/CONTROL.md` has the
//! register map against the integration, line by line.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::{RcMode, Solis as SolisReading};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_modbus::client::{Context, Reader, Writer};
use tokio_modbus::prelude::Slave;

use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::entity::Entity;
use crate::error::{DriverError, Result};

/// Register numbers (= Modbus addresses).
pub mod reg {
    pub const PV_POWER: u16 = 33057;
    pub const BATTERY_VOLTAGE: u16 = 33133;
    pub const BATTERY_DIRECTION: u16 = 33135;
    pub const SOC: u16 = 33139;
    pub const BATTERY_POWER: u16 = 33149;
    pub const FEED_IN_SWITCH: u16 = 43073;
    pub const BACKFLOW: u16 = 43074;
    pub const STORAGE_CONTROL: u16 = 43110;
    pub const RC_DISCHARGE: u16 = 43129;
    pub const RC_MODE: u16 = 43135;
    pub const RC_CHARGE: u16 = 43136;
    pub const RC_TIMEOUT: u16 = 43282;
}

/// Bit of 43073 behind `switch.…_grid_feed_in_power_limit_switch_2`.
pub const FEED_IN_LIMIT_BIT: u8 = 4;
/// Bit of 43110 behind `switch.…_allow_grid_to_charge_the_battery_2` (set = allowed; the
/// component notes the protocol PDF says the opposite and every field capture disagrees).
pub const ALLOW_GRID_CHARGE_BIT: u8 = 5;

/// The blocks read per poll: (input?, start, count).
const BLOCKS: [(bool, u16, u16); 6] = [
    (true, reg::PV_POWER, 2),
    (true, reg::BATTERY_VOLTAGE, 18),
    (false, reg::FEED_IN_SWITCH, 2),
    (false, reg::STORAGE_CONTROL, 1),
    (false, reg::RC_DISCHARGE, 8),
    (false, reg::RC_TIMEOUT, 1),
];

/// Two registers, high word first, as a signed 32-bit value (`helpers.split_s32`).
pub fn s32(hi: u16, lo: u16) -> i32 {
    ((u32::from(hi) << 16) | u32::from(lo)) as i32
}

/// Python's `round()` on `value / step`: ties to even.
fn to_raw(value: u32, step: u32) -> u32 {
    let (q, r) = (value / step, value % step);
    if r * 2 > step || (r * 2 == step && q % 2 == 1) { q + 1 } else { q }
}

/// The Modbus operations the driver needs, so tests can swap the wire for a fake.
#[async_trait]
pub trait ModbusTransport: Send {
    async fn read_input(&mut self, addr: u16, count: u16) -> Result<Vec<u16>>;
    async fn read_holding(&mut self, addr: u16, count: u16) -> Result<Vec<u16>>;
    async fn write_single(&mut self, addr: u16, value: u16) -> Result<()>;
    /// Drop the session; the next operation opens a new one.
    async fn reconnect(&mut self) -> Result<()>;
}

/// How to reach the gateway and how hard to drive it.
#[derive(Debug, Clone)]
pub struct SolisConfig {
    /// `192.168.1.20:502` (config entry `host`/`port`).
    pub addr: SocketAddr,
    /// Modbus unit id (config entry `slave`, 1).
    pub slave: u8,
    /// Per-request timeout (`AsyncModbusTcpClient(timeout=5)`).
    pub timeout: Duration,
    /// Minimum gap between frames, reads and writes alike (`MIN_INTER_FRAME_MS = 310`).
    pub frame_gap: Duration,
    /// The holding (settings) blocks are read on every `settings_every`-th poll and on the poll
    /// after a write (the read-back); the input blocks on every poll.
    pub settings_every: u32,
    /// The RC timeout written with an RC mode when the inverter's own 43282 is missing or
    /// outside 1..30 min, so a forced mode always times out to self-use.
    pub rc_timeout_fallback_min: u16,
    /// HA selects the RC mode on every dispatcher run (30 s), and every select writes 43135 and
    /// re-writes 43136, 43129 and 43282: about 13 000 writes a day. With `Some(every)`, a mode
    /// the inverter already holds (and a valid cached timeout) is not selected again: `None`
    /// writes nothing, a forced mode re-writes only 43135, as the keepalive its RC timeout needs,
    /// at most every `every` and at least every half timeout. `None` (the default) keeps HA's
    /// behaviour: an S6-EH1P ignored the 43135-only keepalive and dropped to self-use.
    pub rc_keepalive: Option<Duration>,
}

impl SolisConfig {
    pub fn new(addr: SocketAddr, slave: u8) -> Self {
        SolisConfig {
            addr,
            slave,
            timeout: Duration::from_secs(5),
            frame_gap: Duration::from_millis(310),
            settings_every: 4,
            rc_timeout_fallback_min: 10,
            rc_keepalive: None,
        }
    }
}

enum Op {
    Input(u16, u16),
    Holding(u16, u16),
    Write(u16, u16),
}

/// Modbus TCP via tokio-modbus: connects lazily, reconnects after a transport error and retries
/// once when a reused connection turns out to be dead.
pub struct TcpModbus {
    cfg: SolisConfig,
    ctx: Option<Context>,
    last_frame: Option<Instant>,
}

impl TcpModbus {
    pub fn new(cfg: SolisConfig) -> Self {
        TcpModbus { cfg, ctx: None, last_frame: None }
    }

    async fn once(&mut self, op: &Op) -> Result<Vec<u16>> {
        if let Some(last) = self.last_frame {
            sleep_until(last + self.cfg.frame_gap).await;
        }
        if self.ctx.is_none() {
            let ctx = timeout(
                self.cfg.timeout,
                tokio_modbus::client::tcp::connect_slave(self.cfg.addr, Slave(self.cfg.slave)),
            );
            self.ctx = Some(ctx.await??);
        }
        let ctx = self.ctx.as_mut().expect("connected");
        let res = timeout(self.cfg.timeout, async {
            match *op {
                Op::Input(a, n) => ctx.read_input_registers(a, n).await,
                Op::Holding(a, n) => ctx.read_holding_registers(a, n).await,
                Op::Write(a, v) => ctx.write_single_register(a, v).await.map(|r| r.map(|()| Vec::new())),
            }
        })
        .await;
        self.last_frame = Some(Instant::now());
        match res {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(code))) => Err(DriverError::ModbusException(code.into())),
            Ok(Err(e)) => {
                self.ctx = None;
                Err(e.into())
            }
            Err(e) => {
                self.ctx = None;
                Err(e.into())
            }
        }
    }

    async fn exec(&mut self, op: Op) -> Result<Vec<u16>> {
        let reused = self.ctx.is_some();
        match self.once(&op).await {
            Err(DriverError::Modbus(_)) if reused => self.once(&op).await,
            r => r,
        }
    }
}

#[async_trait]
impl ModbusTransport for TcpModbus {
    async fn read_input(&mut self, addr: u16, count: u16) -> Result<Vec<u16>> {
        self.exec(Op::Input(addr, count)).await
    }
    async fn read_holding(&mut self, addr: u16, count: u16) -> Result<Vec<u16>> {
        self.exec(Op::Holding(addr, count)).await
    }
    async fn write_single(&mut self, addr: u16, value: u16) -> Result<()> {
        self.exec(Op::Write(addr, value)).await.map(drop)
    }
    async fn reconnect(&mut self) -> Result<()> {
        self.ctx = None;
        Ok(())
    }
}

/// The RC timeout range the inverter accepts (the HA entity's min/max).
const RC_TIMEOUT_RANGE: (u32, u32) = (1, 30);

/// The Solis driver: polls the register blocks, keeps the component's register cache and
/// carries out the `Solis*` commands.
pub struct Solis<T: ModbusTransport = TcpModbus> {
    transport: T,
    cache: HashMap<u16, u16>,
    soc_spikes: u32,
    polls: u32,
    /// Settings reads so far (every fifth also reads the slow extra blocks).
    settings_reads: u32,
    settings_every: u32,
    /// A write happened since the last holding read: read the settings back on the next poll.
    reread: bool,
    rc_timeout_fallback: u16,
    rc_keepalive: Option<Duration>,
    /// When 43135 was last written.
    rc_written: Option<Instant>,
}

impl Solis<TcpModbus> {
    /// Driver for the real gateway.
    pub fn tcp(cfg: SolisConfig) -> Self {
        let (every, fallback, keepalive) = (cfg.settings_every, cfg.rc_timeout_fallback_min, cfg.rc_keepalive);
        Solis {
            settings_every: every.max(1),
            rc_timeout_fallback: fallback,
            rc_keepalive: keepalive,
            ..Solis::new(TcpModbus::new(cfg))
        }
    }
}

/// A planned register write: register, new raw value, the cached value before.
type RegWrite = (u16, u16, Option<u16>);

impl<T: ModbusTransport> Solis<T> {
    /// A driver that reads every block on every poll and selects the RC mode as HA does.
    pub fn new(transport: T) -> Self {
        Solis {
            transport,
            cache: HashMap::new(),
            soc_spikes: 0,
            polls: 0,
            settings_reads: 0,
            settings_every: 1,
            reread: false,
            rc_timeout_fallback: 10,
            rc_keepalive: None,
            rc_written: None,
        }
    }

    /// The component's register cache (last read or written value per register).
    pub fn cached(&self, register: u16) -> Option<u16> {
        self.cache.get(&register).copied()
    }

    /// `data_retrieval.spike_filtering` for 33139: a 0 or 100 reading is only believed on the
    /// third consecutive poll; until then the previous value is kept.
    fn filter_soc(&mut self, value: u16) -> u16 {
        if value != 0 && value != 100 {
            self.soc_spikes = 0;
            return value;
        }
        self.soc_spikes += 1;
        if self.soc_spikes < 3 {
            return self.cached(reg::SOC).unwrap_or(value);
        }
        self.soc_spikes = 0;
        value
    }

    /// Poll the blocks that are due: the input blocks always, the holding blocks every
    /// `settings_every` polls, after a write, and until they have been read once. Any failed
    /// block fails the whole read.
    pub async fn poll(&mut self) -> Result<SolisReading> {
        let settings = self.reread
            || self.polls.is_multiple_of(self.settings_every)
            || BLOCKS.iter().any(|&(input, start, _)| !input && self.cached(start).is_none());
        self.polls = self.polls.wrapping_add(1);
        let mut fresh = HashMap::new();
        for (input, start, count) in BLOCKS {
            let values = if input {
                self.transport.read_input(start, count).await?
            } else if settings {
                self.transport.read_holding(start, count).await?
            } else {
                continue;
            };
            if values.len() != usize::from(count) {
                return Err(DriverError::Malformed(format!("{start}: {} of {count} registers", values.len())));
            }
            fresh.extend((start..).zip(values));
        }
        let soc = self.filter_soc(fresh[&reg::SOC]);
        fresh.insert(reg::SOC, soc);
        self.cache.extend(fresh);
        if settings {
            self.reread = false;
            self.poll_extra().await;
        }
        Ok(self.decode())
    }

    /// The extra blocks due with this settings read ([`extra`]). A block the inverter refuses
    /// blanks its own registers; a transport error ends the extras for this poll. Neither fails
    /// the reading.
    async fn poll_extra(&mut self) {
        let missing =
            |s: &Self, cycle| extra::BLOCKS.iter().any(|&(_, start, _, c)| c == cycle && s.cached(start).is_none());
        let slow = self.settings_reads.is_multiple_of(5) || missing(self, extra::Cycle::Slow);
        self.settings_reads = self.settings_reads.wrapping_add(1);
        for (input, start, count, cycle) in extra::BLOCKS {
            if cycle == extra::Cycle::Slow && !slow {
                continue;
            }
            let r = if input {
                self.transport.read_input(start, count).await
            } else {
                self.transport.read_holding(start, count).await
            };
            match r {
                Ok(values) if values.len() == usize::from(count) => self.cache.extend((start..).zip(values)),
                other => {
                    self.cache.retain(|a, _| !(start..start + count).contains(a));
                    if !matches!(other, Ok(_) | Err(DriverError::ModbusException(_))) {
                        return;
                    }
                }
            }
        }
    }

    fn decode(&self) -> SolisReading {
        let c = |r: u16| self.cache[&r];
        let battery = f64::from(s32(c(reg::BATTERY_POWER), c(reg::BATTERY_POWER + 1)));
        SolisReading {
            soc: Some(f64::from(c(reg::SOC))),
            battery_power_w: Some(if c(reg::BATTERY_DIRECTION) == 0 { 0.0 - battery } else { battery }),
            pv_power_w: Some(f64::from(s32(c(reg::PV_POWER), c(reg::PV_POWER + 1)))),
            rc_mode: match c(reg::RC_MODE) {
                0 => Some(RcMode::None),
                1 => Some(RcMode::ForceCharge),
                2 => Some(RcMode::ForceDischarge),
                _ => None,
            },
            rc_charge_w: Some(f64::from(c(reg::RC_CHARGE)) * 10.0),
            rc_discharge_w: Some(f64::from(c(reg::RC_DISCHARGE)) * 10.0),
            rc_timeout_min: Some(f64::from(c(reg::RC_TIMEOUT))),
            allow_grid_charge: Some(c(reg::STORAGE_CONTROL) >> ALLOW_GRID_CHARGE_BIT & 1 == 1),
            feed_in_limit_on: Some(c(reg::FEED_IN_SWITCH) >> FEED_IN_LIMIT_BIT & 1 == 1),
            backflow_w: Some(f64::from(c(reg::BACKFLOW)) * 100.0),
        }
    }

    /// One planned write, with what the cache holds there now.
    fn planned(&self, register: u16, value: u16) -> RegWrite {
        (register, value, self.cached(register))
    }

    /// `SolisNumberEntity.set_native_value`: skip when unchanged, else write `round(value / step)`.
    fn plan_number(&self, register: u16, value: u32, step: u32, range: (u32, u32)) -> Result<Vec<RegWrite>> {
        if value < range.0 || value > range.1 {
            return Err(DriverError::OutOfRange(format!("{value} for register {register}, allowed {range:?}")));
        }
        if self.cached(register).map(|raw| u32::from(raw) * step) == Some(value) {
            return Ok(Vec::new());
        }
        // Every range above keeps `value / step` within a u16.
        Ok(vec![self.planned(register, to_raw(value, step) as u16)])
    }

    /// `SolisSelectEntity.async_select_option` for an `on_value` option: write the mode, then
    /// the companion registers from cache. One deviation, for the fail-safe: a missing or
    /// out-of-range RC timeout is written as `rc_timeout_fallback` (HA skips a companion it has
    /// no cache for), so a forced mode never runs without a timeout. With `rc_keepalive`, a mode
    /// already held is only kept alive ([`SolisConfig::rc_keepalive`]).
    fn plan_rc_mode(&self, mode: RcMode) -> Vec<RegWrite> {
        let on_value = match mode {
            RcMode::None => 0,
            RcMode::ForceCharge => 1,
            RcMode::ForceDischarge => 2,
        };
        let (lo, hi) = RC_TIMEOUT_RANGE;
        let timeout = self.cached(reg::RC_TIMEOUT).filter(|t| (lo..=hi).contains(&u32::from(*t)));
        if let (Some(every), Some(minutes)) = (self.rc_keepalive, timeout)
            && self.cached(reg::RC_MODE) == Some(on_value)
        {
            let every = every.min(Duration::from_secs(u64::from(minutes) * 30));
            let alive = mode == RcMode::None || self.rc_written.is_some_and(|t| t.elapsed() < every);
            return if alive { Vec::new() } else { vec![self.planned(reg::RC_MODE, on_value)] };
        }
        let mut out = vec![self.planned(reg::RC_MODE, on_value)];
        for companion in [reg::RC_CHARGE, reg::RC_DISCHARGE] {
            if let Some(v) = self.cached(companion) {
                out.push(self.planned(companion, v));
            }
        }
        out.push(self.planned(reg::RC_TIMEOUT, timeout.unwrap_or(self.rc_timeout_fallback)));
        out
    }

    /// `SolisSwitchEntity.set_register_bit`: read-modify-write of one bit, written only when the
    /// register changes. The base is the cache, or a live read (never 0, issue #402).
    async fn plan_bit(&mut self, register: u16, bit: u8, on: bool) -> Result<Vec<RegWrite>> {
        let current = match self.cached(register) {
            Some(v) => v,
            None => {
                let v = *self.transport.read_holding(register, 1).await?.first().ok_or(DriverError::Malformed(
                    format!("empty read of {register}; not writing to avoid clearing other bits"),
                ))?;
                self.cache.insert(register, v);
                v
            }
        };
        let mask = 1u16 << bit;
        let new = if on { current | mask } else { current & !mask };
        Ok(if new == current { Vec::new() } else { vec![(register, new, Some(current))] })
    }

    /// The register writes `cmd` needs now; `None` for a command that is not for the Solis.
    async fn prepare(&mut self, cmd: &Command) -> Result<Option<Vec<RegWrite>>> {
        Ok(Some(match *cmd {
            Command::SolisRcMode(m) => self.plan_rc_mode(m),
            Command::SolisRcChargeW(w) => self.plan_number(reg::RC_CHARGE, w, 10, (0, 655_350))?,
            Command::SolisRcDischargeW(w) => self.plan_number(reg::RC_DISCHARGE, w, 10, (0, 655_350))?,
            Command::SolisRcTimeoutMin(m) => self.plan_number(reg::RC_TIMEOUT, m, 1, RC_TIMEOUT_RANGE)?,
            Command::SolisBackflowW(w) => self.plan_number(reg::BACKFLOW, w, 100, (0, 20_000))?,
            Command::SolisAllowGridCharge(on) => self.plan_bit(reg::STORAGE_CONTROL, ALLOW_GRID_CHARGE_BIT, on).await?,
            Command::SolisFeedInLimit(on) => self.plan_bit(reg::FEED_IN_SWITCH, FEED_IN_LIMIT_BIT, on).await?,
            _ => return Ok(None),
        }))
    }
}

/// The report of planned register writes.
fn describe(writes: &[RegWrite]) -> Vec<Write> {
    writes.iter().map(|&(r, v, before)| Write::new(format!("holding {r}"), v).before(before)).collect()
}

fn applied(writes: &[RegWrite]) -> Applied {
    if writes.is_empty() { Applied::Unchanged } else { Applied::Done }
}

#[async_trait]
impl<T: ModbusTransport> Source for Solis<T> {
    fn name(&self) -> &'static str {
        "solis"
    }

    async fn read(&mut self) -> Result<Reading> {
        Ok(Reading::Solis(self.poll().await?))
    }

    fn unavailable(&self) -> Reading {
        Reading::Solis(SolisReading::default())
    }

    /// The read-only values beyond the reading ([`extra`]).
    fn entities(&self) -> Vec<Entity> {
        extra::entities(&self.cache)
    }
}

#[async_trait]
impl<T: ModbusTransport> Actuator for Solis<T> {
    async fn apply(&mut self, cmd: &Command) -> Result<Applied> {
        Ok(self.execute(cmd).await?.applied)
    }

    /// Write the planned registers in order (one FC 6 write each), each into the cache once the
    /// inverter took it, and read the settings back on the next poll.
    async fn execute(&mut self, cmd: &Command) -> Result<Outcome> {
        if *cmd == Command::SolisReconnect {
            self.transport.reconnect().await?;
            return Ok(Outcome { applied: Applied::Done, writes: Vec::new() });
        }
        let Some(writes) = self.prepare(cmd).await? else {
            return Ok(Outcome { applied: Applied::NotMine, writes: Vec::new() });
        };
        for &(register, value, _) in &writes {
            self.reread = true;
            self.transport.write_single(register, value).await?;
            self.cache.insert(register, value);
            if register == reg::RC_MODE {
                self.rc_written = Some(Instant::now());
            }
        }
        Ok(Outcome { applied: applied(&writes), writes: describe(&writes) })
    }

    /// The writes `execute` would make against the current cache, which is left as it is (a
    /// switch with no cached value still reads the register live).
    async fn plan(&mut self, cmd: &Command) -> Result<Outcome> {
        if *cmd == Command::SolisReconnect {
            return Ok(Outcome { applied: Applied::Done, writes: vec![Write::new("modbus session", "reconnect")] });
        }
        let Some(writes) = self.prepare(cmd).await? else {
            return Ok(Outcome { applied: Applied::NotMine, writes: Vec::new() });
        };
        Ok(Outcome { applied: applied(&writes), writes: describe(&writes) })
    }
}

pub mod extra;

#[cfg(test)]
mod tests;
