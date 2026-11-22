//! Recognise "the prompt does not fit the context window" errors from an
//! OpenAI-compatible upstream and re-word them the way Anthropic's API does.
//!
//! Claude Code recovers from an over-long prompt (reactive compaction) by
//! recognising Anthropic's own error: HTTP 400, `invalid_request_error`, and
//! the message `prompt is too long: <actual> tokens > <max> maximum`. An
//! upstream words the same failure differently, so without this translation
//! the client sees a generic error and never compacts.
//!
//! Detection is deliberately conservative: a false positive would turn a
//! real, different error into a compaction attempt, whereas a miss only
//! leaves today's behaviour. Output-limit errors, image/file errors and
//! token-per-minute rate limits are vetoed.

use std::sync::OnceLock;

use regex::Regex;

use crate::model_rejection::parse_error_envelope;

/// `error.type` Anthropic uses for this failure.
pub const ERROR_KIND: &str = "invalid_request_error";

/// Codes that identify the failure on their own, whatever the message says.
const STRONG_CODES: [&str; 3] = [
    "context_length_exceeded",
    "context_window_exceeded",
    "prompt_too_long",
];

/// Message fragments that say the prompt exceeds the window.
const NEEDLES: [&str; 13] = [
    "prompt is too long",
    "prompt too long",
    "input is too long",
    "input too long",
    "context length",
    "context window",
    "context_length",
    "maximum context",
    "exceeds the context",
    "exceed the context",
    "reduce the length of the messages",
    "reduce the length of your prompt",
    "reduce your prompt",
];

/// A recognised overflow, with the token counts when the upstream gave them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextOverflow {
    pub actual: Option<u64>,
    pub max: Option<u64>,
}

impl ContextOverflow {
    /// The message Claude Code parses. Without both numbers it is the bare
    /// prefix, which Claude Code still recognises; a half-formed figure
    /// would risk being mis-parsed.
    #[must_use]
    pub fn anthropic_message(&self) -> String {
        match (self.actual, self.max) {
            (Some(actual), Some(max)) => {
                format!("prompt is too long: {actual} tokens > {max} maximum")
            }
            _ => "prompt is too long".to_owned(),
        }
    }
}

/// Classify an upstream HTTP error response.
#[must_use]
pub fn detect(status: u16, body: &str) -> Option<ContextOverflow> {
    if !matches!(status, 400 | 413 | 422) {
        return None;
    }
    let env = parse_error_envelope(body);
    let lower = env.message.to_ascii_lowercase();
    let overflow = classify(&env.code, &env.message)?;
    // A 413 is usually a byte-size rejection, which Claude Code treats as a
    // different error: demand a token/context signal there.
    let strong = is_strong(&env.code);
    if status == 413 && !strong && !(lower.contains("context") || lower.contains("tokens")) {
        return None;
    }
    Some(overflow)
}

/// Classify an error delivered inside a stream, where there is no HTTP status.
#[must_use]
pub fn detect_event(code: &str, message: &str) -> Option<ContextOverflow> {
    classify(code, message)
}

/// The `(error type, message)` to put on the wire for a stream error.
#[must_use]
pub fn stream_error(code: &str, message: &str) -> (String, String) {
    detect_event(code, message).map_or_else(
        || ("upstream_error".to_owned(), message.to_owned()),
        |o| (ERROR_KIND.to_owned(), o.anthropic_message()),
    )
}

fn is_strong(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    STRONG_CODES.contains(&code.as_str())
}

fn classify(code: &str, message: &str) -> Option<ContextOverflow> {
    let lower = message.to_ascii_lowercase();
    let strong = is_strong(code);
    if !strong && (vetoed(&lower) || !mentions_overflow(&lower)) {
        return None;
    }
    let (actual, max) = extract_counts(&lower);
    Some(ContextOverflow { actual, max })
}

fn mentions_overflow(lower: &str) -> bool {
    NEEDLES.iter().any(|n| lower.contains(n))
        || (lower.contains("too many tokens")
            && ["prompt", "input", "context"]
                .iter()
                .any(|w| lower.contains(w)))
}

/// Errors that read like an overflow but are about something else.
fn vetoed(lower: &str) -> bool {
    // Token-per-minute and other rate limits.
    if ["per min", "tpm", "rate limit", "tokens per"]
        .iter()
        .any(|m| lower.contains(m))
    {
        return true;
    }
    // An output-length limit with no mention of the context window.
    let output_limit = [
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "completion tokens",
        "output tokens",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if output_limit
        && !["context length", "context window", "maximum context"]
            .iter()
            .any(|m| lower.contains(m))
    {
        return true;
    }
    // Media size limits.
    lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        matches!(
            w,
            "image" | "images" | "pixels" | "pdf" | "base64" | "attachment"
        )
    })
}

fn number(text: &str) -> Option<u64> {
    text.replace(',', "").parse().ok()
}

fn extract_counts(lower: &str) -> (Option<u64>, Option<u64>) {
    static ANTHROPIC: OnceLock<Regex> = OnceLock::new();
    static MAX: OnceLock<Regex> = OnceLock::new();
    static ACTUAL: OnceLock<Regex> = OnceLock::new();
    let anthropic = ANTHROPIC.get_or_init(|| {
        Regex::new(r"prompt is too long:\s*([\d,]+)\s*tokens?\s*>\s*([\d,]+)").expect("valid regex")
    });
    let max_re = MAX.get_or_init(|| {
        Regex::new(r"maximum (?:context )?(?:length|window|limit)(?: is|:)?\s*([\d,]+)")
            .expect("valid regex")
    });
    let actual_re = ACTUAL.get_or_init(|| {
        Regex::new(
            r"(?:resulted in|requested|contains|total of)\s*([\d,]+)\s*tokens|too long:\s*([\d,]+)",
        )
        .expect("valid regex")
    });

    let (actual, max) = anthropic.captures(lower).map_or_else(
        || {
            let max = max_re.captures(lower).and_then(|c| number(&c[1]));
            let actual = actual_re.captures(lower).and_then(|c| {
                c.get(1)
                    .or_else(|| c.get(2))
                    .and_then(|m| number(m.as_str()))
            });
            (actual, max)
        },
        |c| (number(&c[1]), number(&c[2])),
    );
    match (actual, max) {
        // Report the larger figure as the prompt size: a prompt that
        // overflows is by definition bigger than the window.
        (Some(a), Some(m)) if a < m => (Some(m), Some(a)),
        other => other,
    }
}

#[cfg(test)]
mod tests;
