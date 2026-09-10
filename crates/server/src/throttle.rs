//! Bounded key budgets. Registered accounts and unknown-name buckets are separate.
use std::collections::BTreeMap;

pub struct Budget {
    entries: BTreeMap<String, (u64, u32)>,
    capacity: usize,
    attempts: u32,
}

impl Budget {
    pub fn new(capacity: usize, attempts: u32) -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity,
            attempts,
        }
    }

    pub fn allow(&mut self, key: &str, seconds: u64) -> bool {
        self.entries
            .retain(|_, (start, _)| seconds.saturating_sub(*start) < 60);
        if !self.entries.contains_key(key) && self.entries.len() >= self.capacity {
            return false;
        }
        let entry = self.entries.entry(key.to_owned()).or_insert((seconds, 0));
        if entry.1 >= self.attempts {
            return false;
        }
        entry.1 += 1;
        true
    }

    pub fn reset(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_004_principals_have_independent_bounded_budgets() {
        let mut b = Budget::new(2, 2);
        assert!(b.allow("attacker", 0));
        assert!(b.allow("attacker", 0));
        assert!(!b.allow("attacker", 0));
        assert!(b.allow("legitimate", 0));
        assert!(!b.allow("new-key", 0)); // Full map does not evict an existing principal.
        assert!(b.allow("legitimate", 0));
        assert!(!b.allow("legitimate", 0));
        assert!(b.allow("new-key", 60));
        assert_eq!(b.entries.len(), 1);
    }
}
