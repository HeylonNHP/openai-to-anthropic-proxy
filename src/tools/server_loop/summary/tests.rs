use super::*;

use crate::tui::format::{OUTCOME_ERROR, OUTCOME_OK};

#[test]
fn a_row_has_the_five_columns() {
    let summary = CallSummary::new("web_fetch", "https://example.com/docs", OUTCOME_OK)
        .with_detail("18.43k chars");
    let row = summary.row(Duration::from_millis(1240));
    assert_eq!(
        row,
        "  web_fetch  |  https://example.com/docs  |  ok  |  1.24s  |  18.43k chars"
    );
}

#[test]
fn a_row_without_a_detail_says_so() {
    let summary = CallSummary::new("advisor", "gpt-4.1-mini", OUTCOME_ERROR);
    let row = summary.row(Duration::from_millis(20));
    assert_eq!(
        row,
        "  advisor  |  gpt-4.1-mini  |  error  |  0.02s  |  \u{2014}"
    );
}

#[test]
fn the_subject_is_put_on_one_line() {
    let summary = CallSummary::new("web_search", "rust\nasync  runtime", OUTCOME_OK);
    assert_eq!(summary.subject, "rust async runtime");
}

#[test]
fn a_long_subject_keeps_both_ends() {
    let url = "https://example.com/a/very/long/path/that/keeps/going/and/going/page.html";
    let summary = CallSummary::new("web_fetch", url, OUTCOME_OK);
    assert!(summary.subject.chars().count() <= MAX_SUBJECT);
    assert!(summary.subject.starts_with("https://example.com/"));
    assert!(summary.subject.ends_with("going/page.html"));
    assert!(summary.subject.contains('…'));
}

#[test]
fn a_short_subject_is_left_alone() {
    let summary = CallSummary::new("web_fetch", "https://example.com/", OUTCOME_OK);
    assert_eq!(summary.subject, "https://example.com/");
}

#[test]
fn a_long_detail_is_cut() {
    let detail = "x".repeat(MAX_DETAIL * 2);
    let summary =
        CallSummary::new("web_fetch", "https://example.com/", OUTCOME_ERROR).with_detail(detail);
    let detail = summary.detail.expect("the detail is set");
    assert_eq!(detail.chars().count(), MAX_DETAIL);
    assert!(detail.ends_with('…'));
}

#[test]
fn the_subject_never_contains_the_column_separator() {
    let summary = CallSummary::new("web_search", "a  |  b", OUTCOME_OK);
    assert_eq!(summary.subject, "a | b");
}
