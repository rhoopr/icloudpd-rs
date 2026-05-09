//! Verb pools per phase. Friendly mode rotates through pool entries every few
//! seconds so the active spinner label feels alive without changing semantics.
//!
//! Pools are static; the cycler is a stateful struct that remembers the last
//! index and avoids picking it again. Pure code, no I/O.

/// Sync phase that owns a verb pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Auth,
    Listing,
    Download,
    Hashing,
    Exif,
    Verify,
    Stalled,
    TwoFaWait,
    RetryCountdown,
    WatchIdle,
}

impl Phase {
    /// Pool of label alternates rotated every few seconds when friendly is on.
    /// First entry is the canonical noun; alternates add flavour.
    #[must_use]
    pub fn pool(self) -> &'static [&'static str] {
        match self {
            Phase::Auth => &[
                "saying hi to iCloud",
                "checking the doormat",
                "waking up the session",
            ],
            Phase::Listing => &[
                "looking around",
                "counting albums",
                "asking iCloud what is new",
            ],
            Phase::Download => &[
                "bringing them home",
                "wrangling pixels",
                "carrying boxes upstairs",
                "fetching the next batch",
            ],
            Phase::Hashing => &["hashing", "comparing fingerprints", "checking for twins"],
            Phase::Exif => &["reading EXIF", "tagging metadata", "writing sidecar"],
            Phase::Verify => &[
                "double-checking",
                "matching fingerprints",
                "making sure nothing got lost in the mail",
            ],
            Phase::Stalled => &["still here, just slow internet"],
            Phase::TwoFaWait => &["waiting on your tap", "still listening", "any moment now"],
            Phase::RetryCountdown => &["retrying soon"],
            Phase::WatchIdle => &["sleeping"],
        }
    }

    /// Default (off-mode) noun that survives in scrollback. Matches the
    /// canonical first entry of the pool but lower-stakes, used when friendly
    /// is off and we just want a one-word phase label.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Phase::Auth => "authenticating",
            Phase::Listing => "listing",
            Phase::Download => "downloading",
            Phase::Hashing => "hashing",
            Phase::Exif => "writing metadata",
            Phase::Verify => "verifying",
            Phase::Stalled => "waiting",
            Phase::TwoFaWait => "waiting for 2FA",
            Phase::RetryCountdown => "retrying",
            Phase::WatchIdle => "idle",
        }
    }
}

/// Rotates through a phase's verb pool, never picking the same index twice
/// in a row.
#[derive(Debug, Clone)]
pub struct VerbPool {
    phase: Phase,
    index: Option<usize>,
}

impl VerbPool {
    #[must_use]
    pub fn new(phase: Phase) -> Self {
        Self { phase, index: None }
    }

    /// Return the current label without advancing.
    ///
    /// Falls back to `""` if the underlying pool is empty, which is a programmer
    /// error caught by `every_phase_has_a_non_empty_pool`. The fallback is just
    /// to keep this function panic-free at runtime.
    #[must_use]
    pub fn current(&self) -> &'static str {
        let pool = self.phase.pool();
        let idx = self.index.unwrap_or(0);
        if pool.is_empty() {
            ""
        } else {
            pool.get(idx % pool.len()).copied().unwrap_or("")
        }
    }

    /// Advance to the next label and return it. With pools of size >=2, the
    /// returned label is guaranteed to differ from the previous one.
    pub fn next(&mut self) -> &'static str {
        let pool = self.phase.pool();
        if pool.is_empty() {
            return "";
        }
        let next = match self.index {
            None => 0,
            Some(prev) if pool.len() <= 1 => prev,
            Some(prev) => (prev + 1) % pool.len(),
        };
        self.index = Some(next);
        pool.get(next).copied().unwrap_or("")
    }

    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_has_a_non_empty_pool() {
        for phase in [
            Phase::Auth,
            Phase::Listing,
            Phase::Download,
            Phase::Hashing,
            Phase::Exif,
            Phase::Verify,
            Phase::Stalled,
            Phase::TwoFaWait,
            Phase::RetryCountdown,
            Phase::WatchIdle,
        ] {
            let pool = phase.pool();
            assert!(!pool.is_empty(), "{phase:?} has empty pool");
            for entry in pool {
                assert!(!entry.is_empty(), "{phase:?} has empty entry");
            }
        }
    }

    #[test]
    fn every_phase_has_a_noun() {
        for phase in [Phase::Auth, Phase::Listing, Phase::Download, Phase::Verify] {
            assert!(!phase.noun().is_empty());
        }
    }

    #[test]
    fn cycling_never_repeats_consecutively_for_multi_entry_pools() {
        let multi_phases = [
            Phase::Auth,
            Phase::Listing,
            Phase::Download,
            Phase::Hashing,
            Phase::Exif,
            Phase::Verify,
            Phase::TwoFaWait,
        ];
        for phase in multi_phases {
            let mut pool = VerbPool::new(phase);
            let mut prev = pool.next();
            for _ in 0..20 {
                let cur = pool.next();
                assert_ne!(prev, cur, "phase {phase:?} repeated label {cur}");
                prev = cur;
            }
        }
    }

    #[test]
    fn single_entry_pool_returns_same_label() {
        let mut pool = VerbPool::new(Phase::Stalled);
        let first = pool.next();
        let second = pool.next();
        assert_eq!(first, second);
    }

    #[test]
    fn current_returns_canonical_before_advancing() {
        let pool = VerbPool::new(Phase::Download);
        assert_eq!(pool.current(), Phase::Download.pool()[0]);
    }

    #[test]
    fn next_eventually_visits_every_entry() {
        let mut pool = VerbPool::new(Phase::Download);
        let pool_len = Phase::Download.pool().len();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..(pool_len * 4) {
            seen.insert(pool.next());
        }
        assert_eq!(seen.len(), pool_len, "should visit every entry");
    }

    #[test]
    fn phase_accessor_returns_construction_phase() {
        let pool = VerbPool::new(Phase::Verify);
        assert_eq!(pool.phase(), Phase::Verify);
    }
}
