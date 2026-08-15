//! Process-lifetime token accounting for the terminal dashboard.
//!
//! This store is deliberately separate from configuration and runtime model
//! mappings. It is never serialized: dropping the process drops the totals.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Token counters for completed responses attributed to one display model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TokenTotals {
    pub fn add(&mut self, usage: TokenUsage) {
        self.requests += 1;
        self.input_tokens += u64::from(usage.input_tokens);
        self.output_tokens += u64::from(usage.output_tokens);
        self.cache_read_input_tokens += u64::from(usage.cache_read_input_tokens);
        self.cache_creation_input_tokens += u64::from(usage.cache_creation_input_tokens);
        self.reasoning_tokens += u64::from(usage.reasoning_tokens);
    }
}

/// Usage values extracted from either a Responses JSON response or a stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub reasoning_tokens: u32,
}

/// Shared, in-memory session totals. A session is one process lifetime.
#[derive(Debug, Clone, Default)]
struct TotalsByKind {
    inbound: BTreeMap<String, TokenTotals>,
    actual: BTreeMap<String, TokenTotals>,
}

/// A consistent point-in-time view of inbound and fallback/actual buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStatsSnapshot {
    pub inbound: BTreeMap<String, TokenTotals>,
    pub actual: BTreeMap<String, TokenTotals>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStatsStore {
    totals: Arc<Mutex<TotalsByKind>>,
}

impl SessionStatsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record exactly one completed response under its inbound model name.
    pub fn record(&self, model: impl Into<String>, usage: TokenUsage) {
        let mut totals = self.totals.lock().expect("session stats mutex poisoned");
        totals.inbound.entry(model.into()).or_default().add(usage);
    }

    /// Record exactly one completed fallback response under the actual model
    /// that produced it. Keeping this separate prevents a fallback model
    /// whose name happens to be an inbound alias from being misclassified in
    /// the TUI.
    pub fn record_actual(&self, model: impl Into<String>, usage: TokenUsage) {
        let mut totals = self.totals.lock().expect("session stats mutex poisoned");
        totals.actual.entry(model.into()).or_default().add(usage);
    }

    /// Take a consistent point-in-time copy for TUI rendering or tests.
    pub fn snapshot_sections(&self) -> SessionStatsSnapshot {
        let totals = self.totals.lock().expect("session stats mutex poisoned");
        SessionStatsSnapshot {
            inbound: totals.inbound.clone(),
            actual: totals.actual.clone(),
        }
    }

    /// Return all totals merged by display name. Prefer `snapshot_sections`
    /// when the distinction between inbound and actual models matters.
    pub fn snapshot(&self) -> BTreeMap<String, TokenTotals> {
        let totals = self.snapshot_sections();
        let mut merged = totals.inbound;
        for (model, total) in totals.actual {
            merged.entry(model).or_default().add_totals(total);
        }
        merged
    }
}

impl TokenTotals {
    pub(crate) fn add_totals(&mut self, other: Self) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
            reasoning_tokens: 5,
        }
    }

    #[test]
    fn aggregates_by_model() {
        let store = SessionStatsStore::new();
        store.record("claude-sonnet", usage(10, 20));
        store.record("claude-sonnet", usage(2, 3));
        store.record("claude-opus", usage(7, 8));

        let totals = store.snapshot();
        assert_eq!(totals["claude-sonnet"].requests, 2);
        assert_eq!(totals["claude-sonnet"].input_tokens, 12);
        assert_eq!(totals["claude-sonnet"].output_tokens, 23);
        assert_eq!(totals["claude-sonnet"].cache_read_input_tokens, 6);
        assert_eq!(totals["claude-sonnet"].reasoning_tokens, 10);
        assert_eq!(totals["claude-opus"].requests, 1);
    }

    #[test]
    fn fallback_can_be_attributed_to_actual_model() {
        let store = SessionStatsStore::new();
        // The request's original inbound name is intentionally not recorded:
        // the retry's actual fallback model owns this completion.
        store.record_actual("gpt-fallback", usage(11, 12));
        let snapshot = store.snapshot_sections();
        assert!(!snapshot.inbound.contains_key("claude-unknown"));
        assert_eq!(snapshot.actual["gpt-fallback"].output_tokens, 12);
    }
}
