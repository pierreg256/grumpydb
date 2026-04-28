//! Connection-level rate limits and brute-force protection.
//!
//! This module enforces three independent caps to make abuse impractical
//! without breaking legitimate clients:
//!
//! 1. **Per-IP command rate limit** — a token bucket capped at
//!    `commands_per_sec_per_ip` with burst `commands_burst_per_ip`.
//! 2. **Per-IP failed-login limit** — once `failed_logins_per_min_per_ip`
//!    failures accumulate from the same IP, subsequent `LOGIN`s are rejected
//!    for `min(60s, 2^N)` seconds (exponential backoff in N).
//! 3. **Connection caps** — both per-IP and global caps on concurrently
//!    open connections.
//!
//! All limits are best-effort soft caps: state is tracked in memory only and
//! is reset on server restart. The token buckets use the [`governor`] crate;
//! all other counters are protected by `parking_lot::Mutex`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

/// Type alias for the `governor` direct rate limiter we use.
type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Configuration for connection-level rate limits.
///
/// All defaults are tuned to be unobtrusive for typical clients (a few
/// hundred commands per second) while making naive brute-force attacks slow
/// enough to be uninteresting.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Sustained command rate per IP, in commands per second.
    pub commands_per_sec_per_ip: u32,
    /// Burst capacity per IP, in commands.
    pub commands_burst_per_ip: u32,
    /// Maximum failed `LOGIN`s per IP within the backoff window before
    /// further `LOGIN`s are rejected with exponential backoff.
    pub failed_logins_per_min_per_ip: u32,
    /// Maximum concurrent connections from a single IP.
    pub max_conns_per_ip: usize,
    /// Maximum concurrent connections globally.
    pub max_conns_global: usize,
    /// When `true` (default), all limits are bypassed for loopback peers
    /// (`127.0.0.0/8` and `::1`). This makes local development and
    /// integration tests that hammer the server from `127.0.0.1` work
    /// without surprises. Production deployments that expose loopback to
    /// untrusted callers should set this to `false`.
    pub bypass_loopback: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            commands_per_sec_per_ip: 100,
            commands_burst_per_ip: 200,
            failed_logins_per_min_per_ip: 5,
            max_conns_per_ip: 100,
            max_conns_global: 10_000,
            bypass_loopback: true,
        }
    }
}

/// Per-IP failed-login state used to drive the exponential backoff.
struct FailedLoginState {
    /// Number of failures since the last successful login (or counter reset).
    count: u32,
    /// If `Some`, all `LOGIN`s from this IP are rejected until this instant.
    blocked_until: Option<Instant>,
}

/// Reason a [`Limits::try_acquire_conn_with_reason`] call rejected a
/// connection. Used by the listener to label rate-limit metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireConnError {
    /// The global concurrent-connection cap is full.
    Global,
    /// The per-IP concurrent-connection cap for this peer is full.
    PerIp,
}

/// Holds the per-IP token buckets and connection counters.
///
/// Cheap to clone via `Arc`. Construct once at server startup and share
/// across all connection-handler tasks.
pub struct Limits {
    cfg: LimitsConfig,
    cmd_buckets: parking_lot::Mutex<HashMap<IpAddr, Arc<DirectLimiter>>>,
    conn_counts_per_ip: parking_lot::Mutex<HashMap<IpAddr, usize>>,
    global_conns: AtomicUsize,
    failed_logins: parking_lot::Mutex<HashMap<IpAddr, FailedLoginState>>,
}

impl Limits {
    /// Build a new limits enforcer from the given configuration.
    pub fn new(cfg: LimitsConfig) -> Self {
        Self {
            cfg,
            cmd_buckets: parking_lot::Mutex::new(HashMap::new()),
            conn_counts_per_ip: parking_lot::Mutex::new(HashMap::new()),
            global_conns: AtomicUsize::new(0),
            failed_logins: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Read-only view of the configuration.
    pub fn config(&self) -> &LimitsConfig {
        &self.cfg
    }

    /// Returns `true` when `addr` is loopback AND the configuration enables
    /// the loopback bypass (the default — see [`LimitsConfig::bypass_loopback`]).
    fn is_loopback_bypass(&self, addr: IpAddr) -> bool {
        self.cfg.bypass_loopback && addr.is_loopback()
    }

    /// Try to register a new connection from `addr`. Returns `true` if the
    /// connection is allowed, `false` if any cap (per-IP or global) is hit.
    ///
    /// On success, the caller MUST eventually invoke [`Self::release_conn`]
    /// to free the slot. On failure, no state changes.
    pub fn try_acquire_conn(&self, addr: IpAddr) -> bool {
        self.try_acquire_conn_with_reason(addr).is_ok()
    }

    /// Like [`Self::try_acquire_conn`] but reports which cap was hit on
    /// failure. Used by the listener to label rate-limit metrics
    /// accurately.
    pub fn try_acquire_conn_with_reason(&self, addr: IpAddr) -> Result<(), AcquireConnError> {
        // Loopback is bypassed by default — see `bypass_loopback`.
        if self.is_loopback_bypass(addr) {
            return Ok(());
        }

        // Reserve the global slot first with a CAS loop so we never exceed
        // `max_conns_global` even with thousands of concurrent acceptors.
        loop {
            let current = self.global_conns.load(Ordering::Acquire);
            if current >= self.cfg.max_conns_global {
                return Err(AcquireConnError::Global);
            }
            if self
                .global_conns
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // Now reserve the per-IP slot.
        let mut counts = self.conn_counts_per_ip.lock();
        let entry = counts.entry(addr).or_insert(0);
        if *entry >= self.cfg.max_conns_per_ip {
            // Roll back the global reservation we made above.
            drop(counts);
            self.global_conns.fetch_sub(1, Ordering::AcqRel);
            return Err(AcquireConnError::PerIp);
        }
        *entry += 1;
        Ok(())
    }

    /// Release a connection slot. Must be called exactly once per successful
    /// [`Self::try_acquire_conn`].
    pub fn release_conn(&self, addr: IpAddr) {
        let mut counts = self.conn_counts_per_ip.lock();
        if let Some(c) = counts.get_mut(&addr) {
            if *c > 0 {
                *c -= 1;
            }
            if *c == 0 {
                counts.remove(&addr);
            }
        }
        drop(counts);
        // `fetch_sub` underflow guard: only decrement if we have something to
        // give back. This shouldn't be reachable in practice (acquire/release
        // are paired) but is cheap insurance against a future double-release.
        self.global_conns
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                if v == 0 { None } else { Some(v - 1) }
            })
            .ok();
    }

    /// Try to take a token from the per-IP command bucket.
    ///
    /// Returns `true` if the command may proceed, `false` if the IP has
    /// exceeded its rate. Loopback is bypassed.
    pub fn try_take_command(&self, addr: IpAddr) -> bool {
        if self.is_loopback_bypass(addr) {
            return true;
        }
        let bucket = {
            let mut map = self.cmd_buckets.lock();
            if let Some(b) = map.get(&addr) {
                b.clone()
            } else {
                let cps = NonZeroU32::new(self.cfg.commands_per_sec_per_ip.max(1))
                    .expect("commands_per_sec_per_ip non-zero after max(1)");
                let burst = NonZeroU32::new(self.cfg.commands_burst_per_ip.max(1))
                    .expect("commands_burst_per_ip non-zero after max(1)");
                let quota = Quota::per_second(cps).allow_burst(burst);
                let limiter = Arc::new(RateLimiter::direct(quota));
                map.insert(addr, limiter.clone());
                limiter
            }
        };
        bucket.check().is_ok()
    }

    /// Returns `Some(retry_after)` if the IP is currently in failed-login
    /// backoff, `None` otherwise. Loopback never enters backoff when
    /// `bypass_loopback` is true (default).
    pub fn login_backoff(&self, addr: IpAddr) -> Option<Duration> {
        if self.is_loopback_bypass(addr) {
            return None;
        }
        let mut map = self.failed_logins.lock();
        if let Some(state) = map.get_mut(&addr)
            && let Some(deadline) = state.blocked_until
        {
            let now = Instant::now();
            if now < deadline {
                return Some(deadline - now);
            } else {
                // Backoff window has elapsed: clear it but keep the
                // counter so repeated abuse re-arms the next backoff
                // faster.
                state.blocked_until = None;
            }
        }
        None
    }

    /// Record a failed login. Bumps the counter and arms the exponential
    /// backoff once the failure threshold is reached.
    ///
    /// The backoff doubles with every failure past the threshold, capped at
    /// 60 seconds: 1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s, ...
    pub fn record_failed_login(&self, addr: IpAddr) {
        if self.is_loopback_bypass(addr) {
            return;
        }
        let mut map = self.failed_logins.lock();
        let state = map.entry(addr).or_insert(FailedLoginState {
            count: 0,
            blocked_until: None,
        });
        state.count = state.count.saturating_add(1);
        if state.count >= self.cfg.failed_logins_per_min_per_ip {
            // Number of failures past the threshold (0-based).
            let over = state.count - self.cfg.failed_logins_per_min_per_ip;
            let secs = (1u64 << over.min(6)).min(60);
            state.blocked_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    /// Reset the failed-login counter for an IP after a successful login.
    pub fn record_successful_login(&self, addr: IpAddr) {
        let mut map = self.failed_logins.lock();
        map.remove(&addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn cfg() -> LimitsConfig {
        // Unit tests want to exercise the limit logic regardless of the IP
        // they pick, so disable the loopback bypass.
        LimitsConfig {
            bypass_loopback: false,
            ..LimitsConfig::default()
        }
    }

    #[test]
    fn test_command_bucket_allows_below_quota() {
        let limits = Limits::new(LimitsConfig {
            commands_per_sec_per_ip: 100,
            commands_burst_per_ip: 200,
            ..cfg()
        });
        // Take 50 tokens immediately — well under the burst of 200.
        for _ in 0..50 {
            assert!(limits.try_take_command(ip(127, 0, 0, 1)));
        }
    }

    #[test]
    fn test_command_bucket_blocks_above_quota() {
        let limits = Limits::new(LimitsConfig {
            commands_per_sec_per_ip: 10,
            commands_burst_per_ip: 20,
            ..cfg()
        });
        let addr = ip(10, 0, 0, 1);
        // Drain the burst.
        let mut allowed = 0;
        let mut blocked = 0;
        for _ in 0..1000 {
            if limits.try_take_command(addr) {
                allowed += 1;
            } else {
                blocked += 1;
            }
        }
        assert!(
            (20..1000).contains(&allowed),
            "expected ~20 allowed, got {allowed}"
        );
        assert!(blocked > 900, "expected most calls blocked, got {blocked}");
    }

    #[test]
    fn test_failed_login_backoff() {
        let limits = Limits::new(LimitsConfig {
            failed_logins_per_min_per_ip: 3,
            ..cfg()
        });
        let addr = ip(192, 168, 1, 1);
        // No backoff before threshold.
        assert!(limits.login_backoff(addr).is_none());
        limits.record_failed_login(addr);
        assert!(limits.login_backoff(addr).is_none());
        limits.record_failed_login(addr);
        assert!(limits.login_backoff(addr).is_none());
        // 3rd failure crosses the threshold and arms the backoff.
        limits.record_failed_login(addr);
        let d = limits.login_backoff(addr).expect("backoff should be armed");
        assert!(d > Duration::from_millis(0));
        assert!(d <= Duration::from_secs(60));
    }

    #[test]
    fn test_successful_login_resets_backoff() {
        let limits = Limits::new(LimitsConfig {
            failed_logins_per_min_per_ip: 2,
            ..cfg()
        });
        let addr = ip(192, 168, 1, 2);
        limits.record_failed_login(addr);
        limits.record_failed_login(addr);
        assert!(limits.login_backoff(addr).is_some());
        limits.record_successful_login(addr);
        assert!(limits.login_backoff(addr).is_none());
    }

    #[test]
    fn test_per_ip_conn_cap() {
        let limits = Limits::new(LimitsConfig {
            max_conns_per_ip: 3,
            max_conns_global: 100,
            ..cfg()
        });
        let addr = ip(10, 0, 0, 5);
        assert!(limits.try_acquire_conn(addr));
        assert!(limits.try_acquire_conn(addr));
        assert!(limits.try_acquire_conn(addr));
        assert!(!limits.try_acquire_conn(addr));
    }

    #[test]
    fn test_global_conn_cap() {
        let limits = Limits::new(LimitsConfig {
            max_conns_per_ip: 100,
            max_conns_global: 5,
            ..cfg()
        });
        // Five different IPs, each under the per-IP cap, should fill the
        // global cap exactly.
        for i in 0..5 {
            assert!(limits.try_acquire_conn(ip(10, 0, 0, i as u8)));
        }
        assert!(!limits.try_acquire_conn(ip(10, 0, 0, 99)));
    }

    #[test]
    fn test_release_conn_frees_slot() {
        let limits = Limits::new(LimitsConfig {
            max_conns_per_ip: 2,
            max_conns_global: 100,
            ..cfg()
        });
        let addr = ip(10, 0, 0, 7);
        assert!(limits.try_acquire_conn(addr));
        assert!(limits.try_acquire_conn(addr));
        assert!(!limits.try_acquire_conn(addr));
        limits.release_conn(addr);
        assert!(limits.try_acquire_conn(addr));
    }

    #[test]
    fn test_release_conn_frees_global_slot() {
        let limits = Limits::new(LimitsConfig {
            max_conns_per_ip: 100,
            max_conns_global: 2,
            ..cfg()
        });
        let a = ip(10, 0, 0, 1);
        let b = ip(10, 0, 0, 2);
        let c = ip(10, 0, 0, 3);
        assert!(limits.try_acquire_conn(a));
        assert!(limits.try_acquire_conn(b));
        assert!(!limits.try_acquire_conn(c));
        limits.release_conn(a);
        assert!(limits.try_acquire_conn(c));
    }

    #[test]
    fn test_default_config_values() {
        let d = LimitsConfig::default();
        assert_eq!(d.commands_per_sec_per_ip, 100);
        assert_eq!(d.commands_burst_per_ip, 200);
        assert_eq!(d.failed_logins_per_min_per_ip, 5);
        assert_eq!(d.max_conns_per_ip, 100);
        assert_eq!(d.max_conns_global, 10_000);
    }
}
