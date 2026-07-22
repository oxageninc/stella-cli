//! A time source for the context plane, injectable for deterministic tests.
//!
//! Bi-temporal storage needs timestamps that
//! are **lexicographically comparable** so "what did we believe at T1" is a
//! plain string range scan in SQLite. Rather than pull in a date-time crate
//! (`contextgraph-types` is zero-dep by charter; this crate stays lean too), we format
//! RFC-3339 UTC ourselves from a Unix-seconds source. The [`Clock`] trait is
//! the seam: production uses the wall clock; tests inject [`FixedClock`] so a
//! T1→T2 correction is exact and never races real time.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A monotonic-enough wall clock producing RFC-3339 UTC timestamps
/// (`YYYY-MM-DDThh:mm:ssZ`). The string form is deliberately fixed-width and
/// zero-padded so `recorded_at <= ?` string comparison equals time ordering.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch. The only primitive an implementation
    /// must provide; the RFC-3339 rendering is shared.
    fn now_unix_secs(&self) -> i64;

    /// The current instant as an RFC-3339 UTC string.
    fn now_rfc3339(&self) -> String {
        format_rfc3339(self.now_unix_secs())
    }
}

/// The production clock, reading the system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_secs(&self) -> i64 {
        // A clock set before 1970 yields a negative duration; treat that as
        // the epoch rather than panicking — a wrong-but-ordered timestamp is
        // better than a crash in the write path (§1.5 fail-loud, not fail-hard
        // on a benign platform quirk).
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// A deterministic clock for tests and for callers that want to pin
/// transaction time explicitly. Advancing it is how a test models a later
/// correction (`upsert` at T2 superseding a belief recorded at T1).
#[derive(Debug)]
pub struct FixedClock {
    secs: AtomicI64,
}

impl FixedClock {
    /// Construct a clock frozen at `unix_secs`.
    pub fn new(unix_secs: i64) -> Self {
        Self {
            secs: AtomicI64::new(unix_secs),
        }
    }

    /// Construct a shareable clock frozen at `unix_secs`.
    pub fn shared(unix_secs: i64) -> Arc<Self> {
        Arc::new(Self::new(unix_secs))
    }

    /// Jump the clock forward by `delta` seconds and return the new value.
    pub fn advance(&self, delta: i64) -> i64 {
        self.secs.fetch_add(delta, Ordering::SeqCst) + delta
    }

    /// Set the clock to an absolute time.
    pub fn set(&self, unix_secs: i64) {
        self.secs.store(unix_secs, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_unix_secs(&self) -> i64 {
        self.secs.load(Ordering::SeqCst)
    }
}

/// Render Unix seconds as an RFC-3339 UTC string `YYYY-MM-DDThh:mm:ssZ`.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm (public-domain, exact for
/// the full proleptic Gregorian range) so there is no dependency and no
/// off-by-one at month/year boundaries. Negative inputs (pre-1970) render
/// correctly too, which keeps the formatter total.
pub fn format_rfc3339(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a count of days since 1970-01-01 into `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of the cycle.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year (Mar-based), [0, 365]
    let mp = (5 * doy + 2) / 153; // month, Mar=0 .. Feb=11
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_renders_as_the_unix_zero_instant() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_instant_renders_exactly() {
        // 1600000000 is 2020-09-13T12:26:40Z — a fixed, checkable reference.
        assert_eq!(format_rfc3339(1_600_000_000), "2020-09-13T12:26:40Z");
    }

    #[test]
    fn leap_day_boundary_is_exact() {
        // 2020 is a leap year; 1582934400 == 2020-02-29T00:00:00Z.
        assert_eq!(format_rfc3339(1_582_934_400), "2020-02-29T00:00:00Z");
        // One second earlier is still Feb 28.
        assert_eq!(format_rfc3339(1_582_934_399), "2020-02-28T23:59:59Z");
    }

    #[test]
    fn pre_epoch_time_renders_without_panicking() {
        assert_eq!(format_rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn timestamps_sort_lexicographically_by_time() {
        // The whole point: string order == time order, so bi-temporal range
        // scans are correct without parsing.
        let earlier = format_rfc3339(1_600_000_000);
        let later = format_rfc3339(1_600_000_001);
        assert!(earlier < later);
    }

    #[test]
    fn fixed_clock_advances_deterministically() {
        let clock = FixedClock::new(1_000);
        assert_eq!(clock.now_unix_secs(), 1_000);
        assert_eq!(clock.advance(60), 1_060);
        assert_eq!(clock.now_unix_secs(), 1_060);
        clock.set(42);
        assert_eq!(clock.now_rfc3339(), format_rfc3339(42));
    }

    #[test]
    fn system_clock_is_after_2020() {
        // A sanity floor: the real clock must be past a known past instant.
        assert!(SystemClock.now_unix_secs() > 1_600_000_000);
    }
}
