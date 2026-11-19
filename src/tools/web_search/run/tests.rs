use super::*;
use crate::tools::domains::DomainRules;
use crate::tools::web_search::backend::PageText;
use crate::tools::web_search::compact::{CompactRequest, Compactor};
use std::sync::Mutex;

/// A scripted world: `searches` maps a query to its hits (or an error),
/// `pages` maps a URL to its text (or an error).
#[derive(Default)]
struct Fake {
    searches: HashMap<String, Result<Vec<SearchHit>, String>>,
    pages: HashMap<String, Result<String, String>>,
    fetched: Mutex<Vec<String>>,
    searched: Mutex<Vec<(String, usize)>>,
    compact_to: Option<String>,
}

fn hit(url: &str) -> SearchHit {
    SearchHit {
        title: format!("title of {url}"),
        url: url.to_owned(),
        snippet: "snip".to_owned(),
    }
}

impl Compactor for Fake {
    async fn compact(&self, _: CompactRequest<'_>) -> Result<String, String> {
        self.compact_to.clone().ok_or_else(|| "no model".to_owned())
    }
}

impl Backend for Fake {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        self.searched
            .lock()
            .unwrap()
            .push((query.to_owned(), limit));
        self.searches
            .get(query)
            .cloned()
            .unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn fetch_page(&self, url: &str) -> Result<PageText, String> {
        self.fetched.lock().unwrap().push(url.to_owned());
        self.pages
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err("404".to_owned()))
            .map(|text| PageText {
                final_url: format!("{url}#final"),
                title: Some("T".to_owned()),
                text,
            })
    }
}

fn settings() -> RunSettings {
    RunSettings {
        max_queries: 3,
        max_results_per_query: 5,
        threshold_chars: 50,
        search_concurrency: 2,
        page_concurrency: 4,
        stagger: Duration::ZERO,
        domains: DomainRules::default(),
    }
}

fn q(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn queries_are_trimmed_deduplicated_and_capped() {
    let (kept, dropped) =
        normalize_queries(&q(&["  rust  ", "", "Rust", "a  b", "c", "d", "e"]), 3);
    assert_eq!(kept, vec!["rust", "a b", "c"]);
    assert_eq!(dropped, 2);
}

#[tokio::test]
async fn searches_every_query_and_passes_the_result_limit() {
    let mut f = Fake::default();
    f.searches
        .insert("a".into(), Ok(vec![hit("https://a.test/1")]));
    f.searches
        .insert("b".into(), Ok(vec![hit("https://b.test/1")]));
    f.pages
        .insert("https://a.test/1".into(), Ok("page a".into()));
    f.pages
        .insert("https://b.test/1".into(), Ok("page b".into()));

    let report = run(&f, &settings(), &q(&["a", "b"])).await;

    assert_eq!(report.queries.len(), 2);
    assert_eq!(report.dropped_queries, 0);
    let a = report.queries[0].outcome.as_ref().unwrap();
    assert_eq!(a[0].page.as_ref().unwrap().text, "page a");
    assert_eq!(
        a[0].page.as_ref().unwrap().final_url,
        "https://a.test/1#final"
    );
    assert!(a[0].page.as_ref().unwrap().note.is_none());
    let mut searched = f.searched.lock().unwrap().clone();
    searched.sort();
    assert_eq!(searched, vec![("a".into(), 5), ("b".into(), 5)]);
}

#[tokio::test]
async fn a_url_returned_by_two_queries_is_fetched_once() {
    let mut f = Fake::default();
    let shared = hit("https://shared.test/x");
    f.searches.insert("a".into(), Ok(vec![shared.clone()]));
    f.searches.insert("b".into(), Ok(vec![shared]));
    f.pages
        .insert("https://shared.test/x".into(), Ok("body".into()));

    let report = run(&f, &settings(), &q(&["a", "b"])).await;

    assert_eq!(f.fetched.lock().unwrap().len(), 1);
    for query in &report.queries {
        let entries = query.outcome.as_ref().unwrap();
        assert_eq!(entries[0].page.as_ref().unwrap().text, "body");
    }
}

#[tokio::test]
async fn a_failed_search_does_not_discard_the_other_queries() {
    let mut f = Fake::default();
    f.searches.insert("bad".into(), Err("captcha".into()));
    f.searches
        .insert("good".into(), Ok(vec![hit("https://g.test/")]));
    f.pages.insert("https://g.test/".into(), Ok("ok".into()));

    let report = run(&f, &settings(), &q(&["bad", "good"])).await;

    assert_eq!(report.queries[0].outcome, Err("captcha".to_owned()));
    assert!(report.queries[1].outcome.is_ok());
}

#[tokio::test]
async fn a_failed_page_is_kept_as_an_error_entry() {
    let mut f = Fake::default();
    f.searches.insert(
        "a".into(),
        Ok(vec![hit("https://ok.test/"), hit("https://gone.test/")]),
    );
    f.pages.insert("https://ok.test/".into(), Ok("fine".into()));

    let report = run(&f, &settings(), &q(&["a"])).await;

    let entries = report.queries[0].outcome.as_ref().unwrap();
    assert!(entries[0].page.is_ok());
    assert_eq!(entries[1].page, Err("404".to_owned()));
}

#[tokio::test]
async fn long_pages_are_truncated_with_a_note_when_there_is_no_model() {
    let mut f = Fake::default();
    f.searches
        .insert("a".into(), Ok(vec![hit("https://long.test/")]));
    f.pages
        .insert("https://long.test/".into(), Ok("w".repeat(500)));

    let report = run(&f, &settings(), &q(&["a"])).await;

    let page = report.queries[0].outcome.as_ref().unwrap()[0]
        .page
        .as_ref()
        .unwrap();
    assert_eq!(page.text.chars().count(), 50);
    assert!(page.note.as_ref().unwrap().contains("NOT summarised"));
}

#[tokio::test]
async fn long_pages_are_summarised_when_a_model_is_available() {
    let mut f = Fake {
        compact_to: Some("tiny summary".into()),
        ..Fake::default()
    };
    f.searches
        .insert("a".into(), Ok(vec![hit("https://long.test/")]));
    f.pages
        .insert("https://long.test/".into(), Ok("w".repeat(500)));

    let report = run(&f, &settings(), &q(&["a"])).await;

    let page = report.queries[0].outcome.as_ref().unwrap()[0]
        .page
        .as_ref()
        .unwrap();
    assert_eq!(page.text, "tiny summary");
    assert!(page.note.as_ref().unwrap().contains("summarised from 500"));
}

#[tokio::test]
async fn queries_beyond_the_cap_are_reported_not_run() {
    let f = Fake::default();
    let report = run(&f, &settings(), &q(&["a", "b", "c", "d", "e"])).await;
    assert_eq!(report.queries.len(), 3);
    assert_eq!(report.dropped_queries, 2);
    assert_eq!(f.searched.lock().unwrap().len(), 3);
}

fn rules(allowed: &[&str], blocked: &[&str]) -> DomainRules {
    let list = |items: &[&str]| {
        (!items.is_empty()).then(|| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    };
    DomainRules::from_options(&crate::anthropic::ServerToolOptions {
        allowed_domains: list(allowed),
        blocked_domains: list(blocked),
        ..crate::anthropic::ServerToolOptions::default()
    })
}

#[tokio::test]
async fn the_engine_is_asked_with_the_domain_operators_but_the_report_keeps_the_query() {
    let f = Fake::default();
    let settings = RunSettings {
        domains: rules(&["rust-lang.org"], &[]),
        ..settings()
    };

    let report = run(&f, &settings, &q(&["async rust"])).await;

    assert_eq!(
        report.queries[0].query, "async rust",
        "what the client sees"
    );
    let searched = f.searched.lock().unwrap().clone();
    assert_eq!(
        searched[0].0, "async rust site:rust-lang.org",
        "what the engine gets"
    );
}

#[tokio::test]
async fn hits_outside_the_allowed_domains_are_dropped_and_never_fetched() {
    let mut f = Fake::default();
    // The engine is keyed by the query it is actually sent.
    f.searches.insert(
        "q site:rust-lang.org".into(),
        Ok(vec![
            hit("https://docs.rust-lang.org/a"),
            hit("https://evil.test/b"),
        ]),
    );
    f.pages
        .insert("https://docs.rust-lang.org/a".into(), Ok("ok".into()));
    let settings = RunSettings {
        domains: rules(&["rust-lang.org"], &[]),
        ..settings()
    };

    let report = run(&f, &settings, &q(&["q"])).await;

    let entries = report.queries[0].outcome.as_ref().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].hit.url, "https://docs.rust-lang.org/a");
    assert_eq!(
        f.fetched.lock().unwrap().clone(),
        vec!["https://docs.rust-lang.org/a"],
        "a disallowed page is never requested"
    );
}

#[tokio::test]
async fn blocked_domains_are_dropped_from_the_hits() {
    let mut f = Fake::default();
    f.searches.insert(
        "q -site:bad.test".into(),
        Ok(vec![
            hit("https://ok.test/1"),
            hit("https://www.bad.test/2"),
        ]),
    );
    f.pages.insert("https://ok.test/1".into(), Ok("ok".into()));
    let settings = RunSettings {
        domains: rules(&[], &["bad.test"]),
        ..settings()
    };

    let report = run(&f, &settings, &q(&["q"])).await;

    let urls: Vec<&str> = report.queries[0]
        .outcome
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| e.hit.url.as_str())
        .collect();
    assert_eq!(urls, ["https://ok.test/1"]);
}

#[tokio::test]
async fn with_no_rules_the_query_and_hits_are_untouched() {
    let mut f = Fake::default();
    f.searches
        .insert("plain".into(), Ok(vec![hit("https://any.test/1")]));
    f.pages.insert("https://any.test/1".into(), Ok("p".into()));

    let report = run(&f, &settings(), &q(&["plain"])).await;

    assert_eq!(f.searched.lock().unwrap()[0].0, "plain");
    assert_eq!(report.queries[0].outcome.as_ref().unwrap().len(), 1);
}
