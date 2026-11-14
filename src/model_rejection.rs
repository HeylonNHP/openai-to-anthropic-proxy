//! Detecting that an upstream rejected the **model**, not the request.
//!
//! The proxy has a `default_model` safety net: when the upstream says it
//! does not know the model the proxy asked for, the request is retried
//! once with the configured default. That net has been documented since
//! 2026-07-18 — and it never fired once in 539 MB of production logs,
//! because the detector it was built on could not recognise the error
//! bodies the gateway actually sends.
//!
//! The gateway's real body is a **nested** envelope:
//!
//! ```json
//! {"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.",
//!           "type":"not_found_error","param":null,"code":null}}
//! ```
//!
//! Three things make that body hard to match:
//!
//! 1. The model name sits **between** the words, so the phrase
//!    `"model not found"` is not a substring of it.
//! 2. `code` is `null`, so the structured-code channel is empty.
//! 3. The envelope is nested, so the flat Responses shape does not parse.
//!
//! The one strong signal the gateway *does* send — `type:
//! "not_found_error"` — was never read.
//!
//! Two traps for anyone who rewrites this:
//!
//! * **Parse, don't substring.** The detector sees the *raw* body, in
//!   which the quotes are JSON-escaped: `model \\"claude-sonnet-5\\" not
//!   found`. A substring test for `model "X" not found` fails on the raw
//!   text and only works on the parsed `error.message`.
//! * **Never key on a bare "not supported".** The corpus contains
//!   `the web_search tool is not supported` — a *tool* rejection. A
//!   detector that matched it would fire the model fallback on the wrong
//!   error. Every phrase below is about a *model*.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// Structured error codes that unambiguously mean "the upstream does not
/// know this model". Compared case-insensitively against the `code`
/// field of either envelope shape.
const MODEL_ERROR_CODES: &[&str] = &[
    "model_not_found",
    "model_not_supported",
    "model_not_exist",
    "unknown_model",
    "invalid_model",
];

/// Narrow phrases that mean the upstream is talking about a **model**.
///
/// Every entry names a model. A bare "not supported" is deliberately
/// absent — that is the `web_search` false positive.
const MODEL_PHRASES: &[&str] = &[
    "model_not_found",
    "model_not_supported",
    "model not found",
    "model not supported",
    "model not available",
    "model unavailable",
    "model does not exist",
    "model doesn't exist",
    "model is deprecated",
    "model has been deprecated",
    "unknown model",
    "unsupported model",
    "no such model",
    "not supported for the selected provider",
];

/// Negative-polarity words, used only to **corroborate** that a message
/// is about a model.
///
/// Deliberately shorter than the anchored pattern's list: "invalid",
/// "not valid", "not allowed" and "not permitted" are excluded because
/// they routinely describe a *request* ("invalid value for temperature",
/// "'allOf' is not permitted"), not a model.
const NEGATIVE_WORDS: &[&str] = &[
    "not found",
    "not exist",
    "no such",
    "not available",
    "unavailable",
    "not supported",
    "unsupported",
    "unknown",
    "not recognized",
    "not recognised",
    "deprecated",
];

/// Markers of a **transient** condition. A model that is temporarily
/// unavailable is not a model rejection — retrying elsewhere would paper
/// over an outage instead of reporting it.
const TRANSIENT_MARKERS: &[&str] = &[
    "temporar",
    "overload",
    "rate limit",
    "rate-limit",
    "too many requests",
    "try again later",
    "retry after",
    "capacity",
    "throttl",
    "server is busy",
];

/// How far from the model name a negative word may sit and still count
/// as being "about that model".
const NEGATIVE_WINDOW: usize = 80;

/// The minimum length of a model name we are willing to look for inside
/// an error message. Short names would match far too much prose.
const MIN_NAME_LEN: usize = 4;

/// A flattened upstream error, whichever envelope it arrived in.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ErrorEnvelope {
    /// The human-readable message.
    pub message: String,
    /// The error's `type` field (`not_found_error`, `invalid_request_error`, …).
    pub kind: String,
    /// The error's `code` field, stringified. A *number* code is kept —
    /// real bodies carry `"code": 402` and `"code": 400`.
    pub code: String,
    /// The `param` field, if any.
    pub param: String,
    /// Whether the body parsed as JSON at all.
    pub is_json: bool,
}

/// Flatten an upstream error body.
///
/// Handles the nested shape the gateway sends
/// (`{"error":{"message":…,"type":…,"code":…}}`), the flat Responses
/// shape (`{"message":…,"code":…}`), a bare string (`{"error":"…"}`), and
/// a body that is not JSON at all — in which case the whole body becomes
/// the message, so plain-text upstreams still work.
#[must_use]
pub fn parse_error_envelope(body: &str) -> ErrorEnvelope {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return ErrorEnvelope {
            message: body.to_owned(),
            ..ErrorEnvelope::default()
        };
    };
    let mut env = ErrorEnvelope {
        is_json: true,
        ..ErrorEnvelope::default()
    };
    if let Some(err) = value.get("error") {
        if let Some(obj) = err.as_object() {
            env.message = field_as_text(obj.get("message"));
            env.kind = field_as_text(obj.get("type"));
            env.code = field_as_text(obj.get("code"));
            env.param = field_as_text(obj.get("param"));
            return env;
        }
        if let Some(text) = err.as_str() {
            env.message = text.to_owned();
            return env;
        }
    }
    env.message = field_as_text(value.get("message"));
    env.kind = field_as_text(value.get("type"));
    env.code = field_as_text(value.get("code"));
    env.param = field_as_text(value.get("param"));
    env
}

/// Stringify a structured field that may be a string, a number, a
/// boolean, an array, or absent.
fn field_as_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| field_as_text(Some(item)))
            .collect::<Vec<_>>()
            .join(","),
        Some(other) => other.to_string(),
    }
}

/// What the upstream said about the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRejection {
    /// The model the upstream said it did not know, when it named one.
    pub rejected: Option<String>,
    /// The model the upstream proposed instead, when it named one.
    ///
    /// This is **telemetry, not a routing decision**: the suggestion is
    /// untrusted input, it bypasses the operator's model policy, and it
    /// uses a naming convention the alias map does not necessarily know
    /// (the gateway suggests `glm-5.3-flash:cloud`; the operator's map
    /// uses `glm-5.3-flash`).
    pub suggested: Option<String>,
}

/// Detect that an upstream rejected the model rather than the request.
///
/// `sent_model` is the upstream model name the proxy asked for. It is
/// used only as a **corroborating** signal — never as a requirement,
/// because an upstream may name the model differently from the way it
/// was sent.
#[must_use]
pub fn detect_model_rejection(status: u16, body: &str, sent_model: &str) -> Option<ModelRejection> {
    // Only 400 and 404 mean "bad request" / "not found". A 402 (credits)
    // or a 5xx is an outage or a billing problem, not a model rejection.
    if !matches!(status, 400 | 404) {
        return None;
    }

    let env = parse_error_envelope(body);
    let message = env.message.trim();
    let lower = message.to_ascii_lowercase();

    // A temporarily-unavailable model is an outage, not a rejection.
    if TRANSIENT_MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }

    let rejected = extract_rejected_model(message);
    let suggested = parse_suggested_model(message);
    let hit = || {
        Some(ModelRejection {
            rejected: rejected.clone(),
            suggested: suggested.clone(),
        })
    };

    // 1. A structured code that is unambiguously about a model.
    if MODEL_ERROR_CODES
        .iter()
        .any(|c| env.code.eq_ignore_ascii_case(c))
    {
        return hit();
    }

    let model_related = mentions_model(&lower) || names_model(message, sent_model);

    // 2. `not_found_error` AND the message is about a model. This is the
    //    branch that catches every model rejection in the corpus, with
    //    no false positives: a bad-path 404 says "Not Found" and never
    //    mentions a model.
    if env.kind.eq_ignore_ascii_case("not_found_error") && model_related {
        return hit();
    }

    // 3. The anchored pattern: "model <name> <negative>".
    if match_model_rejection(message).is_some() {
        return hit();
    }

    // 4. A narrow phrase that names a model.
    if model_related && MODEL_PHRASES.iter().any(|p| lower.contains(p)) {
        return hit();
    }

    // 5. The message names the exact model we sent AND says something
    //    negative about it nearby.
    if names_model(message, sent_model) && negative_near(&lower, sent_model) {
        return hit();
    }

    None
}

/// The anchored "model <name> <negative>" pattern.
///
/// The name may be quoted (either quote character — the gateway has used
/// both) or bare, in which case it must look like a model id: it has to
/// contain a digit or a separator, so `the model output is not found`
/// does not match while `model gpt-5.4-mini is not supported` does.
fn model_rejection_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            \bmodels?\b
            \s*
            (?:
                ["'`](?P<quoted>[^"'`\n]{1,128})["'`]
              | (?P<bare>[A-Za-z0-9][A-Za-z0-9._:@/+-]{0,127})
            )
            \s*
            (?:is|are|was|were|has\s+been|have\s+been)?
            \s*
            (?:not\s+(?:found|supported|available|recognized|recognised|exist)
              | does\s*n[o']t\s+exist
              | don'?t\s+exist
              | unavailable
              | deprecated
            )
            "#,
        )
        .expect("model rejection regex compiles")
    })
}

/// A match of the anchored pattern, and the model name it captured.
struct PatternMatch {
    name: Option<String>,
}

/// Match the anchored pattern, validating the captured name.
///
/// A **bare** name has to look like a model id — it must contain a digit
/// or a separator — so `the model output is not found` does not match
/// while `model gpt-5.4-mini is not supported` does. The `regex` crate
/// has no look-ahead, so that check happens here rather than in the
/// pattern.
fn match_model_rejection(message: &str) -> Option<PatternMatch> {
    let captures = model_rejection_re().captures(message)?;
    if let Some(quoted) = captures.name("quoted") {
        return Some(PatternMatch {
            name: Some(quoted.as_str().to_owned()),
        });
    }
    let bare = captures.name("bare")?.as_str();
    if !looks_like_model_id(bare) {
        return None;
    }
    Some(PatternMatch {
        name: Some(bare.to_owned()),
    })
}

/// Extract the model name the upstream said it did not know.
fn extract_rejected_model(message: &str) -> Option<String> {
    match_model_rejection(message).and_then(|m| m.name)
}

/// Does the message contain the word "model" (or "models") as a token?
fn mentions_model(lower: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "model" || word == "models")
}

/// Does the message name the exact model we sent?
fn names_model(message: &str, sent_model: &str) -> bool {
    if sent_model.len() < MIN_NAME_LEN {
        return false;
    }
    message
        .to_ascii_lowercase()
        .contains(&sent_model.to_ascii_lowercase())
}

/// Is there a negative word close to the model name?
fn negative_near(lower: &str, sent_model: &str) -> bool {
    let sent_lower = sent_model.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(&sent_lower) {
        let at = from + rel;
        let start = at.saturating_sub(NEGATIVE_WINDOW);
        let end = (at + sent_lower.len() + NEGATIVE_WINDOW).min(lower.len());
        let window = &lower[start..end];
        if NEGATIVE_WORDS.iter().any(|w| window.contains(w)) {
            return true;
        }
        from = at + sent_lower.len();
        if from >= lower.len() {
            break;
        }
    }
    false
}

/// Parse the upstream's "Try <model>." suggestion.
///
/// The suggestion is telemetry only — see [`ModelRejection::suggested`].
/// It is validated so that prose ("Try another model.") and URLs
/// ("Try https://example/x") are not mistaken for model ids.
fn parse_suggested_model(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("try") {
        let at = from + rel;
        let after = at + 3;
        let next = message[after..].chars().next();
        // "try" must be a whole word followed by whitespace.
        if next.is_some_and(char::is_whitespace) {
            let token = message[after..].split_whitespace().next()?;
            let token = token.trim_matches(|c: char| {
                matches!(
                    c,
                    '.' | ',' | ';' | ':' | ')' | '(' | '"' | '\'' | '`' | '[' | ']'
                )
            });
            if looks_like_model_id(token) {
                return Some(token.to_owned());
            }
        }
        from = at + 3;
        if from >= lower.len() {
            break;
        }
    }
    None
}

/// Does this token look like a model id rather than prose?
fn looks_like_model_id(token: &str) -> bool {
    if token.is_empty() || token.len() > 128 {
        return false;
    }
    if token.contains("//") || token.to_ascii_lowercase().starts_with("http") {
        return false;
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ':' | '@' | '+'))
    {
        return false;
    }
    token.chars().any(|c| c.is_ascii_digit()) || token.contains(['-', '.', ':', '/', '@'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact body the gateway sent on 2026-11-12.
    const REAL_404: &str = r#"{"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}"#;
    /// The exact body the gateway sent on 2026-08-16 (single quotes).
    const REAL_404_SINGLE_QUOTED: &str = r#"{"error":{"message":"model 'claude-opus-5' not found","type":"not_found_error","param":null,"code":null}}"#;
    /// The exact body the gateway sent on 2026-09-18 (double quotes, no hint).
    const REAL_404_DOUBLE_QUOTED: &str = r#"{"error":{"message":"model \"claude-fable-5-1\" not found","type":"not_found_error","param":null,"code":null}}"#;

    #[test]
    fn real_gateway_body_is_a_model_rejection() {
        let rejection = detect_model_rejection(404, REAL_404, "claude-sonnet-5")
            .expect("the real gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(rejection.suggested.as_deref(), Some("glm-5.3-flash:cloud"));
    }

    #[test]
    fn real_gateway_body_single_quoted() {
        let rejection = detect_model_rejection(404, REAL_404_SINGLE_QUOTED, "claude-opus-5")
            .expect("the single-quoted gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-opus-5"));
        assert_eq!(rejection.suggested, None);
    }

    #[test]
    fn real_gateway_body_double_quoted() {
        let rejection = detect_model_rejection(404, REAL_404_DOUBLE_QUOTED, "claude-fable-5-1")
            .expect("the double-quoted gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-fable-5-1"));
    }

    #[test]
    fn flat_responses_body_is_a_model_rejection() {
        let body = r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error","code":"model_not_supported"}"#;
        assert!(detect_model_rejection(400, body, "claude-sonnet-5").is_some());
    }

    #[test]
    fn nested_code_is_a_model_rejection() {
        let body = r#"{"error":{"code":"model_not_found","message":"nope"}}"#;
        assert!(detect_model_rejection(404, body, "claude-sonnet-5").is_some());
    }

    #[test]
    fn plain_text_body_is_a_model_rejection() {
        assert!(detect_model_rejection(400, "Model not found", "claude-sonnet-5").is_some());
    }

    #[test]
    fn corpus_non_model_errors_are_not_model_rejections() {
        // The four real non-model bodies in the corpus. The `web_search` one
        // contains "not supported" — a detector keyed on that would fire
        // the model fallback on a tool rejection.
        let bodies = [
            (
                400,
                r#"{"error":{"message":"the web_search tool is not supported (ref: 490f64b3-1e45-4671-aaef-8e7ff3a71f4b)","type":"invalid_request_error","param":null,"code":null}}"#,
            ),
            (
                400,
                r#"{"error":{"message":"invalid reasoning value: \"xhigh\" (must be \"high\", \"medium\", \"low\", \"max\", or \"none\") (ref: 4644c95a)","type":"invalid_request_error","param":null,"code":null}}"#,
            ),
            (
                400,
                r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"error\": {\"message\": \"Invalid schema for function 'SendMessage': In context=('properties', 'to'), 'allOf' is not permitted.\",\"type\": \"invalid_request_error\",\"param\": \"tools[21].parameters\",\"code\": \"invalid_function_parameters\"}}"},"user_id":"user_3GnW"}}"#,
            ),
            (
                402,
                r#"{"error":{"message":"This request would exceed your available credits given your current in-flight requests. Retry after in-flight requests settle, or add credits.","code":402,"metadata":{"reason":"in_flight_budget_exhausted"}},"user_id":"user_3GnW"}"#,
            ),
        ];
        for (status, body) in bodies {
            assert!(
                detect_model_rejection(status, body, "claude-sonnet-5").is_none(),
                "must not treat as a model rejection: {body}"
            );
        }
    }

    #[test]
    fn empty_502_and_503_are_not_model_rejections() {
        assert!(detect_model_rejection(502, "", "claude-sonnet-5").is_none());
        assert!(detect_model_rejection(503, "", "claude-sonnet-5").is_none());
    }

    #[test]
    fn non_model_404_is_not_a_model_rejection() {
        // A bad path on the upstream: "Not Found" with no model mentioned.
        let body = r#"{"error":{"message":"Not Found","type":"not_found_error","param":null,"code":null}}"#;
        assert!(detect_model_rejection(404, body, "claude-sonnet-5").is_none());
    }

    #[test]
    fn transient_outage_is_not_a_model_rejection() {
        let body = r#"{"error":{"message":"claude-sonnet-5 is temporarily unavailable","type":"invalid_request_error"}}"#;
        assert!(detect_model_rejection(404, body, "claude-sonnet-5").is_none());
    }

    #[test]
    fn model_word_in_an_unrelated_message_is_not_a_model_rejection() {
        assert!(detect_model_rejection(404, "the model output is not found", "x").is_none());
    }

    #[test]
    fn model_name_prefixed_to_an_unrelated_400_is_not_a_model_rejection() {
        let body = r#"{"error":{"message":"claude-sonnet-5: invalid value for temperature","type":"invalid_request_error"}}"#;
        assert!(detect_model_rejection(400, body, "claude-sonnet-5").is_none());
    }

    #[test]
    fn suggestion_must_look_like_a_model_id() {
        for prose in [
            "model \"x\" not found. Try another model.",
            "model \"x\" not found. Try https://evil.example/x",
            "model \"x\" not found. Try the default.",
        ] {
            let rejection = detect_model_rejection(404, prose, "x");
            assert_eq!(
                rejection.and_then(|r| r.suggested),
                None,
                "prose must not be read as a suggestion: {prose}"
            );
        }
    }

    #[test]
    fn the_envelope_flattens_both_shapes() {
        let nested = parse_error_envelope(REAL_404);
        assert!(nested.is_json);
        assert_eq!(nested.kind, "not_found_error");
        assert_eq!(nested.code, "");
        assert!(nested.message.contains("claude-sonnet-5"));

        let flat = parse_error_envelope(
            r#"{"message":"flat message","type":"invalid_request_error","code":"model_not_found"}"#,
        );
        assert_eq!(flat.message, "flat message");
        assert_eq!(flat.kind, "invalid_request_error");
        assert_eq!(flat.code, "model_not_found");

        let text = parse_error_envelope("not json at all");
        assert!(!text.is_json);
        assert_eq!(text.message, "not json at all");
    }

    #[test]
    fn a_numeric_code_is_kept() {
        let env = parse_error_envelope(r#"{"error":{"message":"m","code":402}}"#);
        assert_eq!(env.code, "402");
    }
}
