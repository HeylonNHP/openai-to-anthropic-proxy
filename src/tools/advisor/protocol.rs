//! Pure builders for the advisor wire protocol.
//!
//! Every function here returns a [`serde_json::Value`] (or a [`String`] id)
//! and performs no I/O. The blocks are carried through the shared wire types
//! as [`crate::anthropic::ResponseContentBlock::Raw`], so no per-feature enum
//! variant is needed. See `ADVISOR_TOOL_RESEARCH.md` §§3.3, 3.5, 11.5, 11.6.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::responses::ResponsesUsage;

/// The six error codes Claude Code's renderer knows (§3.3). Emitting an
/// unknown one renders "Advisor unavailable (unknown)".
pub const ERROR_CODE_MAX_USES_EXCEEDED: &str = "max_uses_exceeded";
pub const ERROR_CODE_TOO_MANY_REQUESTS: &str = "too_many_requests";
pub const ERROR_CODE_OVERLOADED: &str = "overloaded";
pub const ERROR_CODE_PROMPT_TOO_LONG: &str = "prompt_too_long";
pub const ERROR_CODE_EXECUTION_TIME_EXCEEDED: &str = "execution_time_exceeded";
/// The generic, fail-closed code.
///
/// Every advisor failure degrades to this so the client never sees a 400
/// naming the advisor tool/beta (which would latch the feature off for the
/// whole Claude Code process, §11.5).
pub const ERROR_CODE_UNAVAILABLE: &str = "unavailable";
/// Listed in Anthropic's SDK but **not** recognised by Claude Code. Kept as
/// a constant only so a future version can avoid inventing a string; it
/// must never be emitted on the wire.
pub const ERROR_CODE_MODEL_NOT_FOUND: &str = "model_not_found";

/// All client-known error codes, in the documentation's order.
pub const ADVISOR_ERROR_CODES: &[&str] = &[
    ERROR_CODE_MAX_USES_EXCEEDED,
    ERROR_CODE_TOO_MANY_REQUESTS,
    ERROR_CODE_OVERLOADED,
    ERROR_CODE_PROMPT_TOO_LONG,
    ERROR_CODE_EXECUTION_TIME_EXCEEDED,
    ERROR_CODE_UNAVAILABLE,
];

/// The `server_tool_use` block that opens one consultation.
///
/// `name` must be exactly `"advisor"` — the client matches on that string.
/// `input` is always `{}`: the advisor tool takes no arguments.
#[must_use]
pub fn server_tool_use(id: &str) -> Value {
    json!({
        "type": "server_tool_use",
        "id": id,
        "name": "advisor",
        "input": {},
    })
}

/// A successful `advisor_tool_result` block. `tool_use_id` must equal the
/// `id` on the preceding [`server_tool_use`] block.
///
/// A proxy must emit `advisor_result` (plaintext), **never**
/// `advisor_redacted_result`: the encrypted blob is Anthropic-server-only
/// and cannot be manufactured, so emitting it would show the user a
/// "reviewed" message while the executor never saw any advice.
#[must_use]
pub fn advisor_result(id: &str, text: &str) -> Value {
    json!({
        "type": "advisor_tool_result",
        "tool_use_id": id,
        "content": {
            "type": "advisor_result",
            "text": text,
        },
    })
}

/// A fail-closed `advisor_tool_result` block with a specific client-known
/// error code. See [`unavailable`] for the usual choice.
#[must_use]
pub fn error_result(id: &str, error_code: &str) -> Value {
    // An unknown code renders "Advisor unavailable (unknown)" in Claude
    // Code (§3.3). Keeping the client-known list referenced here also
    // catches a future code accidentally added to the SDK-only list.
    debug_assert!(
        ADVISOR_ERROR_CODES.contains(&error_code),
        "advisor error code must be one Claude Code knows"
    );
    debug_assert_ne!(
        error_code, ERROR_CODE_MODEL_NOT_FOUND,
        "Claude Code does not know the SDK-only model_not_found code"
    );
    json!({
        "type": "advisor_tool_result",
        "tool_use_id": id,
        "content": {
            "type": "advisor_tool_result_error",
            "error_code": error_code,
        },
    })
}

/// The fail-closed `advisor_tool_result` block. Always uses the generic
/// `unavailable` error code so Claude Code renders a known message instead
/// of latching the advisor off for a 400 that names the tool.
#[must_use]
pub fn unavailable(id: &str) -> Value {
    error_result(id, ERROR_CODE_UNAVAILABLE)
}

/// The cap-hit `advisor_tool_result` block. When the executor keeps asking
/// for advice past `max_uses`, the client should say "too many consultations"
/// rather than the generic "unavailable".
#[must_use]
pub fn max_uses_exceeded(id: &str) -> Value {
    error_result(id, ERROR_CODE_MAX_USES_EXCEEDED)
}

/// Collision-resistant id for one consultation.
///
/// `srvtoolu_` is the real prefix; Claude Code does no format validation
/// beyond non-empty + equal `tool_use_id`, but matching the real shape keeps
/// logs honest.
///
/// Uses `SystemTime` nanoseconds plus a process-local atomic counter — no
/// new dependency, no `rand` crate. The counter makes ids unique even when
/// two calls land in the same nanosecond (or the clock is coarse).
#[must_use]
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("srvtoolu_{nanos:x}_{n:x}")
}

/// Token counts shared by both iteration shapes. Kept as a private helper
/// so `advisor_iteration` and `message_iteration` cannot drift apart.
fn token_counts(usage: &ResponsesUsage) -> (u32, u32, u32, u32) {
    let details = usage.input_tokens_details.as_ref();
    (
        // Net of the cache fields, as Anthropic reports it.
        usage.net_input_tokens(),
        usage.output_tokens,
        details.map_or(0, |d| d.cached_tokens),
        details.map_or(0, |d| d.cache_write_tokens),
    )
}

/// An `advisor_message` entry for `usage.iterations[]`.
///
/// Advisor tokens are billed at the advisor model's rate and must NOT be folded
/// into the top-level totals (§3.5). `cache_creation` is the explicit `null` Claude
/// Code sees on the wire for an advisor iteration.
#[must_use]
pub fn advisor_iteration(model: &str, usage: &ResponsesUsage) -> Value {
    let (input, output, cache_read, cache_creation) = token_counts(usage);
    json!({
        "type": "advisor_message",
        "model": model,
        "input_tokens": input,
        "output_tokens": output,
        "cache_read_input_tokens": cache_read,
        "cache_creation_input_tokens": cache_creation,
        "cache_creation": null,
    })
}

/// A `message` entry for `usage.iterations[]` (an executor sampling
/// iteration inside the same API request).
#[must_use]
pub fn message_iteration(usage: &ResponsesUsage) -> Value {
    let (input, output, cache_read, cache_creation) = token_counts(usage);
    json!({
        "type": "message",
        "input_tokens": input,
        "output_tokens": output,
        "cache_read_input_tokens": cache_read,
        "cache_creation_input_tokens": cache_creation,
    })
}

#[cfg(test)]
mod tests;
