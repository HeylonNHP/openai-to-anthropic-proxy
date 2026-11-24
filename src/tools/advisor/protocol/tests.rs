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
            "input_tokens": 805,
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
            "input_tokens": 805,
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
    // The module's Values travel through the shared enums with
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
