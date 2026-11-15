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
/// The generic, fail-closed code. Every advisor failure degrades to this so
/// the client never sees a 400 naming the advisor tool/beta (which would
/// latch the feature off for the whole Claude Code process, §11.5).
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

/// Collision-resistant id for one consultation. `srvtoolu_` is the real
/// prefix; Claude Code does no format validation beyond non-empty + equal
/// `tool_use_id`, but matching the real shape keeps logs honest.
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
        usage.input_tokens,
        usage.output_tokens,
        details.map_or(0, |d| d.cached_tokens),
        details.map_or(0, |d| d.cache_write_tokens),
    )
}

/// An `advisor_message` entry for `usage.iterations[]`. Advisor tokens are
/// billed at the advisor model's rate and must NOT be folded into the
/// top-level totals (§3.5). `cache_creation` is the explicit `null` Claude
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
mod tests {
    use super::*;
    use crate::responses::InputTokensDetails;

    fn usage() -> ResponsesUsage {
        ResponsesUsage {
            input_tokens: 823,
            output_tokens: 1612,
            total_tokens: 2435,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 7,
                cache_write_tokens: 11,
            }),
            output_tokens_details: None,
        }
    }

    fn usage_without_details() -> ResponsesUsage {
        ResponsesUsage {
            input_tokens: 12,
            output_tokens: 34,
            total_tokens: 46,
            input_tokens_details: None,
            output_tokens_details: None,
        }
    }

    #[test]
    fn server_tool_use_shape_is_exact() {
        // Exactly four fields, no input_schema / description / max_uses.
        let v = server_tool_use("srvtoolu_abc123");
        assert_eq!(
            v,
            json!({
                "type": "server_tool_use",
                "id": "srvtoolu_abc123",
                "name": "advisor",
                "input": {},
            })
        );
        // `json!` builds a BTreeMap-backed object, so serialization is
        // key-sorted. Assert on the parsed value instead of byte order —
        // the wire contract is the field set, not the key sequence.
        assert_eq!(
            serde_json::from_str::<Value>(&serde_json::to_string(&v).unwrap()).unwrap(),
            v
        );
    }

    #[test]
    fn advisor_result_shape_is_exact() {
        let v = advisor_result("srvtoolu_abc123", "Use a channel-based pattern…");
        assert_eq!(
            v,
            json!({
                "type": "advisor_tool_result",
                "tool_use_id": "srvtoolu_abc123",
                "content": {
                    "type": "advisor_result",
                    "text": "Use a channel-based pattern…",
                },
            })
        );
        assert_eq!(v["content"]["type"], "advisor_result");
        assert_eq!(v["content"]["text"], "Use a channel-based pattern…");
        // Never the encrypted variant, which a proxy cannot manufacture.
        assert_ne!(v["content"]["type"], "advisor_redacted_result");
    }

    #[test]
    fn unavailable_shape_is_exact_and_uses_a_known_code() {
        let v = unavailable("srvtoolu_xyz");
        assert_eq!(
            v,
            json!({
                "type": "advisor_tool_result",
                "tool_use_id": "srvtoolu_xyz",
                "content": {
                    "type": "advisor_tool_result_error",
                    "error_code": "unavailable",
                },
            })
        );
        assert!(
            ADVISOR_ERROR_CODES.contains(&v["content"]["error_code"].as_str().unwrap()),
            "the fail-closed code must be one the client renders"
        );
    }

    #[test]
    fn max_uses_exceeded_is_a_known_error_code() {
        let v = max_uses_exceeded("srvtoolu_cap");
        assert_eq!(v["content"]["type"], "advisor_tool_result_error");
        assert_eq!(v["content"]["error_code"], "max_uses_exceeded");
        assert_eq!(v["tool_use_id"], "srvtoolu_cap");
        assert!(ADVISOR_ERROR_CODES.contains(&v["content"]["error_code"].as_str().unwrap()));
        assert_eq!(
            v,
            error_result("srvtoolu_cap", ERROR_CODE_MAX_USES_EXCEEDED)
        );
    }

    #[test]
    fn result_and_unavailable_share_the_same_envelope() {
        let id = "srvtoolu_pair";
        let ok = advisor_result(id, "advice");
        let err = unavailable(id);
        assert_eq!(ok["type"], err["type"]);
        assert_eq!(ok["tool_use_id"], err["tool_use_id"]);
        assert_eq!(ok["content"]["type"], "advisor_result");
        assert_eq!(err["content"]["type"], "advisor_tool_result_error");
    }

    #[test]
    fn ids_pair_between_use_and_result() {
        let id = new_id();
        let use_block = server_tool_use(&id);
        let result = advisor_result(&id, "advice");
        assert_eq!(use_block["id"], result["tool_use_id"]);
        assert!(result["tool_use_id"].as_str().unwrap().len() > 8);
        let err = unavailable(&id);
        assert_eq!(use_block["id"], err["tool_use_id"]);
    }

    #[test]
    fn new_id_has_the_real_prefix_and_is_unique() {
        let ids: Vec<String> = (0..64).map(|_| new_id()).collect();
        assert!(ids.iter().all(|s| s.starts_with("srvtoolu_")));
        assert!(
            ids.iter().all(|s| s.len() > "srvtoolu_".len()),
            "ids must be non-empty beyond the prefix"
        );
        let unique: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        assert_eq!(unique.len(), ids.len(), "ids must not collide");
        // No crate-level randomness: ids are hex, not opaque base64.
        assert!(ids.iter().all(|s| {
            s.trim_start_matches("srvtoolu_")
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '_')
        }));
    }

    #[test]
    fn error_code_constants_are_exactly_the_six_client_known_codes() {
        assert_eq!(
            ADVISOR_ERROR_CODES,
            &[
                "max_uses_exceeded",
                "too_many_requests",
                "overloaded",
                "prompt_too_long",
                "execution_time_exceeded",
                "unavailable",
            ]
        );
        assert_eq!(ADVISOR_ERROR_CODES.len(), 6);
        // The SDK-only seventh code is deliberately NOT in the client list.
        assert_eq!(ERROR_CODE_MODEL_NOT_FOUND, "model_not_found");
        assert!(!ADVISOR_ERROR_CODES.contains(&ERROR_CODE_MODEL_NOT_FOUND));
    }

    #[test]
    fn advisor_iteration_shape_is_exact() {
        let v = advisor_iteration("beam", &usage());
        assert_eq!(
            v,
            json!({
                "type": "advisor_message",
                "model": "beam",
                "input_tokens": 823,
                "output_tokens": 1612,
                "cache_read_input_tokens": 7,
                "cache_creation_input_tokens": 11,
                "cache_creation": null,
            })
        );
        assert!(v["cache_creation"].is_null());
        assert_eq!(
            serde_json::from_str::<Value>(&serde_json::to_string(&v).unwrap()).unwrap(),
            v
        );
    }

    #[test]
    fn message_iteration_shape_is_exact() {
        let v = message_iteration(&usage());
        assert_eq!(
            v,
            json!({
                "type": "message",
                "input_tokens": 823,
                "output_tokens": 1612,
                "cache_read_input_tokens": 7,
                "cache_creation_input_tokens": 11,
            })
        );
        assert!(
            v.get("model").is_none(),
            "a message iteration has no model field"
        );
    }

    #[test]
    fn iterations_have_zero_cache_counts_when_details_are_absent() {
        let u = usage_without_details();
        for v in [advisor_iteration("beam", &u), message_iteration(&u)] {
            assert_eq!(v["input_tokens"], 12);
            assert_eq!(v["output_tokens"], 34);
            assert_eq!(v["cache_read_input_tokens"], 0);
            assert_eq!(v["cache_creation_input_tokens"], 0);
        }
    }

    #[test]
    fn iteration_types_are_distinct_and_billing_fields_match() {
        let u = usage();
        let advisor = advisor_iteration("beam", &u);
        let message = message_iteration(&u);
        assert_eq!(advisor["type"], "advisor_message");
        assert_eq!(message["type"], "message");
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            assert_eq!(advisor[field], message[field], "{field} must match");
        }
    }

    #[test]
    fn blocks_serialize_verbatim_through_the_raw_extension_point() {
        // §11.6: the module's Values travel through the shared enums with
        // no per-feature variants and no byte changes.
        use crate::anthropic::{ContentBlockKind, ResponseContentBlock};
        let id = new_id();
        for v in [
            server_tool_use(&id),
            advisor_result(&id, "advice"),
            unavailable(&id),
        ] {
            assert_eq!(
                serde_json::to_value(ResponseContentBlock::Raw(v.clone())).unwrap(),
                v
            );
            assert_eq!(
                serde_json::to_value(ContentBlockKind::Raw(v.clone())).unwrap(),
                v
            );
        }
    }
}
