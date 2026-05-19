//! Verb pool for the active listing label.
//!
//! The download bar starts before the first file is available, so friendly mode
//! rotates a few listing verbs to show that enumeration is still alive. Other
//! phase narration is handled directly in `narration.rs`.

const LISTING_VERBS: &[&str] = &[
    "looking around",
    "counting albums",
    "asking iCloud what is new",
];

/// Rotates through the listing verb pool, never picking the same index twice
/// in a row.
#[derive(Debug, Clone)]
pub struct VerbPool {
    index: Option<usize>,
}

impl VerbPool {
    #[must_use]
    pub fn new() -> Self {
        Self { index: None }
    }

    /// Advance to the next label and return it. The listing pool has at least
    /// two entries, so the returned label differs from the previous one.
    pub fn next(&mut self) -> &'static str {
        let next = match self.index {
            None => 0,
            Some(prev) => (prev + 1) % LISTING_VERBS.len(),
        };
        self.index = Some(next);
        LISTING_VERBS.get(next).copied().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_pool_has_multiple_non_empty_entries() {
        assert!(
            LISTING_VERBS.len() >= 2,
            "listing pool must have enough entries to rotate"
        );
        for entry in LISTING_VERBS {
            assert!(!entry.is_empty(), "listing verb must be non-empty");
        }
    }

    #[test]
    fn cycling_never_repeats_consecutively() {
        let mut pool = VerbPool::new();
        let mut prev = pool.next();
        for _ in 0..20 {
            let cur = pool.next();
            assert_ne!(prev, cur, "repeated listing label {cur}");
            prev = cur;
        }
    }

    #[test]
    fn next_eventually_visits_every_entry() {
        let mut pool = VerbPool::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..(LISTING_VERBS.len() * 4) {
            seen.insert(pool.next());
        }
        assert_eq!(seen.len(), LISTING_VERBS.len(), "should visit every entry");
    }
}
