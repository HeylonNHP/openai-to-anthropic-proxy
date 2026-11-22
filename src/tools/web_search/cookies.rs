//! The search client's cookie jar: a session that survives restarts.
//!
//! A browser keeps its cookies between searches; a client that starts from a
//! blank slate on every request looks like a script. [`PersistentJar`] keeps
//! the cookies the search engine sets (DuckDuckGo sets a long-lived `kl`
//! region cookie), sends them back, and writes them to a file in the state
//! directory so the next run continues the same session.
//!
//! # Expiry
//!
//! Two independent clocks apply, and both are checked on every use:
//!
//! * **The cookies' own** `Expires` / `Max-Age`. An expired cookie is never
//!   sent, never saved and never loaded.
//! * **The session's** (`session_ttl`). A session cookie has no expiry of its
//!   own, so without a limit a persisted one would live forever. When the
//!   session is older than `session_ttl` every cookie is dropped and a fresh
//!   session starts, as a browser's would after a long absence.
//!
//! # The file
//!
//! `search-session.json` in the state directory, written to a temporary file
//! and renamed into place (a crash cannot leave half a file), readable by the
//! owner only on Unix. A missing, unreadable or corrupt file is not an error:
//! the session starts empty and a warning is logged. Failing to SAVE is
//! logged too and never fails a search: the session stays in memory.

use std::convert::Infallible;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cookie_store::{Cookie, CookieStore, RawCookie};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "search-session.json";
const FORMAT_VERSION: u32 = 1;

/// What is written to disk.
#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    /// Unix seconds at which this session began.
    started_at: u64,
    cookies: Vec<Cookie<'static>>,
}

struct State {
    store: CookieStore,
    /// Unix seconds at which the current session began.
    started_at: u64,
    /// The last text written (or loaded), so an unchanged jar is not rewritten.
    last_saved: Option<String>,
}

/// A cookie jar for `reqwest` that expires, rotates and persists.
pub struct PersistentJar {
    state: Mutex<State>,
    /// `None`: in memory only.
    path: Option<PathBuf>,
    session_ttl: Duration,
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl PersistentJar {
    /// Open the jar, continuing the session saved in `state_dir` if it is
    /// still valid.
    #[must_use]
    pub fn open(state_dir: Option<&Path>, session_ttl: Duration) -> Self {
        Self::open_at(state_dir, session_ttl, unix_now())
    }

    pub(crate) fn open_at(state_dir: Option<&Path>, session_ttl: Duration, now: u64) -> Self {
        let path = state_dir.map(|dir| dir.join(FILE_NAME));
        let loaded = path
            .as_deref()
            .and_then(|p| load(p, session_ttl, now))
            .map(|(store, started_at, text)| (store, started_at, Some(text)));
        let (store, started_at, last_saved) =
            loaded.unwrap_or_else(|| (CookieStore::default(), now, None));
        Self {
            state: Mutex::new(State {
                store,
                started_at,
                last_saved,
            }),
            path,
            session_ttl,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start a fresh session if the current one has outlived `session_ttl`.
    fn rotate_if_stale(&self, state: &mut State, now: u64) {
        if is_stale(state.started_at, self.session_ttl, now) {
            tracing::debug!("web_search session expired; starting a new one");
            state.store.clear();
            state.started_at = now;
        }
    }

    pub(crate) fn store_at(&self, headers: &[&str], url: &url::Url, now: u64) {
        let cookies = headers
            .iter()
            .filter_map(|h| RawCookie::parse((*h).to_owned()).ok());
        let mut state = self.lock();
        self.rotate_if_stale(&mut state, now);
        state.store.store_response_cookies(cookies, url);
        self.save(&mut state);
        drop(state);
    }

    pub(crate) fn header_at(&self, url: &url::Url, now: u64) -> Option<String> {
        let mut state = self.lock();
        self.rotate_if_stale(&mut state, now);
        let header = state
            .store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        drop(state);
        (!header.is_empty()).then_some(header)
    }

    /// The text that would be written now: only unexpired cookies.
    fn snapshot(state: &State) -> Option<String> {
        let file = SessionFile {
            version: FORMAT_VERSION,
            started_at: state.started_at,
            cookies: state.store.iter_unexpired().cloned().collect(),
        };
        serde_json::to_string(&file).ok()
    }

    fn save(&self, state: &mut State) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        // Nothing to remember and nothing remembered: leave the disk alone.
        if state.last_saved.is_none() && state.store.iter_unexpired().next().is_none() {
            return;
        }
        let Some(text) = Self::snapshot(state) else {
            return;
        };
        if state.last_saved.as_deref() == Some(text.as_str()) {
            return;
        }
        match write_atomic(path, &text) {
            Ok(()) => state.last_saved = Some(text),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not save the web_search session; it stays in memory only"
            ),
        }
    }
}

impl reqwest::cookie::CookieStore for PersistentJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        let headers: Vec<&str> = cookie_headers.filter_map(|h| h.to_str().ok()).collect();
        self.store_at(&headers, url, unix_now());
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        self.header_at(url, unix_now())
            .and_then(|h| HeaderValue::from_str(&h).ok())
    }
}

fn is_stale(started_at: u64, ttl: Duration, now: u64) -> bool {
    // A start time in the future means the clock moved: do not trust it.
    started_at > now || now - started_at > ttl.as_secs()
}

/// Read the session file. `None` = start fresh (missing, corrupt, from
/// another version, or older than the session limit).
fn load(path: &Path, ttl: Duration, now: u64) -> Option<(CookieStore, u64, String)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read the web_search session; starting a new one");
            return None;
        }
    };
    let file: SessionFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "the web_search session file is corrupt; starting a new one");
            return None;
        }
    };
    if file.version != FORMAT_VERSION {
        tracing::warn!(path = %path.display(), version = file.version, "the web_search session file has an unknown version; starting a new one");
        return None;
    }
    if is_stale(file.started_at, ttl, now) {
        tracing::debug!(path = %path.display(), "the saved web_search session has expired; starting a new one");
        return None;
    }
    // `false`: expired cookies are not loaded.
    let store = CookieStore::from_cookies(file.cookies.into_iter().map(Ok::<_, Infallible>), false)
        .unwrap_or_default();
    Some((store, file.started_at, text))
}

/// Write `text` to `path` through a temporary file in the same directory.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = open_private(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

#[cfg(test)]
mod tests;
