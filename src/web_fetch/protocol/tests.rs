use super::*;

#[test]
fn server_tool_use_names_the_tool_and_carries_the_url() {
    let v = server_tool_use("srvtoolu_abc", "https://example.com/x");
    assert_eq!(v["type"], "server_tool_use");
    assert_eq!(v["id"], "srvtoolu_abc");
    assert_eq!(v["name"], "web_fetch");
    assert_eq!(v["input"]["url"], "https://example.com/x");
}

#[test]
fn ids_match_the_anthropic_pattern() {
    let id = new_id();
    assert!(id.starts_with("srvtoolu_"), "bad id: {id}");
    // The pattern is ^srvtoolu_[a-zA-Z0-9_]+$.
    let suffix = id.strip_prefix("srvtoolu_").expect("prefix");
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "bad id: {id}"
    );
}

#[test]
fn ids_are_unique() {
    let a = new_id();
    let b = new_id();
    assert_ne!(a, b);
}

#[test]
fn a_success_result_pairs_with_its_use() {
    let id = new_id();
    let use_block = server_tool_use(&id, "https://example.com/x");
    let result = web_fetch_result(&id, "https://example.com/x", Some("Title"), "Body text");

    assert_eq!(use_block["id"], result["tool_use_id"]);
    assert_eq!(result["type"], "web_fetch_tool_result");
    assert_eq!(result["content"]["type"], "web_fetch_result");
    assert_eq!(result["content"]["url"], "https://example.com/x");
    assert_eq!(result["content"]["content"]["type"], "document");
    assert_eq!(result["content"]["content"]["source"]["type"], "text");
    assert_eq!(
        result["content"]["content"]["source"]["media_type"],
        "text/plain"
    );
    assert_eq!(result["content"]["content"]["source"]["data"], "Body text");
    assert_eq!(result["content"]["content"]["title"], "Title");
    assert!(result["content"]["retrieved_at"].is_string());
}

#[test]
fn a_result_without_a_title_omits_the_title() {
    let result = web_fetch_result("srvtoolu_1", "https://example.com/x", None, "Body");
    assert!(
        result["content"]["content"].get("title").is_none(),
        "an absent title must not be emitted as null"
    );
}

#[test]
fn an_error_result_carries_a_known_code() {
    let result = error_result("srvtoolu_1", ERROR_CODE_URL_NOT_ACCESSIBLE);
    assert_eq!(result["type"], "web_fetch_tool_result");
    assert_eq!(result["tool_use_id"], "srvtoolu_1");
    assert_eq!(result["content"]["type"], "web_fetch_tool_result_error");
    assert_eq!(result["content"]["error_code"], "url_not_accessible");
}

#[test]
fn the_timestamp_is_iso8601() {
    let ts = now_iso8601();
    assert!(ts.ends_with('Z'), "not a UTC timestamp: {ts}");
    assert_eq!(ts.len(), 20, "not an ISO-8601 second-precision stamp: {ts}");
    // YYYY-MM-DDTHH:MM:SSZ
    let (date, time) = ts.split_once('T').expect("date/time separator");
    assert_eq!(date.len(), 10, "bad date: {date}");
    assert_eq!(time.len(), 9, "bad time: {time}");
}

#[test]
fn the_epoch_formats_correctly() {
    // 1970-01-01T00:00:00Z
    assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
    // 2001-09-09T01:46:40Z — a well-known Unix timestamp.
    assert_eq!(format_epoch(1_000_000_000), "2001-09-09T01:46:40Z");
    // 2026-01-01T00:00:00Z
    assert_eq!(format_epoch(1_767_225_600), "2026-01-01T00:00:00Z");
}
