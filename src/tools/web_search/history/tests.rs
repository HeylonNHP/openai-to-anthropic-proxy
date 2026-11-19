use super::*;
use serde_json::json;

#[test]
fn text_survives_an_encode_decode_round_trip() {
    for text in ["plain", "", "multi\nline\n\ttabs", "unicode: é 日本語 🦀"] {
        assert_eq!(decode(&encode(text)).as_deref(), Some(text));
    }
}

#[test]
fn a_blob_the_proxy_did_not_write_does_not_decode() {
    for blob in [
        "",
        "opaque-not-ours",
        "EqgfCkYIARgB...",
        "oap1",
        "oap1:!!!not base64!!!",
    ] {
        assert!(decode(blob).is_none(), "{blob}");
    }
}

#[test]
fn a_blob_that_decodes_to_invalid_utf8_is_rejected() {
    let bad = format!("{MAGIC}{}", STANDARD.encode([0xff, 0xfe, 0xfd]));
    assert!(decode(&bad).is_none());
}

#[test]
fn an_oversized_result_is_capped_with_a_note() {
    let text = "x".repeat(MAX_REPLAY_CHARS + 500);
    let back = decode(&encode(&text)).unwrap();
    assert!(back.ends_with("[content truncated]"));
    assert!(back.chars().count() < MAX_REPLAY_CHARS + 30);
}

#[test]
fn a_result_exactly_at_the_cap_is_kept_whole() {
    let text = "y".repeat(MAX_REPLAY_CHARS);
    assert_eq!(decode(&encode(&text)).unwrap(), text);
}

#[test]
fn the_cap_never_splits_a_multibyte_character() {
    let text = "é".repeat(MAX_REPLAY_CHARS + 10);
    assert!(decode(&encode(&text)).is_some(), "still valid UTF-8");
}

#[test]
fn the_proxys_own_results_are_replayed_in_full() {
    let content = json!([
        {"type": "web_search_result", "url": "https://a.test/", "title": "A",
         "encrypted_content": encode("title: A\nsnippet: first snippet\ntext: page body")},
    ]);
    let out = replay_text(Some("rust news"), &content);
    assert!(out.contains("for the query \"rust news\""), "{out}");
    assert!(
        out.contains("first snippet") && out.contains("page body"),
        "{out}"
    );
    assert!(out.starts_with("\n[Web search results retrieved earlier"));
    assert!(out.ends_with("]\n"));
}

#[test]
fn a_foreign_result_falls_back_to_its_title_and_url() {
    let content = json!([
        {"type": "web_search_result", "url": "https://blog.rust-lang.org/2026",
         "title": "Rust 2026 released", "page_age": null, "encrypted_content": "opaque-not-ours"},
        {"type": "web_search_result", "url": "https://b.test/", "title": "", "encrypted_content": ""},
    ]);
    let out = replay_text(None, &content);
    assert!(
        out.contains("- Rust 2026 released — https://blog.rust-lang.org/2026"),
        "{out}"
    );
    assert!(out.contains("- https://b.test/"), "{out}");
    assert!(
        !out.contains("opaque-not-ours"),
        "the blob itself is never shown"
    );
    assert!(!out.contains("for the query"), "no query was known");
}

#[test]
fn ours_and_foreign_results_can_share_one_block() {
    let content = json!([
        {"url": "https://a.test/", "title": "A", "encrypted_content": encode("full text of A")},
        {"url": "https://b.test/", "title": "B", "encrypted_content": "nope"},
    ]);
    let out = replay_text(Some("q"), &content);
    assert!(
        out.contains("full text of A") && out.contains("- B — https://b.test/"),
        "{out}"
    );
}

#[test]
fn nothing_is_replayed_for_an_error_or_an_empty_search() {
    let error = json!({"type": "web_search_tool_result_error", "error_code": "unavailable"});
    assert_eq!(replay_text(Some("q"), &error), "");
    assert_eq!(replay_text(Some("q"), &json!([])), "");
    assert_eq!(replay_text(Some("q"), &json!("junk")), "");
}

#[test]
fn a_result_with_no_url_and_no_blob_is_skipped() {
    let content = json!([{"type": "web_search_result", "title": "no url"}]);
    assert_eq!(replay_text(Some("q"), &content), "");
}

#[test]
fn a_blank_query_is_left_out_of_the_header() {
    let content = json!([{"url": "https://a.test/", "title": "A"}]);
    assert!(!replay_text(Some("   "), &content).contains("for the query"));
}
