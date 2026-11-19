use super::*;
use crate::tools::domains::DomainRules;
use crate::tools::web_search::backend::PageText;
use crate::tools::web_search::compact::{CompactRequest, Compactor};
use crate::tools::web_search::ddg::SearchHit;
use std::collections::HashMap;
use std::time::Duration;

/// A scripted world: queries map to hits (or an error); every page is fine.
#[derive(Default)]
struct Fake {
    searches: HashMap<String, Result<Vec<SearchHit>, String>>,
}

impl Compactor for Fake {
    async fn compact(&self, _: CompactRequest<'_>) -> Result<String, String> {
        Err("no model".to_owned())
    }
}

impl Backend for Fake {
    async fn search(&self, query: &str, _limit: usize) -> Result<Vec<SearchHit>, String> {
        self.searches
            .get(query)
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn fetch_page(&self, url: &str) -> Result<PageText, String> {
        Ok(PageText {
            final_url: url.to_owned(),
            title: None,
            text: format!("text of {url}"),
        })
    }
}

fn hit(url: &str) -> SearchHit {
    SearchHit {
        title: format!("title of {url}"),
        url: url.to_owned(),
        snippet: "snip".to_owned(),
    }
}

fn tool(fake: Fake) -> WebSearchTool<Fake> {
    WebSearchTool::new(
        fake,
        RunSettings {
            max_queries: 3,
            max_results_per_query: 5,
            threshold_chars: 1000,
            search_concurrency: 2,
            page_concurrency: 4,
            stagger: Duration::ZERO,
            domains: DomainRules::default(),
        },
    )
}

fn call(args: &str) -> Call {
    Call {
        call_id: "call_1".to_owned(),
        name: EXECUTOR_TOOL_NAME.to_owned(),
        arguments: args.to_owned(),
    }
}

fn raw(block: &ResponseContentBlock) -> &Value {
    match block {
        ResponseContentBlock::Raw(v) => v,
        other => panic!("expected a raw block, got {other:?}"),
    }
}

fn usage_of(tool: &WebSearchTool<Fake>) -> Usage {
    let mut usage = Usage::default();
    tool.contribute_usage(&mut usage);
    usage
}

#[tokio::test]
async fn each_query_becomes_a_matched_pair_and_the_results_reach_the_executor() {
    let mut fake = Fake::default();
    fake.searches
        .insert("a".into(), Ok(vec![hit("https://a.test/1")]));
    fake.searches
        .insert("b".into(), Ok(vec![hit("https://b.test/1")]));
    let mut tool = tool(fake);

    let answer = tool.answer(&call(r#"{"queries":["a","b"]}"#)).await;

    let blocks = &answer.served.blocks;
    assert_eq!(blocks.len(), 4);
    for (i, query) in ["a", "b"].into_iter().enumerate() {
        let (use_block, result) = (raw(&blocks[i * 2]), raw(&blocks[i * 2 + 1]));
        assert_eq!(use_block["type"], "server_tool_use");
        assert_eq!(use_block["name"], "web_search");
        assert_eq!(use_block["input"]["query"], query);
        assert_eq!(result["type"], "web_search_tool_result");
        assert_eq!(result["tool_use_id"], use_block["id"]);
    }
    assert!(answer.failures.is_empty());
    assert!(answer.served.output.contains("text of https://a.test/1"));
    assert!(answer.served.output.contains("https://b.test/1"));
}

#[tokio::test]
async fn usage_counts_every_query_searched_across_calls() {
    let mut tool = tool(Fake::default());
    assert!(
        usage_of(&tool).server_tool_use.is_none(),
        "nothing searched yet"
    );

    tool.answer(&call(r#"{"queries":["a","b"]}"#)).await;
    tool.answer(&call(r#"{"queries":["c"]}"#)).await;

    let used = usage_of(&tool).server_tool_use.expect("search usage");
    assert_eq!(used.web_search_requests, 3);
    assert_eq!(used.web_fetch_requests, 0);
}

#[tokio::test]
async fn a_failed_search_is_an_error_result_and_is_reported() {
    let mut fake = Fake::default();
    fake.searches
        .insert("blocked".into(), Err("CAPTCHA".to_owned()));
    let mut tool = tool(fake);

    let answer = tool.answer(&call(r#"{"queries":["blocked"]}"#)).await;

    let result = raw(&answer.served.blocks[1]);
    assert_eq!(result["content"]["type"], "web_search_tool_result_error");
    assert_eq!(result["content"]["error_code"], "unavailable");
    assert_eq!(
        answer.failures,
        vec![("blocked".to_owned(), "CAPTCHA".to_owned())]
    );
    assert!(answer.served.output.contains("CAPTCHA"));
}

#[tokio::test]
async fn a_call_with_no_usable_queries_is_answered_not_skipped() {
    let mut tool = tool(Fake::default());
    for args in ["not json", r#"{"queries":[]}"#, r#"{"queries":["  "]}"#] {
        let answer = tool.answer(&call(args)).await;
        assert_eq!(answer.served.blocks.len(), 2, "{args}");
        assert_eq!(
            raw(&answer.served.blocks[1])["content"]["error_code"],
            "invalid_input"
        );
        assert_eq!(answer.served.output, INVALID_INPUT);
    }
    assert!(usage_of(&tool).server_tool_use.is_none(), "no search ran");
}

#[test]
fn the_tool_answers_to_the_internal_function_name() {
    assert_eq!(tool(Fake::default()).executor_name(), "proxy_web_search");
}

#[test]
fn queries_are_read_from_the_arguments() {
    assert_eq!(
        parse_queries(r#"{"queries":["ok",3,null,"also"]}"#),
        vec!["ok", "also"]
    );
}

#[test]
fn malformed_arguments_are_an_empty_query_list() {
    for args in [
        "not json",
        r#"{"nope":1}"#,
        r#"{"queries":"x"}"#,
        r#"{"queries":[1,2]}"#,
    ] {
        assert!(parse_queries(args).is_empty(), "{args}");
    }
}

#[tokio::test]
async fn max_uses_runs_what_fits_and_refuses_the_rest_of_the_call() {
    let mut tool = tool(Fake::default()).with_max_uses(Some(1));

    let answer = tool.answer(&call(r#"{"queries":["one","two"]}"#)).await;

    let blocks = &answer.served.blocks;
    assert_eq!(blocks.len(), 4, "a pair per query");
    assert!(raw(&blocks[1])["content"].is_array(), "the first query ran");
    assert_eq!(raw(&blocks[2])["input"]["query"], "two");
    assert_eq!(
        raw(&blocks[3])["content"]["error_code"],
        "max_uses_exceeded"
    );
    assert!(answer.served.output.contains("1 queries were NOT run"));
    assert_eq!(
        usage_of(&tool).server_tool_use.unwrap().web_search_requests,
        1,
        "only the query that ran counts"
    );
}

#[tokio::test]
async fn max_uses_is_counted_across_calls() {
    let mut tool = tool(Fake::default()).with_max_uses(Some(2));
    tool.answer(&call(r#"{"queries":["a"]}"#)).await;
    tool.answer(&call(r#"{"queries":["b"]}"#)).await;

    let answer = tool.answer(&call(r#"{"queries":["c"]}"#)).await;

    assert_eq!(answer.served.blocks.len(), 2);
    assert_eq!(
        raw(&answer.served.blocks[1])["content"]["error_code"],
        "max_uses_exceeded"
    );
    assert_eq!(
        usage_of(&tool).server_tool_use.unwrap().web_search_requests,
        2
    );
}

#[tokio::test]
async fn no_client_max_uses_means_only_the_proxys_own_limits() {
    let mut tool = tool(Fake::default());
    let answer = tool.answer(&call(r#"{"queries":["a","b","c"]}"#)).await;
    assert_eq!(answer.served.blocks.len(), 6);
    assert!(!answer.served.output.contains("NOT run"));
}

#[tokio::test]
async fn queries_over_the_per_call_cap_are_still_reported() {
    // `max_queries` is 3 in the test settings.
    let mut tool = tool(Fake::default());
    let answer = tool
        .answer(&call(r#"{"queries":["a","b","c","d","e"]}"#))
        .await;
    assert!(
        answer
            .served
            .output
            .contains("2 further queries were NOT run")
    );
}
