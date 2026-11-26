//! The gate every request to the upstream model passes through.
//!
//! It does two jobs, and the client sees neither of them:
//!
//! * **Concurrency limit** (`max_concurrent_requests`). Claude Code fans out
//!   subagents and helper calls in parallel, and an upstream with a low
//!   concurrent-request cap answers the surplus with `429`. With a limit set,
//!   the surplus waits here for a free slot instead, so to the client it is a
//!   slow reply, not an error. A slot is held until the reply is finished: for
//!   a streamed reply that is when the stream is dropped, because the upstream
//!   counts a request as in flight until its stream ends. Unset (the default)
//!   is no limit and no waiting.
//! * **`429` retry** (`rate_limit_retries`). A request the upstream refuses
//!   with `429` is sent again after an exponential backoff, up to the
//!   configured number of times. Only when the retries are spent is the `429`
//!   handed on, and the usual error mapping then reports it to the client as a
//!   `rate_limit_error`.
//!
//! A slot is released while a request backs off, so a refused request does not
//! keep the others waiting for nothing. The retry covers the HTTP status
//! only: an error event inside an already-started stream is not retried,
//! because the client has been sent part of the reply by then.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, StatusCode};
use tokio::sync::Semaphore;

/// The wait before the first retry; each further retry doubles it.
const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// The longest backoff between two attempts. An upstream `Retry-After` is
/// honoured above this (the error module caps it at a minute).
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// A held concurrency slot: it gives the slot back when dropped.
///
/// The permit's type is erased on purpose. Holding the slot for as long as
/// the reply lives is the whole point, but clippy's `significant_drop_*`
/// lints would flag every place a response is used, as if holding it were an
/// oversight.
type Slot = Box<dyn Any + Send + Sync>;

/// Limits concurrent upstream requests and retries `429`s. Cheap to clone:
/// every clone shares the same slots.
#[derive(Debug, Clone)]
pub struct UpstreamGate {
    /// `None` is no limit.
    slots: Option<Arc<Semaphore>>,
    retries: u32,
}

impl UpstreamGate {
    /// A gate allowing `max_concurrent` requests in flight (`0` is no limit)
    /// and re-sending a `429`ed request up to `retries` times.
    #[must_use]
    pub fn new(max_concurrent: u32, retries: u32) -> Self {
        let slots = (max_concurrent > 0).then(|| {
            let permits = usize::try_from(max_concurrent)
                .unwrap_or(Semaphore::MAX_PERMITS)
                .min(Semaphore::MAX_PERMITS);
            Arc::new(Semaphore::new(permits))
        });
        Self { slots, retries }
    }

    /// Send `request`, waiting for a free slot first and retrying a `429`.
    ///
    /// Any status other than a retried `429` is returned as it is, for the
    /// caller to judge; so is the last `429` once the retries are spent.
    pub async fn send(&self, request: RequestBuilder) -> Result<GatedResponse, reqwest::Error> {
        let mut request = request;
        let mut attempt = 0u32;
        loop {
            let permit = self.acquire().await;
            // A request whose body cannot be cloned (a stream) cannot be
            // re-sent, so it is simply not retried.
            let spare = if attempt < self.retries {
                request.try_clone()
            } else {
                None
            };
            let response = request.send().await?;
            let Some(next) = spare.filter(|_| response.status() == StatusCode::TOO_MANY_REQUESTS)
            else {
                return Ok(GatedResponse {
                    inner: response,
                    _slot: permit,
                });
            };
            let wait = backoff(
                attempt,
                crate::error::parse_retry_after(response.headers()),
                jitter_unit(),
            );
            tracing::warn!(
                attempt = attempt + 1,
                of = self.retries,
                wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                "the upstream answered 429; retrying after a backoff"
            );
            drop(response);
            drop(permit);
            tokio::time::sleep(wait).await;
            request = next;
            attempt += 1;
        }
    }

    /// Wait for a slot. `None` when no limit is set.
    async fn acquire(&self) -> Option<Slot> {
        let slots = self.slots.as_ref()?;
        if slots.available_permits() == 0 {
            tracing::debug!("all upstream slots are busy; queueing the request");
        }
        // The semaphore is never closed, so this cannot fail; a closed one
        // would mean no limit rather than a stuck request.
        let permit = Arc::clone(slots).acquire_owned().await.ok()?;
        Some(Box::new(permit))
    }
}

/// An upstream reply that still holds its concurrency slot.
///
/// The slot goes back when the reply is consumed (`text`, `bytes`) or, for a
/// stream, when the stream is dropped.
#[derive(Debug)]
pub struct GatedResponse {
    inner: reqwest::Response,
    _slot: Option<Slot>,
}

impl GatedResponse {
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// Read the whole body as text, then free the slot.
    pub async fn text(self) -> Result<String, reqwest::Error> {
        self.inner.text().await
    }

    /// Read the whole body, then free the slot.
    pub async fn bytes(self) -> Result<Bytes, reqwest::Error> {
        self.inner.bytes().await
    }

    /// The body as a stream. The slot is held until the stream is dropped.
    pub fn bytes_stream(self) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        let Self { inner, _slot } = self;
        inner.bytes_stream().map(move |chunk| {
            let _held = &_slot;
            chunk
        })
    }
}

/// How long to wait before retry number `attempt` (0 is the first).
///
/// The wait doubles each time from [`BACKOFF_BASE`] up to [`BACKOFF_CAP`], then
/// is scaled by `jitter` (a value in `0.0..=1.0`) to between half and all of
/// it, so requests refused together do not come back together. When the
/// upstream sent a `Retry-After`, the wait is never shorter than that.
fn backoff(attempt: u32, retry_after: Option<Duration>, jitter: f64) -> Duration {
    let doubled = BACKOFF_BASE.saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX));
    let wait = doubled
        .min(BACKOFF_CAP)
        .mul_f64(0.5f64.mul_add(jitter.clamp(0.0, 1.0), 0.5));
    retry_after.map_or(wait, |asked| asked.max(wait))
}

/// A cheap `0.0..1.0` value for jitter, from the clock's sub-second part.
/// It only needs to differ between requests, not be random.
fn jitter_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1000) / 1000.0
}

#[cfg(test)]
mod tests;
