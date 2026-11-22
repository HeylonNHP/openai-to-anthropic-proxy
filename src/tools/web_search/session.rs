//! The search client's long-lived state: one HTTP client, one cookie session
//! and one result cache per search endpoint, shared by every request.
//!
//! A browser reuses its connection, keeps its cookies and does not repeat a
//! search it just ran. A client built afresh for each request does none of
//! that, which is exactly what an anti-bot system looks for. So the state
//! lives here, process-wide and keyed like [`super::gate`], not in the
//! per-request [`super::backend::LiveBackend`].
//!
//! # What the client can and cannot match
//!
//! The client is `reqwest` over `rustls`. It negotiates HTTP/2, and its HTTP/2
//! connection settings are tuned to Chrome's where `reqwest` exposes them
//! (see [`build_client`]). Its TLS ClientHello (the JA3 / JA4 fingerprint:
//! cipher and extension lists and their order) and its HTTP/2 pseudo-header
//! order are `rustls`' and `hyper`'s, and neither can be changed without
//! replacing the TLS stack (for example with BoringSSL through `wreq`). That
//! gap is real; this module narrows everything else.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::config::WebSearchConfig;
use crate::tools::web_search::cache::ResultCache;
use crate::tools::web_search::cookies::PersistentJar;
use crate::tools::web_search::diagnostics::Context;

/// Chrome's HTTP/2 initial stream window (SETTINGS_INITIAL_WINDOW_SIZE).
const H2_STREAM_WINDOW: u32 = 6_291_456;
/// Chrome's connection window: the 65 535 default plus its first
/// WINDOW_UPDATE of 15 663 105.
const H2_CONNECTION_WINDOW: u32 = 15_728_640;
/// Chrome's SETTINGS_MAX_HEADER_LIST_SIZE.
const H2_MAX_HEADER_LIST: u32 = 262_144;

/// Running totals for one session, kept for the failure log: a block after 40
/// good searches and a block on the first search call for different fixes.
#[derive(Debug, Default)]
struct Stats {
    searches: u64,
    /// Successful searches since the last failure.
    ok_streak: u64,
    last_ok: Option<Instant>,
    last_block: Option<Instant>,
}

/// Where the session stood when a search failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub searches: u64,
    pub ok_streak: u64,
    pub secs_since_last_ok: Option<u64>,
    pub secs_since_last_block: Option<u64>,
}

/// Everything a search needs that outlives one client request.
pub struct SearchSession {
    pub client: reqwest::Client,
    pub cache: ResultCache,
    jar: Arc<PersistentJar>,
    stats: Mutex<Stats>,
}

/// Build the search HTTP client around `jar`.
///
/// # Errors
/// The client could not be built.
pub fn build_client(jar: Arc<PersistentJar>) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .http1_title_case_headers()
        .http2_initial_stream_window_size(H2_STREAM_WINDOW)
        .http2_initial_connection_window_size(H2_CONNECTION_WINDOW)
        .http2_max_header_list_size(H2_MAX_HEADER_LIST)
        .cookie_provider(jar)
        .redirect(reqwest::redirect::Policy::limited(3))
        // No client-wide timeout: the session is shared, so the budget is set
        // on each request.
        // This endpoint is the operator-configured search service, not an
        // arbitrary URL fetched on a model's behalf. System proxy support is
        // safe here; page fetches use their separate pinned client, which
        // never sees this jar.
        .build()
        .map_err(|e| format!("could not build the search HTTP client: {e}"))
}

impl SearchSession {
    fn open(cfg: &WebSearchConfig) -> Result<Self, String> {
        let jar = Arc::new(PersistentJar::open(
            cfg.state_dir.as_deref(),
            Duration::from_secs(cfg.session_ttl_hours.saturating_mul(3600)),
        ));
        Ok(Self {
            client: build_client(Arc::clone(&jar))?,
            jar,
            stats: Mutex::default(),
            cache: ResultCache::new(
                Duration::from_secs(cfg.cache_ttl_secs),
                cfg.cache_max_entries,
            ),
        })
    }
}

impl SearchSession {
    fn stats(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A search got a results page.
    pub fn note_ok(&self) {
        let mut stats = self.stats();
        stats.searches += 1;
        stats.ok_streak += 1;
        stats.last_ok = Some(Instant::now());
    }

    /// A search failed (`blocked`: the engine refused it). Returns where the
    /// session stood BEFORE this failure: how many searches had succeeded in a
    /// row, and how long since the last success and the last block.
    pub fn note_failure(&self, blocked: bool) -> StatsSnapshot {
        let mut stats = self.stats();
        stats.searches += 1;
        let now = Instant::now();
        let snapshot = StatsSnapshot {
            searches: stats.searches,
            ok_streak: stats.ok_streak,
            secs_since_last_ok: stats.last_ok.map(|t| now.duration_since(t).as_secs()),
            secs_since_last_block: stats.last_block.map(|t| now.duration_since(t).as_secs()),
        };
        stats.ok_streak = 0;
        if blocked {
            stats.last_block = Some(now);
        }
        drop(stats);
        snapshot
    }

    /// The request and session facts the failure log carries.
    #[must_use]
    pub fn context(
        &self,
        endpoint: &str,
        region: &str,
        query: &str,
        stats: StatsSnapshot,
    ) -> Context {
        let (cookies_sent, session_age_secs) =
            url::Url::parse(endpoint).map_or((0, 0), |u| self.jar.summary(&u));
        Context {
            query: query.to_owned(),
            region: region.to_owned(),
            endpoint: endpoint.to_owned(),
            cookies_sent,
            session_age_secs,
            searches: stats.searches,
            ok_streak: stats.ok_streak,
            secs_since_last_ok: stats.secs_since_last_ok,
            secs_since_last_block: stats.secs_since_last_block,
        }
    }

    /// The running totals as they stand (for a search that did not fail).
    pub fn snapshot(&self) -> StatsSnapshot {
        let stats = self.stats();
        let now = Instant::now();
        let snapshot = StatsSnapshot {
            searches: stats.searches,
            ok_streak: stats.ok_streak,
            secs_since_last_ok: stats.last_ok.map(|t| now.duration_since(t).as_secs()),
            secs_since_last_block: stats.last_block.map(|t| now.duration_since(t).as_secs()),
        };
        drop(stats);
        snapshot
    }
}

/// The session for `cfg`'s endpoint and state directory, shared by every
/// request in the process. The first request to ask creates it (loading the
/// saved cookies); later requests reuse it.
///
/// # Errors
/// The HTTP client could not be built.
pub fn session_for(cfg: &WebSearchConfig) -> Result<Arc<SearchSession>, String> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<SearchSession>>>> = OnceLock::new();
    let key = format!("{}\n{:?}", cfg.search_endpoint, cfg.state_dir);
    let mut sessions = SESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = sessions.get(&key) {
        return Ok(Arc::clone(existing));
    }
    let session = Arc::new(SearchSession::open(cfg)?);
    sessions.insert(key, Arc::clone(&session));
    drop(sessions);
    Ok(session)
}

#[cfg(test)]
mod tests;
