use super::*;
use crate::tools::web_search::ddg::SearchHit;
use crate::tools::web_search::run::Page;

fn entry(title: &str, url: &str, snippet: &str, page: Result<&str, &str>) -> Entry {
    Entry {
        hit: SearchHit {
            title: title.into(),
            url: url.into(),
            snippet: snippet.into(),
        },
        page: page
            .map(|text| Page {
                final_url: url.into(),
                title: None,
                text: text.into(),
                note: None,
            })
            .map_err(str::to_owned),
    }
}

#[test]
fn the_pair_shares_an_id_and_uses_the_client_visible_name() {
    let use_block = server_tool_use("srvtoolu_1", "rust");
    assert_eq!(use_block["type"], "server_tool_use");
    assert_eq!(use_block["name"], "web_search");
    assert_eq!(use_block["input"]["query"], "rust");

    let entry = entry("T", "https://a.test/", "s", Ok("body"));
    let result = search_result("srvtoolu_1", [&entry]);
    assert_eq!(result["type"], "web_search_tool_result");
    assert_eq!(result["tool_use_id"], use_block["id"]);
    assert_eq!(result["content"][0]["type"], "web_search_result");
    assert_eq!(result["content"][0]["url"], "https://a.test/");
    assert_eq!(result["content"][0]["title"], "T");
}

#[test]
fn an_empty_result_is_an_empty_array_not_an_error() {
    let result = search_result("id", std::iter::empty());
    assert_eq!(result["content"], json!([]));
}

#[test]
fn errors_carry_an_anthropic_error_code() {
    let result = error_result("id", ERROR_UNAVAILABLE);
    assert_eq!(result["content"]["type"], "web_search_tool_result_error");
    assert_eq!(result["content"]["error_code"], "unavailable");
}

#[test]
fn each_result_carries_what_the_model_read_for_the_next_turn() {
    let entry = entry("T", "https://a.test/", "a snippet", Ok("the page body"));
    let result = search_result("id", [&entry]);
    let blob = result["content"][0]["encrypted_content"].as_str().unwrap();
    let text = history::decode(blob).expect("the proxy's own blob");
    assert!(
        text.contains("a snippet") && text.contains("the page body"),
        "{text}"
    );
    assert!(text.contains("https://a.test/"));
    assert!(result["content"][0]["page_age"].is_null());
}

#[test]
fn a_failed_page_is_replayed_as_a_failure_not_as_content() {
    let entry = entry("T", "https://a.test/", "", Err("timed out"));
    let blob = search_result("id", [&entry])["content"][0]["encrypted_content"]
        .as_str()
        .unwrap()
        .to_owned();
    let text = history::decode(&blob).unwrap();
    assert!(text.contains("failed to fetch page: timed out"), "{text}");
}
