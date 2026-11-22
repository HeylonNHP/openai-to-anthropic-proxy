//! A short-lived cache of search results.
//!
//! Models repeat queries (within one turn and across turns), and every search
//! that is answered from here is a request the search engine never sees.
//!
//! * Only SUCCESSFUL, non-empty results are stored: a block, an error or an
//!   empty page is never replayed.
//! * An entry is valid for `ttl`. Expiry is checked on every lookup and
//!   expired entries are swept on every insert, so the cache cannot hold
//!   stale results or grow past `max_entries`.
//! * The key is the region plus the query with whitespace collapsed and case
//!   folded; the number of results asked for is NOT part of it (see
//!   [`ResultCache::get`]).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::tools::web_search::ddg::SearchHit;

struct Entry {
    hits: Vec<SearchHit>,
    /// How many results the search that produced this entry asked for.
    limit: usize,
    stored: Instant,
}

/// Results cached by `(region, query)`.
pub struct ResultCache {
    entries: Mutex<HashMap<(String, String), Entry>>,
    ttl: Duration,
    max_entries: usize,
}

fn key(region: &str, query: &str) -> (String, String) {
    let query = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (region.to_owned(), query)
}

impl ResultCache {
    #[must_use]
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries: max_entries.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(String, String), Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn expired(&self, entry: &Entry, now: Instant) -> bool {
        now.saturating_duration_since(entry.stored) >= self.ttl
    }

    /// The cached results for this search, at most `limit` of them.
    ///
    /// An entry answers a request when it holds at least `limit` hits, or
    /// when its page ran out before reaching its own limit (so asking for
    /// more could not have found more). Otherwise it is a miss.
    pub fn get(&self, region: &str, query: &str, limit: usize) -> Option<Vec<SearchHit>> {
        let now = Instant::now();
        let key = key(region, query);
        let mut entries = self.lock();
        if entries.get(&key).is_some_and(|e| self.expired(e, now)) {
            entries.remove(&key);
            return None;
        }
        let entry = entries.get(&key)?;
        let complete = entry.hits.len() >= limit || entry.hits.len() < entry.limit;
        let hits = complete.then(|| entry.hits.iter().take(limit).cloned().collect());
        drop(entries);
        hits
    }

    /// Remember a successful search. Empty results are not stored.
    pub fn put(&self, region: &str, query: &str, limit: usize, hits: &[SearchHit]) {
        if hits.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut entries = self.lock();
        entries.retain(|_, e| !self.expired(e, now));
        let key = key(region, query);
        if !entries.contains_key(&key) && entries.len() >= self.max_entries {
            let oldest = entries
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            key,
            Entry {
                hits: hits.to_vec(),
                limit,
                stored: now,
            },
        );
    }

    /// How many entries are held (expired ones included until swept).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests;
