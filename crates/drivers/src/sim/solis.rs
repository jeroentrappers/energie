//! Solis S6-EH1P behind a Modbus TCP gateway: input and holding registers with the driver's
//! register map, starting from the live values of 2026-11-07.
//!
//! Reading an address that is not in the map gets exception 2 (illegal data address), like the
//! inverter. Writing 43135/43136/43129 runs a small model: in force charge the battery charges at
//! the RC charge power, in force discharge it discharges at the RC discharge power.
//!
//! The RC timeout runs on a virtual clock that only [`SolisSim::advance`] moves, as the house's
//! S6-EH1P showed on 2026-11-10: a forced mode falls back to `None` (self-use) 43282 minutes
//! after the last write of 43282 or the last change of 43135. Re-writing 43135 with the mode it
//! already holds, or new RC powers, does not restart it.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_modbus::server::Service;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
use tokio_modbus::{ExceptionCode, Request, Response};

use crate::solis::reg;

#[derive(Default)]
struct Regs {
    input: BTreeMap<u16, u16>,
    holding: BTreeMap<u16, u16>,
    writes: Vec<(u16, u16)>,
    fail_next: Option<ExceptionCode>,
    stall: bool,
    /// Virtual seconds ([`SolisSim::advance`]) and when the RC timeout last restarted.
    clock_s: u64,
    rc_restart_s: u64,
}

impl Regs {
    fn live() -> Regs {
        let mut r = Regs::default();
        for a in reg::PV_POWER..reg::PV_POWER + 2 {
            r.input.insert(a, 0);
        }
        for a in reg::BATTERY_VOLTAGE..reg::BATTERY_VOLTAGE + 18 {
            r.input.insert(a, 0);
        }
        // The extra blocks (`solis::extra`), at plausible values.
        for (input, start, count, _) in crate::solis::extra::BLOCKS {
            let map = if input { &mut r.input } else { &mut r.holding };
            for a in start..start + count {
                map.insert(a, 0);
            }
        }
        for (a, v) in [(33030, 9_120), (33035, 87), (33133, 528), (33134, 0xFFE7), (33140, 99), (33162, 3_100)] {
            r.input.insert(a, v);
        }
        for (a, v) in [(33163, 42), (33166, 2_900), (33167, 31), (33170, 4_200), (33171, 55), (33174, 7_000)] {
            r.input.insert(a, v);
        }
        r.input.insert(33175, 12);
        for (a, v) in [(43011, 20), (43018, 10), (43024, 30), (43483, 0x80), (43487, 50), (43488, 40)] {
            r.holding.insert(a, v);
        }
        r.input.insert(reg::PV_POWER + 1, 227);
        r.input.insert(reg::SOC, 100);
        for (a, v) in
            [(reg::FEED_IN_SWITCH, 16), (reg::BACKFLOW, 55), (reg::STORAGE_CONTROL, 17), (reg::RC_TIMEOUT, 10)]
        {
            r.holding.insert(a, v);
        }
        for a in 43128..=reg::RC_CHARGE {
            r.holding.insert(a, 0);
        }
        r.holding.insert(reg::RC_MODE, 1);
        r
    }

    fn read(map: &BTreeMap<u16, u16>, addr: u16, n: u16) -> Result<Vec<u16>, ExceptionCode> {
        (addr..addr + n).map(|a| map.get(&a).copied().ok_or(ExceptionCode::IllegalDataAddress)).collect()
    }

    fn set_battery(&mut self, w: i32) {
        let mag = w.unsigned_abs();
        self.input.insert(reg::BATTERY_DIRECTION, u16::from(w > 0));
        self.input.insert(reg::BATTERY_POWER, (mag >> 16) as u16);
        self.input.insert(reg::BATTERY_POWER + 1, mag as u16);
    }

    fn model(&mut self) {
        let h = |a| i32::from(self.holding[&a]);
        match self.holding[&reg::RC_MODE] {
            1 => self.set_battery(-h(reg::RC_CHARGE) * 10),
            2 => self.set_battery(h(reg::RC_DISCHARGE) * 10),
            _ => {}
        }
    }

    /// Move the virtual clock; a forced mode past its RC timeout falls back to `None`.
    fn advance(&mut self, secs: u64) {
        self.clock_s += secs;
        let timeout_s = u64::from(self.holding.get(&reg::RC_TIMEOUT).copied().unwrap_or(0)) * 60;
        if self.holding.get(&reg::RC_MODE).is_some_and(|m| *m != 0) && self.clock_s - self.rc_restart_s > timeout_s {
            self.holding.insert(reg::RC_MODE, 0);
            self.set_battery(0);
        }
    }

    fn handle(&mut self, req: Request<'static>) -> Result<Response, ExceptionCode> {
        if let Some(code) = self.fail_next.take() {
            return Err(code);
        }
        match req {
            Request::ReadInputRegisters(a, n) => Regs::read(&self.input, a, n).map(Response::ReadInputRegisters),
            Request::ReadHoldingRegisters(a, n) => Regs::read(&self.holding, a, n).map(Response::ReadHoldingRegisters),
            Request::WriteSingleRegister(a, v) => {
                let slot = self.holding.get_mut(&a).ok_or(ExceptionCode::IllegalDataAddress)?;
                let changed = *slot != v;
                *slot = v;
                if a == reg::RC_TIMEOUT || (a == reg::RC_MODE && changed) {
                    self.rc_restart_s = self.clock_s;
                }
                self.writes.push((a, v));
                self.model();
                Ok(Response::WriteSingleRegister(a, v))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        }
    }
}

type Shared = Arc<Mutex<Regs>>;

#[derive(Clone)]
struct Svc(Shared);

impl Service for Svc {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Pin<Box<dyn Future<Output = Result<Response, ExceptionCode>> + Send>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let mut regs = self.0.lock().unwrap();
        if regs.stall {
            return Box::pin(std::future::pending());
        }
        let res = regs.handle(req);
        Box::pin(std::future::ready(res))
    }
}

/// A running simulated inverter.
pub struct SolisSim {
    pub addr: SocketAddr,
    regs: Shared,
    task: JoinHandle<()>,
}

impl Drop for SolisSim {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SolisSim {
    pub async fn start() -> SolisSim {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let regs: Shared = Arc::new(Mutex::new(Regs::live()));
        let svc = Svc(regs.clone());
        let new_service = move |_| Ok(Some(svc.clone()));
        let on_connected = move |stream, peer| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, peer, new_service) }
        };
        let task =
            tokio::spawn(async move { Server::new(listener).serve(&on_connected, |_| {}).await.expect("serve") });
        SolisSim { addr, regs, task }
    }

    pub fn set_input(&self, register: u16, value: u16) {
        self.regs.lock().unwrap().input.insert(register, value);
    }

    pub fn set_holding(&self, register: u16, value: u16) {
        self.regs.lock().unwrap().holding.insert(register, value);
    }

    pub fn holding(&self, register: u16) -> Option<u16> {
        self.regs.lock().unwrap().holding.get(&register).copied()
    }

    /// Remove a holding register from the map (reads of it then fail with exception 2).
    pub fn remove_holding(&self, register: u16) {
        self.regs.lock().unwrap().holding.remove(&register);
    }

    /// Battery power, W, + discharge / − charge.
    pub fn set_battery(&self, w: i32) {
        self.regs.lock().unwrap().set_battery(w);
    }

    pub fn set_pv(&self, w: u32) {
        let mut r = self.regs.lock().unwrap();
        r.input.insert(reg::PV_POWER, (w >> 16) as u16);
        r.input.insert(reg::PV_POWER + 1, w as u16);
    }

    /// Let `secs` of virtual time pass for the RC timeout (nothing else in the model is timed).
    pub fn advance(&self, secs: u64) {
        self.regs.lock().unwrap().advance(secs);
    }

    /// Every register write received, in order.
    pub fn writes(&self) -> Vec<(u16, u16)> {
        self.regs.lock().unwrap().writes.clone()
    }

    /// Answer the next request with this exception.
    pub fn fail_next(&self, code: ExceptionCode) {
        self.regs.lock().unwrap().fail_next = Some(code);
    }

    /// Stop answering (requests hang) until set back to `false`.
    pub fn set_stall(&self, stall: bool) {
        self.regs.lock().unwrap().stall = stall;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_modbus::client::Reader;
    use tokio_modbus::prelude::Slave;

    /// The RC timeout as the S6-EH1P keeps it: restarted by a 43282 write or a new mode, not by
    /// re-writing the held mode or the powers.
    #[tokio::test]
    async fn the_rc_timeout_restarts_only_on_43282_or_a_new_mode() {
        use tokio_modbus::client::Writer;
        let sim = SolisSim::start().await;
        let mut ctx = tokio_modbus::client::tcp::connect_slave(sim.addr, Slave(1)).await.unwrap();
        // Live start: force charge held, timeout 10 min, restarted at 0.
        sim.advance(540);
        ctx.write_single_register(reg::RC_MODE, 1).await.unwrap().unwrap();
        ctx.write_single_register(reg::RC_CHARGE, 300).await.unwrap().unwrap();
        sim.advance(61);
        assert_eq!(sim.holding(reg::RC_MODE), Some(0), "lapsed 601 s after the last restart");
        ctx.write_single_register(reg::RC_MODE, 1).await.unwrap().unwrap();
        sim.advance(590);
        ctx.write_single_register(reg::RC_TIMEOUT, 10).await.unwrap().unwrap();
        sim.advance(590);
        assert_eq!(sim.holding(reg::RC_MODE), Some(1), "43282 restarted it");
        sim.advance(11);
        assert_eq!(sim.holding(reg::RC_MODE), Some(0));
    }

    /// Function codes the inverter map does not implement get exception 1 (illegal function).
    #[tokio::test]
    async fn unsupported_function_is_illegal() {
        let sim = SolisSim::start().await;
        let mut ctx = tokio_modbus::client::tcp::connect_slave(sim.addr, Slave(1)).await.unwrap();
        assert_eq!(ctx.read_coils(0, 1).await.unwrap(), Err(ExceptionCode::IllegalFunction));
    }
}
