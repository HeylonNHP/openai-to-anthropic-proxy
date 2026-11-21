//! Configuration loading.
//!
//! Resolution order (highest priority first):
//!   1. Environment variables (`LISTEN_ADDR`, `UPSTREAM_BASE_URL`, ...).
//!   2. `proxy.json` in the working directory, if present.
//!   3. Built-in defaults.
//!
//! Environment variables always win over the JSON file. This lets a deployment
//! override individual values without editing the config file.
//!
//! ## Warnings raised while resolving
//!
//! The configuration is resolved BEFORE the binary installs the tracing
//! subscriber (`Config::load()` runs first, `init_tracing()` second), so a
//! `tracing::warn!` emitted from this module would reach NO subscriber and
//! vanish — the operator would never learn about it. Warnings raised here
//! are therefore COLLECTED on the resolved [`Config`] as [`ConfigWarning`]s,
//! and the binary emits them right after `init_tracing`: on stderr (always
//! visible) and as tracing events (so they land in the log file).
//!
//! **Rule for this module: never call `tracing::warn!` while resolving.**
//! Push a [`ConfigWarning`] onto the resolver's warning list instead.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8085";
const DEFAULT_UPSTREAM_PATH: &str = "/v1/responses";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;
/// Maximum accepted inbound `/v1/messages` body size, in bytes (32 MiB).
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Prompt caching is opt-in and off by default. When disabled, the
/// proxy does not emit any OpenAI prompt-cache fields, so the feature
/// has zero impact on upstreams that don't understand it.
/// Default `reasoning_effort` for upstream chat-completions requests.
/// Some upstreams (notably airia-backed reasoning models) reject
/// function tools when `reasoning_effort` is unset — they default to a
/// non-`"none"` value, and the resulting combination is unsupported.
/// Pinning the default to `"none"` keeps tool-use requests working out
/// of the box; operators who need reasoning for non-tool calls can
/// override via env or JSON.
const DEFAULT_REASONING_EFFORT: &str = "none";
/// Cheap advisor model used when `advisor.model` is unset. `beam` is
/// deliberately the default: it is the cheapest sensible upstream in the
/// catalogue (§7 of `ADVISOR_TOOL_RESEARCH.md`), and an advisor consult
/// re-runs the executor, so a wrong default multiplies cost. Never default
/// to a kimi model.
const DEFAULT_ADVISOR_MODEL: &str = "beam";
const DEFAULT_ADVISOR_MAX_USES: u32 = 2;
/// Cap on the advisor's ANSWER (not its reasoning; see
/// [`DEFAULT_ADVISOR_REASONING_MAX_TOKENS`]).
const DEFAULT_ADVISOR_MAX_TOKENS: u32 = 4096;
/// Extra output tokens the advisor may spend REASONING before it answers.
/// Upstream `max_output_tokens` counts reasoning and answer together, so the
/// proxy sends `max_tokens + reasoning_max_tokens`.
const DEFAULT_ADVISOR_REASONING_MAX_TOKENS: u32 = 16384;
/// The advisor has NO timeout of its own by default (`0` = no limit): a
/// reasoning advisor reading a long transcript is slow, and cutting it off
/// only turns a good answer into a failure. The global `request_timeout_secs`
/// still bounds every upstream call.
const DEFAULT_ADVISOR_TIMEOUT_SECS: u64 = 0;

/// Default total timeout for one `web_fetch` request, in seconds.
///
/// The proxy performs the fetch ITSELF (it is the server-side tool), so
/// this bound covers the whole operation: connect, read, extract. It is
/// the wall-clock ceiling for a single fetch, not a per-byte or
/// per-redirect budget.
const DEFAULT_WEB_FETCH_TIMEOUT_SECS: u64 = 20;

// ─── `server_tools` defaults and hard ceiling ───────────────────────────────

/// Anthropic's own server-side loop stops after 10 sampling iterations.
const DEFAULT_SERVER_TOOLS_MAX_ROUNDS: u64 = 10;

// ─── `web_search` defaults and hard ceilings ────────────────────────────────
//
// Every `web_search` bound has BOTH a default and a ceiling, and none of them
// can be set to "unlimited": the tool makes outbound requests on a model's
// behalf, and an unbounded search loop is an easy way to get the proxy's IP
// rate-limited or CAPTCHA'd. See `src/tools/web_search/README.md`.

/// Where the proxy scrapes search results from.
pub const DEFAULT_WEB_SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const DEFAULT_WEB_SEARCH_MAX_QUERIES: u64 = 3;
const MAX_WEB_SEARCH_MAX_QUERIES: u64 = 10;
const DEFAULT_WEB_SEARCH_MAX_RESULTS_PER_QUERY: u64 = 5;
/// DuckDuckGo's first HTML page carries about 10 results; the proxy does not
/// paginate, so asking for more than that could never be honoured.
const MAX_WEB_SEARCH_MAX_RESULTS_PER_QUERY: u64 = 10;
const DEFAULT_WEB_SEARCH_COMPACTION_THRESHOLD_CHARS: u64 = 5_000;
const MAX_WEB_SEARCH_COMPACTION_THRESHOLD_CHARS: u64 = 100_000;
const DEFAULT_WEB_SEARCH_SEARCH_TIMEOUT_SECS: u64 = 15;
const DEFAULT_WEB_SEARCH_PAGE_TIMEOUT_SECS: u64 = 15;
const MAX_WEB_SEARCH_TIMEOUT_SECS: u64 = 120;
const DEFAULT_WEB_SEARCH_MIN_INTERVAL_MS: u64 = 1_500;
const MAX_WEB_SEARCH_MIN_INTERVAL_MS: u64 = 30_000;
const DEFAULT_WEB_SEARCH_BLOCK_WAIT_MINUTES: u64 = 10;
const MAX_WEB_SEARCH_BLOCK_WAIT_MINUTES: u64 = 120;
const DEFAULT_WEB_SEARCH_MAX_PAGE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WEB_SEARCH_MAX_PAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Warning text for an advisor whose consultation loop is UNBOUNDED.
///
/// `advisor.max_uses` = 0 = "no limit". Shared by the config resolver and
/// by the binary's startup banner so the two can never drift.
pub const UNBOUNDED_ADVISOR_LOOP_WARNING: &str = "advisor.max_uses is 0 (no limit): the consultation loop is unbounded — each iteration costs two upstream calls (executor re-run + advisor)";

/// A warning raised while the configuration was resolved.
///
/// ## Why this type exists
///
/// The configuration is resolved BEFORE the binary installs the tracing
/// subscriber, so a `tracing::warn!` emitted during resolution hits no
/// subscriber and disappears. The resolver COLLECTS its warnings here and
/// the binary emits them right after `init_tracing` — on stderr (always
/// visible, like the `proxy_key` warning) and as tracing events (so they
/// land in `target/logs/proxy.log`). ONE mechanism for every config-time
/// warning, so a new warning can never be silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// The operator-facing message. Printed on stderr and written to the
    /// log file, so it must read well on its own.
    pub message: String,
    /// Structured key/value pairs attached to the tracing event, so a
    /// postmortem can grep for e.g. `field="advisor.max_uses"`.
    pub fields: Vec<(String, String)>,
}

impl ConfigWarning {
    /// A warning with just a message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fields: Vec::new(),
        }
    }

    /// Attach one structured field to the tracing event.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }
}

/// An integer limit for which `0` means "no limit".
///
/// The house rule (`0` = unlimited) is expressed once, here, so every
/// consumer of a limit applies it the same way.
pub trait AdvisorLimitValue: Copy + PartialEq {
    /// The value that means "no limit".
    const NO_LIMIT: Self;
}

impl AdvisorLimitValue for u32 {
    const NO_LIMIT: Self = 0;
}

impl AdvisorLimitValue for u64 {
    const NO_LIMIT: Self = 0;
}

/// The effective bound of an advisor limit, with the house
/// **"0 means no limit"** rule applied.
///
/// * `None` → `None` — no limit;
/// * `Some(0)` → `None` — no limit, the SAME meaning as `None`;
/// * `Some(n)` → `Some(n)` — that bound.
///
/// The config resolver normalises a configured `0` into `None`, but a
/// `Some(0)` can still reach the advisor session from a hand-built
/// `AdvisorConfig` (tests, embedders). Applying the rule at BOTH ends gives
/// `0` exactly ONE meaning in the whole codebase: **no limit**.
#[must_use]
pub fn effective_limit<T: AdvisorLimitValue>(value: Option<T>) -> Option<T> {
    value.filter(|v| *v != T::NO_LIMIT)
}

/// Resolved proxy configuration. Cheap to clone (`String`s and a `Duration`).
#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub upstream_base_url: String,
    pub upstream_api_key: String,
    pub upstream_path: String,
    pub request_timeout: Duration,
    /// Maximum accepted inbound `/v1/messages` body size, in bytes.
    pub max_request_body_bytes: usize,
    /// Outbound `reasoning_effort` for Responses requests. See
    /// [`DEFAULT_REASONING_EFFORT`] for why this exists. Forwarded to
    /// the upstream as `reasoning: { effort: "..." }` (the Responses
    /// shape), not the legacy top-level `reasoning_effort` string.
    pub reasoning_effort: Option<String>,
    /// Per-model reasoning configuration: fixed per-model effort,
    /// a default fallback, and the Claude-Code-effort → upstream-effort
    /// translation maps (both for explicit effort and for disabled
    /// thinking). See [`ReasoningConfig`] for the resolution rules.
    pub reasoning: ReasoningConfig,
    /// Map from inbound model name (e.g. `claude-sonnet-5`, what
    /// Claude Code sends) to upstream model name (e.g. `gpt-5.4-mini`,
    /// what the gateway serves). Used by [`Config::upstream_model_for`]
    /// to rewrite the model field on the way out. If no alias is set
    /// for a given inbound name, the proxy passes the name through
    /// unchanged — so deployments that don't need aliases (a single
    /// model, or model names that already match) need no config here.
    pub model_aliases: ModelAliases,
    /// Prompt-caching settings. Disabled by default. When a model is listed
    /// in `models`, the proxy translates Anthropic `cache_control: {type: "ephemeral"}`
    /// on user/system text and image blocks into OpenAI `prompt_cache_breakpoint`
    /// markers and sets the top-level `prompt_cache_key`. Models not in the list
    /// get no prompt-caching fields.
    pub prompt_caching: PromptCachingConfig,
    /// How a prior turn's extended thinking is replayed back to the
    /// upstream. See [`ReasoningReplayConfig`] for the modes.
    pub reasoning_replay: ReasoningReplayConfig,
    /// How `image` blocks inside a `tool_result` are forwarded. See
    /// [`ToolResultImages`]. Defaults to [`ToolResultImages::Array`],
    /// the encoding the Responses API specifies; `lift` and `drop` are
    /// escape hatches for gateways that only accept string tool outputs.
    pub tool_result_images: ToolResultImages,
    /// Where a `role: "system"` message in the middle of the conversation
    /// goes. See [`SystemMessagePlacement`].
    pub system_message_placement: SystemMessagePlacement,
    /// Shared secret required on the inbound `X-Proxy-Key` header.
    /// `None` means no client authentication (current behavior); a
    /// warning is printed at startup. `Some(_)` means every request
    /// must carry the matching header or the proxy returns 401.
    pub proxy_key: Option<String>,
    /// Whether to write structured `tracing` events to
    /// `target/logs/proxy.log`. Defaults to `false` (off). When
    /// `false`, tracing events are **dropped** — they reach neither
    /// the terminal nor a file, so the operator's terminal shows
    /// only the explicit `println!` / `eprintln!` user-facing lines.
    /// This is the safe default: no PII is persisted and the
    /// terminal stays uncluttered. Operators who want logs for
    /// postmortem inspection set this to `true` in `proxy.json` or
    /// `LOG_TO_DISK=1` in the env.
    pub log_to_disk: bool,
    /// Advisor (server-side second opinion) policy. Off by default; see
    /// [`AdvisorConfig`] and `crate::tools::advisor`. When `enabled` is false the
    /// request pipeline behaves exactly as it did before the advisor feature
    /// existed (no extra upstream call, no synthesized tool, no wire fields).
    pub advisor: AdvisorConfig,
    /// Server-side `web_fetch` policy. See [`WebFetchConfig`] and
    /// `crate::tools::web_fetch`. When `enabled` is false the proxy never fetches
    /// a URL on the model's behalf and the request pipeline is unchanged.
    pub web_fetch: WebFetchConfig,
    /// Proxy-native `web_search` policy. See [`WebSearchConfig`] and
    /// `crate::tools::web_search`. When `enabled` is false (the default) the
    /// client's `web_search` tool keeps going to the upstream's own search.
    pub web_search: WebSearchConfig,
    /// How long the proxy's server-tool loop may run in one request. See
    /// [`ServerToolsConfig`].
    pub server_tools: ServerToolsConfig,
    /// When true, prefix claude- to every model id returned by /v1/models so Claude Code auto-discovers them (it only adds ids beginning with claude/anthropic).
    pub prefix_models_for_discovery: bool,
    /// Warnings raised while resolving this configuration.
    ///
    /// The resolver runs BEFORE the binary installs the tracing
    /// subscriber, so it cannot emit `tracing` events itself — it
    /// collects them here and the binary prints each one on stderr and
    /// emits it as a tracing event right after `init_tracing`. See
    /// [`ConfigWarning`].
    pub warnings: Vec<ConfigWarning>,
}

/// Resolved advisor settings (the `advisor` section of `proxy.json`).
///
/// The feature is **off by default**: `enabled: false` means the proxy
/// makes no advisor calls and emits no advisor wire fields. Enabling it
/// requires an explicit `"enabled": true` in `proxy.json` (or
/// `ADVISOR_ENABLED=1`) AND an inbound tool declaration named `advisor`.
/// The consultation loop lives in `crate::tools::advisor::tool`.
///
/// **`model` is an explicit UPSTREAM model name, not an inbound alias.**
/// It is written directly into the advisor's own upstream request; it is
/// never passed through `RuntimeMappings::resolve` / `model_aliases.map`,
/// which only run on client-supplied model names. Keeping it separate also
/// decouples the advisor's effort and cost from the executor's own alias.
///
/// `effort` is an optional upstream reasoning effort string, kept separate
/// from `reasoning.models` (which is keyed on the executor's upstream name).
///
/// ## The three limits, and what `0` means
///
/// `max_uses`, `max_tokens`, `reasoning_max_tokens` and `timeout_secs` are
/// all [`Option`]: `None`
/// means **no limit**, `Some(n)` means a bound of `n`. The rule is the same
/// in `proxy.json` and in the env vars:
///
/// * **key ABSENT** → the shipped default (`max_uses` 2, `max_tokens` 4096,
///   `reasoning_max_tokens` 16384; `timeout_secs` has NO default limit) — an absent key
///   is *not* "no limit" for the other three;
/// * **`0`** → **no limit** — the house precedent of
///   `reasoning_replay.max_chars`, where `0` means unlimited;
/// * **`n > 0`** → that bound;
/// * **an INVALID value** (negative, fractional, or too large for the
///   field's type) → the shipped default **with a warning** — the proxy
///   still starts. An operator typo in one limit must not take the whole
///   deployment down (see `resolve_advisor_limit`).
///
/// "No limit" means something DIFFERENT per field, and the difference
/// matters:
///
/// * `max_uses: None` — the consultation loop is bounded only by the
///   executor's behaviour. Every iteration costs TWO upstream calls (the
///   executor re-run and the advisor), so an executor that keeps asking
///   will spin. The proxy does NOT impose a ceiling of its own: it warns
///   once at startup (config-resolve time) and again at runtime when one
///   turn passes 10 consultations.
/// * `max_tokens: None` or `reasoning_max_tokens: None` — the proxy OMITS
///   `max_output_tokens` from the advisor's own upstream request (the
///   request limit is their SUM, so one unbounded part leaves it unbounded),
///   and the **upstream's own default** applies. That default may be SMALLER
///   than the shipped one, so "no limit" here means "the proxy imposes no
///   cap", NOT "the answer may be long".
/// * `timeout_secs: None` — no per-request timeout override. The global
///   `request_timeout` still applies (the reqwest client is built with
///   `.timeout(config.request_timeout)`), and a *bounded* value is always
///   clamped to it, so the advisor can never extend the operator's bound.
///
/// ## `Some(0)` means the same as `None`
///
/// The RESOLVED struct has ONE interpretation of `0`: **no limit**. The
/// resolver turns a configured `0` into `None`, and
/// [`effective_limit`] makes a hand-built `Some(0)` mean exactly the same
/// thing where the limits are consumed (`advisor_cap` → unbounded,
/// `advisor_timeout` → no override, `max_output_tokens` → omitted). There
/// is no "zero consultations" or "zero seconds" anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub model: String,
    pub effort: Option<String>,
    /// Consultations per turn. `None` = no limit (see the module docs).
    pub max_uses: Option<u32>,
    /// Cap on the advisor's own ANSWER, reasoning excluded. `None` = no
    /// limit: the proxy sends no `max_output_tokens` and the upstream's
    /// default applies.
    pub max_tokens: Option<u32>,
    /// Extra output tokens the advisor may spend on REASONING, on top of
    /// `max_tokens`. Upstream `max_output_tokens` counts reasoning and answer
    /// together and cannot be split, so the proxy sends their sum. `None` =
    /// no limit: the proxy sends no `max_output_tokens` at all.
    pub reasoning_max_tokens: Option<u32>,
    /// Per-request timeout for the advisor's upstream call. `None` = no
    /// per-request override; the global `request_timeout` applies.
    pub timeout_secs: Option<u64>,
}

/// Resolved `web_fetch` settings (the `web_fetch` section of `proxy.json`).
///
/// The proxy is the SERVER for Anthropic's server-side `web_fetch` tool:
/// it performs the HTTP fetch itself and hands the extracted text back to
/// the executor model. Every knob here therefore bounds OUTBOUND work the
/// proxy does on the model's behalf.
///
/// The defaults are deliberately permissive — there is no domain allowlist
/// and no URL-length limit — because the operator asked for "fetch any URL
/// the model asks for". The bounds that remain are the ones that protect
/// the PROCESS (timeout) and the ones the operator can tighten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchConfig {
    /// Whether the proxy will fetch URLs on the model's behalf.
    pub enabled: bool,
    /// Total wall-clock budget for ONE fetch (connect + read + extract).
    /// `None` = no timeout.
    pub timeout_secs: Option<u64>,
    /// Maximum number of bytes read from the network for one fetch.
    /// `None` = unlimited.
    pub max_bytes: Option<u64>,
    /// Maximum size of the EXTRACTED text, in bytes, before it is
    /// truncated at a paragraph boundary. `None` = no truncation.
    ///
    /// This is separate from [`Self::max_bytes`]: `max_bytes` bounds the
    /// network read, this one bounds what is handed to the model.
    pub max_content_bytes: Option<u64>,
}

impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: Some(DEFAULT_WEB_FETCH_TIMEOUT_SECS),
            max_bytes: None,
            max_content_bytes: None,
        }
    }
}

/// Resolved `server_tools` settings (the `server_tools` section of
/// `proxy.json`): the limits of the loop shared by `web_search`, `web_fetch`
/// and the advisor.
///
/// The loop runs the model, serves its server-tool calls, and runs the model
/// again. Each pass is a ROUND, and every round is an upstream call, so a
/// request runs at most `max_rounds` of them (10 by default, as Anthropic's
/// own loop does). As everywhere else in this config, `0` means NO LIMIT: the
/// loop is then bounded only by the model's behaviour. That is what makes an
/// unlimited `advisor.max_uses` or `web_fetch` actually unlimited.
///
/// What happens at the limit is `pause_turn`:
///
/// * `false` (the default): the model is told the limit was reached and gets
///   one last run to answer with what it has. The client sees an ordinary
///   finished turn. This is the safe choice for Claude Code, which does not
///   resume a `pause_turn`.
/// * `true`: the response is returned as it stands with
///   `stop_reason: "pause_turn"`, exactly as Anthropic's API does, and the
///   client resends the conversation to carry on (the Anthropic SDKs do this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerToolsConfig {
    /// `None` is "no limit".
    pub max_rounds: Option<usize>,
    pub pause_turn: bool,
}

impl Default for ServerToolsConfig {
    fn default() -> Self {
        Self {
            max_rounds: Some(DEFAULT_SERVER_TOOLS_MAX_ROUNDS as usize),
            pause_turn: false,
        }
    }
}

/// Resolved `web_search` settings (the `web_search` section of `proxy.json`).
///
/// When `enabled`, the proxy answers the client's server-side `web_search`
/// tool ITSELF (DuckDuckGo scraping + page fetching + compaction) instead of
/// forwarding it to the upstream's built-in search. When disabled, nothing
/// about the request pipeline changes.
///
/// Unlike [`WebFetchConfig`], NOTHING here is unlimited: every bound has a
/// default and a hard ceiling (see the `*_WEB_SEARCH_*` constants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchConfig {
    /// Use the proxy's own search instead of the upstream's.
    pub enabled: bool,
    /// Explicit UPSTREAM model used to compact over-long pages (alias
    /// resolution is bypassed, like `advisor.model`). `None` = no model call:
    /// over-long pages are truncated instead.
    pub compaction_model: Option<String>,
    /// A page whose text is longer than this many characters is compacted
    /// (or truncated) to fit within it.
    pub compaction_threshold_chars: usize,
    /// Most search queries the model may run in ONE tool call.
    pub max_queries: usize,
    /// Most result pages fetched per query.
    pub max_results_per_query: usize,
    /// Wall-clock budget for one search-engine request, in seconds.
    pub search_timeout_secs: u64,
    /// Wall-clock budget for fetching one result page, in seconds.
    pub page_timeout_secs: u64,
    /// Network read cap for one result page, in bytes.
    pub max_page_bytes: u64,
    /// The search endpoint (DuckDuckGo's HTML interface by default).
    pub search_endpoint: String,
    /// Searches run one at a time, with at least this many milliseconds
    /// between the end of one and the start of the next (per endpoint, across
    /// all requests), so a burst of queries does not look like a bot.
    pub search_min_interval_ms: u64,
    /// How many minutes the model is told to wait before searching again when
    /// the search engine blocks a search (bot challenge / rate limit).
    pub block_wait_minutes: u64,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compaction_model: None,
            compaction_threshold_chars: DEFAULT_WEB_SEARCH_COMPACTION_THRESHOLD_CHARS as usize,
            max_queries: DEFAULT_WEB_SEARCH_MAX_QUERIES as usize,
            max_results_per_query: DEFAULT_WEB_SEARCH_MAX_RESULTS_PER_QUERY as usize,
            search_timeout_secs: DEFAULT_WEB_SEARCH_SEARCH_TIMEOUT_SECS,
            page_timeout_secs: DEFAULT_WEB_SEARCH_PAGE_TIMEOUT_SECS,
            max_page_bytes: DEFAULT_WEB_SEARCH_MAX_PAGE_BYTES,
            search_endpoint: DEFAULT_WEB_SEARCH_ENDPOINT.to_owned(),
            search_min_interval_ms: DEFAULT_WEB_SEARCH_MIN_INTERVAL_MS,
            block_wait_minutes: DEFAULT_WEB_SEARCH_BLOCK_WAIT_MINUTES,
        }
    }
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_ADVISOR_MODEL.to_owned(),
            effort: None,
            max_uses: Some(DEFAULT_ADVISOR_MAX_USES),
            max_tokens: Some(DEFAULT_ADVISOR_MAX_TOKENS),
            reasoning_max_tokens: Some(DEFAULT_ADVISOR_REASONING_MAX_TOKENS),
            timeout_secs: None,
        }
    }
}

impl AdvisorConfig {
    /// True when the consultation loop is UNBOUNDED — i.e. `max_uses` asks
    /// for no limit. `None` and `Some(0)` mean the SAME thing (see
    /// [`effective_limit`]), so both count as unbounded.
    #[must_use]
    pub fn has_unbounded_loop(&self) -> bool {
        effective_limit(self.max_uses).is_none()
    }
}

/// Inbound → upstream model name aliases.
///
/// Lets the operator route requests for one model name to a
/// different upstream model. The most common case: Claude Code's
/// subagents request `claude-sonnet-5` (or another Anthropic-native
/// name), but the gateway only serves OpenAI-family models; the
/// operator maps each Anthropic name to the airia (or other) model
/// they want the request to actually hit.
///
/// Resolution is exact-string match. No glob, no regex, no fallback
/// chain — if a model isn't in the map, the proxy passes the name
/// through unchanged.
///
/// `default_model` is a safety net: if the upstream rejects an
/// aliased or passed-through model — the upstream's error body says
/// the *model* is unknown, as opposed to the request being invalid —
/// the proxy retries the request once with this model. Every
/// fallback is logged at WARN, along with the upstream's own
/// suggestion. `None` disables the safety net.
///
/// The proxy tells a model rejection apart from an unrelated error by
/// inspecting the upstream's error body — see the `model_rejection`
/// module.
#[derive(Debug, Clone, Default)]
pub struct ModelAliases {
    pub map: BTreeMap<String, String>,
    pub default_model: Option<String>,
}

/// Per-model reasoning configuration.
///
/// Two independent surfaces exist:
///
/// 1. **Fixed per-model effort** (`models` / `default`) — used when the
///    client does not send an explicit effort or thinking-disabled signal.
///    Resolution (highest priority first):
///    1. `models[upstream_model]`
///    2. `default`
///    3. legacy `Config::reasoning_effort`
///    4. hardcoded [`DEFAULT_REASONING_EFFORT`] (`"none"`)
///
/// 2. **Request-driven effort translation** (`effort_map` /
///    `thinking_disabled`) — used when Claude Code sends
///    `output_config.effort` or `thinking.type = "disabled"`. These
///    translate the *client's* intent into the *upstream model's* effort
///    vocabulary, since not every upstream understands the same set of
///    values. Both maps support a `default` entry plus per-model entries.
///
/// Valid upstream values are forwarded verbatim ("none", "low",
/// "medium", "high", "xhigh", "max", ...). The proxy doesn't enforce the
/// set — it forwards whatever the operator wrote — so a typo surfaces at
/// the upstream as a 400 rather than at proxy startup. That's deliberate:
/// it's friendlier than refusing to start.
#[derive(Debug, Clone, Default)]
pub struct ReasoningConfig {
    /// Default fixed `reasoning_effort` for upstream models not in `models`.
    pub default: Option<String>,
    /// Fixed per-upstream-model `reasoning_effort`. Keys are upstream
    /// model names; compared exactly.
    pub models: std::collections::BTreeMap<String, String>,
    /// Claude-Code-effort → upstream-effort translation. Left-hand keys
    /// are Claude Code effort values (`low`/`medium`/`high`/`xhigh`/`max`);
    /// right-hand values are upstream effort strings, or `None` to omit the
    /// upstream `reasoning` object entirely.
    pub effort_map: EffortMap,
    /// `thinking.type = "disabled"` → upstream-effort translation. A
    /// value of `"none"` sends `reasoning.effort = "none"`; `None` omits
    /// the upstream `reasoning` object. Disabled thinking takes precedence
    /// over both `output_config.effort` and the fixed per-model config.
    pub thinking_disabled: EffortMap,
}

/// A translation map from a client intent key (a Claude effort value or a
/// disabled-thinking sentinel) to an upstream effort value. Supports a
/// `default` for all models plus per-model overrides.
#[derive(Debug, Clone, Default)]
pub struct EffortMap {
    /// Fallback for upstream models not present in [`EffortMap::models`].
    pub default: BTreeMap<String, Option<String>>,
    /// Per-upstream-model translations. Keys are upstream model names;
    /// inner keys are the client intent values.
    pub models: BTreeMap<String, BTreeMap<String, Option<String>>>,
}

/// The resolved reasoning intent for a single request, after applying the
/// configured translation maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningDecision {
    /// Explicit disabled thinking → mapped upstream value (usually
    /// `"none"`). Takes precedence over an effort selection.
    Disabled(String),
    /// A translated effort level.
    Effort(String),
    /// Send no upstream `reasoning` object.
    Omit,
}

impl ReasoningDecision {
    /// The upstream `reasoning.effort` string to send, or `None` when the
    /// upstream `reasoning` object should be omitted entirely.
    #[must_use]
    pub fn upstream_effort(&self) -> Option<&str> {
        match self {
            Self::Disabled(e) | Self::Effort(e) => Some(e),
            Self::Omit => None,
        }
    }
}

/// Prompt-caching configuration.
///
/// `enabled` is the master switch. When `false` (the default), the
/// proxy does not emit `prompt_cache_key`, `prompt_cache_options`, or
/// `prompt_cache_breakpoint` fields, so deployments pointing at Ollama,
/// OpenRouter, vLLM, or other non-OpenAI upstreams are unaffected.
/// When `true`, Anthropic `cache_control: {type: "ephemeral"}` hints
/// from the client are translated into explicit OpenAI breakpoints.
///
/// `cache_key` is optional; if set it is forwarded verbatim as
/// `prompt_cache_key`.
#[derive(Debug, Clone, Default)]
pub struct PromptCachingConfig {
    pub models: Vec<String>,
    pub cache_key: Option<String>,
}

/// How the proxy replays a prior turn's extended thinking back to the upstream.
///
/// Anthropic clients (Claude Code) echo the model's `thinking` blocks back on
/// the next request. The upstream, however, is an OpenAI-Responses-API
/// endpoint, which has no equivalent of the Anthropic `thinking` block. Three
/// encodings are possible:
///
/// * [`ReasoningReplayMode::Off`] — drop the blocks. This is the pre-fix
///   behaviour and it silently destroys the model's memory of its own
///   reasoning, so it is not the default.
/// * [`ReasoningReplayMode::Text`] — fold the reasoning *text* into the
///   assistant message. Safe for **every** upstream: it is ordinary
///   assistant text, so it cannot introduce a new input-item type and
///   therefore cannot trip an upstream's unknown-item rejection. This is the
///   default.
/// * [`ReasoningReplayMode::Native`] — emit a Responses `reasoning` input
///   item. This is OpenAI's documented replay mechanism and DeepSeek's
///   Responses API requires it, but it is a provider-specific wire shape
///   (OpenAI pairs a reasoning item with the item that follows it, and some
///   endpoints reject a reasoning item without an `id`). Opt in per model
///   and verify against your upstream first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayMode {
    Off,
    #[default]
    Text,
    Native,
}

/// How the proxy handles `image` blocks that appear **inside a `tool_result`**
/// on the way to the upstream.
///
/// The Responses API lets a `function_call_output` carry `input_image` parts,
/// so a tool that returns a screenshot does not have to lose the pixels.
/// Third-party gateways that only understand the string form, however, may
/// reject the array form outright — hence the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolResultImages {
    /// Send `output` as an array of `input_text` / `input_image` parts.
    /// This is what OpenAI and DeepSeek's Responses APIs accept. Default.
    #[default]
    Array,
    /// Send text-only in `function_call_output` and pass the images in a
    /// follow-up `user` message carrying the `input_image` parts. For
    /// gateways that reject array-form tool outputs.
    Lift,
    /// Drop the images (with a warning). This is what the proxy did before
    /// the feature existed.
    Drop,
}

impl ToolResultImages {
    /// Lenient parse used by the config resolver. Matching is
    /// case-insensitive; anything else returns `None` so the caller can
    /// fall back to the default with a warning.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "array" => Some(Self::Array),
            "lift" => Some(Self::Lift),
            "drop" => Some(Self::Drop),
            _ => None,
        }
    }
}

/// Where a `role: "system"` message that sits **inside the conversation** goes
/// on the way to the upstream.
///
/// Claude Code does not only send a system prompt at the top. It also puts
/// `system` messages into `messages` (the environment block, the skills list,
/// "the user hasn't heard from you in a while", hook output), each at the
/// point in the conversation where it applies.
///
/// An upstream's prompt cache can only reuse a prompt whose START is
/// unchanged. Folding those messages into `instructions` (the very start of
/// the prompt) changes the start every time one arrives, so the whole
/// conversation after it is re-read at full price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SystemMessagePlacement {
    /// Append the message to `instructions`. Works with every upstream, but
    /// breaks the prompt cache each time a system message arrives. Default,
    /// because it is the behaviour every upstream is known to accept.
    #[default]
    Instructions,
    /// Send it as a `system` message at its own position in `input`. The
    /// start of the prompt then stays the same and the cache keeps hitting.
    /// The upstream must accept system messages after the first turn; the
    /// OpenAI Responses API does, and a chat-template-based upstream may not.
    Inline,
}

impl SystemMessagePlacement {
    /// Lenient parse used by the config resolver (case-insensitive).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "instructions" => Some(Self::Instructions),
            "inline" => Some(Self::Inline),
            _ => None,
        }
    }
}

/// Reasoning-replay settings.
///
/// `models` holds per-upstream-model overrides keyed by the **alias-resolved
/// upstream** model name (the same convention as `prompt_caching.models`).
/// `default` applies to every upstream model not listed.
#[derive(Debug, Clone)]
pub struct ReasoningReplayConfig {
    pub default: ReasoningReplayMode,
    pub models: BTreeMap<String, ReasoningReplayMode>,
    /// Max characters of reasoning text replayed per assistant turn.
    /// `0` means unlimited. Replaying reasoning costs input tokens on every
    /// subsequent turn, so this is the operator's throttle.
    pub max_chars: usize,
}

impl Default for ReasoningReplayConfig {
    fn default() -> Self {
        Self {
            default: ReasoningReplayMode::Text,
            models: BTreeMap::new(),
            max_chars: 0,
        }
    }
}

/// The resolved reasoning-replay policy for one request.
///
/// Carries a per-request counter of how much reasoning text was actually
/// replayed, so the proxy can report it on the "→ upstream" line. The
/// counter is the operator's signal that the upstream is still producing
/// reasoning: if it drops to zero across every request, the upstream has
/// stopped populating its reasoning summary and the replay has silently
/// become a no-op.
#[derive(Debug, Clone)]
pub struct ReasoningReplay {
    pub mode: ReasoningReplayMode,
    /// Max characters of reasoning text replayed per assistant turn.
    /// `0` means unlimited.
    pub max_chars: usize,
    /// Characters of reasoning text replayed so far in this request.
    pub replayed_chars: std::cell::Cell<u64>,
}

impl Default for ReasoningReplay {
    /// The safe default: fold the reasoning text into the assistant message.
    fn default() -> Self {
        Self {
            mode: ReasoningReplayMode::default(),
            max_chars: 0,
            replayed_chars: std::cell::Cell::new(0),
        }
    }
}

impl Config {
    /// Load configuration from the environment and optional `proxy.json`.
    pub fn load() -> Result<Self> {
        Self::load_from(Path::new("proxy.json"))
    }

    /// Load configuration, looking for the JSON file at `json_path`.
    ///
    /// Exposed for tests; production code should call [`Config::load`].
    pub fn load_from(json_path: &Path) -> Result<Self> {
        let from_file = if json_path.exists() {
            let raw = fs::read_to_string(json_path)
                .with_context(|| format!("read config file at {}", json_path.display()))?;
            Some(JsonConfig::parse(&raw).with_context(|| "parse proxy.json")?)
        } else {
            None
        };

        let env_inputs = EnvInputs::capture();
        Self::resolve(from_file.as_ref(), &env_inputs)
    }

    /// Resolve from explicit inputs. `pub(crate)` so integration tests in
    /// `tests/` can't reach in and bypass the env-loading entry point, but
    /// unit tests inside this module can.
    pub(crate) fn resolve(file: Option<&JsonConfig>, env: &EnvInputs) -> Result<Self> {
        let listen_addr = pick_str(
            file.and_then(|f| f.listen_addr.as_deref()),
            env.listen_addr.as_deref(),
        )
        .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_owned())
        .parse::<SocketAddr>()
        .context("LISTEN_ADDR is not a valid socket address")?;

        let upstream_base_url = pick_str(
            file.and_then(|f| f.upstream_base_url.as_deref()),
            env.upstream_base_url.as_deref(),
        )
        .context("UPSTREAM_BASE_URL is required (set env var or set in proxy.json)")?;

        // Validate that the URL parses. We don't keep the parsed form because
        // reqwest will re-parse it at request time, and storing both is noise.
        url::Url::parse(&upstream_base_url).context("UPSTREAM_BASE_URL is not a valid URL")?;

        let upstream_api_key = pick_str(
            file.and_then(|f| f.upstream_api_key.as_deref()),
            env.upstream_api_key.as_deref(),
        )
        .context("UPSTREAM_API_KEY is required (set env var or set in proxy.json)")?;

        let upstream_path = pick_str(
            file.and_then(|f| f.upstream_path.as_deref()),
            env.upstream_path.as_deref(),
        )
        .unwrap_or_else(|| DEFAULT_UPSTREAM_PATH.to_owned());

        let request_timeout_secs = pick_u64(
            file.and_then(|f| f.request_timeout_secs),
            env.request_timeout_secs,
        )
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);

        // File > env > default. The default is what fixes the airia
        // "function tools with reasoning_effort" 400; an operator who
        // wants something different can set REASONING_EFFORT (or the
        // `reasoning_effort` JSON key) to override.
        let reasoning_effort = pick_str(
            file.and_then(|f| f.reasoning_effort.as_deref()),
            env.reasoning_effort.as_deref(),
        )
        .or_else(|| Some(DEFAULT_REASONING_EFFORT.to_owned()));

        // Per-model reasoning table. `default` from the file wins over
        // env REASONING_EFFORT only as a per-section default — the
        // legacy field still feeds the per-request resolution chain
        // (see `reasoning_for_model`), so the operator can use either
        // surface area to set the global default.
        let reasoning_json = file.and_then(|f| f.reasoning.as_ref());
        let reasoning = ReasoningConfig {
            default: reasoning_json
                .and_then(|r| r.default.clone())
                .or_else(|| env.reasoning_effort.clone()),
            models: reasoning_json.map(|r| r.models.clone()).unwrap_or_default(),
            effort_map: reasoning_json
                .map(|r| r.effort_map.clone().into())
                .unwrap_or_default(),
            thinking_disabled: reasoning_json
                .map(|r| r.thinking_disabled.clone().into())
                .unwrap_or_default(),
        };

        let model_aliases = ModelAliases {
            map: file
                .and_then(|f| f.model_aliases.as_ref())
                .map(|a| a.map.clone())
                .unwrap_or_default(),
            default_model: file
                .and_then(|f| f.model_aliases.as_ref())
                .and_then(|a| a.default_model.clone()),
        };

        let prompt_caching_models: Vec<String> = env
            .prompt_caching_models
            .as_ref()
            .map(|s| {
                s.split(',')
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .or_else(|| {
                file.and_then(|f| f.prompt_caching.as_ref())
                    .and_then(|p| p.models.clone())
            })
            .unwrap_or_default();

        let prompt_cache_key = pick_str(
            file.and_then(|f| f.prompt_caching.as_ref())
                .and_then(|p| p.cache_key.as_deref()),
            env.prompt_cache_key.as_deref(),
        );

        let prompt_caching = PromptCachingConfig {
            models: prompt_caching_models,
            cache_key: prompt_cache_key,
        };

        // `reasoning_replay`: how a prior turn's extended thinking is sent
        // back to the upstream. Default is `Text` (fold the reasoning into
        // the assistant message) — the only mode that is safe for an
        // upstream we know nothing about.
        let reasoning_replay = {
            let json = file.and_then(|f| f.reasoning_replay.as_ref());
            ReasoningReplayConfig {
                default: json
                    .and_then(|r| r.default)
                    .unwrap_or(ReasoningReplayMode::Text),
                models: json.map(|r| r.models.clone()).unwrap_or_default(),
                max_chars: json.and_then(|r| r.max_chars).unwrap_or(0),
            }
        };

        // `proxy_key`: env wins over file, then blank values mean no client auth.
        // Treat whitespace-only values as unset so `PROXY_KEY=` cannot
        // accidentally enable an empty authentication secret.
        let proxy_key = pick_str(
            file.and_then(|f| f.proxy_key.as_deref()),
            env.proxy_key.as_deref(),
        )
        .and_then(|key| (!key.trim().is_empty()).then_some(key));

        // The warnings are COLLECTED, not emitted: the tracing subscriber
        // does not exist yet (the binary installs it after this returns), so
        // a `tracing::warn!` here would hit no subscriber and vanish.
        let mut warnings: Vec<ConfigWarning> = Vec::new();

        // Env > file > default. The request body limit must be a positive
        // whole number of bytes; invalid numeric values warn and use the
        // default rather than making startup fail.
        let max_request_body_bytes = resolve_positive_usize(
            pick_limit(
                file.and_then(|f| f.max_request_body_bytes.as_ref()),
                env.max_request_body_bytes.as_ref(),
            ),
            DEFAULT_MAX_REQUEST_BODY_BYTES,
            "max_request_body_bytes",
            &mut warnings,
        );

        // `tool_result_images`: one of `array` (default — the Responses
        // API's array-of-parts form), `lift` (text-only tool output +
        // images in a following user message, for gateways that reject
        // array form) or `drop` (strip images, the pre-fix behaviour).
        // Env wins over file. An unrecognised value falls back to the
        // default WITH a warning rather than failing startup.
        // An empty/whitespace value counts as unset everywhere, so
        // `TOOL_RESULT_IMAGES=` cannot silently force a fallback.
        let file_v = file
            .and_then(|f| f.tool_result_images.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let env_v = env
            .tool_result_images
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let raw = env_v.or(file_v);
        let tool_result_images = raw.map_or_else(ToolResultImages::default, |v| {
                ToolResultImages::parse(v).unwrap_or_else(|| {
                    warnings.push(
                        ConfigWarning::new(format!(
                            "tool_result_images = {v:?} is not one of \"array\", \"lift\" or \"drop\"; using the default \"array\""
                        ))
                        .with_field("tool_result_images", v.to_owned()),
                    );
                    ToolResultImages::default()
                })
            });

        // `system_message_placement`: `instructions` (default) or `inline`.
        // Env wins over file; an unrecognised value falls back to the default
        // with a warning, like `tool_result_images`.
        let placement_raw = env
            .system_message_placement
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                file.and_then(|f| f.system_message_placement.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            });
        let system_message_placement = placement_raw.map_or_else(Default::default, |v| {
            SystemMessagePlacement::parse(v).unwrap_or_else(|| {
                warnings.push(
                    ConfigWarning::new(format!(
                        "system_message_placement = {v:?} is not one of \"instructions\" or \"inline\"; using the default \"instructions\""
                    ))
                    .with_field("system_message_placement", v.to_owned()),
                );
                SystemMessagePlacement::default()
            })
        });

        // `log_to_disk`: env wins over file, then default to `false`.
        // For a boolean we can't reuse `pick_str`; do it inline.
        let log_to_disk = env
            .log_to_disk
            .or_else(|| file.and_then(|f| f.log_to_disk))
            .unwrap_or(false);

        // `advisor`: env > file > default, following `pick_str`. The whole
        // feature is off unless `enabled` resolves to true. A limit of `0`
        // means "no limit" (see `AdvisorConfig`); a value that cannot be a
        // limit (negative, fractional, or too big for the field's type)
        // falls back to the default with a warning rather than failing
        // startup — the same house style as an unusable upstream value
        // surfacing as a 400 instead of a startup error.
        let advisor_defaults = AdvisorConfig::default();
        let advisor_json = file.and_then(|f| f.advisor.as_ref());
        let advisor_enabled = env
            .advisor_enabled
            .or_else(|| advisor_json.and_then(|a| a.enabled))
            .unwrap_or(advisor_defaults.enabled);
        let advisor_model = pick_str(
            advisor_json.and_then(|a| a.model.as_deref()),
            env.advisor_model.as_deref(),
        )
        .unwrap_or(advisor_defaults.model);
        let advisor_effort = pick_str(
            advisor_json.and_then(|a| a.effort.as_deref()),
            env.advisor_effort.as_deref(),
        );
        let advisor_max_uses = resolve_advisor_limit(
            pick_limit(
                advisor_json.and_then(|a| a.max_uses.as_ref()),
                env.advisor_max_uses.as_ref(),
            ),
            DEFAULT_ADVISOR_MAX_USES,
            "advisor.max_uses",
            &mut warnings,
        );
        let advisor_max_tokens = resolve_advisor_limit(
            pick_limit(
                advisor_json.and_then(|a| a.max_tokens.as_ref()),
                env.advisor_max_tokens.as_ref(),
            ),
            DEFAULT_ADVISOR_MAX_TOKENS,
            "advisor.max_tokens",
            &mut warnings,
        );
        let advisor_reasoning_max_tokens = resolve_advisor_limit(
            pick_limit(
                advisor_json.and_then(|a| a.reasoning_max_tokens.as_ref()),
                env.advisor_reasoning_max_tokens.as_ref(),
            ),
            DEFAULT_ADVISOR_REASONING_MAX_TOKENS,
            "advisor.reasoning_max_tokens",
            &mut warnings,
        );
        let advisor_timeout_secs = resolve_advisor_limit(
            pick_limit(
                advisor_json.and_then(|a| a.timeout_secs.as_ref()),
                env.advisor_timeout_secs.as_ref(),
            ),
            DEFAULT_ADVISOR_TIMEOUT_SECS,
            "advisor.timeout_secs",
            &mut warnings,
        )
        // The default is "no limit", which `resolve_advisor_limit` reports
        // as `Some(0)` for an absent key.
        .filter(|secs| *secs != 0);

        let advisor = AdvisorConfig {
            enabled: advisor_enabled,
            model: advisor_model,
            effort: advisor_effort,
            max_uses: advisor_max_uses,
            max_tokens: advisor_max_tokens,
            reasoning_max_tokens: advisor_reasoning_max_tokens,
            timeout_secs: advisor_timeout_secs,
        };

        // An unlimited consultation loop is a real hazard: every iteration
        // costs TWO upstream calls (the executor re-run and the advisor), so
        // an executor that keeps asking would spin. The operator asked for
        // no limit, so the proxy does NOT impose a ceiling — it only makes
        // the hazard visible, once, at startup.
        if advisor.enabled && advisor.has_unbounded_loop() {
            warnings.push(
                ConfigWarning::new(UNBOUNDED_ADVISOR_LOOP_WARNING.to_owned())
                    .with_field("advisor_model", advisor.model.clone()),
            );
        }

        // `web_fetch`: env > file > default, exactly like `advisor`.
        //
        // The three limits use the same RAW-number discipline (the resolver
        // judges them, an invalid value warns instead of failing startup)
        // and the same house rule that `0` means "no limit".
        //
        // The DEFAULTS are permissive on purpose: `max_bytes` and
        // `max_content_bytes` are UNLIMITED unless the operator sets them,
        // because the operator asked for "fetch any URL" with no
        // arbitrary limits. Only the timeout has a shipped default.
        let web_fetch_defaults = WebFetchConfig::default();
        let web_fetch_json = file.and_then(|f| f.web_fetch.as_ref());
        let web_fetch_enabled = env
            .web_fetch_enabled
            .or_else(|| web_fetch_json.and_then(|w| w.enabled))
            .unwrap_or(web_fetch_defaults.enabled);
        let web_fetch_timeout_secs = resolve_web_fetch_limit(
            pick_limit(
                web_fetch_json.and_then(|w| w.timeout_secs.as_ref()),
                env.web_fetch_timeout_secs.as_ref(),
            ),
            web_fetch_defaults.timeout_secs,
            "web_fetch.timeout_secs",
            &mut warnings,
        );
        let web_fetch_max_bytes = resolve_web_fetch_limit(
            pick_limit(
                web_fetch_json.and_then(|w| w.max_bytes.as_ref()),
                env.web_fetch_max_bytes.as_ref(),
            ),
            web_fetch_defaults.max_bytes,
            "web_fetch.max_bytes",
            &mut warnings,
        );
        let web_fetch_max_content_bytes = resolve_web_fetch_limit(
            pick_limit(
                web_fetch_json.and_then(|w| w.max_content_bytes.as_ref()),
                env.web_fetch_max_content_bytes.as_ref(),
            ),
            web_fetch_defaults.max_content_bytes,
            "web_fetch.max_content_bytes",
            &mut warnings,
        );

        let web_fetch = WebFetchConfig {
            enabled: web_fetch_enabled,
            timeout_secs: web_fetch_timeout_secs,
            max_bytes: web_fetch_max_bytes,
            max_content_bytes: web_fetch_max_content_bytes,
        };

        // `server_tools`: env > file > default; `max_rounds` follows the house
        // rule (`0` = no limit).
        let server_tools_defaults = ServerToolsConfig::default();
        let server_tools_json = file.and_then(|f| f.server_tools.as_ref());
        let server_tools = ServerToolsConfig {
            max_rounds: resolve_web_fetch_limit(
                pick_limit(
                    server_tools_json.and_then(|w| w.max_rounds.as_ref()),
                    env.server_tools_max_rounds.as_ref(),
                ),
                Some(DEFAULT_SERVER_TOOLS_MAX_ROUNDS),
                "server_tools.max_rounds",
                &mut warnings,
            )
            .map(|rounds| usize::try_from(rounds).unwrap_or(usize::MAX)),
            pause_turn: env
                .server_tools_pause_turn
                .or_else(|| server_tools_json.and_then(|w| w.pause_turn))
                .unwrap_or(server_tools_defaults.pause_turn),
        };

        // `web_search`: env > file > default. Every limit is BOUNDED: absent
        // = the shipped default, `0` / negative / fractional = the default
        // with a warning (there is deliberately no "unlimited"), and a value
        // above the ceiling is clamped with a warning.
        let web_search_defaults = WebSearchConfig::default();
        let web_search_json = file.and_then(|f| f.web_search.as_ref());
        let web_search = WebSearchConfig {
            enabled: env
                .web_search_enabled
                .or_else(|| web_search_json.and_then(|w| w.enabled))
                .unwrap_or(web_search_defaults.enabled),
            compaction_model: env
                .web_search_compaction_model
                .clone()
                .or_else(|| web_search_json.and_then(|w| w.compaction_model.clone()))
                .map(|m| m.trim().to_owned())
                .filter(|m| !m.is_empty()),
            compaction_threshold_chars: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.compaction_threshold_chars.as_ref()),
                    env.web_search_compaction_threshold_chars.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_COMPACTION_THRESHOLD_CHARS,
                MAX_WEB_SEARCH_COMPACTION_THRESHOLD_CHARS,
                "web_search.compaction_threshold_chars",
                &mut warnings,
            ) as usize,
            max_queries: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.max_queries.as_ref()),
                    env.web_search_max_queries.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_MAX_QUERIES,
                MAX_WEB_SEARCH_MAX_QUERIES,
                "web_search.max_queries",
                &mut warnings,
            ) as usize,
            max_results_per_query: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.max_results_per_query.as_ref()),
                    env.web_search_max_results_per_query.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_MAX_RESULTS_PER_QUERY,
                MAX_WEB_SEARCH_MAX_RESULTS_PER_QUERY,
                "web_search.max_results_per_query",
                &mut warnings,
            ) as usize,
            search_timeout_secs: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.search_timeout_secs.as_ref()),
                    env.web_search_search_timeout_secs.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_SEARCH_TIMEOUT_SECS,
                MAX_WEB_SEARCH_TIMEOUT_SECS,
                "web_search.search_timeout_secs",
                &mut warnings,
            ),
            page_timeout_secs: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.page_timeout_secs.as_ref()),
                    env.web_search_page_timeout_secs.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_PAGE_TIMEOUT_SECS,
                MAX_WEB_SEARCH_TIMEOUT_SECS,
                "web_search.page_timeout_secs",
                &mut warnings,
            ),
            max_page_bytes: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.max_page_bytes.as_ref()),
                    env.web_search_max_page_bytes.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_MAX_PAGE_BYTES,
                MAX_WEB_SEARCH_MAX_PAGE_BYTES,
                "web_search.max_page_bytes",
                &mut warnings,
            ),
            search_endpoint: env
                .web_search_endpoint
                .clone()
                .or_else(|| web_search_json.and_then(|w| w.search_endpoint.clone()))
                .map(|e| e.trim().to_owned())
                .filter(|e| !e.is_empty())
                .unwrap_or(web_search_defaults.search_endpoint),
            search_min_interval_ms: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.search_min_interval_ms.as_ref()),
                    env.web_search_search_min_interval_ms.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_MIN_INTERVAL_MS,
                MAX_WEB_SEARCH_MIN_INTERVAL_MS,
                "web_search.search_min_interval_ms",
                &mut warnings,
            ),
            block_wait_minutes: resolve_bounded_limit(
                pick_limit(
                    web_search_json.and_then(|w| w.block_wait_minutes.as_ref()),
                    env.web_search_block_wait_minutes.as_ref(),
                ),
                DEFAULT_WEB_SEARCH_BLOCK_WAIT_MINUTES,
                MAX_WEB_SEARCH_BLOCK_WAIT_MINUTES,
                "web_search.block_wait_minutes",
                &mut warnings,
            ),
        };

        let prefix_models_for_discovery = env.prefix_models_for_discovery.unwrap_or_else(|| {
            file.and_then(|f| f.prefix_models_for_discovery)
                .unwrap_or(false)
        });

        Ok(Self {
            listen_addr,
            upstream_base_url,
            upstream_api_key,
            upstream_path,
            request_timeout: Duration::from_secs(request_timeout_secs),
            max_request_body_bytes,
            reasoning_effort,
            reasoning,
            model_aliases,
            prompt_caching,
            reasoning_replay,
            tool_result_images,
            system_message_placement,
            proxy_key,
            log_to_disk,
            advisor,
            web_fetch,
            web_search,
            server_tools,
            prefix_models_for_discovery,
            warnings,
        })
    }

    /// Pick the `reasoning_effort` to send to the upstream for a given
    /// inbound `model`. See [`ReasoningConfig`] for the resolution
    /// order. Always returns `Some(_)` because step 4 in the chain
    /// (`DEFAULT_REASONING_EFFORT`) is a constant.
    #[must_use]
    pub fn reasoning_for_model(&self, model: &str) -> Option<String> {
        if let Some(v) = self.reasoning.models.get(model) {
            return Some(v.clone());
        }
        if let Some(v) = &self.reasoning.default {
            return Some(v.clone());
        }
        if let Some(v) = &self.reasoning_effort {
            return Some(v.clone());
        }
        Some(DEFAULT_REASONING_EFFORT.to_owned())
    }

    /// Resolve the reasoning intent for a single inbound request.
    ///
    /// Precedence:
    ///   1. Explicit `thinking.type = "disabled"` (from `thinking_disabled`
    ///      map) — overrides everything else.
    ///   2. Explicit `output_config.effort` (from `effort_map`) — model
    ///      entry, then `default` entry, then identity if the requested
    ///      value is a recognized upstream value.
    ///   3. No client signal → existing fixed per-model resolution
    ///      (`reasoning.models` → `reasoning.default` → legacy
    ///      `reasoning_effort` → `DEFAULT_REASONING_EFFORT`).
    ///
    /// `requested_effort` is the normalized Claude Code effort string
    /// (`low`/`medium`/`high`/`xhigh`/`max`), and `thinking_disabled`
    /// indicates the client asked for no thinking.
    #[must_use]
    pub fn reasoning_for_request(
        &self,
        upstream_model: &str,
        requested_effort: Option<&str>,
        thinking_disabled: bool,
    ) -> ReasoningDecision {
        // 1. Disabled thinking has highest precedence.
        if thinking_disabled {
            if let Some(v) = self
                .reasoning
                .thinking_disabled
                .models
                .get(upstream_model)
                .and_then(|m| m.get("disabled"))
            {
                return v
                    .clone()
                    .map_or(ReasoningDecision::Omit, ReasoningDecision::Disabled);
            }
            if let Some(v) = self.reasoning.thinking_disabled.default.get("disabled") {
                return v
                    .clone()
                    .map_or(ReasoningDecision::Omit, ReasoningDecision::Disabled);
            }
            // No configured disabled mapping: fall through to effort/
            // fixed resolution rather than inventing a value.
        }

        // 2. Explicit effort request.
        if let Some(effort) = requested_effort {
            let norm = effort.trim().to_ascii_lowercase();
            if norm.is_empty() {
                // Treated as no effort signal.
            } else if let Some(v) = self
                .reasoning
                .effort_map
                .models
                .get(upstream_model)
                .and_then(|m| m.get(&norm))
            {
                return v
                    .clone()
                    .map_or(ReasoningDecision::Omit, ReasoningDecision::Effort);
            } else if let Some(v) = self.reasoning.effort_map.default.get(&norm) {
                return v
                    .clone()
                    .map_or(ReasoningDecision::Omit, ReasoningDecision::Effort);
            } else if is_known_upstream_effort(&norm) {
                // Identity fallback for upstream-native values.
                return ReasoningDecision::Effort(norm);
            }
            // Unmapped/unrecognized requested value: fall through to
            // the fixed resolution below.
        }

        // 3. Fixed per-model resolution.
        ReasoningDecision::Effort(self.reasoning_for_model(upstream_model).unwrap_or_default())
    }

    /// Normalize a raw Claude Code `output_config.effort` value. Returns
    /// `None` for absent/blank values so callers can treat them as "no
    /// effort signal".
    #[must_use]
    pub fn normalize_effort(raw: Option<&str>) -> Option<String> {
        let norm = raw?.trim().to_ascii_lowercase();
        if norm.is_empty() { None } else { Some(norm) }
    }

    /// Resolve the inbound `model` to the upstream model name. If no
    /// alias is configured, the inbound name is returned unchanged.
    /// The caller should use the *returned* name for both the
    /// upstream request's `model` field AND for the
    /// `reasoning_for_model` lookup, so an aliased request picks up
    /// the right reasoning entry too.
    #[must_use]
    pub fn upstream_model_for(&self, model: &str) -> String {
        self.model_aliases
            .map
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned())
    }

    /// Fallback model to retry with when the upstream rejects a
    /// request with a "model not supported" / "model not found" error.
    /// `None` means the proxy surfaces the rejection to the client
    /// without retrying.
    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.model_aliases.default_model.as_deref()
    }

    /// Prompt-caching settings to use for a given upstream model.
    /// Returns a config with `models` populated only if the model is
    /// in the configured list, so callers can check `!models.is_empty()`
    /// to decide whether to emit prompt-caching fields.
    #[must_use]
    pub fn prompt_caching_for_model(&self, model: &str) -> PromptCachingConfig {
        let enabled = self.prompt_caching.models.iter().any(|m| m == model);
        PromptCachingConfig {
            models: if enabled {
                self.prompt_caching.models.clone()
            } else {
                Vec::new()
            },
            cache_key: self.prompt_caching.cache_key.clone(),
        }
    }

    /// Reasoning-replay policy for a given upstream model.
    ///
    /// `models` wins; anything not listed falls back to `default` (which is
    /// [`ReasoningReplayMode::Text`] unless the operator changed it).
    #[must_use]
    pub fn reasoning_replay_for_model(&self, model: &str) -> ReasoningReplay {
        ReasoningReplay {
            mode: self
                .reasoning_replay
                .models
                .get(model)
                .copied()
                .unwrap_or(self.reasoning_replay.default),
            max_chars: self.reasoning_replay.max_chars,
            replayed_chars: std::cell::Cell::new(0),
        }
    }
}

/// Environment-variable values relevant to the proxy. Captured once at load
/// time so the resolver doesn't read process state. Tests construct one
/// directly to avoid race conditions on `env::set_var`.
#[derive(Debug, Default, Clone)]
pub struct EnvInputs {
    pub listen_addr: Option<String>,
    pub upstream_base_url: Option<String>,
    pub upstream_api_key: Option<String>,
    pub upstream_path: Option<String>,
    pub request_timeout_secs: Option<u64>,
    /// Raw numeric override; invalid numbers are warned about by the resolver.
    pub max_request_body_bytes: Option<serde_json::Number>,
    pub reasoning_effort: Option<String>,
    pub prompt_caching_models: Option<String>,
    pub prompt_cache_key: Option<String>,
    /// `LOG_TO_DISK=1` enables file logging (and silences the
    /// default drop-everything mode). Other values are treated as
    /// `false` so a typo in the env doesn't quietly enable
    /// structured logs.
    pub log_to_disk: Option<bool>,
    /// `PROXY_KEY=<secret>` enables client auth via `X-Proxy-Key`.
    pub proxy_key: Option<String>,
    /// `TOOL_RESULT_IMAGES=array|lift|drop`. See [`ToolResultImages`].
    /// A value that is not one of the three is judged by the resolver, the
    /// same as a bad value in `proxy.json` (warn + fall back to the
    /// default), never a hard startup failure.
    pub tool_result_images: Option<String>,
    /// `SYSTEM_MESSAGE_PLACEMENT=instructions|inline`. See
    /// [`SystemMessagePlacement`]; judged by the resolver like
    /// `TOOL_RESULT_IMAGES`.
    pub system_message_placement: Option<String>,
    /// Advisor overrides. `ADVISOR_ENABLED` uses the same truthy values as
    /// `LOG_TO_DISK`; a value that is not a number at all is ignored (the
    /// file/default value wins) rather than silently disabling the section.
    ///
    /// The three limits are read as RAW numbers (`serde_json::Number`), so
    /// a negative or fractional value REACHES the resolver instead of
    /// being dropped in silence: the resolver judges it exactly like a
    /// value from `proxy.json` — an invalid one falls back to the shipped
    /// default WITH a warning, and a valid one is that bound.
    pub advisor_enabled: Option<bool>,
    pub advisor_model: Option<String>,
    pub advisor_effort: Option<String>,
    pub advisor_max_uses: Option<serde_json::Number>,
    pub advisor_max_tokens: Option<serde_json::Number>,
    pub advisor_reasoning_max_tokens: Option<serde_json::Number>,
    pub advisor_timeout_secs: Option<serde_json::Number>,
    /// `web_fetch` overrides. `WEB_FETCH_ENABLED` uses the same truthy
    /// values as `LOG_TO_DISK`; the three limits are read as RAW numbers
    /// so the resolver judges them exactly like the file's values.
    pub web_fetch_enabled: Option<bool>,
    pub web_fetch_timeout_secs: Option<serde_json::Number>,
    pub web_fetch_max_bytes: Option<serde_json::Number>,
    pub web_fetch_max_content_bytes: Option<serde_json::Number>,
    /// `web_search` overrides, read like the `web_fetch` ones.
    pub web_search_enabled: Option<bool>,
    pub web_search_compaction_model: Option<String>,
    pub web_search_compaction_threshold_chars: Option<serde_json::Number>,
    pub web_search_max_queries: Option<serde_json::Number>,
    pub web_search_max_results_per_query: Option<serde_json::Number>,
    pub web_search_search_timeout_secs: Option<serde_json::Number>,
    pub web_search_page_timeout_secs: Option<serde_json::Number>,
    pub web_search_max_page_bytes: Option<serde_json::Number>,
    pub web_search_endpoint: Option<String>,
    pub web_search_search_min_interval_ms: Option<serde_json::Number>,
    pub web_search_block_wait_minutes: Option<serde_json::Number>,
    /// `server_tools` overrides.
    pub server_tools_max_rounds: Option<serde_json::Number>,
    pub server_tools_pause_turn: Option<bool>,
    pub prefix_models_for_discovery: Option<bool>,
}

fn parse_truthy(raw: &str) -> bool {
    let v = raw.trim().to_ascii_lowercase();
    matches!(v.as_str(), "1" | "true" | "yes" | "on")
}

impl EnvInputs {
    /// Read the current process environment into an `EnvInputs` snapshot.
    pub fn capture() -> Self {
        Self {
            listen_addr: env::var("LISTEN_ADDR").ok(),
            upstream_base_url: env::var("UPSTREAM_BASE_URL").ok(),
            upstream_api_key: env::var("UPSTREAM_API_KEY").ok(),
            upstream_path: env::var("UPSTREAM_PATH").ok(),
            request_timeout_secs: env::var("REQUEST_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok()),
            max_request_body_bytes: env::var("MAX_REQUEST_BODY_BYTES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            tool_result_images: env::var("TOOL_RESULT_IMAGES").ok(),
            system_message_placement: env::var("SYSTEM_MESSAGE_PLACEMENT").ok(),
            reasoning_effort: env::var("REASONING_EFFORT").ok(),
            prompt_caching_models: env::var("PROMPT_CACHING_MODELS").ok(),
            prompt_cache_key: env::var("PROMPT_CACHE_KEY").ok(),
            log_to_disk: env::var("LOG_TO_DISK").ok().map(|s| parse_truthy(&s)),
            proxy_key: env::var("PROXY_KEY").ok(),
            advisor_enabled: env::var("ADVISOR_ENABLED").ok().map(|s| parse_truthy(&s)),
            advisor_model: env::var("ADVISOR_MODEL").ok(),
            advisor_effort: env::var("ADVISOR_EFFORT").ok(),
            advisor_max_uses: env::var("ADVISOR_MAX_USES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            advisor_max_tokens: env::var("ADVISOR_MAX_TOKENS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            advisor_reasoning_max_tokens: env::var("ADVISOR_REASONING_MAX_TOKENS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            advisor_timeout_secs: env::var("ADVISOR_TIMEOUT_SECS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_fetch_enabled: env::var("WEB_FETCH_ENABLED").ok().map(|s| parse_truthy(&s)),
            web_fetch_timeout_secs: env::var("WEB_FETCH_TIMEOUT_SECS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_fetch_max_bytes: env::var("WEB_FETCH_MAX_BYTES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_fetch_max_content_bytes: env::var("WEB_FETCH_MAX_CONTENT_BYTES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            server_tools_max_rounds: env::var("SERVER_TOOLS_MAX_ROUNDS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            server_tools_pause_turn: env::var("SERVER_TOOLS_PAUSE_TURN")
                .ok()
                .map(|s| parse_truthy(&s)),
            web_search_enabled: env::var("WEB_SEARCH_ENABLED")
                .ok()
                .map(|s| parse_truthy(&s)),
            web_search_compaction_model: env::var("WEB_SEARCH_COMPACTION_MODEL").ok(),
            web_search_compaction_threshold_chars: env::var(
                "WEB_SEARCH_COMPACTION_THRESHOLD_CHARS",
            )
            .ok()
            .and_then(|s| parse_env_limit(&s)),
            web_search_max_queries: env::var("WEB_SEARCH_MAX_QUERIES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_max_results_per_query: env::var("WEB_SEARCH_MAX_RESULTS_PER_QUERY")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_search_timeout_secs: env::var("WEB_SEARCH_SEARCH_TIMEOUT_SECS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_page_timeout_secs: env::var("WEB_SEARCH_PAGE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_max_page_bytes: env::var("WEB_SEARCH_MAX_PAGE_BYTES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_endpoint: env::var("WEB_SEARCH_ENDPOINT").ok(),
            web_search_search_min_interval_ms: env::var("WEB_SEARCH_SEARCH_MIN_INTERVAL_MS")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            web_search_block_wait_minutes: env::var("WEB_SEARCH_BLOCK_WAIT_MINUTES")
                .ok()
                .and_then(|s| parse_env_limit(&s)),
            prefix_models_for_discovery: env::var("PREFIX_MODELS_FOR_DISCOVERY")
                .ok()
                .map(|s| parse_truthy(&s)),
        }
    }
}

fn pick_str(file_value: Option<&str>, env_value: Option<&str>) -> Option<String> {
    env_value.or(file_value).map(str::to_owned)
}

/// Recognized upstream effort values. Used as the identity fallback when
/// a client requests a value that is not present in the configured
/// `effort_map` — the proxy passes it through unchanged because the
/// upstream already speaks it.
fn is_known_upstream_effort(v: &str) -> bool {
    matches!(
        v,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

fn pick_u64(file_value: Option<u64>, env_value: Option<u64>) -> Option<u64> {
    env_value.or(file_value)
}

/// Pick an advisor limit: env > file.
///
/// The value is a RAW number (see [`JsonAdvisorConfig`]); the resolver is
/// the only place that judges it.
fn pick_limit(
    file_value: Option<&serde_json::Number>,
    env_value: Option<&serde_json::Number>,
) -> Option<serde_json::Number> {
    env_value.or(file_value).cloned()
}

/// How a raw (unresolved) advisor limit number was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawLimit {
    /// A whole number that fits in `u64`.
    Whole(u64),
    /// A negative number.
    Negative,
    /// A number with a fractional part.
    Fractional,
    /// A whole number too large to be a limit.
    TooLarge,
}

/// Classify a raw advisor limit number.
///
/// The JSON layer accepts ANY number (so a typo can never stop the proxy
/// from starting); this is where the number is judged:
///
/// * a whole, non-negative number → `Whole(v)`;
/// * a negative number → `Negative`;
/// * a number with a fractional part → `Fractional`;
/// * a whole number too large for `u64` → `TooLarge`.
fn classify_limit(number: &serde_json::Number) -> RawLimit {
    if let Some(v) = number.as_u64() {
        return RawLimit::Whole(v);
    }
    if let Some(v) = number.as_i64() {
        // serde_json stores every negative integer as an `i64`.
        return if v < 0 {
            RawLimit::Negative
        } else {
            RawLimit::Whole(v as u64)
        };
    }
    // A float: serde_json only stores one when the number is not a plain
    // integer (or is too big for `u64`).
    let Some(v) = number.as_f64() else {
        // Unreachable in practice: a `serde_json::Number` is always an
        // integer or a float.
        return RawLimit::Fractional;
    };
    if !v.is_finite() {
        return RawLimit::TooLarge;
    }
    if v.fract() != 0.0 {
        return RawLimit::Fractional;
    }
    if v < 0.0 {
        return RawLimit::Negative;
    }
    if v >= u64::MAX as f64 {
        return RawLimit::TooLarge;
    }
    RawLimit::Whole(v as u64)
}

/// Resolve one advisor limit from its raw JSON/env value.
///
/// * **absent** → the shipped default (an absent key is NOT "no limit");
/// * **`0`** → **no limit** (`None`);
/// * **`n > 0`** → `Some(n)`;
/// * **anything else** (negative, fractional, too large for the field's
///   type) → the shipped default WITH a warning.
///
/// A value that cannot be a limit falls back to the default instead of
/// failing the startup: an operator typo must not stop the proxy from
/// starting.
///
/// The warning is COLLECTED, not emitted: the resolver runs before the
/// tracing subscriber exists, so a `tracing::warn!` here would vanish.
/// See [`ConfigWarning`].
fn resolve_advisor_limit<T>(
    value: Option<serde_json::Number>,
    default: T,
    field: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<T>
where
    T: Copy + std::fmt::Display + TryFrom<u64>,
    T::Error: std::fmt::Display,
{
    let Some(number) = value.as_ref() else {
        return Some(default);
    };
    let v = match classify_limit(number) {
        RawLimit::Whole(v) => v,
        RawLimit::Negative => {
            push_invalid_limit_warning(warnings, field, number, default, "it is negative");
            return Some(default);
        }
        RawLimit::Fractional => {
            push_invalid_limit_warning(
                warnings,
                field,
                number,
                default,
                "it is not a whole number",
            );
            return Some(default);
        }
        RawLimit::TooLarge => {
            push_invalid_limit_warning(warnings, field, number, default, "it is out of range");
            return Some(default);
        }
    };
    if v == 0 {
        // `0` = no limit (see `AdvisorConfig`).
        return None;
    }
    match T::try_from(v) {
        Ok(n) => Some(n),
        Err(err) => {
            warnings.push(
                ConfigWarning::new(format!(
                    "{field} = {v} is out of range ({err}); using the default {default}"
                ))
                .with_field("field", field)
                .with_field("value", v.to_string())
                .with_field("fallback", default.to_string()),
            );
            Some(default)
        }
    }
}

/// Resolve one BOUNDED `web_search` limit from its raw JSON/env value.
///
/// Unlike [`resolve_web_fetch_limit`] there is no "unlimited": the tool makes
/// outbound requests on a model's behalf, so every limit keeps a ceiling.
///
/// * **absent** → `default`;
/// * **`0`, negative, fractional, out of range** → `default`, WITH a warning;
/// * **`1..=ceiling`** → that value;
/// * **above `ceiling`** → `ceiling`, WITH a warning.
fn resolve_bounded_limit(
    value: Option<serde_json::Number>,
    default: u64,
    ceiling: u64,
    field: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> u64 {
    let Some(number) = value.as_ref() else {
        return default;
    };
    let invalid = match classify_limit(number) {
        RawLimit::Whole(0) => "it must be at least 1 (web_search has no unlimited setting)",
        RawLimit::Whole(v) if v > ceiling => {
            warnings.push(
                ConfigWarning::new(format!(
                    "{field} = {number} is above the maximum; clamped to {ceiling}"
                ))
                .with_field("field", field)
                .with_field("value", number.to_string())
                .with_field("fallback", ceiling.to_string()),
            );
            return ceiling;
        }
        RawLimit::Whole(v) => return v,
        RawLimit::Negative => "it is negative",
        RawLimit::Fractional => "it is not a whole number",
        RawLimit::TooLarge => "it is out of range",
    };
    push_invalid_limit_warning(warnings, field, number, default, invalid);
    default
}

/// Resolve one `web_fetch` limit from its raw JSON/env value.
///
/// Same discipline as [`resolve_advisor_limit`], with one difference: the
/// default is an `Option`, because two of the three `web_fetch` limits
/// default to UNLIMITED (`None`).
///
/// * **absent** → the shipped default (which may itself be `None`);
/// * **`0`** → **no limit** (`None`), the house rule;
/// * **`n > 0`** → `Some(n)`;
/// * **anything else** → the shipped default WITH a warning.
fn resolve_web_fetch_limit(
    value: Option<serde_json::Number>,
    default: Option<u64>,
    field: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<u64> {
    let Some(number) = value.as_ref() else {
        return default;
    };
    let v = match classify_limit(number) {
        RawLimit::Whole(v) => v,
        RawLimit::Negative => {
            push_invalid_web_fetch_limit_warning(
                warnings,
                field,
                number,
                default,
                "it is negative",
            );
            return default;
        }
        RawLimit::Fractional => {
            push_invalid_web_fetch_limit_warning(
                warnings,
                field,
                number,
                default,
                "it is not a whole number",
            );
            return default;
        }
        RawLimit::TooLarge => {
            push_invalid_web_fetch_limit_warning(
                warnings,
                field,
                number,
                default,
                "it is out of range",
            );
            return default;
        }
    };
    if v == 0 {
        // `0` = no limit (the house rule).
        return None;
    }
    Some(v)
}

/// Record the warning for a `web_fetch` limit that cannot be a limit.
///
/// The fallback is an `Option`, so it is rendered as "unlimited" rather
/// than a number — `Option<u64>` is not `Display`.
fn push_invalid_web_fetch_limit_warning(
    warnings: &mut Vec<ConfigWarning>,
    field: &str,
    number: &serde_json::Number,
    default: Option<u64>,
    reason: &str,
) {
    let fallback = default.map_or_else(|| "unlimited".to_owned(), |v| v.to_string());
    warnings.push(
        ConfigWarning::new(format!(
            "{field} = {number} is not a valid limit ({reason}); using the default {fallback}"
        ))
        .with_field("field", field)
        .with_field("value", number.to_string())
        .with_field("fallback", fallback),
    );
}

/// Record the warning for an advisor limit that cannot be a limit
/// (negative, fractional, too large). COLLECTED, not emitted — see
/// [`ConfigWarning`].
fn push_invalid_limit_warning<T: std::fmt::Display>(
    warnings: &mut Vec<ConfigWarning>,
    field: &str,
    number: &serde_json::Number,
    default: T,
    reason: &str,
) {
    warnings.push(
        ConfigWarning::new(format!(
            "{field} = {number} is not a valid limit ({reason}); using the default {default}"
        ))
        .with_field("field", field)
        .with_field("value", number.to_string())
        .with_field("fallback", default.to_string()),
    );
}

/// Parse an advisor limit from an env var.
///
/// The env var is a free-form STRING, so it can hold anything the operator
/// typed. Every JSON NUMBER is kept — negative, fractional, huge — so the
/// resolver judges it exactly like a value from `proxy.json` (and warns
/// about the invalid ones instead of dropping them in silence).
///
/// A value that is not a number at all is IGNORED: the file/default value
/// wins. That is the long-standing behaviour for these vars.
fn parse_env_limit(raw: &str) -> Option<serde_json::Number> {
    match serde_json::from_str::<serde_json::Value>(raw.trim()) {
        Ok(serde_json::Value::Number(n)) => Some(n),
        _ => None,
    }
}

/// Resolve a positive whole-number byte limit. Unlike advisor limits, zero
/// is invalid here: a zero-byte body limit would effectively disable the
/// endpoint. Invalid numeric values fall back with a collected warning.
fn resolve_positive_usize(
    value: Option<serde_json::Number>,
    default: usize,
    field: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> usize {
    let Some(number) = value.as_ref() else {
        return default;
    };
    let raw = match classify_limit(number) {
        RawLimit::Whole(v) if v > 0 => v,
        RawLimit::Whole(_) => {
            push_invalid_limit_warning(
                warnings,
                field,
                number,
                default,
                "it must be greater than zero",
            );
            return default;
        }
        RawLimit::Negative => {
            push_invalid_limit_warning(warnings, field, number, default, "it is negative");
            return default;
        }
        RawLimit::Fractional => {
            push_invalid_limit_warning(
                warnings,
                field,
                number,
                default,
                "it is not a whole number",
            );
            return default;
        }
        RawLimit::TooLarge => {
            push_invalid_limit_warning(warnings, field, number, default, "it is out of range");
            return default;
        }
    };
    usize::try_from(raw).unwrap_or_else(|_| {
        push_invalid_limit_warning(warnings, field, number, default, "it is out of range");
        default
    })
}

/// JSON representation of `proxy.json`. Every field is optional; missing
/// fields fall through to env vars and then to defaults.
///
/// `PartialEq` (and `Eq`) so a test can compare a config with its
/// round-tripped self: the advisor limits are raw JSON numbers, which
/// serde_json keeps `Eq` (its floats are always finite).
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JsonConfig {
    pub listen_addr: Option<String>,
    pub upstream_base_url: Option<String>,
    pub upstream_api_key: Option<String>,
    pub upstream_path: Option<String>,
    pub request_timeout_secs: Option<u64>,
    /// Positive whole-number `/v1/messages` body limit in bytes. Raw JSON
    /// number retained so invalid values can warn and fall back at resolve
    /// time rather than failing config parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_request_body_bytes: Option<serde_json::Number>,
    pub reasoning_effort: Option<String>,
    /// Per-model `reasoning_effort` overrides. Sub-object because
    /// `deny_unknown_fields` rejects any keys we haven't declared
    /// here; the object itself is optional.
    pub reasoning: Option<JsonReasoningConfig>,
    /// Inbound → upstream model name aliases. See [`ModelAliases`].
    pub model_aliases: Option<JsonModelAliases>,
    /// Prompt-caching settings. See [`PromptCachingConfig`].
    pub prompt_caching: Option<JsonPromptCachingConfig>,
    /// How a prior turn's extended thinking is replayed upstream.
    /// See [`ReasoningReplayConfig`].
    ///
    /// `skip_serializing_if` matters: the TUI rewrites `proxy.json` from
    /// this struct, and `JsonConfig` is `deny_unknown_fields`, so writing
    /// a `reasoning_replay: null` key would make the PREVIOUS binary refuse
    /// to start — a rollback hazard for anyone who saved from the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_replay: Option<JsonReasoningReplayConfig>,
    /// How `image` blocks inside a `tool_result` are forwarded.
    /// See [`ToolResultImages`]. `skip_serializing_if` keeps an unset key
    /// out of the file the TUI rewrites, so a config file that never sets
    /// it round-trips unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_images: Option<String>,
    /// Where a `role: "system"` message inside the conversation goes. See
    /// [`SystemMessagePlacement`]. Unset keeps the file unchanged when the
    /// TUI rewrites it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message_placement: Option<String>,
    /// Shared secret required on inbound `X-Proxy-Key` header.
    /// Omit to leave `/v1/messages` unauthenticated (with a
    /// startup-time warning).
    pub proxy_key: Option<String>,
    /// When `true`, the proxy writes structured `tracing` events to
    /// `target/logs/proxy.log`. When `false` (the default), tracing
    /// events are dropped — nothing reaches the terminal or a file.
    /// Set to `true` when you want postmortem logs; leave unset (or
    /// `false`) for a clean terminal in interactive use.
    pub log_to_disk: Option<bool>,
    /// Advisor (server-side second opinion) settings. Off by default.
    /// See [`AdvisorConfig`].
    ///
    /// `skip_serializing_if` is **mandatory**: the TUI rewrites the whole
    /// `proxy.json` from this struct, and `JsonConfig` is
    /// `deny_unknown_fields`, so writing an `advisor: null` key would make
    /// the PREVIOUS binary refuse to start — a rollback hazard for anyone
    /// who saved from the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<JsonAdvisorConfig>,
    /// Server-side `web_fetch` settings. Off by default.
    /// See [`WebFetchConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<JsonWebFetchConfig>,
    /// Proxy-native `web_search` settings. Off by default.
    /// See [`WebSearchConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<JsonWebSearchConfig>,
    /// Limits of the shared server-tool loop. See [`ServerToolsConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tools: Option<JsonServerToolsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_models_for_discovery: Option<bool>,
}

/// JSON shape of the `web_fetch` section of `proxy.json`.
///
/// Every field is optional. `deny_unknown_fields` keeps a typo from being
/// silently ignored.
///
/// The limits are RAW numbers (same rationale as [`JsonAdvisorConfig`]):
/// the RESOLVER is the only place that interprets one, so a negative or
/// fractional value produces a WARNING instead of stopping the proxy.
///
/// There is deliberately **no** domain allowlist and **no** URL-length
/// limit: the operator asked for "fetch any URL the model asks for".
/// The bounds that exist are the timeout, the network read cap, and the
/// extracted-content cap.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonWebFetchConfig {
    pub enabled: Option<bool>,
    /// Raw limit, exactly as written. The RESOLVER judges it.
    pub timeout_secs: Option<serde_json::Number>,
    /// Raw limit, exactly as written. The RESOLVER judges it.
    pub max_bytes: Option<serde_json::Number>,
    /// Raw limit, exactly as written. The RESOLVER judges it.
    pub max_content_bytes: Option<serde_json::Number>,
}

/// JSON shape of the `web_search` section of `proxy.json`.
///
/// Every field is optional and `deny_unknown_fields` catches typos. The
/// limits are RAW numbers (same rationale as [`JsonWebFetchConfig`]); the
/// resolver judges them and every one stays bounded.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonWebSearchConfig {
    pub enabled: Option<bool>,
    /// Explicit upstream model used to compact long pages. Unset = truncate.
    pub compaction_model: Option<String>,
    pub compaction_threshold_chars: Option<serde_json::Number>,
    pub max_queries: Option<serde_json::Number>,
    pub max_results_per_query: Option<serde_json::Number>,
    pub search_timeout_secs: Option<serde_json::Number>,
    pub page_timeout_secs: Option<serde_json::Number>,
    pub max_page_bytes: Option<serde_json::Number>,
    /// Search endpoint override (default: DuckDuckGo's HTML interface).
    pub search_endpoint: Option<String>,
    pub search_min_interval_ms: Option<serde_json::Number>,
    pub block_wait_minutes: Option<serde_json::Number>,
}

/// JSON shape of the `server_tools` section of `proxy.json`. `max_rounds` is a
/// RAW number so the resolver can judge it, like the other limits.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonServerToolsConfig {
    pub max_rounds: Option<serde_json::Number>,
    pub pause_turn: Option<bool>,
}

/// JSON shape of the `advisor` section of `proxy.json`.
///
/// Every field is optional so an operator can set only `enabled`, or only
/// `model`. `deny_unknown_fields` keeps a typo from being silently ignored.
/// `model` is an explicit **upstream** model name (default `"beam"`, the
/// cheap advisor), not an inbound alias — see [`AdvisorConfig`].
///
/// An ABSENT `max_uses` / `max_tokens` / `timeout_secs` means the shipped
/// default; `0` means **no limit**. See [`AdvisorConfig`] for what "no
/// limit" means per field.
///
/// ## The three limits are RAW numbers
///
/// `max_uses` / `max_tokens` / `timeout_secs` are
/// `Option<serde_json::Number>` — the number EXACTLY as the operator
/// wrote it, with NO sign, fraction or range check at parse time. That is
/// deliberate:
///
/// * a NEGATIVE number (`-1`) and a FRACTIONAL one (`2.5`) used to be
///   rejected by serde at PARSE time (`invalid value: integer -1,
///   expected u64`), so the proxy EXITED — the opposite of the house rule
///   that an operator typo in one advisor limit must not take the whole
///   deployment down;
/// * a number too big for the field's range used to fail the same way.
///
/// Now the JSON layer accepts ANY JSON number and the RESOLVER is the
/// only place that interprets one: `0` → no limit, a positive whole
/// number in range → that bound, and anything else (negative,
/// fractional, too large) → the shipped default WITH a warning. The
/// serialized shape is unchanged for every value, so the TUI's rewrite of
/// `proxy.json` (§17.2) and its rollback guarantee are unaffected: an
/// invalid value is written back VERBATIM, so the operator can still see
/// (and fix) their typo.
///
/// A value that is not a NUMBER at all (a string, say) is still a
/// configuration error: serde rejects it with a clear message.
///
/// `Eq` is safe: `serde_json::Number` only ever holds FINITE floats, so
/// it is `Eq` (see serde_json's own `Number`).
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonAdvisorConfig {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Raw limit, exactly as written. The RESOLVER judges it — see the
    /// struct docs.
    pub max_uses: Option<serde_json::Number>,
    /// Raw limit, exactly as written. The RESOLVER judges it — see the
    /// struct docs.
    pub max_tokens: Option<serde_json::Number>,
    /// Raw limit, exactly as written. The RESOLVER judges it — see the
    /// struct docs.
    pub reasoning_max_tokens: Option<serde_json::Number>,
    /// Raw limit, exactly as written. The RESOLVER judges it — see the
    /// struct docs.
    pub timeout_secs: Option<serde_json::Number>,
}

/// JSON shape of `reasoning`.
///
/// - `default` — fallback fixed effort for models not in `models`.
/// - `models` — flat model-name → effort object (legacy fixed per-model).
/// - `effort_map` — Claude-Code-effort → upstream-effort translation.
///   Keys are Claude effort values; values are upstream effort strings or
///   `null` (omit the upstream `reasoning` object).
/// - `thinking_disabled` — mapping for `thinking.type = "disabled"`
///   requests. Same shape as `effort_map`, keyed by the `"disabled"`
///   sentinel. A value of `"none"` sends `reasoning.effort = "none"`;
///   `null` omits the upstream `reasoning` object.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JsonReasoningConfig {
    pub default: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, String>,
    #[serde(default)]
    pub effort_map: JsonEffortMap,
    #[serde(default)]
    pub thinking_disabled: JsonEffortMap,
}

/// JSON shape of a translation map (`effort_map` / `thinking_disabled`).
///
/// `default` is the fallback for all upstream models; `models` holds
/// per-upstream-model translations. Inner values are `Option<String>` so
/// `null` can mean "omit the upstream `reasoning` object".
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JsonEffortMap {
    #[serde(default)]
    pub default: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub models: BTreeMap<String, BTreeMap<String, Option<String>>>,
}

impl From<JsonEffortMap> for EffortMap {
    fn from(j: JsonEffortMap) -> Self {
        Self {
            default: j.default,
            models: j.models,
        }
    }
}

/// JSON shape of `model_aliases`. `map` is a flat string→string object:
/// inbound model name → upstream model name. `default_model` is the
/// safety-net fallback used when the upstream rejects a model.
///
/// `#[serde(default)]` lets either field be omitted from the JSON —
/// a `model_aliases` object with only `default_model` is valid.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonModelAliases {
    pub map: BTreeMap<String, String>,
    pub default_model: Option<String>,
}

/// JSON shape of `prompt_caching`.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonPromptCachingConfig {
    #[serde(default)]
    pub models: Option<Vec<String>>,
    pub cache_key: Option<String>,
}

/// JSON shape of `reasoning_replay`.
///
/// - `default` — mode for upstream models not listed in `models`.
///   One of `"off"`, `"text"` (default), `"native"`.
/// - `models` — flat upstream-model-name → mode object.
/// - `max_chars` — max characters of reasoning text replayed per assistant
///   turn. `0` (the default) means unlimited.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonReasoningReplayConfig {
    pub default: Option<ReasoningReplayMode>,
    #[serde(default)]
    pub models: BTreeMap<String, ReasoningReplayMode>,
    pub max_chars: Option<usize>,
}

impl JsonConfig {
    /// Parse JSON into a `JsonConfig`. Public so the TUI can read
    /// the existing config before updating and saving just the mappings.
    pub fn parse(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).context("invalid JSON (check syntax and field names)")
    }
}

#[cfg(test)]
mod tests;
