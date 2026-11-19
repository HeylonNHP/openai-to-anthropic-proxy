use super::*;

const PAGE: &str = r##"
<html><body><div id="links" class="results">
  <div class="result results_links web-result result--ad">
    <h2><a href="//duckduckgo.com/y.js?ad=1">Sponsored</a></h2>
    <a class="result__snippet">Buy now</a>
  </div>
  <div class="result results_links web-result">
    <h2 class="result__title"><a class="result__a"
      href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc">  First
      Result </a></h2>
    <a class="result__snippet" href="x">A   snippet
      about it.</a>
  </div>
  <div class="result results_links web-result">
    <h2><a href="https://direct.example.org/b">Second</a></h2>
    <a class="result__snippet">Another.</a>
  </div>
  <div class="result results_links web-result">
    <h2><a href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1">Duplicate</a></h2>
  </div>
  <div class="result results_links web-result">
    <h2><a href="javascript:void(0)">Bad scheme</a></h2>
  </div>
</div></body></html>"##;

#[test]
fn parses_titles_urls_and_snippets() {
    let page = parse_results(PAGE, 10);
    assert!(!page.challenged);
    assert_eq!(
        page.hits,
        vec![
            SearchHit {
                title: "First Result".into(),
                url: "https://example.com/a?x=1".into(),
                snippet: "A snippet about it.".into(),
            },
            SearchHit {
                title: "Second".into(),
                url: "https://direct.example.org/b".into(),
                snippet: "Another.".into(),
            },
        ]
    );
}

#[test]
fn skips_ads_duplicates_and_non_http_links() {
    let page = parse_results(PAGE, 10);
    assert_eq!(page.hits.len(), 2);
    assert!(page.hits.iter().all(|h| !h.title.contains("Sponsored")));
}

#[test]
fn respects_the_limit() {
    assert_eq!(parse_results(PAGE, 1).hits.len(), 1);
    assert!(parse_results(PAGE, 0).hits.is_empty());
}

#[test]
fn falls_back_to_the_legacy_result_class() {
    let html = r#"<div class="result"><a class="result__a" href="https://old.example/x">Old</a>
        <div class="result__snippet">legacy</div></div>"#;
    let page = parse_results(html, 5);
    assert_eq!(page.hits.len(), 1);
    assert_eq!(page.hits[0].url, "https://old.example/x");
    assert_eq!(page.hits[0].snippet, "legacy");
}

#[test]
fn detects_the_bot_challenge() {
    let html = r#"<form id="challenge-form"><input name="x"></form>"#;
    let page = parse_results(html, 5);
    assert!(page.challenged);
    assert!(page.hits.is_empty());
}

#[test]
fn an_empty_page_is_not_a_challenge() {
    let page = parse_results("<html><body>No results.</body></html>", 5);
    assert!(!page.challenged);
    assert!(page.hits.is_empty());
}

#[test]
fn a_known_country_maps_to_its_region() {
    let loc = |c: &str| serde_json::json!({"type": "approximate", "country": c});
    assert_eq!(region_for(Some(&loc("US"))), "us-en");
    assert_eq!(region_for(Some(&loc("DE"))), "de-de");
    assert_eq!(
        region_for(Some(&loc("GB"))),
        "uk-en",
        "DuckDuckGo calls GB `uk`"
    );
    assert_eq!(
        region_for(Some(&loc(" gb "))),
        "uk-en",
        "case and padding are ignored"
    );
}

#[test]
fn no_location_or_an_unknown_country_is_no_region() {
    assert_eq!(region_for(None), NO_REGION);
    assert_eq!(
        region_for(Some(&serde_json::json!({"city": "Paris"}))),
        NO_REGION
    );
    assert_eq!(
        region_for(Some(&serde_json::json!({"country": "ZZ"}))),
        NO_REGION
    );
    assert_eq!(
        region_for(Some(&serde_json::json!({"country": 5}))),
        NO_REGION
    );
}
