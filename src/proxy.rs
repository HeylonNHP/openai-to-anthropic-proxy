//! axum router and request handlers.
//!
//! Wires together: read Anthropic request → translate to Responses →
//! forward to upstream → translate response (or stream) back → ship to
//! Claude Code.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use eventsource_stream::{EventStream, Eventsource};
use futures_util::{Stream, StreamExt};

use crate::anthropic::{CreateMessageRequest, StreamEvent};
use crate::capabilities::{self, CapabilityStore};
use crate::config::Config;
use crate::error::AppError;
use crate::model_rejection;
use crate::responses;
use crate::stream::StreamTranslator;
use crate::translate;
use crate::tui::MappingsStore;
use eventsource_stream::Event as SseEvent;
use eventsource_stream::EventStreamError;

/// Shared state passed to every handler.
///
/// `config` holds the static, startup-only settings (listen address,
/// upstream URL, API key, ...). `mappings` is the runtime, hot-editable
/// alias store; handlers take a snapshot from it at the start of each
/// request so an in-flight request finishes with the mapping it began
/// with. `output` is the per-process log sink that routes request
/// lines into the TUI log pane or, with `--no-tui`, to stdout.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub mappings: Arc<MappingsStore>,
    pub client: reqwest::Client,
    pub output: crate::tui::OutputSink,
    /// Process-lifetime token totals shared with the TUI.
    pub stats: Arc<crate::tui::SessionStatsStore>,
    /// Process-lifetime record of parameter support learned from
    /// upstream rejections (see [`crate::capabilities`]). Shared across
    /// every request so a fact learned once is applied everywhere.
    pub capabilities: Arc<CapabilityStore>,
    /// Inbound model names seen that are **not** in the alias map, so
    /// the operator can be told about them once each.
    ///
    /// A model that is not in `model_aliases.map` is passed through to the
    /// upstream unchanged. That is sometimes right (the upstream may serve
    /// the name itself) and sometimes the reason a request 404s — and it
    /// used to happen *silently*, which is how eleven upstream 404s went
    /// unremarked. This set makes the passthrough visible without
    /// flooding the log with one line per request.
    pub unmapped_models: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// Make a client-controlled string safe to put in a log line.
///
/// Model names come from the client, so they can carry newlines, ANSI
/// escapes, or anything else that would forge a log record or repaint
/// the operator's terminal. Control characters are dropped and the
/// length is capped.
fn log_safe(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "<empty>".to_owned()
    } else {
        cleaned
    }
}

/// How many distinct unmapped inbound model names we remember before
/// stopping tracking new ones. A client could otherwise send an unbounded
/// number of them.
const MAX_TRACKED_UNMAPPED_MODELS: usize = 256;

/// Build the axum router.
pub fn router(
    config: Arc<Config>,
    mappings: Arc<MappingsStore>,
    client: reqwest::Client,
    output: crate::tui::OutputSink,
) -> Router {
    router_with_stats(
        config,
        mappings,
        client,
        output,
        Arc::new(crate::tui::SessionStatsStore::new()),
    )
}

/// Build a router using the process-lifetime stats store shared by the TUI.
pub fn router_with_stats(
    config: Arc<Config>,
    mappings: Arc<MappingsStore>,
    client: reqwest::Client,
    output: crate::tui::OutputSink,
    stats: Arc<crate::tui::SessionStatsStore>,
) -> Router {
    router_with_stores(
        config,
        mappings,
        client,
        output,
        stats,
        Arc::new(CapabilityStore::new()),
    )
}

/// Build a router with every long-lived store supplied by the caller.
///
/// `capabilities` is the process-lifetime record of parameter support
/// learned from upstream rejections. `main.rs` creates one and shares
/// it across the server paths so the knowledge survives for the whole
/// process rather than for a single connection.
pub fn router_with_stores(
    config: Arc<Config>,
    mappings: Arc<MappingsStore>,
    client: reqwest::Client,
    output: crate::tui::OutputSink,
    stats: Arc<crate::tui::SessionStatsStore>,
    capabilities: Arc<CapabilityStore>,
) -> Router {
    let state = AppState {
        config,
        mappings,
        client,
        unmapped_models: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        output,
        stats,
        capabilities,
    };
    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/healthz", get(handle_health))
        .with_state(state)
}

// ─── Handlers ─────────────────────────────────────────────────────────────

async fn handle_health() -> &'static str {
    "ok"
}

async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    // Auth check: if `proxy_key` is set, the client must include a
    // matching `X-Proxy-Key` header. The check happens before any
    // body parsing so an unauthorized client can't even probe for
    // valid request shapes.
    check_proxy_key(&state.config, &headers)?;

    let req: CreateMessageRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))?;

    // Snapshot the runtime mappings at handler entry. A change
    // applied mid-request never tears the snapshot — the request
    // finishes with the mappings it started with. This matches the
    // design contract on `MappingsStore` (see `tui/runtime.rs`):
    // readers vastly outnumber writers, so we take a cheap atomic
    // refcount bump and release the snapshot when the handler ends.
    let mappings_snapshot = state.mappings.load_live().clone();

    // Resolve the inbound model name to the upstream model name BEFORE
    // building the request — so the reasoning-effort lookup uses the
    // *resolved* name (e.g. an aliased `claude-sonnet-5` request picks
    // up the reasoning entry for `gpt-5.4-mini`, not the entry for
    // `claude-sonnet-5`). The snapshot is what makes TUI edits take
    // effect: the static `Config::model_aliases` is the startup-only
    // seed, while `mappings_snapshot` is what the operator sees when
    // they hit `e` in the TUI.
    let upstream_model = mappings_snapshot.resolve(&req.model);
    // An inbound name that is not in the alias map is passed through
    // unchanged. That is legitimate when the upstream serves the name
    // itself, but it is also exactly how a request ends up 404ing — and
    // it used to happen silently. Say so, once per distinct name.
    if !mappings_snapshot.map.contains_key(&req.model) {
        let first_time = state
            .unmapped_models
            .lock()
            .map(|mut seen| {
                // Bounded: a client could otherwise send an unbounded
                // number of distinct model names.
                if seen.len() >= MAX_TRACKED_UNMAPPED_MODELS {
                    return false;
                }
                seen.insert(req.model.clone())
            })
            .unwrap_or(false);
        if first_time {
            tracing::warn!(
                inbound_model = %log_safe(&req.model),
                upstream_model = %log_safe(&upstream_model),
                "inbound model is not in the alias map; passing it through unchanged"
            );
            state.output.emit(crate::tui::LogLine {
                kind: crate::tui::LogKind::Warning,
                text: format!(
                    "  \u{26a0} {} is not in the alias map — add it to model_aliases.map \
                     (sent to the upstream as {})",
                    log_safe(&req.model),
                    log_safe(&upstream_model)
                ),
            });
        }
    }
    // Resolve the reasoning intent from the client's explicit signals
    // (output_config.effort, thinking.type = "disabled") against the
    // configured effort/thinking-disabled maps, falling back to the
    // fixed per-model config when the client sent neither. Owned here so
    // `req` can move into `Attempt` without a self-referential borrow.
    let requested_effort = req.output_config.as_ref().and_then(|oc| oc.effort.clone());
    let thinking_disabled = req
        .thinking
        .as_ref()
        .is_some_and(|t| t.r#type.eq_ignore_ascii_case("disabled"));
    // Both the first attempt and the `default_model` fallback go through
    // `build_outbound`, so their policies cannot drift apart.
    // `ReasoningReplay` holds a `Cell` (!Sync): read its counter in the
    // log below and let it drop before `Attempt::run` reaches an await.
    let (outbound, registry) = {
        let (outbound, registry, reasoning_replay) = build_outbound(
            &req,
            state.config.as_ref(),
            &upstream_model,
            requested_effort.as_deref(),
            thinking_disabled,
        )?;

        // Serialize once, log a short summary, and stash the full body for
        // the error path. The body is the single most useful artifact when
        tracing::info!(
            inbound_model = %log_safe(&req.model),
            upstream_model = %log_safe(&outbound.model),
            stream = outbound.stream,
            tools = outbound.tools.as_ref().map_or(0, Vec::len),
            inbound_thinking = ?req.thinking.as_ref().map(|t| t.r#type.as_str()),
            inbound_effort = ?requested_effort,
            reasoning_effort = ?outbound.reasoning.as_ref().map(|r| r.effort.clone()),
            // How many characters of prior reasoning were replayed upstream in
            // this request. Reads 0 on every request once the upstream stops
            // producing a reasoning summary — at that point the replay has
            // silently become a no-op. This is the first attempt's count; a
            // `default_model` fallback re-translates into its own counter.
            replayed_chars = reasoning_replay.replayed_chars.get(),
            "→ upstream"
        );
        (outbound, registry)
    };

    let start = Instant::now();

    // Print a user-readable request summary to the terminal.
    let tool_count = outbound.tools.as_ref().map_or(0, Vec::len);
    let msg_count = req.messages.len();
    let stream_label = if outbound.stream { "  |  stream" } else { "" };
    let reasoning = outbound
        .reasoning
        .as_ref()
        .map(|r| format!("  |  reasoning: {}", r.effort))
        .unwrap_or_default();
    state.output.emit(crate::tui::LogLine {
        kind: crate::tui::LogKind::Inbound,
        text: format!(
            "  → {} → {}  |  {} tools  |  {} messages{}{}",
            log_safe(&req.model),
            log_safe(&outbound.model),
            tool_count,
            msg_count,
            stream_label,
            reasoning
        ),
    });

    Attempt {
        state,
        headers,
        req,
        outbound,
        registry,
        start,
        mappings_snapshot,
        fallback_used: false,
        param_retries: 0,
        attempt: 1,
        requested_effort,
        thinking_disabled,
    }
    .run()
    .await
}

/// One POST to the upstream with the shared headers. Extracted from the
/// retry loop so the hidden validation-retry path can reuse it.
async fn send_upstream_once(
    state: &AppState,
    url: &str,
    headers: &HeaderMap,
    outbound: &responses::ResponsesRequest,
) -> Result<reqwest::Response, AppError> {
    let mut upstream_req = state
        .client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .bearer_auth(&state.config.upstream_api_key)
        .json(outbound);

    // Forward a few select headers if the client set them (e.g. tracing
    // identifiers). We don't blindly forward — that risks leaking
    // credentials or breaking the upstream contract.
    for key in ["x-request-id", "anthropic-version"] {
        if let Some(v) = headers.get(key) {
            upstream_req = upstream_req.header(key, v.clone());
        }
    }

    upstream_req.send().await.map_err(AppError::from)
}

/// Read and parse a non-streaming upstream body. Extracted so the
/// hidden validation-retry loop can re-read a fresh response.
async fn parse_upstream_body(
    resp: reqwest::Response,
) -> Result<responses::ResponsesResponse, AppError> {
    let bytes = resp.bytes().await?;
    tracing::debug!(
        raw_body = %String::from_utf8_lossy(&bytes),
        "raw upstream response (non-streaming)"
    );
    serde_json::from_slice(&bytes).map_err(|e| AppError::Upstream {
        status: StatusCode::BAD_GATEWAY,
        body: format!("upstream returned non-JSON body: {e}"),
    })
}

/// One reusable request-execution unit: everything the request handler
/// used to own as locals. Keeping the policy in one struct means a future
/// caller can re-run the whole pipe without duplicating the preamble.
struct Attempt {
    state: AppState,
    headers: HeaderMap,
    req: CreateMessageRequest,
    outbound: responses::ResponsesRequest,
    // Original (client-supplied) tool schemas captured at request
    // translation; threaded to both response paths so tool-call
    // arguments can be repaired before shipping. Rebuilt by the
    // fallback-model path so its repaired-input path stays bound to
    // that attempt's tools.
    registry: crate::repair::ToolSchemaRegistry,
    start: Instant,
    // Snapshot of the runtime mappings taken at handler entry. Used
    // for the `default_model` fallback so an edit made mid-request
    // doesn't tear the response — the same contract the alias
    // resolution above honours.
    mappings_snapshot: Arc<crate::tui::RuntimeMappings>,
    // Retry-loop state. `param_retries` is bounded by
    // `RequestParam::ALL.len()`, `attempt` gates the one-shot
    // `default_model` fallback, and `fallback_used` drives stats
    // attribution and the streaming `Drop` hook.
    fallback_used: bool,
    param_retries: usize,
    attempt: u8,
    // Policy inputs, resolved once from the client's explicit signals.
    requested_effort: Option<String>,
    thinking_disabled: bool,
}

/// The successful upstream response, split on `outbound.stream`.
/// `Streaming` owns the RAW `reqwest::Response` so the caller can still
/// consume `.bytes_stream()`; `Buffered` carries the parsed JSON body.
enum Outcome {
    Streaming(reqwest::Response),
    Buffered(responses::ResponsesResponse),
}

/// Translate one inbound request for one upstream model. Both the first
/// attempt and the `default_model` fallback call this, so reasoning,
/// prompt-caching, reasoning-replay and the cache key are recomputed from
/// the same policy rather than being hand-applied twice.
fn build_outbound(
    req: &CreateMessageRequest,
    config: &Config,
    model: &str,
    requested_effort: Option<&str>,
    thinking_disabled: bool,
) -> Result<
    (
        responses::ResponsesRequest,
        crate::repair::ToolSchemaRegistry,
        crate::config::ReasoningReplay,
    ),
    AppError,
> {
    let reasoning_decision =
        config.reasoning_for_request(model, requested_effort, thinking_disabled);
    let prompt_caching = config.prompt_caching_for_model(model);
    let reasoning_replay = config.reasoning_replay_for_model(model);
    let (mut outbound, registry) = translate::anthropic_to_responses(
        req,
        Some(reasoning_decision),
        &prompt_caching,
        &reasoning_replay,
    )
    .map_err(|e| AppError::BadRequest(format!("translation error: {e}")))?;
    // The translator copies the inbound model name verbatim; rewrite
    // it here so the upstream sees the alias-resolved name.
    outbound.model = model.to_owned();

    // If the operator configured a prompt-cache key, forward it. This
    // is done after translation so the key is independent of the
    // alias-resolved model name.
    if !prompt_caching.models.is_empty() {
        outbound.prompt_cache_key = prompt_caching.cache_key;
    }
    Ok((outbound, registry, reasoning_replay))
}

impl Attempt {
    fn url(&self) -> String {
        format!(
            "{}{}",
            self.state.config.upstream_base_url.trim_end_matches('/'),
            self.state.config.upstream_path
        )
    }

    /// Send repeatedly until the upstream answers with a success status.
    /// Owns the `capabilities.apply_known` before the first send and after
    /// a fallback re-translation, the parameter-strip retry (bounded by
    /// `RequestParam::ALL.len()`), the one-shot `attempt == 1`
    /// `default_model` fallback and `map_upstream_error`. Returns the raw
    /// response so the streaming branch can still take `.bytes_stream()`.
    async fn send_success(&mut self, url: &str) -> Result<reqwest::Response, AppError> {
        // Apply anything already learned about this model before the first
        // send, so a fact cached on an earlier request (or by a concurrent
        // one) is honoured immediately instead of re-triggering the 400.
        let already_dropped = self.state.capabilities.apply_known(&mut self.outbound);
        if !already_dropped.is_empty() {
            tracing::debug!(
                model = %self.outbound.model,
                params = ?already_dropped,
                "omitting parameters previously rejected by this model"
            );
        }
        loop {
            let resp = send_upstream_once(&self.state, url, &self.headers, &self.outbound).await?;
            let status = resp.status();
            if status.is_success() {
                break Ok(resp);
            }
            let body = resp.text().await.unwrap_or_default();
            // The body of the request that actually failed. Serialised from
            // `outbound` right here, so it reflects *this* attempt — including
            // a `default_model` fallback and any parameter the capability
            // store stripped. (The old code read a task-local captured before
            // the loop, which meant the log showed the pre-fallback body.)
            let sent = serde_json::to_string(&self.outbound).unwrap_or_default();
            tracing::warn!(%status, body = %body, "← upstream error");
            tracing::warn!(
                target: "upstream_payload",
                %status,
                sent_body = %sent,
                "← upstream error (full payload)"
            );

            // Retry without a parameter the upstream says it does not
            // support (e.g. `temperature` on an always-reasoning model).
            // Two guard rails keep this conservative: the error must name a
            // parameter we actually sent, and the body must say it is
            // *unsupported* rather than merely invalid (see
            // `capabilities::unsupported_parameter`). Each fact is recorded
            // for the rest of the process, so the retry costs at most one
            // wasted request per (model, parameter) per process lifetime.
            if self.param_retries < capabilities::RequestParam::ALL.len()
                && let Some(param) = capabilities::unsupported_parameter(status, &body)
                && param.is_set(&self.outbound)
            {
                let newly_learned = self
                    .state
                    .capabilities
                    .record_unsupported(&self.outbound.model, param);
                tracing::warn!(
                    model = %self.outbound.model,
                    param = %param.as_str(),
                    newly_learned,
                    "upstream rejected request parameter; retrying without it"
                );
                // Surface the discovery in the operator's log pane (and on
                // stdout with --no-tui). Only on first sight of the fact:
                // a model that rejects both temperature and top_p produces
                // one line each, and repeats would just be noise.
                if newly_learned {
                    self.state.output.emit(crate::tui::LogLine {
                        kind: crate::tui::LogKind::Warning,
                        text: format!(
                            "  \u{26a0} learned: {} does not support {}  (dropping it from now on; press c to re-probe)",
                            self.outbound.model,
                            param.as_str()
                        ),
                    });
                }
                param.clear_from(&mut self.outbound);
                self.param_retries += 1;
                continue;
            }

            // Decide whether to retry with the default model. The retry
            // happens before any response bytes are written to the client
            // (streaming or not), so it is safe for both paths. Only one
            // retry; the loop ends after attempt 2 regardless. The
            // fallback name comes from the runtime snapshot, so an edit
            // made via the TUI is honoured just like an alias edit is.
            //
            // `outbound.model` is the *alias-resolved* upstream name — the
            // same name the fallback would carry — so comparing the two is
            // exactly right.
            let rejection = model_rejection::detect_model_rejection(status.as_u16(), &body);
            // `default_model` may be written as an inbound name or as an
            // upstream name; resolving it through the alias map makes both
            // work, and makes the comparison below like-for-like (both sides
            // are then alias-resolved upstream names).
            let fallback = self
                .mappings_snapshot
                .default_model()
                .map(|fb| self.mappings_snapshot.resolve(fb));
            let should_retry = self.attempt == 1
                && rejection.is_some()
                && fallback
                    .as_deref()
                    .is_some_and(|fb| fb != self.outbound.model);

            if should_retry {
                let fallback = fallback.unwrap_or_default();
                tracing::warn!(
                    // The model the *client* asked for.
                    inbound_model = %log_safe(&self.req.model),
                    // The upstream name that was rejected.
                    rejected_model = %log_safe(&self.outbound.model),
                    fallback_model = %log_safe(&fallback),
                    // The name the upstream itself used, when it named one.
                    upstream_named_model = ?rejection.as_ref().and_then(|r| r.rejected.as_deref()),
                    // What the upstream said it would serve instead. Telemetry
                    // only — we do NOT route to it (it is untrusted input and
                    // it bypasses the operator's model policy), but it is the
                    // most actionable thing in the log: it tells the operator
                    // exactly which alias to add.
                    suggested_model = ?rejection.as_ref().and_then(|r| r.suggested.as_deref()),
                    "upstream rejected model; falling back to default_model"
                );

                // Rebuild the translated request for the fallback model. Prompt
                // cache breakpoints are embedded in `input`, so patching only the
                // model and reasoning fields would leak the first model's cache
                // policy into the retry. The registry is also rebuilt so its
                // repaired-input path stays bound to this attempt's tools.
                // The fallback's replay counter is intentionally never
                // reported today — only the first attempt logs `replayed_chars`.
                let (fb_outbound, fb_registry, _) = build_outbound(
                    &self.req,
                    self.state.config.as_ref(),
                    &fallback,
                    self.requested_effort.as_deref(),
                    self.thinking_disabled,
                )?;
                self.outbound = fb_outbound;
                self.registry = fb_registry;
                // The rebuilt request carries the client's full parameter
                // set again, so apply anything already learned about the
                // fallback model before it goes on the wire.
                self.state.capabilities.apply_known(&mut self.outbound);
                self.fallback_used = true;
                self.attempt = 2;
                continue;
            }

            return Err(map_upstream_error(status, &body));
        }
    }

    /// Send and classify. The hidden validation retry is non-streaming
    /// only: streaming bytes are already flowing at that point.
    async fn execute(&mut self, url: &str) -> Result<Outcome, AppError> {
        let upstream_resp = self.send_success(url).await?;
        let status = upstream_resp.status();
        debug_assert!(status.is_success(), "loop above only breaks on success");

        if self.outbound.stream {
            return Ok(Outcome::Streaming(upstream_resp));
        }

        let mut upstream_resp_body = parse_upstream_body(upstream_resp).await?;

        // Hidden validation retry: if the repaired tool input still
        // violates the client's *original* schema (e.g. a genuinely
        // required parameter the model botched), give the model one
        // corrective round-trip instead of shipping a call the client
        // will reject with "Invalid tool parameters". Bounded to a
        // single extra attempt; the streaming path can't do this at
        // all (bytes are already flowing).
        let mut validation_retries = 0u8;
        while validation_retries < 1 {
            let violations =
                crate::repair::tool_input_violations(&upstream_resp_body, &self.registry);
            if violations.is_empty() {
                break;
            }
            tracing::warn!(
                violations = ?violations,
                "tool input violates the original schema after repair; issuing corrective retry"
            );
            crate::repair::append_corrective_input(
                &mut self.outbound.input,
                &upstream_resp_body,
                &violations,
            );
            let resp = send_upstream_once(&self.state, url, &self.headers, &self.outbound).await?;
            if !resp.status().is_success() {
                // Upstream error on the retry: keep the original
                // (already-successful) response body for error mapping.
                break;
            }
            upstream_resp_body = parse_upstream_body(resp).await?;
            validation_retries += 1;
        }

        Ok(Outcome::Buffered(upstream_resp_body))
    }

    async fn run(mut self) -> Result<Response, AppError> {
        let url = self.url();
        // Retries are issued here -- before any response bytes are written,
        // so the streaming path is safe too (see TODO at the end of this
        // file): once per sampling parameter the upstream says it does not
        // support, and once with the configured `default_model` if the
        // upstream rejects the requested model entirely.
        match self.execute(&url).await? {
            Outcome::Streaming(resp) => render_streaming(self, resp),
            Outcome::Buffered(body) => render_json(&self, body),
        }
    }
}

/// Render the raw upstream SSE response into an Anthropic-shaped stream.
/// `TranslatorStream` takes ownership of the registry and clones the sink /
/// stats; its `Drop` hook prints the response line and records usage.
fn render_streaming(
    attempt: Attempt,
    upstream_resp: reqwest::Response,
) -> Result<Response, AppError> {
    let content_type = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("text/event-stream"));
    let byte_stream = upstream_resp.bytes_stream();
    let sse: EventStream<_> = byte_stream.eventsource();

    let msg_id = synthetic_message_id(&attempt.req);
    let model = attempt.outbound.model.clone();
    let translator_stream = TranslatorStream::new(
        sse,
        TranslatorStreamConfig {
            msg_id,
            model,
            registry: attempt.registry,
            start: attempt.start,
            sink: attempt.state.output.clone(),
            session_stats: attempt.state.stats.clone(),
            stats_model: stats_model(
                &attempt.req.model,
                &attempt.outbound.model,
                attempt.fallback_used,
            ),
            fallback_used: attempt.fallback_used,
        },
    );

    let body = Body::from_stream(translator_stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .map_err(|e| AppError::Internal(format!("build streaming response: {e}")))
}

/// Translate a buffered upstream response, record stats once, emit the
/// operator-visible `←` line, and return the Anthropic JSON response.
fn render_json(
    attempt: &Attempt,
    upstream_resp_body: responses::ResponsesResponse,
) -> Result<Response, AppError> {
    let anth = translate::responses_to_anthropic(&upstream_resp_body, &attempt.registry);
    if is_countable_response_status(&upstream_resp_body.status)
        && let Some(usage) = upstream_resp_body.usage.as_ref()
    {
        let usage = token_usage(usage);
        if attempt.fallback_used {
            attempt
                .state
                .stats
                .record_actual(attempt.outbound.model.clone(), usage);
        } else {
            attempt.state.stats.record(attempt.req.model.clone(), usage);
        }
    }
    let elapsed = attempt.start.elapsed();
    let usage = &upstream_resp_body.usage;
    let in_tokens = usage.as_ref().map_or(0, |u| u.input_tokens);
    let out_tokens = usage.as_ref().map_or(0, |u| u.output_tokens);
    let thinking = usage
        .as_ref()
        .and_then(|u| u.output_tokens_details.as_ref())
        .map_or(0, |d| d.reasoning_tokens);
    let cache_read = usage
        .as_ref()
        .and_then(|u| u.input_tokens_details.as_ref())
        .map_or(0, |d| d.cached_tokens);
    let cache_write = usage
        .as_ref()
        .and_then(|u| u.input_tokens_details.as_ref())
        .map_or(0, |d| d.cache_write_tokens);
    let thinking_str = if thinking > 0 {
        format!("  |  {} thinking", thinking)
    } else {
        String::new()
    };
    let line = format!(
        "  ← {}  |  {:.2}s  |  {} in  |  {} out{}  |  cache: {}r {}w",
        upstream_resp_body.status,
        elapsed.as_secs_f64(),
        in_tokens,
        out_tokens,
        thinking_str,
        cache_read,
        cache_write,
    );
    attempt.state.output.emit(crate::tui::LogLine {
        kind: crate::tui::LogKind::Response,
        text: line,
    });
    Ok(Json(anth).into_response())
}

/// Print a response stats line from a `StreamStats` (used by the streaming path).
fn token_usage(usage: &responses::ResponsesUsage) -> crate::tui::TokenUsage {
    crate::tui::TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens),
        cache_creation_input_tokens: usage
            .input_tokens_details
            .as_ref()
            .map_or(0, |d| d.cache_write_tokens),
        reasoning_tokens: usage
            .output_tokens_details
            .as_ref()
            .map_or(0, |d| d.reasoning_tokens),
    }
}

/// Use the inbound name for ordinary requests, but attribute a retry to the
/// fallback model that actually produced the successful response.
fn is_countable_response_status(status: &str) -> bool {
    // `incomplete` is a terminal upstream response with authoritative usage,
    // unlike a stream interrupted before it emitted a terminal event.
    matches!(status, "completed" | "incomplete")
}

fn stats_model(inbound: &str, actual_model: &str, fallback_used: bool) -> String {
    if fallback_used {
        actual_model.to_owned()
    } else {
        inbound.to_owned()
    }
}

fn print_stream_stats(
    stats: &crate::stream::StreamStats,
    elapsed: std::time::Duration,
    sink: &crate::tui::OutputSink,
) {
    let usage = &stats.usage;
    let in_tokens = usage.as_ref().map_or(0, |u| u.input_tokens);
    let out_tokens = usage.as_ref().map_or(0, |u| u.output_tokens);
    let thinking = usage.as_ref().map_or(0, |u| u.thinking_tokens);
    let cache_read = stats.cache_read_input_tokens;
    let cache_write = stats.cache_creation_input_tokens;
    let stop_reason = stats.stop_reason.as_ref().map_or("completed", |s| match s {
        crate::anthropic::StopReason::EndTurn => "completed",
        crate::anthropic::StopReason::MaxTokens => "max_tokens",
        crate::anthropic::StopReason::StopSequence => "stop_sequence",
        crate::anthropic::StopReason::ToolUse => "tool_use",
        crate::anthropic::StopReason::Refusal => "refusal",
        crate::anthropic::StopReason::PauseTurn => "paused",
    });
    let thinking_str = if thinking > 0 {
        format!("  |  {} thinking", thinking)
    } else {
        String::new()
    };
    let line = format!(
        "  ← {}  |  {:.2}s  |  {} in  |  {} out{}  |  cache: {}r {}w",
        stop_reason,
        elapsed.as_secs_f64(),
        in_tokens,
        out_tokens,
        thinking_str,
        cache_read,
        cache_write,
    );
    sink.emit(crate::tui::LogLine {
        kind: crate::tui::LogKind::Response,
        text: line,
    });
}

// ─── Streaming bridge ─────────────────────────────────────────────────────

/// Bridges the upstream SSE event stream with our `StreamTranslator` and
/// yields Anthropic-formatted SSE bytes ready to write to the client.
///
/// This is its own struct (not a closure chain) because the translator
/// carries state across events — the closure-in-`filter_map` shape
/// can't share that state cleanly.
struct TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    inner: S,
    translator: Option<StreamTranslator>,
    /// When the request started, for computing elapsed time.
    start: Instant,
    /// Captured stats after the translator is consumed, so `Drop` can print them.
    stats: Option<crate::stream::StreamStats>,
    /// Shared process-lifetime token totals.
    session_stats: Arc<crate::tui::SessionStatsStore>,
    /// Inbound or actual model bucket for this request.
    stats_model: String,
    /// Whether this response was produced by a fallback retry.
    fallback_used: bool,
    /// Output sink for the response log line. Cloned cheaply (Arc inside).
    sink: crate::tui::OutputSink,
}

/// Configuration for creating a `TranslatorStream`.
struct TranslatorStreamConfig {
    msg_id: String,
    model: String,
    registry: crate::repair::ToolSchemaRegistry,
    start: Instant,
    sink: crate::tui::OutputSink,
    session_stats: Arc<crate::tui::SessionStatsStore>,
    stats_model: String,
    fallback_used: bool,
}

impl<S> TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    fn new(inner: S, config: TranslatorStreamConfig) -> Self {
        Self {
            inner,
            translator: Some(StreamTranslator::new(
                config.msg_id,
                config.model,
                config.registry,
            )),
            start: config.start,
            stats: None,
            session_stats: config.session_stats,
            stats_model: config.stats_model,
            fallback_used: config.fallback_used,
            sink: config.sink,
        }
    }

    /// Take the translator, capture its stats, and return the events.
    fn take_and_report(&mut self) -> Option<Vec<StreamEvent>> {
        let t = self.translator.take()?;
        self.stats = Some(t.stats());
        let events = t.finish();
        if events.is_empty() {
            None
        } else {
            Some(events)
        }
    }

    /// Take the translator for an error path, capture stats, return events.
    fn take_and_report_error(
        &mut self,
        kind: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<StreamEvent> {
        let t = self.translator.take().expect("translator already taken");
        self.stats = Some(t.stats());
        t.emit_error(kind, message)
    }
}

impl<S> Drop for TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    fn drop(&mut self) {
        if let Some(stats) = self.stats.as_ref() {
            let elapsed = self.start.elapsed();
            print_stream_stats(stats, elapsed, &self.sink);
            if stats.completed && stats.usage.is_some() {
                let usage = crate::tui::TokenUsage {
                    input_tokens: stats.input_tokens,
                    output_tokens: stats.usage.as_ref().map_or(0, |u| u.output_tokens),
                    cache_read_input_tokens: stats.cache_read_input_tokens,
                    cache_creation_input_tokens: stats.cache_creation_input_tokens,
                    reasoning_tokens: stats.usage.as_ref().map_or(0, |u| u.thinking_tokens),
                };
                if self.fallback_used {
                    self.session_stats
                        .record_actual(self.stats_model.clone(), usage);
                } else {
                    self.session_stats.record(self.stats_model.clone(), usage);
                }
            }
        }
    }
}

impl<S> Stream for TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: we never move the inner stream out of `self`.
        let this = self.get_mut();
        loop {
            // The translator is `None` after a clean close (terminal
            // event, error event, or a truncated stream). Any further
            // event from the inner stream is a stray — drop it and end
            // the outer stream.
            if this.translator.is_none() {
                match this.inner.poll_next_unpin(cx) {
                    Poll::Ready(Some(_)) => continue,
                    _ => return Poll::Ready(None),
                }
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(msg))) => {
                    if msg.data.trim() == "[DONE]" {
                        // The Responses API does not use a `[DONE]`
                        // sentinel; its terminal event is
                        // `response.completed` / `incomplete` / `failed`,
                        // which the translator handles directly. If we
                        // see `[DONE]` here, it's a stray from a
                        // non-Responses upstream — flush the translator
                        // and end the stream.
                        let events = this.take_and_report().unwrap_or_default();
                        if events.is_empty() {
                            return Poll::Ready(None);
                        }
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    if msg.data.is_empty() {
                        continue;
                    }
                    let event: responses::ResponsesStreamEvent =
                        match serde_json::from_str(&msg.data) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    event_kind = %msg.event,
                                    "dropping malformed upstream SSE event"
                                );
                                continue;
                            }
                        };
                    // Detect `response.failed` and route to
                    // `emit_error` so the bridge surfaces a clean
                    // error event rather than a half-streamed message.
                    if matches!(event, responses::ResponsesStreamEvent::Failed { .. }) {
                        // Best-effort message extraction; the
                        // translator will read it from the response
                        // envelope.
                        let (kind, message) = match &event {
                            responses::ResponsesStreamEvent::Failed { response } => {
                                let msg = response
                                    .error
                                    .as_ref()
                                    .map(|e| e.message.clone())
                                    .unwrap_or_else(|| "upstream response failed".to_owned());
                                ("upstream_error", msg)
                            }
                            _ => unreachable!(),
                        };
                        let events = this.take_and_report_error(kind, message);
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    if let responses::ResponsesStreamEvent::Error { message, .. } = &event {
                        let events = this.take_and_report_error("upstream_error", message.clone());
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    let terminal = matches!(
                        event,
                        responses::ResponsesStreamEvent::Completed { .. }
                            | responses::ResponsesStreamEvent::Incomplete { .. }
                    );
                    let events = if terminal {
                        let mut translator =
                            this.translator.take().expect("translator already taken");
                        let mut events = translator.feed_event(&event);
                        // Capture terminal usage before yielding its events. If
                        // the downstream disconnects immediately after receiving
                        // the terminal event, Drop still has enough information
                        // to count this completed response exactly once.
                        this.stats = Some(translator.stats());
                        events.extend(translator.finish());
                        events
                    } else {
                        let Some(translator) = this.translator.as_mut() else {
                            return Poll::Ready(None);
                        };
                        translator.feed_event(&event)
                    };
                    if events.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                }
                Poll::Ready(Some(Err(e))) => {
                    // Surface a clean close: emit an error event and end.
                    tracing::warn!(error = %e, "upstream SSE error; closing stream");
                    let events =
                        this.take_and_report_error("api_error", format!("upstream SSE error: {e}"));
                    return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                }
                Poll::Ready(None) => {
                    let completed = this
                        .translator
                        .as_ref()
                        .is_some_and(|translator| translator.stats().completed);
                    if completed {
                        if let Some(events) = this.take_and_report() {
                            return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                        }
                    } else if this.translator.is_some() {
                        let events = this.take_and_report_error(
                            "api_error",
                            "upstream stream ended before a terminal response event",
                        );
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn encode_sse_events(events: &[StreamEvent]) -> Bytes {
    let mut out = Vec::new();
    for event in events {
        let kind = event_kind(event);
        let data = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize stream event");
                continue;
            }
        };
        out.extend_from_slice(format!("event: {kind}\ndata: {data}\n\n").as_bytes());
    }
    Bytes::from(out)
}

fn event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::MessageStart { .. } => "message_start",
        StreamEvent::ContentBlockStart { .. } => "content_block_start",
        StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
        StreamEvent::ContentBlockStop { .. } => "content_block_stop",
        StreamEvent::MessageDelta { .. } => "message_delta",
        StreamEvent::MessageStop {} => "message_stop",
        StreamEvent::Ping {} => "ping",
        StreamEvent::Error { .. } => "error",
    }
}

// ─── Upstream error mapping ───────────────────────────────────────────────

fn map_upstream_error(status: StatusCode, body: &str) -> AppError {
    // Upstreams use two envelopes: the flat Responses shape
    // (`{"message": "...", "type": "..."}`) and the nested one
    // (`{"error": {"message": "...", "type": "..."}}`) that the gateway in
    // production actually sends. Parsing only the flat one meant the
    // client received the raw JSON blob as its error message.
    let env = model_rejection::parse_error_envelope(body);
    if env.message.is_empty() {
        // Nothing extractable — hand the body over verbatim so the client
        // never loses the upstream's text.
        return AppError::Upstream {
            status,
            body: body.to_string(),
        };
    }
    let kind = if env.kind.is_empty() {
        "upstream_error"
    } else {
        env.kind.as_str()
    };
    AppError::Upstream {
        status,
        body: format!("{kind}: {}", env.message),
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

/// Synthesize an Anthropic-shaped message id for streaming responses.
/// Uses nanoseconds since the UNIX epoch, hex-encoded, plus a sanitized
/// model fragment. Not a UUID, but unique enough for our purposes.
/// The translator replaces this with the real `resp_...` id once
/// `response.created` arrives.
fn synthetic_message_id(req: &CreateMessageRequest) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_{:x}_{}", nanos, sanitize_model(&req.model))
}

fn sanitize_model(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Name of the HTTP header clients must set when `proxy_key` is
/// configured. Case-insensitive (HTTP headers are).
const PROXY_KEY_HEADER: &str = "x-proxy-key";

/// Constant-time string equality. Avoids leaking the key's length or
/// position-of-first-mismatch through response timing. Marked
/// `#[inline(never)]` so the compiler can't partially inline it
/// back into a per-byte branchy form.
#[inline(never)]
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Check the inbound `X-Proxy-Key` header against the configured
/// `proxy_key`. Returns `Ok(())` if auth is disabled (no key
/// configured) or the header matches; `Err(401)` otherwise. The
/// comparison is constant-time over the equal-length case.
fn check_proxy_key(config: &Config, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(expected) = config.proxy_key.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(PROXY_KEY_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or(&[]);
    if ct_eq(provided, expected.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_message_id_uses_msg_prefix() {
        let req = CreateMessageRequest {
            model: "gpt-4o".into(),
            max_tokens: 16,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(true),
            metadata: None,
            output_config: None,
            thinking: None,
        };
        let id = synthetic_message_id(&req);
        assert!(id.starts_with("msg_"));
        assert!(id.contains("gpt-4o"));
    }

    #[test]
    fn sanitize_model_keeps_safe_chars() {
        assert_eq!(sanitize_model("gpt-4o"), "gpt-4o");
        assert_eq!(sanitize_model("claude/opus 4.8"), "claude_opus_4_8");
    }

    #[test]
    fn stats_model_groups_mapped_requests_by_inbound_name() {
        assert_eq!(
            stats_model("claude-sonnet", "gpt-sonnet", false),
            "claude-sonnet"
        );
    }

    #[test]
    fn stats_model_attributes_fallback_to_actual_model() {
        assert_eq!(
            stats_model("claude-sonnet", "gpt-fallback", true),
            "gpt-fallback"
        );
    }

    #[test]
    fn stats_model_keeps_unmapped_inbound_name_separate() {
        assert_eq!(
            stats_model("claude-unknown", "claude-unknown", false),
            "claude-unknown"
        );
    }

    fn test_config_with_key(key: Option<&str>) -> Arc<Config> {
        // Build a minimal config that satisfies the fields
        // `check_proxy_key` reads. The other fields don't matter.
        Arc::new(Config {
            proxy_key: key.map(String::from),
            ..test_config_minimal()
        })
    }

    /// The operator-facing docs (the config guide, the scratchpad) tell the
    /// operator to watch `replayed_chars` on the "→ upstream" line. This
    /// guards against that field being documented but never actually added
    /// to the log statement — which is exactly what happened once already.
    #[test]
    fn the_upstream_log_line_reports_the_replayed_reasoning_count() {
        let source = std::fs::read_to_string(file!()).expect("read proxy.rs");
        let marker = "\"→ upstream\"";
        let marker_at = source.rfind(marker).expect("the → upstream log statement");
        // The fields of a `tracing::info!` sit between its opening paren
        // and the message, so the statement is the text from the last
        // `tracing::info!(` before the marker up to the marker itself.
        let before = &source[..marker_at];
        let statement = before
            .rfind("tracing::info!(")
            .map(|at| &before[at..])
            .expect("the → upstream tracing::info! statement");
        assert!(
            statement.contains("replayed_chars"),
            "the → upstream log line does not report replayed_chars"
        );
        assert!(
            statement.contains("reasoning_replay.replayed_chars.get()"),
            "the → upstream log line does not read the replayed counter"
        );
    }

    fn test_config_minimal() -> Config {
        Config {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            upstream_base_url: "https://example.com".into(),
            upstream_api_key: "test".into(),
            upstream_path: "/v1/responses".into(),
            request_timeout: std::time::Duration::from_secs(60),
            reasoning_effort: None,
            reasoning: crate::config::ReasoningConfig::default(),
            reasoning_replay: crate::config::ReasoningReplayConfig::default(),
            model_aliases: crate::config::ModelAliases::default(),
            prompt_caching: crate::config::PromptCachingConfig::default(),
            log_to_disk: false,
            proxy_key: None,
            advisor: crate::config::AdvisorConfig::default(),
        }
    }

    #[test]
    fn ct_eq_matches_identical_strings() {
        assert!(ct_eq(b"hello", b"hello"));
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"a-longer-key-12345", b"a-longer-key-12345"));
    }

    #[test]
    fn ct_eq_rejects_different_strings() {
        assert!(!ct_eq(b"hello", b"world"));
        assert!(!ct_eq(b"hello", b"hell"));
        assert!(!ct_eq(b"hell", b"hello"));
        // Empty vs non-empty.
        assert!(!ct_eq(b"", b"x"));
        assert!(!ct_eq(b"x", b""));
    }

    #[test]
    fn check_proxy_key_no_key_configured_allows_all() {
        // No `proxy_key` set → no auth, no header required. Current
        // behavior: any reachable client can use the proxy.
        let config = test_config_with_key(None);
        let headers = HeaderMap::new();
        assert!(check_proxy_key(&config, &headers).is_ok());
    }

    #[test]
    fn check_proxy_key_correct_header_passes() {
        let config = test_config_with_key(Some("shared-secret-1234"));
        let mut headers = HeaderMap::new();
        headers.insert(PROXY_KEY_HEADER, "shared-secret-1234".parse().unwrap());
        assert!(check_proxy_key(&config, &headers).is_ok());
    }

    #[test]
    fn check_proxy_key_missing_header_rejects() {
        let config = test_config_with_key(Some("shared-secret-1234"));
        let headers = HeaderMap::new();
        let err = check_proxy_key(&config, &headers).unwrap_err();
        // Should be Unauthorized, not BadRequest or Internal.
        assert!(matches!(err, AppError::Unauthorized));
    }

    #[test]
    fn check_proxy_key_wrong_header_rejects() {
        let config = test_config_with_key(Some("shared-secret-1234"));
        let mut headers = HeaderMap::new();
        headers.insert(PROXY_KEY_HEADER, "wrong".parse().unwrap());
        let err = check_proxy_key(&config, &headers).unwrap_err();
        assert!(matches!(err, AppError::Unauthorized));
    }

    #[test]
    fn check_proxy_key_header_is_case_insensitive() {
        // HTTP header names are case-insensitive. Verify the lookup
        // works regardless of how the client capitalized the header.
        let config = test_config_with_key(Some("k"));
        for variant in ["X-Proxy-Key", "x-proxy-key", "X-PROXY-KEY", "x-Proxy-Key"] {
            let mut headers = HeaderMap::new();
            headers.insert(variant, "k".parse().unwrap());
            assert!(
                check_proxy_key(&config, &headers).is_ok(),
                "{variant:?} should match"
            );
        }
    }

    #[test]
    fn unauthorized_response_returns_401() {
        // Pin the wire shape of an unauthorized response so a future
        // refactor can't accidentally downgrade it to 400 or 500.
        let resp = AppError::Unauthorized.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
