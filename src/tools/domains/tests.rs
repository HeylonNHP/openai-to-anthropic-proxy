use super::*;

fn rules(allowed: &[&str], blocked: &[&str]) -> DomainRules {
    let list = |items: &[&str]| {
        (!items.is_empty()).then(|| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    };
    DomainRules::from_options(&ServerToolOptions {
        allowed_domains: list(allowed),
        blocked_domains: list(blocked),
        ..ServerToolOptions::default()
    })
}

#[test]
fn no_rules_allow_everything() {
    let r = DomainRules::default();
    assert!(r.is_unrestricted());
    assert!(r.allows("https://anything.example/x"));
    assert!(r.allows("not even a url"), "nothing to enforce");
    assert_eq!(r.narrow_query("rust"), "rust");
}

#[test]
fn a_domain_rule_covers_its_subdomains_but_not_lookalikes() {
    let r = rules(&["example.com"], &[]);
    assert!(r.allows("https://example.com/"));
    assert!(r.allows("https://docs.example.com/a"));
    assert!(r.allows("https://a.b.example.com/"));
    assert!(
        !r.allows("https://notexample.com/"),
        "suffix is not a subdomain"
    );
    assert!(!r.allows("https://example.com.evil.test/"));
    assert!(!r.allows("https://other.org/"));
}

#[test]
fn a_subdomain_rule_matches_only_that_subdomain_and_below() {
    let r = rules(&["docs.example.com"], &[]);
    assert!(r.allows("https://docs.example.com/"));
    assert!(r.allows("https://v2.docs.example.com/"));
    assert!(!r.allows("https://example.com/"));
    assert!(!r.allows("https://blog.example.com/"));
}

#[test]
fn a_path_rule_matches_by_prefix() {
    let r = rules(&["example.com/blog"], &[]);
    assert!(r.allows("https://example.com/blog"));
    assert!(r.allows("https://example.com/blog/post-1"));
    assert!(!r.allows("https://example.com/shop"));
    assert!(!r.allows("https://example.com/"));
}

#[test]
fn a_block_list_removes_only_what_it_names() {
    let r = rules(&[], &["bad.example", "x.org/private"]);
    assert!(!r.allows("https://bad.example/"));
    assert!(!r.allows("https://www.bad.example/a"));
    assert!(!r.allows("https://x.org/private/doc"));
    assert!(r.allows("https://x.org/public"));
    assert!(r.allows("https://good.example/"));
}

#[test]
fn matching_ignores_case_and_a_pasted_scheme_or_slash() {
    let r = rules(&["HTTPS://Example.COM/"], &[]);
    assert!(r.allows("https://EXAMPLE.com/Page"));
    assert!(r.allows("http://docs.example.com/"));
}

#[test]
fn an_empty_or_blank_list_is_no_list() {
    assert!(rules(&[], &[]).is_unrestricted());
    assert!(rules(&["", "  ", "/"], &[]).is_unrestricted());
}

#[test]
fn a_non_url_is_refused_while_a_rule_is_set() {
    let r = rules(&["example.com"], &[]);
    assert!(!r.allows("example.com"));
    assert!(!r.allows("mailto:a@example.com"));
}

#[test]
fn the_query_is_narrowed_with_search_operators() {
    assert_eq!(
        rules(&["rust-lang.org"], &[]).narrow_query("async rust"),
        "async rust site:rust-lang.org"
    );
    assert_eq!(
        rules(&["a.com", "b.com/docs"], &[]).narrow_query("q"),
        "q (site:a.com OR site:b.com/docs)"
    );
    assert_eq!(
        rules(&[], &["x.com", "y.com"]).narrow_query("q"),
        "q -site:x.com -site:y.com"
    );
}

#[test]
fn a_very_long_list_is_capped_in_the_query_only() {
    let many: Vec<String> = (0..30).map(|i| format!("d{i}.test")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    let r = rules(&refs, &[]);
    assert_eq!(
        r.narrow_query("q").matches("site:").count(),
        MAX_QUERY_RULES
    );
    assert!(
        r.allows("https://d29.test/"),
        "filtering still sees every rule"
    );
}

#[test]
fn a_trailing_dot_host_is_the_same_host() {
    // `example.com.` is the fully-qualified spelling of `example.com`: DNS
    // resolves both to the same place, so a block must cover both.
    let blocked = rules(&[], &["example.com"]);
    assert!(!blocked.allows("https://example.com./"));
    assert!(!blocked.allows("https://www.example.com./a"));
    assert!(!blocked.allows("https://EXAMPLE.COM../"));

    let allowed = rules(&["example.com"], &[]);
    assert!(allowed.allows("https://example.com./"));
    assert!(allowed.allows("https://docs.example.com./x"));
    assert!(!allowed.allows("https://other.org./"));
}
