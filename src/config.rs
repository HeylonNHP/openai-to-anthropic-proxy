//! Configuration loading.
//!
//! Resolution order (highest priority first):
//!   1. Environment variables (`LISTEN_ADDR`, `UPSTREAM_BASE_URL`, ...).
//!   2. `proxy.json` in the working directory, if present.
//!   3. Built-in defaults.
//!
//! Environment variables always win over the JSON file. This lets a deployment
//! override individual values without editing the config file.

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
const DEFAULT_ADVISOR_MAX_TOKENS: u32 = 2048;
const DEFAULT_ADVISOR_TIMEOUT_SECS: u64 = 45;

/// Warning text for an advisor whose consultation loop is UNBOUNDED.
///
/// `advisor.max_uses` = 0 = "no limit". Shared by the config resolver and
/// by the binary's startup banner so the two can never drift.
pub const UNBOUNDED_ADVISOR_LOOP_WARNING: &str = "advisor.max_uses is 0 (no limit): the consultation loop is unbounded — each iteration costs two upstream calls (executor re-run + advisor)";

/// Resolved proxy configuration. Cheap to clone (`String`s and a `Duration`).
#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub upstream_base_url: String,
    pub upstream_api_key: String,
    pub upstream_path: String,
    pub request_timeout: Duration,
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
    /// [`AdvisorConfig`] and `crate::advisor`. When `enabled` is false the
    /// request pipeline behaves exactly as it did before the advisor feature
    /// existed (no extra upstream call, no synthesized tool, no wire fields).
    pub advisor: AdvisorConfig,
}

/// Resolved advisor settings (the `advisor` section of `proxy.json`).
///
/// The feature is **off by default**: `enabled: false` means the proxy
/// makes no advisor calls and emits no advisor wire fields. Enabling it
/// requires an explicit `"enabled": true` in `proxy.json` (or
/// `ADVISOR_ENABLED=1`) AND an inbound tool declaration named `advisor`.
/// The consultation loop lives in `crate::advisor::session`.
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
/// `max_uses`, `max_tokens` and `timeout_secs` are all [`Option`]: `None`
/// means **no limit**, `Some(n)` means a bound of `n`. The rule is the same
/// in `proxy.json` and in the env vars:
///
/// * **key ABSENT** → the shipped default (`max_uses` 2, `max_tokens` 2048,
///   `timeout_secs` 45) — an absent key is *not* "no limit";
/// * **`0`** → **no limit** — the house precedent of
///   `reasoning_replay.max_chars`, where `0` means unlimited;
/// * **`n > 0`** → that bound.
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
/// * `max_tokens: None` — the proxy OMITS `max_output_tokens` from the
///   advisor's own upstream request, so the **upstream's own default**
///   applies. That default may be SMALLER than 2048 — and for reasoning
///   models the cap must also cover the reasoning tokens — so "no limit"
///   here means "the proxy imposes no cap", NOT "the answer may be long".
/// * `timeout_secs: None` — no per-request timeout override. The global
///   `request_timeout` still applies (the reqwest client is built with
///   `.timeout(config.request_timeout)`), and a *bounded* value is always
///   clamped to it, so the advisor can never extend the operator's bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub model: String,
    pub effort: Option<String>,
    /// Consultations per turn. `None` = no limit (see the module docs).
    pub max_uses: Option<u32>,
    /// Cap on the advisor's own answer. `None` = no limit: the proxy sends
    /// no `max_output_tokens` and the upstream's default applies.
    pub max_tokens: Option<u32>,
    /// Per-request timeout for the advisor's upstream call. `None` = no
    /// per-request override; the global `request_timeout` applies.
    pub timeout_secs: Option<u64>,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: DEFAULT_ADVISOR_MODEL.to_owned(),
            effort: None,
            max_uses: Some(DEFAULT_ADVISOR_MAX_USES),
            max_tokens: Some(DEFAULT_ADVISOR_MAX_TOKENS),
            timeout_secs: Some(DEFAULT_ADVISOR_TIMEOUT_SECS),
        }
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

        // `log_to_disk`: env wins over file, then default to `false`.
        // For a boolean we can't reuse `pick_str`; do it inline.
        let log_to_disk = env
            .log_to_disk
            .or_else(|| file.and_then(|f| f.log_to_disk))
            .unwrap_or(false);

        // `advisor`: env > file > default, following `pick_str`. The whole
        // feature is off unless `enabled` resolves to true. A limit of `0`
        // means "no limit" (see `AdvisorConfig`); a value that does not fit
        // the field's integer type falls back to the default with a WARN
        // rather than failing startup — the same house style as an unusable
        // upstream value surfacing as a 400 instead of a startup error.
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
            pick_u64(
                advisor_json.and_then(|a| a.max_uses).map(u64::from),
                env.advisor_max_uses,
            ),
            DEFAULT_ADVISOR_MAX_USES,
            "advisor.max_uses",
        );
        let advisor_max_tokens = resolve_advisor_limit(
            pick_u64(
                advisor_json.and_then(|a| a.max_tokens).map(u64::from),
                env.advisor_max_tokens,
            ),
            DEFAULT_ADVISOR_MAX_TOKENS,
            "advisor.max_tokens",
        );
        let advisor_timeout_secs = resolve_advisor_limit(
            pick_u64(
                advisor_json.and_then(|a| a.timeout_secs),
                env.advisor_timeout_secs,
            ),
            DEFAULT_ADVISOR_TIMEOUT_SECS,
            "advisor.timeout_secs",
        );

        let advisor = AdvisorConfig {
            enabled: advisor_enabled,
            model: advisor_model,
            effort: advisor_effort,
            max_uses: advisor_max_uses,
            max_tokens: advisor_max_tokens,
            timeout_secs: advisor_timeout_secs,
        };

        // An unlimited consultation loop is a real hazard: every iteration
        // costs TWO upstream calls (the executor re-run and the advisor), so
        // an executor that keeps asking would spin. The operator asked for
        // no limit, so the proxy does NOT impose a ceiling — it only makes
        // the hazard visible, once, at startup.
        if advisor.enabled && advisor.max_uses.is_none() {
            tracing::warn!(
                advisor_model = %advisor.model,
                "{}",
                UNBOUNDED_ADVISOR_LOOP_WARNING
            );
        }

        Ok(Self {
            listen_addr,
            upstream_base_url,
            upstream_api_key,
            upstream_path,
            request_timeout: Duration::from_secs(request_timeout_secs),
            reasoning_effort,
            reasoning,
            model_aliases,
            prompt_caching,
            reasoning_replay,
            proxy_key,
            log_to_disk,
            advisor,
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
    /// Advisor overrides. `ADVISOR_ENABLED` uses the same truthy values as
    /// `LOG_TO_DISK`; numeric values that fail to parse are ignored (the
    /// file/default value wins) rather than silently disabling the section.
    ///
    /// The two token limits are read as `u64` so an out-of-range value
    /// (`ADVISOR_MAX_USES=99999999999`) reaches the resolver and is
    /// reported with a WARN instead of being dropped in silence.
    pub advisor_enabled: Option<bool>,
    pub advisor_model: Option<String>,
    pub advisor_effort: Option<String>,
    pub advisor_max_uses: Option<u64>,
    pub advisor_max_tokens: Option<u64>,
    pub advisor_timeout_secs: Option<u64>,
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
                .and_then(|s| s.parse().ok()),
            advisor_max_tokens: env::var("ADVISOR_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok()),
            advisor_timeout_secs: env::var("ADVISOR_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok()),
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

/// Resolve one advisor limit from its JSON/env value.
///
/// * **absent** → the shipped default (an absent key is NOT "no limit");
/// * **`0`** → **no limit** (`None`);
/// * **`n > 0`** → `Some(n)`.
///
/// A value that does not fit the field's integer type falls back to the
/// default with a structured WARN: an operator typo must not stop the proxy
/// from starting. (`0` is meaningful now, so there is no other invalid
/// value for these three fields.)
fn resolve_advisor_limit<T>(value: Option<u64>, default: T, field: &str) -> Option<T>
where
    T: Copy + std::fmt::Display + TryFrom<u64>,
    T::Error: std::fmt::Display,
{
    match value {
        None => Some(default),
        Some(0) => None,
        Some(v) => match T::try_from(v) {
            Ok(n) => Some(n),
            Err(err) => {
                tracing::warn!(field, value = %v, error = %err, fallback = %default, "advisor limit out of range; using default");
                Some(default)
            }
        },
    }
}

/// JSON representation of `proxy.json`. Every field is optional; missing
/// fields fall through to env vars and then to defaults.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JsonConfig {
    pub listen_addr: Option<String>,
    pub upstream_base_url: Option<String>,
    pub upstream_api_key: Option<String>,
    pub upstream_path: Option<String>,
    pub request_timeout_secs: Option<u64>,
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
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JsonAdvisorConfig {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_uses: Option<u32>,
    pub max_tokens: Option<u32>,
    pub timeout_secs: Option<u64>,
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
#[derive(Debug, Default, Deserialize, Serialize)]
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
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
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
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JsonModelAliases {
    pub map: BTreeMap<String, String>,
    pub default_model: Option<String>,
}

/// JSON shape of `prompt_caching`.
#[derive(Debug, Default, Deserialize, Serialize)]
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
#[derive(Debug, Default, Deserialize, Serialize)]
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
mod tests {
    use super::*;

    fn env_with_required() -> EnvInputs {
        EnvInputs {
            upstream_base_url: Some("https://api.example.com".into()),
            upstream_api_key: Some("sk-test".into()),
            ..EnvInputs::default()
        }
    }

    #[test]
    fn url_must_parse() {
        let env = EnvInputs {
            upstream_base_url: Some("not a url".into()),
            upstream_api_key: Some("sk-test".into()),
            ..EnvInputs::default()
        };
        let result = Config::resolve(None, &env);
        assert!(result.is_err(), "expected URL parse error");
    }

    #[test]
    fn missing_required_env_yields_error() {
        let env = EnvInputs::default();
        let result = Config::resolve(None, &env);
        assert!(result.is_err());
    }

    #[test]
    fn defaults_apply_when_only_required_set() {
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(cfg.listen_addr.to_string(), DEFAULT_LISTEN_ADDR);
        assert_eq!(cfg.upstream_path, DEFAULT_UPSTREAM_PATH);
        assert_eq!(
            cfg.request_timeout,
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)
        );
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("none"));
    }

    #[test]
    fn default_reasoning_effort_is_none() {
        // Pins the default so a future refactor can't silently change
        // it; airia-backed reasoning models 400 without this.
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(
            cfg.reasoning_effort.as_deref(),
            Some(DEFAULT_REASONING_EFFORT)
        );
    }

    #[test]
    fn env_overrides_file_reasoning_effort() {
        let file = JsonConfig {
            reasoning_effort: Some("low".into()),
            ..JsonConfig::default()
        };
        let env = EnvInputs {
            reasoning_effort: Some("high".into()),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn per_model_reasoning_picks_exact_match() {
        // `reasoning.models` entry wins over the default and the
        // legacy reasoning_effort field.
        let file = JsonConfig {
            reasoning: Some(JsonReasoningConfig {
                default: Some("medium".into()),
                models: BTreeMap::from([
                    ("gpt-5.4-mini".into(), "high".into()),
                    ("gpt-5.6-luna".into(), "none".into()),
                ]),
                ..JsonReasoningConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(
            cfg.reasoning_for_model("gpt-5.4-mini").as_deref(),
            Some("high")
        );
        assert_eq!(
            cfg.reasoning_for_model("gpt-5.6-luna").as_deref(),
            Some("none")
        );
    }

    #[test]
    fn per_model_reasoning_falls_back_to_default() {
        // Model not in the map falls back to reasoning.default.
        let file = JsonConfig {
            reasoning: Some(JsonReasoningConfig {
                default: Some("low".into()),
                models: BTreeMap::from([("gpt-5.4-mini".into(), "high".into())]),
                ..JsonReasoningConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.reasoning_for_model("gpt-4o").as_deref(), Some("low"));
    }

    #[test]
    fn per_model_reasoning_falls_back_to_hardcoded_default() {
        // No `reasoning` object, no env, no legacy field — still gets
        // "none" so airia doesn't 400. This is the regression guard
        // for the bug that started this whole change.
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(
            cfg.reasoning_for_model("anything").as_deref(),
            Some(DEFAULT_REASONING_EFFORT)
        );
    }

    #[test]
    fn per_model_reasoning_legacy_field_used_when_no_table() {
        // Legacy `reasoning_effort` (file or env) still works as the
        // global default when no `reasoning` object is present.
        let file = JsonConfig {
            reasoning_effort: Some("medium".into()),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(
            cfg.reasoning_for_model("any-model").as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn model_alias_hit() {
        // The common case: Claude Code sends `claude-sonnet-5` (its
        // subagent model), the proxy rewrites it to `gpt-5.4-mini`
        // for the gateway.
        let file = JsonConfig {
            model_aliases: Some(JsonModelAliases {
                map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
                default_model: None,
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.upstream_model_for("claude-sonnet-5"), "gpt-5.4-mini");
    }

    #[test]
    fn model_alias_miss_passes_through() {
        // No alias for this model → name goes out unchanged. This is
        // the default behavior; aliases are opt-in.
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(cfg.upstream_model_for("gpt-5.6-luna"), "gpt-5.6-luna");
    }

    #[test]
    fn alias_and_reasoning_compose() {
        // The user aliases `claude-sonnet-5` → `gpt-5.4-mini`, and
        // also sets a per-model reasoning entry for `gpt-5.4-mini`.
        // The proxy should look up reasoning by the *resolved*
        // upstream name, not the inbound one — so a subagent request
        // for `claude-sonnet-5` lands on `gpt-5.4-mini` with
        // `high` reasoning, exactly as if the request had arrived
        // with `model: gpt-5.4-mini` directly.
        let file = JsonConfig {
            model_aliases: Some(JsonModelAliases {
                map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
                default_model: None,
            }),
            reasoning: Some(JsonReasoningConfig {
                default: Some("none".into()),
                models: BTreeMap::from([("gpt-5.4-mini".into(), "high".into())]),
                ..JsonReasoningConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        let resolved = cfg.upstream_model_for("claude-sonnet-5");
        assert_eq!(resolved, "gpt-5.4-mini");
        assert_eq!(cfg.reasoning_for_model(&resolved).as_deref(), Some("high"));
    }

    #[test]
    fn default_model_field_round_trips() {
        // The JSON `model_aliases.default_model` value should land on
        // Config::model_aliases.default_model verbatim. This is the
        // round-trip half of the contract: the field gets read.
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "model_aliases": {
                "default_model": "gpt-4o-mini"
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(
            cfg.model_aliases.default_model.as_deref(),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn default_model_method_returns_value() {
        // The Config::default_model() accessor must reflect whatever
        // landed on model_aliases.default_model. The proxy call site
        // uses this accessor; if it returns None when the field is
        // set, the fallback path will silently never run.
        let file = JsonConfig {
            model_aliases: Some(JsonModelAliases {
                map: BTreeMap::new(),
                default_model: Some("claude-haiku-4-5".into()),
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.default_model(), Some("claude-haiku-4-5"));
    }

    // ── Reasoning replay (BUG 1) ───────────────────────────────────────────

    #[test]
    fn reasoning_replay_defaults_to_text() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k"
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(cfg.reasoning_replay.default, ReasoningReplayMode::Text);
        // A model that is not listed gets the default.
        let for_model = cfg.reasoning_replay_for_model("some-unlisted-model");
        assert_eq!(for_model.mode, ReasoningReplayMode::Text);
        assert_eq!(for_model.max_chars, 0);
    }

    #[test]
    fn reasoning_replay_model_override_wins() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {
                "default": "off",
                "models": {"gpt-5.6-luna": "native"},
                "max_chars": 512
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(
            cfg.reasoning_replay_for_model("gpt-5.6-luna").mode,
            ReasoningReplayMode::Native
        );
        assert_eq!(
            cfg.reasoning_replay_for_model("other-model").mode,
            ReasoningReplayMode::Off
        );
        assert_eq!(cfg.reasoning_replay_for_model("other-model").max_chars, 512);
    }

    #[test]
    fn reasoning_replay_off_is_honoured() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"default": "off"}
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(
            cfg.reasoning_replay_for_model("any-model").mode,
            ReasoningReplayMode::Off
        );
    }

    #[test]
    fn reasoning_replay_rejects_an_unknown_mode() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"default": "nonsense"}
        }"#;
        assert!(
            JsonConfig::parse(json_raw).is_err(),
            "expected an unknown reasoning_replay mode to be rejected"
        );
    }

    #[test]
    fn reasoning_replay_rejects_an_unknown_key() {
        // `JsonConfig` is `deny_unknown_fields`, so a typo must not be
        // silently ignored.
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key": "k",
            "reasoning_replay": {"defualt": "text"}
        }"#;
        assert!(JsonConfig::parse(json_raw).is_err());
    }

    #[test]
    fn the_saved_config_does_not_carry_a_key_the_previous_binary_would_reject() {
        // The TUI rewrites `proxy.json` from `JsonConfig`, and
        // `JsonConfig` is `deny_unknown_fields`. If the save path wrote a
        // `reasoning_replay: null` key, the PREVIOUS binary (which does
        // not know the key) would refuse to start — a rollback hazard for
        // anyone who saved from the TUI.
        let saved = serde_json::to_string_pretty(&JsonConfig::default()).unwrap();
        assert!(
            !saved.contains("reasoning_replay"),
            "the saved config carries a key the previous binary rejects: {saved}"
        );
        // ...but an explicitly-configured replay policy must survive the
        // round-trip.
        let parsed = JsonConfig::parse(
            r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
                "reasoning_replay":{"default":"native","models":{"m":"off"},"max_chars":42}}"#,
        )
        .expect("parse json");
        let saved = serde_json::to_string_pretty(&parsed).unwrap();
        assert!(saved.contains("reasoning_replay"), "{saved}");
        let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
        assert_eq!(
            reparsed.reasoning_replay.as_ref().and_then(|r| r.default),
            Some(ReasoningReplayMode::Native)
        );
        assert_eq!(
            reparsed.reasoning_replay.as_ref().and_then(|r| r.max_chars),
            Some(42)
        );
    }

    // ── advisor config ────────────────────────────────────────────────

    #[test]
    fn advisor_is_off_by_default_with_cheap_model() {
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(cfg.advisor, AdvisorConfig::default());
        assert!(!cfg.advisor.enabled, "advisor must be opt-in");
        assert_eq!(cfg.advisor.model, "beam", "default advisor must be cheap");
        assert_eq!(cfg.advisor.effort, None);
        assert_eq!(cfg.advisor.max_uses, Some(2));
        assert_eq!(cfg.advisor.max_tokens, Some(2048));
        assert_eq!(cfg.advisor.timeout_secs, Some(45));
    }

    #[test]
    fn advisor_json_section_parses_and_resolves() {
        // The exact shape from §17.2 must parse despite JsonConfig's
        // deny_unknown_fields, and every value must reach the resolved
        // config.
        let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "model": "glm-5.3-flash",
                "effort": "medium",
                "max_uses": 3,
                "max_tokens": 1024,
                "timeout_secs": 20
            }
        }"#;
        let file = JsonConfig::parse(raw).expect("proxy.json with advisor must parse");
        assert!(file.advisor.is_some());
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(
            cfg.advisor,
            AdvisorConfig {
                enabled: true,
                model: "glm-5.3-flash".into(),
                effort: Some("medium".into()),
                max_uses: Some(3),
                max_tokens: Some(1024),
                timeout_secs: Some(20),
            }
        );
    }

    #[test]
    fn advisor_env_overrides_file_then_default() {
        let file = JsonConfig {
            advisor: Some(JsonAdvisorConfig {
                enabled: Some(true),
                model: Some("file-model".into()),
                effort: Some("low".into()),
                max_uses: Some(5),
                max_tokens: Some(999),
                timeout_secs: Some(30),
            }),
            ..JsonConfig::default()
        };
        let env = EnvInputs {
            advisor_enabled: Some(false),
            advisor_model: Some("env-model".into()),
            advisor_effort: Some("high".into()),
            advisor_max_uses: Some(1),
            advisor_max_tokens: Some(77),
            advisor_timeout_secs: Some(9),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(
            cfg.advisor,
            AdvisorConfig {
                enabled: false,
                model: "env-model".into(),
                effort: Some("high".into()),
                max_uses: Some(1),
                max_tokens: Some(77),
                timeout_secs: Some(9),
            }
        );

        // With no env override, the file wins; with neither, the defaults.
        let from_file = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(from_file.advisor.model, "file-model");
        assert_eq!(from_file.advisor.max_uses, Some(5));
        let defaults = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(defaults.advisor, AdvisorConfig::default());
    }

    #[test]
    fn advisor_zero_limits_mean_no_limit() {
        // The operator's rule: `0` means "no limit" (the house precedent of
        // `reasoning_replay.max_chars`), and an ABSENT key means the
        // shipped default. The two are NOT the same thing.
        let file = JsonConfig {
            advisor: Some(JsonAdvisorConfig {
                enabled: Some(true),
                max_uses: Some(0),
                max_tokens: Some(0),
                timeout_secs: Some(0),
                ..JsonAdvisorConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert!(cfg.advisor.enabled, "the section itself stays enabled");
        assert_eq!(cfg.advisor.max_uses, None, "0 = no limit");
        assert_eq!(cfg.advisor.max_tokens, None, "0 = no limit");
        assert_eq!(cfg.advisor.timeout_secs, None, "0 = no limit");

        // An ABSENT key is the default, NOT "no limit".
        let absent = JsonConfig {
            advisor: Some(JsonAdvisorConfig {
                enabled: Some(true),
                ..JsonAdvisorConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&absent), &env_with_required()).unwrap();
        assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
        assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
        assert_eq!(cfg.advisor.timeout_secs, Some(DEFAULT_ADVISOR_TIMEOUT_SECS));

        // An explicit `n > 0` is that bound.
        let explicit = JsonConfig {
            advisor: Some(JsonAdvisorConfig {
                enabled: Some(true),
                max_uses: Some(3),
                max_tokens: Some(1024),
                timeout_secs: Some(20),
                ..JsonAdvisorConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&explicit), &env_with_required()).unwrap();
        assert_eq!(cfg.advisor.max_uses, Some(3));
        assert_eq!(cfg.advisor.max_tokens, Some(1024));
        assert_eq!(cfg.advisor.timeout_secs, Some(20));
    }

    #[test]
    fn advisor_zero_limits_from_the_env_mean_no_limit() {
        // The env override follows the same rule as the file: `0` = no
        // limit, absent = the default.
        let env = EnvInputs {
            advisor_max_uses: Some(0),
            advisor_max_tokens: Some(0),
            advisor_timeout_secs: Some(0),
            ..env_with_required()
        };
        let cfg = Config::resolve(None, &env).unwrap();
        assert_eq!(cfg.advisor.max_uses, None);
        assert_eq!(cfg.advisor.max_tokens, None);
        assert_eq!(cfg.advisor.timeout_secs, None);

        // An env `0` also beats a file value: env > file.
        let file = JsonConfig {
            advisor: Some(JsonAdvisorConfig {
                enabled: Some(true),
                max_uses: Some(5),
                max_tokens: Some(999),
                timeout_secs: Some(30),
                ..JsonAdvisorConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(cfg.advisor.max_uses, None, "env 0 wins over the file");
        assert_eq!(cfg.advisor.max_tokens, None);
        assert_eq!(cfg.advisor.timeout_secs, None);
    }

    #[test]
    fn advisor_out_of_range_limit_falls_back_to_the_default() {
        // A value that does not fit `u32` is not silently dropped: it falls
        // back to the default (the house style — a bad value must not stop
        // the proxy from starting).
        let env = EnvInputs {
            advisor_max_uses: Some(u64::from(u32::MAX) + 1),
            advisor_max_tokens: Some(u64::from(u32::MAX) + 1),
            ..env_with_required()
        };
        let cfg = Config::resolve(None, &env).unwrap();
        assert_eq!(cfg.advisor.max_uses, Some(DEFAULT_ADVISOR_MAX_USES));
        assert_eq!(cfg.advisor.max_tokens, Some(DEFAULT_ADVISOR_MAX_TOKENS));
    }

    #[test]
    fn a_real_proxy_json_with_and_without_the_advisor_section_loads() {
        // Goes through the real `Config::load_from` entry point (fs read +
        // JsonConfig::parse + resolve), not just the resolver. Proves the
        // gate: an operator can drop the new section into proxy.json and the
        // binary still starts; and a legacy file without it is unchanged.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("phase2-advisor-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxy.json");

        let legacy = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k"
        }"#;
        std::fs::write(&path, legacy).unwrap();
        let cfg = Config::load_from(&path).expect("legacy proxy.json must load");
        assert_eq!(cfg.advisor, AdvisorConfig::default());
        assert!(!cfg.advisor.enabled);

        let with_advisor = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k",
            "advisor": {
                "enabled": true,
                "model": "beam",
                "effort": "medium",
                "max_uses": 2,
                "max_tokens": 2048,
                "timeout_secs": 45
            }
        }"#;
        std::fs::write(&path, with_advisor).unwrap();
        let cfg = Config::load_from(&path).expect("proxy.json WITH the advisor section must load");
        assert_eq!(
            cfg.advisor,
            AdvisorConfig {
                enabled: true,
                model: "beam".into(),
                effort: Some("medium".into()),
                max_uses: Some(2),
                max_tokens: Some(2048),
                timeout_secs: Some(45),
            }
        );
        // Every other resolved setting is identical to the legacy file.
        assert_eq!(cfg.upstream_base_url, "https://api.example.com");
        assert_eq!(cfg.upstream_path, DEFAULT_UPSTREAM_PATH);
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("none"));

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn advisor_unknown_json_key_is_rejected() {
        // deny_unknown_fields on the sub-object too: a typo fails fast.
        let raw = r#"{"advisor": {"enabled": true, "max_use": 1}}"#;
        assert!(JsonConfig::parse(raw).is_err());
    }

    #[test]
    fn advisor_absent_key_is_rollback_safe_and_behaves_as_before() {
        // (1) A saved config with `advisor: None` must NOT emit the key —
        // the TUI rewrites proxy.json wholesale and the previous binary's
        // deny_unknown_fields would refuse to start.
        let saved = serde_json::to_string_pretty(&JsonConfig::default()).unwrap();
        assert!(
            !saved.contains("advisor"),
            "the saved config carries a key the previous binary rejects: {saved}"
        );
        // (2) A file with NO advisor key must resolve to exactly the same
        // AdvisorConfig as no file at all — zero behaviour change.
        let raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "k"
        }"#;
        let file = JsonConfig::parse(raw).expect("parse");
        assert!(file.advisor.is_none());
        let env = env_with_required();
        let absent = Config::resolve(Some(&file), &env).unwrap();
        let no_file = Config::resolve(None, &env).unwrap();
        assert_eq!(absent.advisor, no_file.advisor);
        assert!(!absent.advisor.enabled);
    }

    #[test]
    fn advisor_explicit_section_survives_a_save_round_trip() {
        let parsed = JsonConfig::parse(
            r#"{"upstream_base_url":"https://api.example.com","upstream_api_key":"k",
                "advisor":{"enabled":true,"model":"beam","max_uses":2}}"#,
        )
        .expect("parse json");
        let saved = serde_json::to_string_pretty(&parsed).unwrap();
        assert!(saved.contains("advisor"), "{saved}");
        let reparsed = JsonConfig::parse(&saved).expect("the saved config must re-parse");
        assert_eq!(
            reparsed.advisor.as_ref().and_then(|a| a.enabled),
            Some(true)
        );
        assert_eq!(
            reparsed.advisor.as_ref().and_then(|a| a.model.as_deref()),
            Some("beam")
        );
    }

    #[test]
    fn advisor_model_is_not_an_inbound_alias() {
        // The advisor model must be usable verbatim even when the alias map
        // would rewrite the same string on an ordinary client request.
        let file = JsonConfig {
            model_aliases: Some(JsonModelAliases {
                map: BTreeMap::from([("beam".into(), "gpt-5.6-luna".into())]),
                ..JsonModelAliases::default()
            }),
            advisor: Some(JsonAdvisorConfig {
                model: Some("beam".into()),
                ..JsonAdvisorConfig::default()
            }),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.advisor.model, "beam");
        assert_eq!(
            cfg.upstream_model_for("beam"),
            "gpt-5.6-luna",
            "the alias map still applies to inbound names"
        );
    }

    #[test]
    fn the_shipped_example_config_parses() {
        // `proxy.json.example` is the template operators copy. `JsonConfig`
        // is `deny_unknown_fields`, so a key the example uses but the schema
        // does not declare would make the example unusable.
        let raw =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/proxy.json.example"))
                .expect("read proxy.json.example");
        let parsed = JsonConfig::parse(&raw).expect("proxy.json.example should parse");
        // The example deliberately does NOT set `reasoning_replay`: the
        // default is already the safe mode, and `JsonConfig` is
        // `deny_unknown_fields`, so a key in the example would make the
        // PREVIOUS binary refuse to start — a rollback hazard for anyone
        // who copied it.
        assert!(parsed.reasoning_replay.is_none());
    }

    #[test]
    fn prompt_caching_defaults_to_disabled() {
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert!(cfg.prompt_caching.models.is_empty());
        assert!(cfg.prompt_caching.cache_key.is_none());
    }

    #[test]
    fn prompt_caching_json_sets_models_and_key() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna", "gpt-5.6-terra"],
                "cache_key": "my-app"
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(cfg.prompt_caching.models.len(), 2);
        assert!(
            cfg.prompt_caching
                .models
                .contains(&"gpt-5.6-luna".to_string())
        );
        assert!(
            cfg.prompt_caching
                .models
                .contains(&"gpt-5.6-terra".to_string())
        );
        assert_eq!(cfg.prompt_caching.cache_key.as_deref(), Some("my-app"));
    }

    #[test]
    fn prompt_caching_for_model_filters_by_model() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna", "gpt-5.6-terra"],
                "cache_key": "my-app"
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        // Model in the list gets caching
        let for_luna = cfg.prompt_caching_for_model("gpt-5.6-luna");
        assert!(!for_luna.models.is_empty());
        assert_eq!(for_luna.cache_key.as_deref(), Some("my-app"));
        // Model NOT in the list gets no caching
        let for_other = cfg.prompt_caching_for_model("gpt-5.4-mini");
        assert!(for_other.models.is_empty());
        assert_eq!(for_other.cache_key.as_deref(), Some("my-app"));
    }

    #[test]
    fn prompt_caching_env_overrides_json() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "prompt_caching": {
                "models": ["gpt-5.6-luna"],
                "cache_key": "from-file"
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let env = EnvInputs {
            prompt_caching_models: Some("".into()),
            prompt_cache_key: Some("from-env".into()),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&parsed), &env).unwrap();
        assert!(cfg.prompt_caching.models.is_empty());
        assert_eq!(cfg.prompt_caching.cache_key.as_deref(), Some("from-env"));
    }

    #[test]
    fn default_model_unset_returns_none() {
        // No `model_aliases` object at all → accessor returns None,
        // the proxy surfaces upstream errors unchanged.
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert_eq!(cfg.default_model(), None);
    }

    #[test]
    fn reasoning_object_with_only_default_parses() {
        // A `reasoning` object containing just `default` must be
        // accepted; the `models` sub-object is optional. Without
        // `#[serde(default)]` on `JsonReasoningConfig.models`, this
        // shape would fail to deserialize and the proxy would refuse
        // to start.
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {
                "default": "none"
            }
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(cfg.reasoning.default.as_deref(), Some("none"));
        assert!(cfg.reasoning.models.is_empty());
        assert_eq!(
            cfg.reasoning_for_model("any-model").as_deref(),
            Some("none")
        );
    }

    #[test]
    fn env_overrides_file() {
        let file = JsonConfig {
            listen_addr: Some("0.0.0.0:9999".into()),
            ..JsonConfig::default()
        };
        let env = EnvInputs {
            listen_addr: Some("0.0.0.0:1234".into()),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:1234");
    }

    #[test]
    fn file_fills_in_when_env_unset() {
        let file = JsonConfig {
            listen_addr: Some("0.0.0.0:9999".into()),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:9999");
    }

    #[test]
    fn log_to_disk_defaults_to_false() {
        // Default is off so a fresh install never silently writes a
        // log file (PII concern: request bodies used to be logged at
        // WARN on every upstream error).
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert!(!cfg.log_to_disk);
    }

    #[test]
    fn log_to_disk_env_true_enables() {
        // `LOG_TO_DISK=1` is the documented "on" trigger.
        let env = EnvInputs {
            log_to_disk: Some(true),
            ..env_with_required()
        };
        let cfg = Config::resolve(None, &env).unwrap();
        assert!(cfg.log_to_disk);
    }

    #[test]
    fn log_to_disk_env_truthy_values_enable() {
        // All of these should turn it on.
        for v in ["1", "true", "yes", "on", "TRUE", "Yes", " on "] {
            let env = EnvInputs {
                log_to_disk: Some(true), // already parsed by capture
                ..env_with_required()
            };
            // Re-parse the value through the same logic capture() uses.
            let parsed = {
                let v = v.to_ascii_lowercase();
                let v = v.trim();
                matches!(v, "1" | "true" | "yes" | "on")
            };
            assert!(parsed, "expected {v:?} to enable log_to_disk");
            // Sanity: the resolved config respects the env when set.
            let cfg = Config::resolve(None, &env).unwrap();
            assert!(cfg.log_to_disk, "value {v:?} should be on");
        }
    }

    #[test]
    fn log_to_disk_env_false_disables() {
        // Anything other than the truthy set disables. A typo in the
        // env var name should NOT turn logging on.
        let env = EnvInputs {
            log_to_disk: Some(false),
            ..env_with_required()
        };
        let cfg = Config::resolve(None, &env).unwrap();
        assert!(!cfg.log_to_disk);
    }

    #[test]
    fn log_to_disk_json_fills_in_when_env_unset() {
        // JSON fallback: if env is unset, the file's value applies.
        let file = JsonConfig {
            log_to_disk: Some(true),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert!(cfg.log_to_disk);
    }

    #[test]
    fn log_to_disk_env_overrides_json() {
        // Env wins over file (consistent with every other field).
        let file = JsonConfig {
            log_to_disk: Some(true),
            ..JsonConfig::default()
        };
        let env = EnvInputs {
            log_to_disk: Some(false),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert!(!cfg.log_to_disk);
    }

    #[test]
    fn proxy_key_defaults_to_none() {
        // No key set → `None` → no client auth. The startup warning
        // is emitted by main.rs, not by the config layer.
        let cfg = Config::resolve(None, &env_with_required()).unwrap();
        assert!(cfg.proxy_key.is_none());
    }

    #[test]
    fn proxy_key_json_round_trips() {
        let json_raw = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "proxy_key": "shared-secret-1234"
        }"#;
        let parsed = JsonConfig::parse(json_raw).expect("parse json");
        let cfg = Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap();
        assert_eq!(cfg.proxy_key.as_deref(), Some("shared-secret-1234"));
    }

    #[test]
    fn proxy_key_env_overrides_json() {
        let file = JsonConfig {
            proxy_key: Some("from-file".into()),
            ..JsonConfig::default()
        };
        let env = EnvInputs {
            proxy_key: Some("from-env".into()),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(cfg.proxy_key.as_deref(), Some("from-env"));
    }

    #[test]
    fn blank_proxy_key_values_disable_auth() {
        let file = JsonConfig {
            proxy_key: Some("   ".into()),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert!(cfg.proxy_key.is_none());

        let env = EnvInputs {
            proxy_key: Some(String::new()),
            ..env_with_required()
        };
        let file = JsonConfig {
            proxy_key: Some("from-file".into()),
            ..JsonConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert!(cfg.proxy_key.is_none());
    }

    // ── reasoning_for_request (effort_map / thinking_disabled) ──────

    fn cfg_from_json(json: &str) -> Config {
        let parsed = JsonConfig::parse(json).expect("parse json");
        Config::resolve(Some(&parsed), &EnvInputs::default()).unwrap()
    }

    const REASONING_JSON: &str = r#"{
        "upstream_base_url": "https://api.example.com",
        "upstream_api_key":  "sk-test",
        "reasoning": {
            "default": "high",
            "effort_map": {
                "default": {
                    "low": "none",
                    "medium": "low",
                    "high": "medium",
                    "xhigh": "high",
                    "max": "high"
                },
                "models": {
                    "gpt-5.6-luna": {
                        "low": "low",
                        "medium": "medium",
                        "high": "high",
                        "xhigh": "xhigh",
                        "max": "max"
                    }
                }
            },
            "thinking_disabled": {
                "default": {
                    "disabled": "none"
                }
            }
        }
    }"#;

    #[test]
    fn effort_map_default_entry_translates() {
        let cfg = cfg_from_json(REASONING_JSON);
        // gpt-5.4-mini has no per-model entry → uses effort_map.default.
        let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("high"), false);
        assert_eq!(d, ReasoningDecision::Effort("medium".into()));
        let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("low"), false);
        assert_eq!(d, ReasoningDecision::Effort("none".into()));
    }

    #[test]
    fn effort_map_model_entry_overrides_default() {
        let cfg = cfg_from_json(REASONING_JSON);
        // gpt-5.6-luna has a per-model entry that overrides default.
        let d = cfg.reasoning_for_request("gpt-5.6-luna", Some("high"), false);
        assert_eq!(d, ReasoningDecision::Effort("high".into()));
        let d = cfg.reasoning_for_request("gpt-5.6-luna", Some("xhigh"), false);
        assert_eq!(d, ReasoningDecision::Effort("xhigh".into()));
    }

    #[test]
    fn effort_map_null_omits_reasoning() {
        let json = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {
                "default": "high",
                "effort_map": {
                    "default": {"high": null}
                }
            }
        }"#;
        let cfg = cfg_from_json(json);
        let d = cfg.reasoning_for_request("any-model", Some("high"), false);
        assert_eq!(d, ReasoningDecision::Omit);
        assert_eq!(d.upstream_effort(), None);
    }

    #[test]
    fn effort_map_identity_fallback_for_known_values() {
        let cfg = cfg_from_json(REASONING_JSON);
        // A requested value with no mapping and no per-model entry falls
        // back to identity when it's a known upstream value. "minimal" is
        // not in the default map, so it hits the identity path.
        let d = cfg.reasoning_for_request("some-other-model", Some("minimal"), false);
        assert_eq!(d, ReasoningDecision::Effort("minimal".into()));
    }

    #[test]
    fn thinking_disabled_takes_precedence_over_effort() {
        let cfg = cfg_from_json(REASONING_JSON);
        // Even when an effort is requested, explicit disabled thinking wins.
        let d = cfg.reasoning_for_request("gpt-5.4-mini", Some("max"), true);
        assert_eq!(d, ReasoningDecision::Disabled("none".into()));
    }

    #[test]
    fn no_client_signal_uses_fixed_default() {
        let cfg = cfg_from_json(REASONING_JSON);
        let d = cfg.reasoning_for_request("gpt-5.4-mini", None, false);
        assert_eq!(d, ReasoningDecision::Effort("high".into()));
    }

    #[test]
    fn thinking_disabled_with_no_config_falls_through() {
        // If no thinking_disabled mapping is configured, a disabled
        // request still falls through to fixed resolution rather than
        // inventing a value.
        let json = r#"{
            "upstream_base_url": "https://api.example.com",
            "upstream_api_key":  "sk-test",
            "reasoning": {"default": "medium"}
        }"#;
        let cfg = cfg_from_json(json);
        let d = cfg.reasoning_for_request("any-model", None, true);
        assert_eq!(d, ReasoningDecision::Effort("medium".into()));
    }

    #[test]
    fn normalize_effort_lowercases_and_trims() {
        assert_eq!(
            Config::normalize_effort(Some("  HIGH ")),
            Some("high".into())
        );
        assert_eq!(Config::normalize_effort(Some("")), None);
        assert_eq!(Config::normalize_effort(None), None);
    }
}
