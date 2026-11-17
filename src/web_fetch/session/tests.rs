use super::*;

use crate::responses::OutputItem;

fn function_call(name: &str, call_id: &str, arguments: &str) -> OutputItem {
    OutputItem::FunctionCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
        id: Some(format!("fc_{call_id}")),
        status: None,
    }
}

#[test]
fn finds_a_web_fetch_call() {
    let output = vec![function_call(
        "web_fetch",
        "call_1",
        r#"{"url": "https://example.com/page"}"#,
    )];
    let call = find_web_fetch_call(&output).expect("must find the call");
    assert_eq!(call.call_id, "call_1");
    assert_eq!(call.url, "https://example.com/page");
}

#[test]
fn ignores_a_call_to_another_tool() {
    let output = vec![function_call("Bash", "call_1", r#"{"command": "ls"}"#)];
    assert!(find_web_fetch_call(&output).is_none());
}

#[test]
fn a_call_with_no_url_is_not_a_fetch() {
    // A malformed call must not spin the loop: it is skipped, so the
    // executor's next response ends the turn.
    let output = vec![function_call("web_fetch", "call_1", r#"{"url": ""}"#)];
    assert!(find_web_fetch_call(&output).is_none());

    let output = vec![function_call("web_fetch", "call_1", r#"{"nope": 1}"#)];
    assert!(find_web_fetch_call(&output).is_none());

    let output = vec![function_call("web_fetch", "call_1", "not json")];
    assert!(find_web_fetch_call(&output).is_none());
}

#[test]
fn the_url_is_trimmed() {
    let output = vec![function_call(
        "web_fetch",
        "call_1",
        r#"{"url": "  https://example.com/page  "}"#,
    )];
    let call = find_web_fetch_call(&output).expect("must find the call");
    assert_eq!(call.url, "https://example.com/page");
}

#[test]
fn the_first_web_fetch_call_wins() {
    let output = vec![
        function_call("web_fetch", "call_1", r#"{"url": "https://one.example/"}"#),
        function_call("web_fetch", "call_2", r#"{"url": "https://two.example/"}"#),
    ];
    let call = find_web_fetch_call(&output).expect("must find the call");
    assert_eq!(call.url, "https://one.example/");
}
