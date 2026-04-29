//! Hybrid Logical Clock — physical wall-clock milliseconds + logical
//! counter, packed into a single `u64` for cheap comparison and storage.
//!
//! HLCs are used to stamp every WAL record so that records produced on
//! different nodes can be totally ordered while still staying close to
//! wall-clock time. Two HLCs are totally ordered by their packed `u64`
//! representation: this is the property that makes them a drop-in
//! replacement for the bare LSN in single-node code paths.

use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

/// 48 bits physical milliseconds (since UNIX epoch — good for ~8900 years)
/// + 16 bits logical counter (up to 65535 events per millisecond).
///
/// Two HLCs are totally ordered by their packed `u64` representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc(pub u64);

impl Hlc {
    /// Zero value (epoch, logical 0). Used as a sentinel.
    pub const ZERO: Hlc = Hlc(0);
    /// Number of bits reserved for the physical time component.
    pub const PHYSICAL_BITS: u32 = 48;
    /// Number of bits reserved for the logical counter component.
    pub const LOGICAL_BITS: u32 = 16;
    /// Bit mask for extracting the logical counter.
    pub const LOGICAL_MASK: u64 = (1 << Self::LOGICAL_BITS) - 1;

    /// Constructs an HLC from its packed `u64` representation.
    pub fn from_packed(packed: u64) -> Self {
        Hlc(packed)
    }

    /// Packs a `(physical_ms, logical)` pair. The physical component is
    /// silently truncated to 48 bits — callers are expected to feed it
    /// values that fit (UNIX millisecond timestamps do, until year ~10889).
    pub fn pack(physical_ms: u64, logical: u16) -> Self {
        let phys = physical_ms & ((1u64 << Self::PHYSICAL_BITS) - 1);
        Hlc((phys << Self::LOGICAL_BITS) | (logical as u64))
    }

    /// Returns the physical (millisecond) component.
    pub fn physical_ms(&self) -> u64 {
        self.0 >> Self::LOGICAL_BITS
    }

    /// Returns the logical (intra-millisecond) counter.
    pub fn logical(&self) -> u16 {
        (self.0 & Self::LOGICAL_MASK) as u16
    }

    /// Encodes the packed value as little-endian bytes.
    pub fn to_le_bytes(&self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    /// Decodes the packed value from little-endian bytes.
    pub fn from_le_bytes(buf: [u8; 8]) -> Self {
        Hlc(u64::from_le_bytes(buf))
    }
}

/// Errors raised by [`HlcClock`].
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum HlcError {
    /// A remote HLC's physical component exceeds the local wall clock
    /// by more than the configured tolerance.
    #[error("clock skew of {0} ms exceeds the {1} ms tolerance")]
    SkewExceeded(u64, u64),
    /// The 16-bit logical counter would overflow within a single
    /// physical millisecond.
    #[error("logical counter overflow at physical ms {0}")]
    LogicalOverflow(u64),
    /// The host's monotonic system clock returned a time before the
    /// UNIX epoch (rare but possible on misconfigured machines).
    #[error("system clock returned a time before the UNIX epoch")]
    BeforeEpoch,
}

/// Default tolerated skew between local wall-clock and a remote HLC's
/// physical component. One hour is generous for typical deployments.
pub const DEFAULT_MAX_SKEW_MS: u64 = 60 * 60 * 1000;

/// Stateful HLC source. One instance per node, held by the engine.
///
/// Cheap to wrap in `Arc<HlcClock>` and share between threads.
pub struct HlcClock {
    state: Mutex<Hlc>,
    /// Maximum allowed skew between local wall-clock and a remote HLC's
    /// physical component, in milliseconds. Refuse `update()` beyond this.
    max_skew_ms: u64,
}

impl HlcClock {
    /// Creates a new clock seeded at the current wall-clock time, using
    /// the [`DEFAULT_MAX_SKEW_MS`] tolerance.
    pub fn new() -> Self {
        Self::with_max_skew(DEFAULT_MAX_SKEW_MS)
    }

    /// Creates a new clock seeded at the current wall-clock time with a
    /// custom skew tolerance (in milliseconds).
    pub fn with_max_skew(max_skew_ms: u64) -> Self {
        let initial = wall_now_ms().unwrap_or(0);
        Self {
            state: Mutex::new(Hlc::pack(initial, 0)),
            max_skew_ms,
        }
    }

    /// Local event: returns a new HLC strictly greater than any value
    /// previously returned. Bumps the logical counter if the wall-clock
    /// has not advanced; otherwise resets logical to 0 and uses the new
    /// physical time.
    pub fn now(&self) -> Result<Hlc, HlcError> {
        let mut state = self.state.lock();
        let wall = wall_now_ms()?;
        let prev_phys = state.physical_ms();
        let prev_log = state.logical();

        let next = if wall > prev_phys {
            Hlc::pack(wall, 0)
        } else {
            // wall hasn't advanced (or our clock is slightly ahead) — bump
            // logical so the result is strictly greater than the previous.
            let new_log = prev_log
                .checked_add(1)
                .ok_or(HlcError::LogicalOverflow(prev_phys))?;
            Hlc::pack(prev_phys, new_log)
        };
        *state = next;
        Ok(next)
    }

    /// Receive event: blends a remote HLC into the local state. The
    /// returned HLC is greater than both `self.last` and `remote`.
    /// Refuses if `remote.physical_ms()` exceeds the local wall clock
    /// by more than `max_skew_ms`.
    pub fn update(&self, remote: Hlc) -> Result<Hlc, HlcError> {
        let mut state = self.state.lock();
        let wall = wall_now_ms()?;

        if remote.physical_ms() > wall + self.max_skew_ms {
            return Err(HlcError::SkewExceeded(
                remote.physical_ms() - wall,
                self.max_skew_ms,
            ));
        }

        let prev_phys = state.physical_ms();
        let prev_log = state.logical();
        let rem_phys = remote.physical_ms();
        let rem_log = remote.logical();

        // Standard HLC merge: physical = max(local, remote, wall).
        let new_phys = wall.max(prev_phys).max(rem_phys);
        let new_log = if new_phys == prev_phys && new_phys == rem_phys {
            // Both sides agree on physical → bump max(local_log, remote_log).
            prev_log
                .max(rem_log)
                .checked_add(1)
                .ok_or(HlcError::LogicalOverflow(new_phys))?
        } else if new_phys == prev_phys {
            // Local physical wins → bump our logical.
            prev_log
                .checked_add(1)
                .ok_or(HlcError::LogicalOverflow(new_phys))?
        } else if new_phys == rem_phys {
            // Remote physical wins → bump remote logical.
            rem_log
                .checked_add(1)
                .ok_or(HlcError::LogicalOverflow(new_phys))?
        } else {
            // Wall advanced past both → reset logical.
            0
        };

        let next = Hlc::pack(new_phys, new_log);
        *state = next;
        Ok(next)
    }

    /// Reads the last issued HLC without advancing the clock.
    pub fn read(&self) -> Hlc {
        *self.state.lock()
    }
}

impl Default for HlcClock {
    fn default() -> Self {
        Self::new()
    }
}

fn wall_now_ms() -> Result<u64, HlcError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| HlcError::BeforeEpoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hlc_pack_unpack_round_trip() {
        let cases = [
            (0u64, 0u16),
            (1, 1),
            (1234567890, 42),
            ((1u64 << 48) - 1, u16::MAX),
        ];
        for (phys, log) in cases {
            let h = Hlc::pack(phys, log);
            assert_eq!(h.physical_ms(), phys, "physical mismatch");
            assert_eq!(h.logical(), log, "logical mismatch");
            // bytes round trip
            assert_eq!(Hlc::from_le_bytes(h.to_le_bytes()), h);
        }
    }

    #[test]
    fn test_hlc_now_strictly_monotonic() {
        let clock = HlcClock::new();
        let mut prev = Hlc::ZERO;
        for _ in 0..1000 {
            let h = clock.now().unwrap();
            assert!(h > prev, "HLC must be strictly monotonic: {h:?} > {prev:?}");
            prev = h;
        }
    }

    #[test]
    fn test_hlc_update_with_higher_remote() {
        let clock = HlcClock::new();
        let local_now = clock.now().unwrap();
        // Force remote into the future (still within tolerance).
        let remote = Hlc::pack(local_now.physical_ms() + 1000, 5);
        let after = clock.update(remote).unwrap();
        assert!(after >= remote);
        assert!(after.physical_ms() >= remote.physical_ms());
    }

    #[test]
    fn test_hlc_update_with_lower_remote() {
        let clock = HlcClock::new();
        let local = clock.now().unwrap();
        // Pick a remote significantly in the past.
        let remote = Hlc::pack(local.physical_ms().saturating_sub(10_000), 0);
        let after = clock.update(remote).unwrap();
        assert!(
            after >= local,
            "after ({after:?}) must be >= local ({local:?})"
        );
    }

    #[test]
    fn test_hlc_update_with_equal_remote() {
        let clock = HlcClock::with_max_skew(60_000);
        let local = clock.now().unwrap();
        // Equal physical — logical must bump.
        let remote = Hlc::pack(local.physical_ms(), local.logical());
        let after = clock.update(remote).unwrap();
        assert!(
            after > local,
            "after ({after:?}) must be > local ({local:?})"
        );
        assert!(after > remote);
    }

    #[test]
    fn test_hlc_update_skew_exceeded() {
        let clock = HlcClock::with_max_skew(1_000); // 1 second tolerance
        let wall = wall_now_ms().unwrap();
        let remote = Hlc::pack(wall + 7_200_000, 0); // 2 hours ahead
        let err = clock.update(remote).unwrap_err();
        match err {
            HlcError::SkewExceeded(skew, tol) => {
                assert!(skew >= 7_200_000 - 1_000);
                assert_eq!(tol, 1_000);
            }
            other => panic!("expected SkewExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_hlc_logical_overflow_safety() {
        // Synthesise the case: clock state at logical=u16::MAX, then call
        // now() with the wall clock NOT advancing. We can't easily freeze
        // the wall clock, so we drive update() with a remote at the same
        // physical and exhausted logical to force the bump.
        let clock = HlcClock::new();
        let local = clock.now().unwrap();
        let remote = Hlc::pack(local.physical_ms(), u16::MAX);
        let err = clock.update(remote).unwrap_err();
        assert!(matches!(err, HlcError::LogicalOverflow(_)));
    }

    #[test]
    fn test_hlc_read_does_not_advance() {
        let clock = HlcClock::new();
        let h1 = clock.now().unwrap();
        let r1 = clock.read();
        let r2 = clock.read();
        assert_eq!(r1, h1);
        assert_eq!(r2, h1);
    }

    #[test]
    fn test_hlc_ordering_via_packed() {
        let a = Hlc::pack(100, 0);
        let b = Hlc::pack(100, 1);
        let c = Hlc::pack(101, 0);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }
}
