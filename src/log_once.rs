//! Say a thing once per process.
//!
//! Several startup facts are established per `bench` invocation — which
//! library carries DWARF, which thermal zone was resolved, that the hitrace
//! level was raised. Printed once, each is useful. A suite runs `bench` per
//! cell, so twenty workloads across two engines print each of them forty
//! times, and the lines that matter (warnings, failed iterations) are lost in
//! the repetition.

use std::collections::BTreeSet;
use std::sync::Mutex;

static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// True the first time this `key` is offered, false afterwards.
///
/// Keys, not messages, so a fact whose text varies — a path, a zone name —
/// still prints once per *distinct* value: a run that switches engines
/// mid-suite should say so for each library, not stay silent about the second.
pub fn first_time(key: &str) -> bool {
    match SEEN.lock() {
        Ok(mut seen) => seen.insert(key.to_string()),
        // A poisoned mutex would only mean a previous logger panicked; saying
        // it twice is better than losing it.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_wins_per_key() {
        assert!(first_time("log_once::test::a"));
        assert!(!first_time("log_once::test::a"));
        // A different value of the same kind of fact still gets said.
        assert!(first_time("log_once::test::b"));
    }
}
