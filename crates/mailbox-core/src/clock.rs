//! The one injected source of time, and the one injected source of randomness
//! (§6.7).
//!
//! Nothing in the core calls `SystemTime::now()` or a thread-local RNG
//! directly. Everything that needs either takes a [`Clock`] or an [`Rng`], so a
//! test can pin both and get byte-identical output — which is what makes the
//! golden files of §14 and the convergence oracle of §14.3 possible at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds. The store's native time unit, so this is what crosses every
/// internal boundary; RFC3339 formatting happens at the edge, in the contract.
pub type Timestamp = i64;

/// Where "now" comes from.
#[derive(Debug, Clone)]
pub enum Clock {
    /// The host clock.
    System,
    /// Pinned by `MAILBOX_CLOCK`, and advanced only by [`Clock::advance`].
    Fixed(Arc<AtomicU64>),
}

impl Clock {
    /// Reads `MAILBOX_CLOCK` (RFC3339, or bare unix seconds) and pins the clock
    /// when it is set. Unset is the normal case and yields the host clock.
    pub fn from_env() -> Clock {
        match std::env::var("MAILBOX_CLOCK") {
            Ok(v) if !v.is_empty() => match parse_timestamp(&v) {
                Some(ts) => Clock::Fixed(Arc::new(AtomicU64::new(ts as u64))),
                // A malformed pin is a test-harness bug, and silently running
                // on the host clock would make it look like a flaky test.
                None => panic!("MAILBOX_CLOCK is not an RFC3339 time or unix timestamp: {v:?}"),
            },
            _ => Clock::System,
        }
    }

    pub fn fixed(ts: Timestamp) -> Clock {
        Clock::Fixed(Arc::new(AtomicU64::new(ts as u64)))
    }

    pub fn now(&self) -> Timestamp {
        match self {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            Clock::Fixed(t) => t.load(Ordering::SeqCst) as i64,
        }
    }

    /// Moves a pinned clock forward. A no-op on the system clock, so a test
    /// that forgets to pin fails on its assertions rather than on this call.
    pub fn advance(&self, seconds: u64) {
        if let Clock::Fixed(t) = self {
            t.fetch_add(seconds, Ordering::SeqCst);
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::from_env()
    }
}

/// A counter-mode PRNG over BLAKE3.
///
/// Deterministic when seeded from `MAILBOX_SEED`, drawn from the OS otherwise.
/// This is not the pairing PAKE's RNG — that arrives in M4 with its own
/// review — it is for the ordinary things that must not be ambient: idempotency
/// keys, generated `Message-ID`s, socket names.
#[derive(Debug)]
pub struct Rng {
    key: [u8; 32],
    counter: AtomicU64,
}

impl Rng {
    /// Seeded by `MAILBOX_SEED` when set, by the OS otherwise.
    pub fn from_env() -> Rng {
        match std::env::var("MAILBOX_SEED") {
            Ok(v) if !v.is_empty() => Rng::seeded(v.as_bytes()),
            _ => {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key).expect("the OS refused to provide entropy");
                Rng {
                    key,
                    counter: AtomicU64::new(0),
                }
            }
        }
    }

    pub fn seeded(seed: &[u8]) -> Rng {
        Rng {
            key: *blake3::hash(seed).as_bytes(),
            counter: AtomicU64::new(0),
        }
    }

    pub fn fill(&self, out: &mut [u8]) {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(&n.to_le_bytes());
        h.finalize_xof().fill(out);
    }

    pub fn bytes<const N: usize>(&self) -> [u8; N] {
        let mut out = [0u8; N];
        self.fill(&mut out);
        out
    }

    /// Lowercase hex, `n` bytes wide.
    pub fn hex(&self, n: usize) -> String {
        let mut out = vec![0u8; n];
        self.fill(&mut out);
        crate::hex::encode(&out)
    }
}

impl Default for Rng {
    fn default() -> Self {
        Rng::from_env()
    }
}

/// Accepts RFC3339 (`2026-08-01T10:00:00Z`) or bare unix seconds.
pub fn parse_timestamp(s: &str) -> Option<Timestamp> {
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|dt| dt.unix_timestamp())
}

/// RFC3339 in UTC, seconds precision — the contract's only date format.
pub fn format_rfc3339(ts: Timestamp) -> String {
    let dt =
        time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    dt.replace_nanosecond(0)
        .unwrap_or(dt)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
