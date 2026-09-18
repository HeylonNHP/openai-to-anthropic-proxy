//! Process-lifetime registry of *learned* upstream capabilities.
//!
//! Some upstream models reject request parameters that others accept --
//! most visibly `temperature` and `top_p` on the newer always-reasoning
//! models. Rather than hard-coding a table of which model supports what
//! (the static, hand-maintained capability map LiteLLM's `drop_params`
//! needs), the proxy *learns* the capability from the upstream's own
//! rejection:
//!
//! 1. Send the request optimistically, carrying every parameter the
//!    client asked for.
//! 2. If the upstream answers 400 naming one of those parameters as
//!    unsupported, drop *that* parameter and retry once -- before any
//!    response bytes reach the client, so the client sees a single
//!    clean attempt.
//! 3. Remember the fact for the rest of the process, so later requests
//!    for the same model never send the doomed parameter again.
//!
//! This is the classic *EAFP* ("easier to ask forgiveness than
//! permission") shape: assume the rich request is fine and react to the
//! specific rejection, instead of predicting support up front. The
//! remembered failure is a *negative cache* (cf. RFC 2308, which caches
//! the knowledge that a DNS name does not exist rather than only
//! successful lookups): we cache the knowledge that something is
//! *un* supported, not the happy path.
//!
//! # Why a latch, not a circuit breaker
//!
//! A circuit breaker (Nygard, *Release It!*) is the wrong metaphor here.
//! Its OPEN/HALF-OPEN/CLOSED states exist to ride out *transient* faults
//! that may heal on their own. "Model X rejects parameter Y" is not a
//! transient fault, it is a durable capability fact, so the store is a
//! deliberate one-way latch: once learned, it is never re-probed within
//! the process. Each fresh process starts empty and therefore re-probes
//! exactly once per (model, parameter) pair -- a natural half-open that
//! costs at most one rejected request per model per deploy and cannot
//! permanently mask a capability the upstream has since regained.

use arc_swap::ArcSwap;
use axum::http::StatusCode;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::responses::{ResponsesError, ResponsesRequest};

/// A request parameter the proxy may learn to drop.
///
/// Adding a variant (plus its entry in [`RequestParam::ALL`] and its
/// arms in the three small `match`es below) is the only change needed
/// to teach the proxy about another optional parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RequestParam {
    /// `temperature` -- sampling randomness.
    Temperature,
    /// `top_p` -- nucleus sampling.
    TopP,
}

impl RequestParam {
    /// Every parameter the proxy knows how to drop.
    pub const ALL: [Self; 2] = [Self::Temperature, Self::TopP];

    /// Canonical wire name, as it appears in an upstream rejection.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
        }
    }

    /// Alternate spellings upstreams use for the same parameter. A
    /// rejection may echo the name in any of these forms, so the
    /// matcher accepts them all rather than only the canonical one.
    fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Temperature => &["temperature"],
            Self::TopP => &["top_p", "topp", "top-p"],
        }
    }

    /// Is this parameter currently set on an outbound request?
    pub fn is_set(self, req: &ResponsesRequest) -> bool {
        match self {
            Self::Temperature => req.temperature.is_some(),
            Self::TopP => req.top_p.is_some(),
        }
    }

    /// Remove this parameter from an outbound request.
    pub fn clear_from(self, req: &mut ResponsesRequest) {
        match self {
            Self::Temperature => req.temperature = None,
            Self::TopP => req.top_p = None,
        }
    }
}

/// Phrases that signal a parameter is *not supported* (as opposed to
/// present but invalid). Kept narrow on purpose -- see
/// [`unsupported_parameter`].
const SIGNAL_PHRASES: &[&str] = &[
    "unsupported",
    "not supported",
    "does not support",
    "doesn't support",
    "do not support",
    "unknown parameter",
    "unrecognized",
    "unexpected parameter",
    "not permitted",
    "not allowed",
    "extra inputs",
    "additional properties",
];

/// Detect a response that rejects one of our optional request parameters.
///
/// Conservative by design. A *false negative* merely means the caller
/// sees the upstream's original error -- no worse than before this
/// feature existed. A *false positive*, by contrast, would silently
/// strip a parameter the model actually supports, which is exactly the
/// silent-degradation failure OpenRouter's `require_parameters` guard
/// exists to prevent. We therefore require two independent signals
/// before acting:
///
/// 1. an "unsupported"-flavoured signal (a known code, or one of
///    [`SIGNAL_PHRASES`] anywhere in the body), and
/// 2. an explicit mention of one of our parameters, in the structured
///    `param` field or the message text.
///
/// The split matters: an invalid *value* names the parameter yet is not
/// a support problem, e.g. `"temperature must be between 0 and 2"`.
/// That carries no unsupported-flavoured phrase, so it does not match
/// and the rich request is left alone.
pub fn unsupported_parameter(status: StatusCode, body: &str) -> Option<RequestParam> {
    // Parameter-support rejections arrive as 400. 422 is accepted as a
    // belt-and-braces allowance for validators that use it.
    if !matches!(status.as_u16(), 400 | 422) {
        return None;
    }

    let envelope = ErrorEnvelope::parse(body);
    if !envelope.is_unsupported_signal() {
        return None;
    }

    RequestParam::ALL
        .into_iter()
        .find(|param| envelope.names(*param))
}

/// The handful of error-envelope fields we care about, flattened from
/// either wire shape: the Responses API's flat body
/// (`{ "code": .., "message": .., "param": .. }`) or the Chat
/// Completions wrapping `{ "error": { .. } }`.
#[derive(Default)]
struct ErrorEnvelope {
    code: String,
    param: String,
    message: String,
}

impl ErrorEnvelope {
    /// Parse whichever envelope shape the body uses. Falls back to
    /// scanning the raw body when it is not JSON at all (some
    /// gateways return plain text or HTML on a 400).
    fn parse(body: &str) -> Self {
        if let Ok(err) = serde_json::from_str::<ResponsesError>(body) {
            return Self {
                code: value_to_text(err.code.as_ref()),
                param: value_to_text(err.param.as_ref()),
                message: err.message,
            };
        }

        if let Ok(v) = serde_json::from_str::<Value>(body)
            && let Some(err) = v.get("error")
        {
            return Self {
                code: value_to_text(err.get("code")),
                param: value_to_text(err.get("param")),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            };
        }

        Self {
            message: body.to_owned(),
            ..Self::default()
        }
    }

    /// Everything a phrase might hide in, lowercased once.
    fn haystack(&self) -> String {
        format!("{} {} {}", self.code, self.param, self.message).to_ascii_lowercase()
    }

    /// Does the body carry an "unsupported"-flavoured signal?
    fn is_unsupported_signal(&self) -> bool {
        let code = self.code.to_ascii_lowercase();
        if code.contains("unsupported") || code.contains("unknown") || code.contains("unrecognized")
        {
            return true;
        }
        let hay = self.haystack();
        SIGNAL_PHRASES.iter().any(|phrase| hay.contains(phrase))
    }

    /// Does the body name this specific parameter?
    fn names(&self, param: RequestParam) -> bool {
        let structured = self.param.to_ascii_lowercase();
        if !structured.is_empty() && param.aliases().contains(&structured.as_str()) {
            return true;
        }
        let hay = self.haystack();
        param.aliases().iter().any(|alias| hay.contains(alias))
    }
}

/// Stringify a structured error field that may be a string, an array of
/// strings, or absent.
fn value_to_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Immutable snapshot of learned capabilities. Cheap to share behind an
/// [`Arc`]; readers never block writers and vice versa.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    /// Upstream model name -> parameters it has rejected.
    unsupported: BTreeMap<String, Vec<RequestParam>>,
}

impl CapabilityRegistry {
    /// Parameters known to be rejected by `model` (empty if none).
    pub fn unsupported_for(&self, model: &str) -> &[RequestParam] {
        self.unsupported.get(model).map_or(&[], Vec::as_slice)
    }

    /// Has this exact (model, parameter) pair been learned?
    pub fn is_unsupported(&self, model: &str, param: RequestParam) -> bool {
        self.unsupported_for(model).contains(&param)
    }

    /// Every learned fact, sorted by model name. Used by the TUI to
    /// render the current blacklist. `BTreeMap` iteration is already in
    /// model order, so no extra sort is needed.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &[RequestParam])> {
        self.unsupported
            .iter()
            .map(|(model, params)| (model.as_str(), params.as_slice()))
    }

    /// Number of models with at least one learned rejection.
    pub fn model_count(&self) -> usize {
        self.unsupported.len()
    }

    /// Copy-on-write variant with `param` recorded for `model`.
    fn with_learned(&self, model: &str, param: RequestParam) -> Self {
        let mut next = self.clone();
        let entry = next.unsupported.entry(model.to_owned()).or_default();
        if let Err(idx) = entry.binary_search(&param) {
            entry.insert(idx, param);
        }
        next
    }
}

/// Process-lifetime, read-mostly shared registry of learned capabilities.
///
/// Backed by [`ArcSwap`] rather than `RwLock<HashMap<..>>` for the same
/// reason [`crate::tui::MappingsStore`] is: reads happen on every
/// request and stay lock-free, while writes are rare (at most once per
/// model+parameter per process) and copy-on-write.
///
/// `rcu` is used rather than `store` because the new value is *derived*
/// from the current one: under two concurrent first-time learnings of
/// different models, a plain `store` could drop one model's fact, while
/// `rcu` re-runs the update against the winning value until it sticks.
pub struct CapabilityStore {
    registry: ArcSwap<CapabilityRegistry>,
}

impl CapabilityStore {
    /// A new, empty store. Nothing is learned until an upstream says so.
    pub fn new() -> Self {
        Self {
            registry: ArcSwap::from_pointee(CapabilityRegistry::default()),
        }
    }

    /// Lock-free read guard; use for a single quick lookup.
    pub fn load(&self) -> arc_swap::Guard<Arc<CapabilityRegistry>> {
        self.registry.load()
    }

    /// Owned snapshot for callers that need to outlive the guard.
    pub fn snapshot(&self) -> Arc<CapabilityRegistry> {
        self.registry.load_full()
    }

    /// Record that `model` rejects `param`. Returns `true` when this is
    /// news (the fact was not already known), so the caller can log the
    /// discovery exactly once.
    pub fn record_unsupported(&self, model: &str, param: RequestParam) -> bool {
        let mut learned = false;
        self.registry.rcu(|current| {
            if current.is_unsupported(model, param) {
                // Already known: keep the same Arc so ArcSwap does not
                // churn (and so `learned` stays false).
                return Arc::clone(current);
            }
            learned = true;
            Arc::new(current.with_learned(model, param))
        });
        learned
    }

    /// Strip every parameter already known to be rejected by the
    /// request's target model, returning what was removed.
    ///
    /// Called before each send so a learned fact is applied to the very
    /// next request instead of being rediscovered by a wasted 400.
    pub fn apply_known(&self, req: &mut ResponsesRequest) -> Vec<RequestParam> {
        let known = self.load().unsupported_for(&req.model).to_vec();
        for param in &known {
            param.clear_from(req);
        }
        known
    }

    /// Forget every learned fact, returning how many models were
    /// affected.
    ///
    /// The TUI exposes this as `c` (clear). It is the operator's escape
    /// hatch for the one-way latch: after an upstream change, clearing
    /// re-probes each model on its next request instead of waiting for
    /// a proxy restart. Dropping the last facts is not a concern -- the
    /// store is tiny and the only cost of being wrong is one 400.
    pub fn clear(&self) -> usize {
        let removed = self.registry.load().model_count();
        self.registry.store(Arc::new(CapabilityRegistry::default()));
        removed
    }
}

impl Default for CapabilityStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(param: &str) -> String {
        format!(r#"{{"message":"Unsupported parameter: {param}","param":"{param}","code":"unsupported_parameter"}}"#)
    }

    #[test]
    fn detects_flat_responses_error_naming_param() {
        let body = flat("temperature");
        assert_eq!(
            unsupported_parameter(StatusCode::BAD_REQUEST, &body),
            Some(RequestParam::Temperature)
        );
    }

    #[test]
    fn detects_wrapped_chat_completions_error_naming_param() {
        let body = r#"{"error":{"message":"Unsupported parameter: 'top_p' is not supported with this model.","type":"invalid_request_error","param":"top_p","code":"unsupported_parameter"}}"#;
        assert_eq!(
            unsupported_parameter(StatusCode::BAD_REQUEST, body),
            Some(RequestParam::TopP)
        );
    }

    #[test]
    fn detects_phrase_without_structured_param_field() {
        // Older gateways put the reason only in the message text.
        let body = r#"{"message":"temperature is not supported for this model"}"#;
        assert_eq!(
            unsupported_parameter(StatusCode::BAD_REQUEST, body),
            Some(RequestParam::Temperature)
        );
    }

    #[test]
    fn detects_plain_text_body() {
        let body = "400 Bad Request: unexpected parameter: top_p";
        assert_eq!(
            unsupported_parameter(StatusCode::BAD_REQUEST, body),
            Some(RequestParam::TopP)
        );
    }

    #[test]
    fn ignores_invalid_value_that_names_param() {
        // Names the parameter, but it is a *value* problem, not a
        // support problem -- must not match, or we'd strip a
        // parameter the model honours.
        let body = r#"{"message":"Invalid value for 'temperature': temperature must be between 0 and 2.","param":"temperature","code":"invalid_request_error"}"#;
        assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
    }

    #[test]
    fn ignores_model_not_supported() {
        // Belongs to the model-fallback path, not this one.
        let body = r#"{"message":"The model 'gpt-x' does not exist","code":"model_not_found"}"#;
        assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
    }

    #[test]
    fn ignores_unsupported_signal_that_names_no_known_param() {
        let body = r#"{"message":"Unsupported parameter: foo","param":"foo","code":"unsupported_parameter"}"#;
        assert_eq!(unsupported_parameter(StatusCode::BAD_REQUEST, body), None);
    }

    #[test]
    fn ignores_non_request_status_codes() {
        let body = flat("temperature");
        assert_eq!(unsupported_parameter(StatusCode::INTERNAL_SERVER_ERROR, &body), None);
        assert_eq!(unsupported_parameter(StatusCode::NOT_FOUND, &body), None);
    }

    #[test]
    fn accepts_unprocessable_entity_as_well_as_bad_request() {
        let body = r#"{"message":"temperature not allowed"}"#;
        assert_eq!(
            unsupported_parameter(StatusCode::UNPROCESSABLE_ENTITY, body),
            Some(RequestParam::Temperature)
        );
    }

    #[test]
    fn store_learns_and_reports_novelty_once() {
        let store = CapabilityStore::new();
        assert!(!store.load().is_unsupported("gpt-6-astra", RequestParam::Temperature));

        assert!(store.record_unsupported("gpt-6-astra", RequestParam::Temperature));
        assert!(!store.record_unsupported("gpt-6-astra", RequestParam::Temperature));

        assert!(store.load().is_unsupported("gpt-6-astra", RequestParam::Temperature));
        assert!(!store.load().is_unsupported("gpt-6-astra", RequestParam::TopP));
        assert!(!store.load().is_unsupported("other-model", RequestParam::Temperature));
    }

    #[test]
    fn store_keeps_facts_for_distinct_models_and_params() {
        let store = CapabilityStore::new();
        store.record_unsupported("a", RequestParam::Temperature);
        store.record_unsupported("a", RequestParam::TopP);
        store.record_unsupported("b", RequestParam::TopP);

        assert_eq!(
            store.load().unsupported_for("a"),
            &[RequestParam::Temperature, RequestParam::TopP]
        );
        assert_eq!(store.load().unsupported_for("b"), &[RequestParam::TopP]);
    }

    #[test]
    fn entries_lists_learned_facts_sorted_by_model() {
        let store = CapabilityStore::new();
        store.record_unsupported("zeta", RequestParam::TopP);
        store.record_unsupported("alpha", RequestParam::Temperature);
        store.record_unsupported("alpha", RequestParam::TopP);

        let listed: Vec<_> = store
            .load()
            .entries()
            .map(|(m, p)| (m.to_owned(), p.to_vec()))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("alpha".to_owned(), vec![RequestParam::Temperature, RequestParam::TopP]),
                ("zeta".to_owned(), vec![RequestParam::TopP]),
            ]
        );
    }

    #[test]
    fn clear_forgets_everything_and_reports_affected_models() {
        let store = CapabilityStore::new();
        store.record_unsupported("a", RequestParam::Temperature);
        store.record_unsupported("b", RequestParam::TopP);
        assert_eq!(store.clear(), 2);

        assert_eq!(store.load().model_count(), 0);
        assert!(!store.load().is_unsupported("a", RequestParam::Temperature));
        // Clearing an empty store is a no-op, not an error.
        assert_eq!(store.clear(), 0);
    }

    #[test]
    fn clear_from_removes_only_the_named_param() {
        let mut req = sample_request();
        RequestParam::Temperature.clear_from(&mut req);
        assert!(req.temperature.is_none());
        assert_eq!(req.top_p, Some(0.9));
    }

    #[test]
    fn apply_known_strips_every_learned_param_for_the_target_model() {
        let store = CapabilityStore::new();
        store.record_unsupported("gpt-6-astra", RequestParam::Temperature);
        store.record_unsupported("gpt-6-astra", RequestParam::TopP);

        let mut req = sample_request();
        req.model = "gpt-6-astra".to_owned();
        let dropped = store.apply_known(&mut req);

        assert_eq!(dropped, vec![RequestParam::Temperature, RequestParam::TopP]);
        assert!(req.temperature.is_none());
        assert!(req.top_p.is_none());
    }

    #[test]
    fn apply_known_leaves_unknown_models_untouched() {
        let store = CapabilityStore::new();
        store.record_unsupported("gpt-6-astra", RequestParam::Temperature);

        let mut req = sample_request();
        req.model = "gpt-5.4-mini".to_owned();
        assert!(store.apply_known(&mut req).is_empty());
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.top_p, Some(0.9));
    }

    fn sample_request() -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-5.4-mini".to_owned(),
            input: crate::responses::Input::Text("hi".to_owned()),
            instructions: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            text: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_output_tokens: None,
            parallel_tool_calls: None,
            store: None,
            previous_response_id: None,
            user: None,
            stream: false,
            metadata: None,
            prompt_cache_key: None,
            prompt_cache_options: None,
        }
    }
}
