//! Pacing for the search engine: one search at a time, a minimum gap between
//! them, and a record of when the engine last blocked us.
//!
//! The engine is scraped, and it answers a burst of queries from one address
//! with a bot challenge. Two things follow from that:
//!
//! * Searches are **serialised** and spaced out, across every request the
//!   proxy is serving (the gate is process-wide, keyed by endpoint), so the
//!   model cannot fire a burst however many queries it asks for at once.
//! * When a search IS blocked, the model is told to wait; if it comes back
//!   and is blocked again, it is told how long ago the previous blocked
//!   search was, so it can see how fast it is hammering the engine. A search
//!   that succeeds clears that record.
//!
//! Nothing here refuses a search: a blocked engine is still queried when the
//! model asks, because the block may well have lifted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Instant;

#[derive(Debug, Default)]
struct State {
    /// When the last search through the gate finished.
    last_finished: Option<Instant>,
    /// When the engine last blocked a search; cleared by a success.
    last_blocked: Option<Instant>,
}

/// The pacing state for one search endpoint.
#[derive(Debug, Default)]
pub struct SearchGate {
    state: Mutex<State>,
}

/// The gate for `endpoint`, shared by every request in the process.
pub fn gate_for(endpoint: &str) -> Arc<SearchGate> {
    static GATES: OnceLock<StdMutex<HashMap<String, Arc<SearchGate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(gates.entry(endpoint.to_owned()).or_default())
}

/// Exclusive use of the gate for one search. Dropping it marks the search
/// finished, which is what the next search's gap is measured from.
pub struct Slot<'a> {
    state: MutexGuard<'a, State>,
}

impl SearchGate {
    /// Wait for any search already running, then for `min_interval` since it
    /// finished.
    pub async fn acquire(&self, min_interval: Duration) -> Slot<'_> {
        let state = self.state.lock().await;
        if let Some(finished) = state.last_finished {
            let ready = finished + min_interval;
            if ready > Instant::now() {
                tokio::time::sleep_until(ready).await;
            }
        }
        Slot { state }
    }
}

impl Slot<'_> {
    /// The search got results: the engine is not blocking us.
    pub fn succeeded(&mut self) {
        self.state.last_blocked = None;
    }

    /// The engine blocked this search. Returns how long ago it last blocked
    /// one, if it has since the last success.
    pub fn blocked(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let previous = self.state.last_blocked.map(|t| now.duration_since(t));
        self.state.last_blocked = Some(now);
        previous
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.state.last_finished = Some(Instant::now());
    }
}

/// `83 seconds (1.4 minutes)`.
fn describe(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    #[allow(clippy::cast_precision_loss)]
    let minutes = secs as f64 / 60.0;
    let unit = if secs == 1 { "second" } else { "seconds" };
    format!("{secs} {unit} ({minutes:.1} minutes)")
}

/// The tool-result text for a blocked search: what happened, how long to wait
/// and, if the model was blocked before, how recently.
pub fn blocked_message(what: &str, since_previous: Option<Duration>, wait_minutes: u64) -> String {
    let unit = if wait_minutes == 1 {
        "minute"
    } else {
        "minutes"
    };
    let wait = format!("Wait about {wait_minutes} {unit} before searching again.");
    since_previous.map_or_else(
        || format!("{what}. {wait}"),
        |elapsed| {
            format!(
                "{what}, again. The previous blocked search was {} ago, so searches are being \
                 retried faster than the search engine allows. {wait}",
                describe(elapsed)
            )
        },
    )
}

#[cfg(test)]
mod tests;
