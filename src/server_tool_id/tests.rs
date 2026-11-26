use super::*;

/// Anthropic's Messages API constrains a `server_tool_use` id to the
/// `srvtoolu_` prefix plus `[a-zA-Z0-9_]`. The API checks that shape on
/// INBOUND history, so an id that does not match is rejected when the
/// client replays the conversation against Anthropic itself.
#[test]
fn generated_ids_match_the_anthropic_pattern() {
    let id = new_server_tool_id();
    assert!(
        is_server_tool_id(&id),
        "id does not match {SERVER_TOOL_ID_PATTERN}: {id}"
    );
    assert!(id.starts_with(SERVER_TOOL_ID_PREFIX), "bad id: {id}");
}

/// The pattern, spelled out verbatim from Anthropic's request schema.
///
/// Every other check in this file — the regex one, the validator's, and
/// the end-to-end one — derives from `SERVER_TOOL_ID_PATTERN`, so they
/// could all drift together. This anchors the chain at the documented
/// rule instead.
#[test]
fn the_pattern_is_the_one_anthropic_documents() {
    assert_eq!(SERVER_TOOL_ID_PATTERN, "^srvtoolu_[a-zA-Z0-9_]+$");
    assert_eq!(SERVER_TOOL_ID_PREFIX, "srvtoolu_");
}

/// The pattern that actually reaches Anthropic's validator. Checked
/// against the literal regex so a change to `is_server_tool_id` cannot
/// quietly disagree with the API's rule.
#[test]
fn generated_ids_match_the_pattern_regex() {
    let re = regex::Regex::new(SERVER_TOOL_ID_PATTERN).expect("valid pattern");
    for _ in 0..64 {
        let id = new_server_tool_id();
        assert!(re.is_match(&id), "id does not match the regex: {id}");
    }
}

/// Ids must not repeat: Anthropic pairs a `server_tool_use` with its
/// result block BY ID, so a repeated id makes the pairing ambiguous.
#[test]
fn generated_ids_are_unique() {
    let ids: Vec<String> = (0..256).map(|_| new_server_tool_id()).collect();
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "ids must not collide");
}

/// The validator must reject the shapes that would fail at Anthropic:
/// a missing `srvtoolu_` prefix, an empty suffix, and characters the
/// pattern does not allow.
#[test]
fn the_validator_rejects_non_conforming_ids() {
    assert!(!is_server_tool_id("stoolu_web_search_01"));
    assert!(!is_server_tool_id("toolu_01abcdefghijklmnopqrstuvwx"));
    assert!(!is_server_tool_id("srvtoolu_"));
    assert!(!is_server_tool_id("srvtoolu_a-b"));
    assert!(!is_server_tool_id(""));
    assert!(!is_server_tool_id("srvtoolu_a b"));
}

/// And it must accept what the generator produces, plus the shape
/// Anthropic's own API mints.
#[test]
fn the_validator_accepts_conforming_ids() {
    assert!(is_server_tool_id("srvtoolu_01NcFN16bsKymDW6LYVnJFAM"));
    assert!(is_server_tool_id("srvtoolu_18eb24de4cc6e3c6_0"));
    assert!(is_server_tool_id("srvtoolu_a"));
}

/// The generator is the single source of server-tool ids, so the two
/// tools that mint their own must produce ids the API accepts too.
#[test]
fn the_tools_that_mint_their_own_ids_produce_conforming_ids() {
    for id in [
        crate::tools::web_fetch::protocol::new_id(),
        crate::tools::advisor::protocol::new_id(),
    ] {
        assert!(
            is_server_tool_id(&id),
            "id does not match {SERVER_TOOL_ID_PATTERN}: {id}"
        );
    }
}
