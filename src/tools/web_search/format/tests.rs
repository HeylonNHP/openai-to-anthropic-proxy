use super::*;
use crate::tools::web_search::ddg::SearchHit;
use crate::tools::web_search::run::{Page, QueryReport};

fn entry(url: &str, page: Result<Page, String>) -> Entry {
    Entry {
        hit: SearchHit {
            title: "Hit Title".into(),
            url: url.into(),
            snippet: "a snippet".into(),
        },
        page,
    }
}

fn page(final_url: &str, text: &str, note: Option<&str>) -> Page {
    Page {
        final_url: final_url.into(),
        title: Some("Page Title".into()),
        text: text.into(),
        note: note.map(str::to_owned),
    }
}

#[test]
fn renders_results_and_a_numbered_sources_list() {
    let report = Report {
        queries: vec![QueryReport {
            query: "rust async".into(),
            outcome: Ok(vec![
                entry(
                    "https://a.test/",
                    Ok(page("https://a.test/", "body a", None)),
                ),
                entry(
                    "https://b.test/",
                    Ok(page("https://b.test/x", "body b", Some("[Note: cut]"))),
                ),
            ]),
        }],
        dropped_queries: 0,
    };
    let text = render(&report);
    assert!(text.starts_with("web_search_results:"));
    assert!(text.contains("queries_used: 1"));
    assert!(text.contains("query: rust async\nresults: 2"));
    assert!(text.contains("result_1_url: https://a.test/"));
    assert!(text.contains("result_1_text:\nbody a"));
    assert!(text.contains("result_2_final_url: https://b.test/x"));
    assert!(text.contains("[Note: cut]"));
    assert!(text.contains("[1] Page Title — https://a.test/"));
    assert!(text.contains("[2] Page Title — https://b.test/x"));
}

#[test]
fn a_failed_search_and_a_failed_page_are_visible_to_the_model() {
    let report = Report {
        queries: vec![
            QueryReport {
                query: "blocked".into(),
                outcome: Err("bot challenge".into()),
            },
            QueryReport {
                query: "ok".into(),
                outcome: Ok(vec![entry("https://x.test/", Err("timed out".into()))]),
            },
        ],
        dropped_queries: 0,
    };
    let text = render(&report);
    assert!(text.contains("query: blocked\nresults: 0\nsearch_error: bot challenge"));
    assert!(text.contains("(failed to fetch page: timed out)"));
    // A failed page's search URL is still citable.
    assert!(text.contains("[1] Hit Title — https://x.test/"));
}

#[test]
fn sources_are_deduplicated_across_queries() {
    let shared = || entry("https://s.test/", Ok(page("https://s.test/", "t", None)));
    let report = Report {
        queries: vec![
            QueryReport {
                query: "a".into(),
                outcome: Ok(vec![shared()]),
            },
            QueryReport {
                query: "b".into(),
                outcome: Ok(vec![shared()]),
            },
        ],
        dropped_queries: 0,
    };
    let text = render(&report);
    assert_eq!(text.matches("— https://s.test/").count(), 1);
}

#[test]
fn dropped_queries_are_disclosed() {
    let report = Report {
        queries: vec![QueryReport {
            query: "a".into(),
            outcome: Ok(vec![]),
        }],
        dropped_queries: 2,
    };
    let text = render(&report);
    assert!(text.contains("2 further queries were NOT run because only 1 are allowed"));
    assert!(!text.contains("SOURCES"));
}
