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
//! "not_found_error"` — was never read either. The detector below does
//! not use it: it reads the **message** and the **code**, because those
//! say what the upstream actually refused. (`type` is still parsed, and
//! `ErrorEnvelope::kind` is available, but it is not a recognition
//! channel — `not_found_error` is neither necessary nor sufficient: a
//! bad path on the upstream is also a `not_found_error`, and a model
//! rejection can arrive with any type.)
//!
//! # The asymmetry that shapes this module
//!
//! A **false negative** means the client sees the upstream's 404 — the
//! status quo, and visible. A **false positive** means the proxy
//! silently retries with a *different model*, answering the request with
//! a model the operator never chose and masking the real error. So this
//! detector is deliberately **conservative**: every branch below has to
//! be quite sure the upstream is talking about the model.
//!
//! Three traps for anyone who rewrites this:
//!
//! * **Parse, don't substring.** The detector sees the *raw* body, in
//!   which the quotes are JSON-escaped: `model \\"claude-sonnet-5\\" not
//!   found`. A substring test for `model "X" not found` fails on the raw
//!   text and only works on the parsed `error.message`.
//! * **Never key on a bare "not supported".** The corpus contains
//!   `the web_search tool is not supported` — a *tool* rejection. A
//!   detector that matched it would fire the model fallback on the wrong
//!   error, and the client would be answered by another model.
//! * **Check the subject.** `the web_search tool is not supported for
//!   model claude-sonnet-5` mentions the model *and* says something
//!   negative — but the subject is the tool, and answering it with a
//!   different model would be silent corruption. Two things separate
//!   the two: the anchored pattern only accepts the **absence** words
//!   ("not found", "does not exist"), and a short list of
//!   request-subject words vetoes the rest.

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
/// Every entry names the model as the *subject* of the negative. A bare
/// "not supported" is deliberately absent — that is the `web_search`
/// false positive.
const MODEL_PHRASES: &[&str] = &[
    "model_not_found",
    "model_not_supported",
    "model not found",
    "model not supported",
    "model not exist",
    "model not available",
    "model unavailable",
    "model does not exist",
    "model doesn't exist",
    "model is deprecated",
    "model has been deprecated",
    "unknown model",
    "unsupported model",
    "no such model",
    "invalid model",
    "not supported for the selected provider",
    // OpenRouter's wording when no provider serves the requested model.
    "no endpoints found that match",
    // OpenAI's wording when the model exists but is not accessible.
    "does not exist or you do not have access",
];

/// Words that make an error about the **request** rather than the
/// model.
///
/// This is a short list on purpose. It used to be a long one, and a
/// long deny-list is a leak in both directions: it vetoed real
/// rejections ("model X not found for this request" contains
/// "request") while still missing new request subjects. The anchored
/// pattern now carries the discrimination instead — see
/// [`model_rejection_re`] — so this list only holds the words that
/// cannot appear in a model rejection at all.
///
/// Matched as whole tokens, so `requested` is not `request` and
/// `web_search` is `web` + `search`.
const REQUEST_SUBJECT_WORDS: &[&str] = &[
    "tool",
    "tools",
    "tool_choice",
    "function",
    "functions",
    "parameter",
    "parameters",
    "schema",
    "schemas",
];

/// Markers of a **transient** condition. A model that is temporarily
/// unavailable is not a model rejection — retrying elsewhere would paper
/// over an outage instead of reporting it.
///
/// Deliberately limited to *availability* words. Generic advice like
/// "please try again later" is excluded: it follows all kinds of error,
/// including a genuine "model not found".
const TRANSIENT_MARKERS: &[&str] = &[
    "temporar",
    "overload",
    "rate limit",
    "rate-limit",
    "too many requests",
    "capacity",
    "throttl",
    "server is busy",
];

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
#[must_use]
pub fn detect_model_rejection(status: u16, body: &str) -> Option<ModelRejection> {
    // Only 400 and 404 mean "bad request" / "not found". A 402 (credits)
    // or a 5xx is an outage or a billing problem, not a model rejection.
    if !matches!(status, 400 | 404) {
        return None;
    }

    let env = parse_error_envelope(body);
    let message = env.message.trim();
    let lower = message.to_ascii_lowercase();

    // 1. A structured code that is unambiguously about a model. Checked
    //    first: it is the strongest signal, and it survives a message that
    //    happens to mention a tool.
    if MODEL_ERROR_CODES
        .iter()
        .any(|c| env.code.eq_ignore_ascii_case(c))
    {
        return Some(build_rejection(message));
    }

    // 2. A temporarily-unavailable model is an outage, not a rejection.
    if TRANSIENT_MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }

    // 3. The error is about the request, not the model.
    if mentions_request_subject(&lower) {
        return None;
    }

    // 4. The anchored pattern: "model <name> <negative>".
    if match_model_rejection(message).is_some() {
        return Some(build_rejection(message));
    }

    // 5. A narrow phrase that names the model as the subject.
    if MODEL_PHRASES.iter().any(|p| lower.contains(p)) {
        return Some(build_rejection(message));
    }

    None
}

/// Build the rejection, extracting the name the upstream used and any
/// suggestion it made.
fn build_rejection(message: &str) -> ModelRejection {
    ModelRejection {
        rejected: extract_rejected_model(message),
        suggested: parse_suggested_model(message),
    }
}

/// The anchored "model <name> <negative>" pattern.
///
/// The name may be quoted (either quote character — the gateway has used
/// both) or bare, in which case it must look like a model id: it has to
/// contain a digit or a separator, so `the model output is not found`
/// does not match while `model gpt-5.4-mini not found` does.
///
/// The negative is restricted to the **absence** words — "not found",
/// "does not exist". That is deliberate: "not supported" and "not
/// available" are the words request-scoped errors use ("the web_search
/// tool is not supported", "model X is not available for
/// web_search"), and admitting them here would let a request error
/// swap the model. A message where "model" sits *immediately* before
/// the negative ("model not supported") is still recognised — that is
/// the [`MODEL_PHRASES`] branch, where adjacency leaves no room for a
/// different subject.
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
            (?:not\s+(?:found|exist)
              | does\s*n[o']t\s+exist
              | don'?t\s+exist
              | deprecated
            )
            "#,
        )
        .expect("model rejection regex compiles")
    })
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

/// A match of the anchored pattern, and the model name it captured.
struct PatternMatch {
    name: Option<String>,
}

/// Extract the model name the upstream said it did not know.
fn extract_rejected_model(message: &str) -> Option<String> {
    match_model_rejection(message).and_then(|m| m.name)
}

/// Does the message name something that makes it about the *request*
/// rather than the model?
fn mentions_request_subject(lower: &str) -> bool {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| REQUEST_SUBJECT_WORDS.contains(&word))
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

/// Parse the upstream's "Try <model>." suggestion.
///
/// The suggestion is telemetry only — see [`ModelRejection::suggested`].
/// It is validated strictly: a suggestion has to contain a **digit**, so
/// prose ("Try another model."), paths ("Try /usr/local/bin") and
/// addresses ("Try user@example.com") are all rejected.
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
            if looks_like_model_suggestion(token) {
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

/// Does this token look like a model id we could log as a suggestion?
///
/// Stricter than [`looks_like_model_id`]: it must contain a digit, so
/// "another", "the default" and "e.g" are all rejected.
fn looks_like_model_suggestion(token: &str) -> bool {
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
    token.chars().any(|c| c.is_ascii_digit())
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
        let rejection =
            detect_model_rejection(404, REAL_404).expect("the real gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(rejection.suggested.as_deref(), Some("glm-5.3-flash:cloud"));
    }

    #[test]
    fn real_gateway_body_single_quoted() {
        let rejection = detect_model_rejection(404, REAL_404_SINGLE_QUOTED)
            .expect("the single-quoted gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-opus-5"));
        assert_eq!(rejection.suggested, None);
    }

    #[test]
    fn real_gateway_body_double_quoted() {
        let rejection = detect_model_rejection(404, REAL_404_DOUBLE_QUOTED)
            .expect("the double-quoted gateway body must be detected");
        assert_eq!(rejection.rejected.as_deref(), Some("claude-fable-5-1"));
    }

    #[test]
    fn flat_responses_body_is_a_model_rejection() {
        let body = r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error","code":"model_not_supported"}"#;
        assert!(detect_model_rejection(400, body).is_some());
    }

    #[test]
    fn nested_code_is_a_model_rejection() {
        let body = r#"{"error":{"code":"model_not_found","message":"nope"}}"#;
        assert!(detect_model_rejection(404, body).is_some());
    }

    #[test]
    fn plain_text_body_is_a_model_rejection() {
        assert!(detect_model_rejection(400, "Model not found").is_some());
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
                detect_model_rejection(status, body).is_none(),
                "must not treat as a model rejection: {body}"
            );
        }
    }

    #[test]
    fn empty_502_and_503_are_not_model_rejections() {
        assert!(detect_model_rejection(502, "").is_none());
        assert!(detect_model_rejection(503, "").is_none());
    }

    #[test]
    fn non_model_404_is_not_a_model_rejection() {
        // A bad path on the upstream: "Not Found" with no model mentioned.
        let body = r#"{"error":{"message":"Not Found","type":"not_found_error","param":null,"code":null}}"#;
        assert!(detect_model_rejection(404, body).is_none());
    }

    #[test]
    fn transient_outage_is_not_a_model_rejection() {
        let body = r#"{"error":{"message":"claude-sonnet-5 is temporarily unavailable","type":"invalid_request_error"}}"#;
        assert!(detect_model_rejection(404, body).is_none());
    }

    #[test]
    fn model_word_in_an_unrelated_message_is_not_a_model_rejection() {
        assert!(detect_model_rejection(404, "the model output is not found").is_none());
        // ...and the same sentence inside a JSON envelope, which is the
        // shape that actually matters.
        let body =
            r#"{"error":{"message":"the model output is not found","type":"not_found_error"}}"#;
        assert!(detect_model_rejection(404, body).is_none());
    }

    #[test]
    fn model_name_prefixed_to_an_unrelated_400_is_not_a_model_rejection() {
        let body = r#"{"error":{"message":"claude-sonnet-5: invalid value for temperature","type":"invalid_request_error"}}"#;
        assert!(detect_model_rejection(400, body).is_none());
    }

    /// A rejection that mentions the model but whose **subject** is
    /// something else — a tool, a parameter, a schema. These must never
    /// fire the fallback, because firing it answers the request with a
    /// model the operator never chose.
    #[test]
    fn a_rejection_about_something_else_is_not_a_model_rejection() {
        let bodies = [
            "the web_search tool is not supported for model claude-sonnet-5",
            "claude-sonnet-5 is not supported for the web_search tool",
            "Invalid request for model claude-sonnet-5: images are not supported in this mode",
            "function calling is unsupported by claude-sonnet-5",
            "unknown parameter for claude-sonnet-5",
            "temperature is not supported by claude-sonnet-5",
            "the schema for model claude-sonnet-5 is invalid",
        ];
        for body in bodies {
            let json =
                format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
            assert!(
                detect_model_rejection(400, &json).is_none(),
                "must not treat a request-scoped error as a model rejection: {body}"
            );
            // ...and the same body as plain text.
            assert!(
                detect_model_rejection(400, body).is_none(),
                "must not treat a request-scoped error as a model rejection: {body}"
            );
        }
    }

    #[test]
    fn a_non_ascii_message_does_not_panic() {
        // The window arithmetic in an earlier draft sliced at a byte
        // offset that was not a char boundary, which killed the request.
        let body = "模型 claude-sonnet-5 不可用：请求参数不受支持，这是很长的一个句子用来推动偏移量越过字符边界";
        let json = format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
        for offset in 0..40 {
            let mut padded = body.to_string();
            padded.push_str(&"x".repeat(offset));
            let json =
                format!(r#"{{"error":{{"message":"{padded}","type":"invalid_request_error"}}}}"#);
            let _ = detect_model_rejection(400, &json);
        }
        let _ = detect_model_rejection(400, &json);
    }

    /// Every request-scoped noun a review turned up. The error is about
    /// the request, not the model, so the fallback must not fire — a
    /// fallback answers the request with a *different model*, which is
    /// silent corruption.
    ///
    /// Each is tried in both shapes: with the word "model" before the
    /// name (so the anchored pattern could match) and without it.
    #[test]
    fn a_request_scoped_error_never_swaps_the_model() {
        let nouns = [
            "web_search",
            "attachment",
            "vector store",
            "assistant",
            "agent",
            "thread",
            "conversation",
            "code",
            "script",
            "shell",
            "command",
            "syntax",
            "regex",
            "encoding",
            "token limit",
            "tokens",
            "context",
        ];
        for noun in nouns {
            for template in [
                "model claude-sonnet-5 is not supported for the __NOUN__",
                "claude-sonnet-5 is not supported for the __NOUN__",
                "model claude-sonnet-5 is not available: __NOUN__ problem",
                "the __NOUN__ is not supported by model claude-sonnet-5",
            ] {
                let body = template.replace("__NOUN__", noun);
                let json =
                    format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
                assert!(
                    detect_model_rejection(400, &json).is_none(),
                    "must not swap the model for: {body}"
                );
                assert!(
                    detect_model_rejection(400, &body).is_none(),
                    "must not swap the model for: {body}"
                );
            }
        }
    }

    /// The absence negatives the gateway actually uses must still fire.
    #[test]
    fn the_absence_negatives_still_fire() {
        for body in [
            "model \"claude-sonnet-5\" not found",
            "model 'claude-opus-5' not found",
            "model claude-sonnet-5 not found",
            "model claude-sonnet-5 does not exist",
            "model claude-sonnet-5 doesn't exist",
            "model claude-sonnet-5 is deprecated",
        ] {
            let json = format!(r#"{{"error":{{"message":"{body}","type":"not_found_error"}}}}"#);
            assert!(
                detect_model_rejection(404, &json).is_some(),
                "must still fire for: {body}"
            );
        }
    }

    /// A message where "model" sits immediately before the negative has
    /// no room for a different subject, so it is recognised.
    #[test]
    fn adjacent_model_phrases_still_fire() {
        for body in [
            "Model not found",
            "MODEL NOT SUPPORTED",
            "model not supported",
            "model not available",
            "model unavailable",
            "Unknown model: foo",
            "no such model",
            "The requested model claude-sonnet-5 is not supported for the selected provider.",
        ] {
            assert!(
                detect_model_rejection(400, body).is_some(),
                "must still fire for: {body}"
            );
        }
    }

    #[test]
    fn suggestion_must_look_like_a_model_id() {
        for prose in [
            "model \"x\" not found. Try another model.",
            "model \"x\" not found. Try https://evil.example/x",
            "model \"x\" not found. Try the default.",
            "model \"x\" not found. Try /usr/local/bin",
            "model \"x\" not found. Try user@example.com",
            "model \"x\" not found. Try e.g. something",
        ] {
            let rejection = detect_model_rejection(404, prose);
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
