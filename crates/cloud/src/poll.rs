//! Rate limit and backoff shared by every client.
//!
//! A [`Poller`] decides, before a client touches the network, whether a fetch is due: within
//! the poll interval after a good read it hands back the cached value; after a failure it
//! refuses until the backoff has passed (exponential, capped). Credential errors jump straight
//! to the cap, so a rejected password is not hammered into an account lock.

use std::time::{Duration, Instant};

use crate::error::{CloudError, Result};

/// What a client should do now.
#[derive(Debug, PartialEq)]
pub enum Gate<'a, T> {
    /// Go to the network.
    Fetch,
    /// The last good reading is still fresh.
    Cached(&'a T),
    /// In backoff for this much longer.
    Wait(Duration),
}

/// Poll interval plus exponential backoff for one client.
#[derive(Debug, Clone)]
pub struct Poller<T> {
    interval: Duration,
    backoff_base: Duration,
    backoff_max: Duration,
    last: Option<(Instant, T)>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl<T: Clone> Poller<T> {
    /// Fetch at most once per `interval`; after `n` consecutive failures wait
    /// `backoff_base * 2^(n-1)`, at most `backoff_max`.
    pub fn new(interval: Duration, backoff_base: Duration, backoff_max: Duration) -> Self {
        Poller { interval, backoff_base, backoff_max, last: None, failures: 0, retry_at: None }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Decide what to do at `now`.
    pub fn gate(&self, now: Instant) -> Gate<'_, T> {
        if let Some(at) = self.retry_at.filter(|at| *at > now) {
            return Gate::Wait(at - now);
        }
        match &self.last {
            Some((at, v)) if now.duration_since(*at) < self.interval => Gate::Cached(v),
            _ => Gate::Fetch,
        }
    }

    /// Record the outcome of a fetch made at `now` and pass it on.
    pub fn record(&mut self, now: Instant, outcome: Result<T>) -> Result<T> {
        match outcome {
            Ok(v) => {
                self.failures = 0;
                self.retry_at = None;
                self.last = Some((now, v.clone()));
                Ok(v)
            }
            Err(e) => {
                self.last = None;
                self.failures = if e.is_permanent() { u32::MAX } else { self.failures.saturating_add(1) };
                self.retry_at = Some(now + self.delay());
                Err(e)
            }
        }
    }

    /// Like [`Poller::record`] for a failure, but wait exactly `delay` (a server's `Retry-At`).
    pub fn fail_for(&mut self, now: Instant, delay: Duration, e: CloudError) -> CloudError {
        self.last = None;
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + delay);
        e
    }

    /// The backoff after the current failure streak.
    fn delay(&self) -> Duration {
        let shift = self.failures.saturating_sub(1).min(16);
        self.backoff_base.saturating_mul(1 << shift).min(self.backoff_max)
    }

    /// Forget the failure streak and the cache: the next gate is a fetch (new credentials).
    pub fn reset(&mut self) {
        self.failures = 0;
        self.retry_at = None;
        self.last = None;
    }

    /// The last good value, if it has not been invalidated by a failure.
    pub fn last(&self) -> Option<&T> {
        self.last.as_ref().map(|(_, v)| v)
    }

    /// Change the cached value in place (after a command whose effect is known).
    pub fn update(&mut self, f: impl FnOnce(&mut T)) {
        if let Some((_, v)) = &mut self.last {
            f(v);
        }
    }

    /// Run `fetch` if a fetch is due, otherwise answer from the cache or with
    /// [`CloudError::Backoff`].
    pub async fn poll<F>(&mut self, fetch: F) -> Result<T>
    where
        F: AsyncFnOnce() -> Result<T>,
    {
        match self.gate(Instant::now()) {
            Gate::Cached(v) => return Ok(v.clone()),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        let outcome = fetch().await;
        self.record(Instant::now(), outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn poller() -> Poller<u32> {
        Poller::new(10 * S, 2 * S, 7 * S)
    }

    /// Within the interval after a good read the cache answers; after it a fetch is due.
    #[test]
    fn caches_within_the_interval() {
        let t0 = Instant::now();
        let mut p = poller();
        assert_eq!(p.interval(), 10 * S);
        assert_eq!(p.gate(t0), Gate::Fetch);
        assert_eq!(p.record(t0, Ok(5)).unwrap(), 5);
        assert_eq!(p.gate(t0 + 9 * S), Gate::Cached(&5));
        assert_eq!(p.gate(t0 + 10 * S), Gate::Fetch);
        assert_eq!(p.last(), Some(&5));
        p.update(|v| *v = 6);
        assert_eq!(p.last(), Some(&6));
    }

    /// Failures double the wait up to the cap and drop the cached value; a success resets.
    #[test]
    fn backs_off_exponentially() {
        let t0 = Instant::now();
        let mut p = poller();
        p.record(t0, Ok(1)).unwrap();
        assert!(p.record(t0, Err(CloudError::Status(500))).is_err());
        assert_eq!(p.last(), None);
        p.update(|v| *v = 9);
        assert_eq!(p.last(), None);
        assert_eq!(p.gate(t0), Gate::Wait(2 * S));
        assert_eq!(p.gate(t0 + 2 * S), Gate::Fetch);
        p.record(t0, Err(CloudError::Status(500))).unwrap_err();
        assert_eq!(p.gate(t0), Gate::Wait(4 * S));
        p.record(t0, Err(CloudError::Status(500))).unwrap_err();
        assert_eq!(p.gate(t0), Gate::Wait(7 * S));
        p.record(t0, Ok(2)).unwrap();
        assert_eq!(p.gate(t0), Gate::Cached(&2));
        p.record(t0, Err(CloudError::Unauthorized("x".into()))).unwrap_err();
        p.reset();
        assert_eq!(p.gate(t0), Gate::Fetch);
    }

    /// A rejected credential waits the maximum at once.
    #[test]
    fn permanent_errors_wait_the_maximum() {
        let t0 = Instant::now();
        let mut p = poller();
        p.record(t0, Err(CloudError::Unauthorized("x".into()))).unwrap_err();
        assert_eq!(p.gate(t0), Gate::Wait(7 * S));
        p.record(t0, Err(CloudError::Status(500))).unwrap_err();
        assert_eq!(p.gate(t0), Gate::Wait(7 * S));
    }

    /// A server-supplied retry time overrides the backoff.
    #[test]
    fn fail_for_waits_the_given_time() {
        let t0 = Instant::now();
        let mut p = poller();
        p.record(t0, Ok(1)).unwrap();
        let e = p.fail_for(t0, 60 * S, CloudError::RateLimited("x".into()));
        assert!(matches!(e, CloudError::RateLimited(_)));
        assert_eq!(p.gate(t0), Gate::Wait(60 * S));
        assert_eq!(p.last(), None);
    }

    /// `poll` fetches once, then serves the cache; a failure turns into `Backoff`.
    #[tokio::test]
    async fn poll_fetches_caches_and_backs_off() {
        let mut p = poller();
        let mut calls = 0;
        assert_eq!(p.poll(async || Ok(calls + 1)).await.unwrap(), 1);
        calls += 1;
        assert_eq!(p.poll(async || Ok(calls + 1)).await.unwrap(), 1);
        let mut q = poller();
        assert!(matches!(q.poll(async || Err(CloudError::Status(502))).await, Err(CloudError::Status(502))));
        assert!(matches!(q.poll(async || Ok(1)).await, Err(CloudError::Backoff(_))));
    }
}
