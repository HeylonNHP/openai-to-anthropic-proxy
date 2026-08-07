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
/// aliased or passed-through model with a "model not supported"
/// error, the proxy retries the request once with this model. Every
/// fallback is logged at WARN. `None` disables the safety net.
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
            models: reasoning_json
                .map(|r| r.models.clone())
                .unwrap_or_default(),
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
            proxy_key,
            log_to_disk,
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
        if norm.is_empty() {
            None
        } else {
            Some(norm)
        }
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
            log_to_disk: env::var("LOG_TO_DISK").ok().map(|s| {
                let v = s.trim().to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            }),
            proxy_key: env::var("PROXY_KEY").ok(),
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
        EffortMap {
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
        assert_eq!(Config::normalize_effort(Some("  HIGH ")), Some("high".into()));
        assert_eq!(Config::normalize_effort(Some("")), None);
        assert_eq!(Config::normalize_effort(None), None);
    }
}
