//! Configuration loading.
//!
//! Resolution order (highest priority first):
//!   1. Environment variables (`LISTEN_ADDR`, `UPSTREAM_BASE_URL`, ...).
//!   2. `proxy.toml` in the working directory, if present.
//!   3. Built-in defaults.
//!
//! Environment variables always win over the TOML file. This lets a deployment
//! override individual values without editing the config file.

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:8085";
const DEFAULT_UPSTREAM_PATH: &str = "/v1/chat/completions";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;
/// Default `reasoning_effort` for upstream chat-completions requests.
/// Some upstreams (notably airia-backed reasoning models) reject
/// function tools when `reasoning_effort` is unset — they default to a
/// non-`"none"` value, and the resulting combination is unsupported.
/// Pinning the default to `"none"` keeps tool-use requests working out
/// of the box; operators who need reasoning for non-tool calls can
/// override via env or TOML.
const DEFAULT_REASONING_EFFORT: &str = "none";

/// Resolved proxy configuration. Cheap to clone (`String`s and a `Duration`).
#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub upstream_base_url: String,
    pub upstream_api_key: String,
    pub upstream_path: String,
    pub request_timeout: Duration,
    /// Outbound `reasoning_effort` for chat-completions requests. See
    /// [`DEFAULT_REASONING_EFFORT`] for why this exists.
    pub reasoning_effort: Option<String>,
}

impl Config {
    /// Load configuration from the environment and optional `proxy.toml`.
    pub fn load() -> Result<Self> {
        Self::load_from(Path::new("proxy.toml"))
    }

    /// Load configuration, looking for the TOML file at `toml_path`.
    ///
    /// Exposed for tests; production code should call [`Config::load`].
    pub fn load_from(toml_path: &Path) -> Result<Self> {
        let from_file = if toml_path.exists() {
            let raw = fs::read_to_string(toml_path)
                .with_context(|| format!("read config file at {}", toml_path.display()))?;
            Some(TomlConfig::parse(&raw).with_context(|| "parse proxy.toml")?)
        } else {
            None
        };

        let env_inputs = EnvInputs::capture();
        Self::resolve(from_file.as_ref(), &env_inputs)
    }

    /// Resolve from explicit inputs. `pub(crate)` so integration tests in
    /// `tests/` can't reach in and bypass the env-loading entry point, but
    /// unit tests inside this module can.
    pub(crate) fn resolve(file: Option<&TomlConfig>, env: &EnvInputs) -> Result<Self> {
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
        .context("UPSTREAM_BASE_URL is required (set env var or set in proxy.toml)")?;

        // Validate that the URL parses. We don't keep the parsed form because
        // reqwest will re-parse it at request time, and storing both is noise.
        url::Url::parse(&upstream_base_url).context("UPSTREAM_BASE_URL is not a valid URL")?;

        let upstream_api_key = pick_str(
            file.and_then(|f| f.upstream_api_key.as_deref()),
            env.upstream_api_key.as_deref(),
        )
        .context("UPSTREAM_API_KEY is required (set env var or set in proxy.toml)")?;

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
        // `reasoning_effort` TOML key) to override.
        let reasoning_effort = pick_str(
            file.and_then(|f| f.reasoning_effort.as_deref()),
            env.reasoning_effort.as_deref(),
        )
        .or_else(|| Some(DEFAULT_REASONING_EFFORT.to_owned()));

        Ok(Self {
            listen_addr,
            upstream_base_url,
            upstream_api_key,
            upstream_path,
            request_timeout: Duration::from_secs(request_timeout_secs),
            reasoning_effort,
        })
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
        }
    }
}

fn pick_str(file_value: Option<&str>, env_value: Option<&str>) -> Option<String> {
    env_value.or(file_value).map(str::to_owned)
}

fn pick_u64(file_value: Option<u64>, env_value: Option<u64>) -> Option<u64> {
    env_value.or(file_value)
}

/// TOML representation of `proxy.toml`. Every field is optional; missing
/// fields fall through to env vars and then to defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TomlConfig {
    listen_addr: Option<String>,
    upstream_base_url: Option<String>,
    upstream_api_key: Option<String>,
    upstream_path: Option<String>,
    request_timeout_secs: Option<u64>,
    reasoning_effort: Option<String>,
}

impl TomlConfig {
    fn parse(raw: &str) -> Result<Self> {
        toml::from_str(raw).context("invalid TOML")
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
        let file = TomlConfig {
            reasoning_effort: Some("low".into()),
            ..TomlConfig::default()
        };
        let env = EnvInputs {
            reasoning_effort: Some("high".into()),
            ..env_with_required()
        };
        let cfg = Config::resolve(Some(&file), &env).unwrap();
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn env_overrides_file() {
        let file = TomlConfig {
            listen_addr: Some("0.0.0.0:9999".into()),
            ..TomlConfig::default()
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
        let file = TomlConfig {
            listen_addr: Some("0.0.0.0:9999".into()),
            ..TomlConfig::default()
        };
        let cfg = Config::resolve(Some(&file), &env_with_required()).unwrap();
        assert_eq!(cfg.listen_addr.to_string(), "0.0.0.0:9999");
    }
}
