//! A simple departure-time bandwidth limiter (M4-005).
//!
//! The `proton-drive` CLI is a one-shot subprocess with no progress hook, so
//! there's no byte-level visibility into its own network I/O to throttle
//! mid-transfer. Instead, each whole transfer is scheduled a "departure
//! time" computed from its size and the configured rate: a transfer waits
//! its turn, then runs at full speed. A large file on a low limit visibly
//! delays whatever is queued behind it rather than being slowed down itself
//! - an accepted simplification given the CLI offers no other lever.
//!
//! Safe to share across concurrent callers (worker threads): each caller is
//! assigned its own non-overlapping slot on a virtual timeline, so the
//! combined throughput of every caller together stays at or under the
//! configured rate - not `threads x rate`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    next_departure: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            next_departure: Mutex::new(Instant::now()),
        }
    }

    /// Blocks the calling thread until it's this transfer's turn, given its
    /// size in bytes and the current limit in KB/s (0 = unlimited, returns
    /// immediately). The limit is read fresh on every call rather than fixed
    /// at construction time, so a limit changed mid-run (e.g. from the GUI)
    /// takes effect on the very next transfer.
    pub fn acquire(&self, bytes: u64, kbps: u64) {
        if kbps == 0 || bytes == 0 {
            return;
        }
        let bytes_per_sec = kbps as f64 * 1000.0;
        let wait = {
            let mut next = self.next_departure.lock().unwrap();
            let now = Instant::now();
            let start = (*next).max(now);
            let duration = Duration::from_secs_f64(bytes as f64 / bytes_per_sec);
            *next = start + duration;
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// A clone starts its own fresh schedule rather than sharing the original's
/// in-flight state - correct for how `Config` (which owns one of these per
/// direction) is cloned: always as an independent snapshot (a GUI settings
/// edit, a `Controller`'s per-call config snapshot), never as a second handle
/// meant to share live scheduling state with the original. By the time any
/// caller's `acquire()` returns, real time has already caught up to the
/// schedule it was granted, so a fresh clone never skips ahead of where the
/// original would be.
impl Clone for RateLimiter {
    fn clone(&self) -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimiter;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn zero_limit_never_blocks() {
        let rl = RateLimiter::new();
        let start = Instant::now();
        rl.acquire(1_000_000_000, 0);
        assert!(start.elapsed().as_millis() < 50);
    }

    #[test]
    fn zero_bytes_never_blocks() {
        let rl = RateLimiter::new();
        let start = Instant::now();
        rl.acquire(0, 1);
        assert!(start.elapsed().as_millis() < 50);
    }

    #[test]
    fn paces_sequential_transfers_to_the_configured_rate() {
        // 10 KB/s limit (10_000 bytes/sec), 0.5s per 5000-byte transfer. The
        // very first call ever is never delayed (nothing queued ahead of it
        // yet), so 3 calls impose only 2 real 0.5s waits - >= ~1s, not 1.5s.
        let rl = RateLimiter::new();
        let start = Instant::now();
        for _ in 0..3 {
            rl.acquire(5_000, 10);
        }
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            elapsed >= 0.9,
            "2 paced transfers after the free first one should take >= ~1s, took {elapsed}s"
        );
    }

    #[test]
    fn shares_the_rate_across_concurrent_callers() {
        // Prime the schedule with one call first (consuming the "nothing
        // queued yet" free slot), so the two timed calls below both have to
        // queue behind it. Two 5000-byte transfers against a shared 10KB/s
        // limiter must then serialize to ~1s combined (0.5s queued, then another
        // 0.5s): if the limiter weren't actually shared, both would instead
        // land their own independent "free" slot and finish in ~0.5s.
        let rl = Arc::new(RateLimiter::new());
        rl.acquire(5_000, 10);
        let start = Instant::now();
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let rl = Arc::clone(&rl);
                std::thread::spawn(move || rl.acquire(5_000, 10))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let elapsed = start.elapsed().as_secs_f64();
        assert!(
            elapsed >= 0.9,
            "two concurrent 5000-byte transfers sharing a primed 10KB/s limit \
             should take >= ~1s combined, took {elapsed}s"
        );
    }
}
