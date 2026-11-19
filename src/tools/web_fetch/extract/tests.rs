use super::*;

const PAGE_URL: &str = "https://example.com/article";

#[test]
fn plain_text_is_returned_verbatim() {
    let (title, text) = extract(b"just some words", PAGE_URL, Some("text/plain"));
    assert_eq!(text, "just some words");
    assert!(title.is_none());
}

#[test]
fn html_yields_the_article_text_not_the_navigation() {
    let html = r#"<!DOCTYPE html>
<html><head><title>Article Title</title></head>
<body>
  <nav><a href="/">Home</a><a href="/about">About</a></nav>
  <article>
    <h1>Article Title</h1>
    <p>The first paragraph of the article body.</p>
    <p>The second paragraph of the article body.</p>
  </article>
  <footer>Copyright notice</footer>
</body></html>"#;
    let (title, text) = extract(html.as_bytes(), PAGE_URL, Some("text/html"));

    assert_eq!(title.as_deref(), Some("Article Title"));
    assert!(
        text.contains("first paragraph"),
        "the article body must survive: {text}"
    );
    assert!(
        text.contains("second paragraph"),
        "the article body must survive: {text}"
    );
    assert!(
        !text.contains("Copyright notice"),
        "the footer must be dropped: {text}"
    );
    assert!(
        !text.contains("About"),
        "the navigation must be dropped: {text}"
    );
}

#[test]
fn a_page_without_an_article_falls_back_to_the_whole_page() {
    // A docs landing page: Readability finds no article, so the
    // full-page Markdown fallback runs.
    let html = r#"<!DOCTYPE html><html><body><ul><li>One</li><li>Two</li></ul></body></html>"#;
    let (_, text) = extract(html.as_bytes(), PAGE_URL, Some("text/html"));
    assert!(!text.is_empty(), "the fallback must produce something");
    assert!(
        text.contains("One") && text.contains("Two"),
        "the fallback must keep the content: {text}"
    );
}

#[test]
fn html_is_detected_without_a_content_type() {
    let html = "<!DOCTYPE html><html><body><p>Body</p></body></html>";
    let (_, text) = extract(html.as_bytes(), PAGE_URL, None);
    assert!(text.contains("Body"), "got {text}");
}

#[test]
fn a_declared_non_html_type_is_not_run_through_the_html_extractors() {
    // A JSON API response must not be mangled by the HTML extractors.
    let json = br#"{"results": [{"title": "One"}]}"#;
    let (_, text) = extract(json, PAGE_URL, Some("application/json"));
    assert_eq!(text, r#"{"results": [{"title": "One"}]}"#);
}

#[test]
fn invalid_utf8_is_decoded_lossily_rather_than_dropped() {
    let bytes = b"valid text \xff\xfe more text";
    let (_, text) = extract(bytes, PAGE_URL, Some("text/plain"));
    assert!(text.contains("valid text"), "got {text}");
    assert!(text.contains("more text"), "got {text}");
}

#[test]
fn the_page_title_is_reported_when_there_is_one() {
    let html = r#"<html><head><title>My Title</title></head><body><article><p>Body text that is long enough to be considered an article by readability heuristics so that it is actually extracted.</p></article></body></html>"#;
    let (title, _) = extract(html.as_bytes(), PAGE_URL, Some("text/html"));
    assert_eq!(title.as_deref(), Some("My Title"));
}

#[test]
fn content_types_are_classified() {
    use ContentKind::{NonText, Text, Unknown};
    for ct in [
        "text/html; charset=utf-8",
        "text/plain",
        "TEXT/CSV",
        "application/json",
        "application/ld+json",
        "application/atom+xml",
        "image/svg+xml",
        "application/javascript",
    ] {
        assert_eq!(content_kind(Some(ct)), Text, "{ct}");
    }
    for ct in [
        "application/pdf",
        "image/png",
        "application/zip",
        "audio/mpeg",
        "video/mp4",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ] {
        assert_eq!(content_kind(Some(ct)), NonText, "{ct}");
    }
    assert_eq!(content_kind(None), Unknown);
    assert_eq!(content_kind(Some("")), Unknown);
    assert_eq!(content_kind(Some("application/octet-stream")), Unknown);
}

#[test]
fn a_nul_byte_marks_an_undeclared_body_as_binary() {
    assert!(looks_binary(b"%PDF-1.7\n\x00\x01binary"));
    assert!(!looks_binary(b"plain text, nothing odd"));
    assert!(!looks_binary(b""));
}

#[test]
fn the_non_text_notice_names_the_type_and_size() {
    let notice = non_text_notice(Some("application/pdf"), Some(1234));
    assert!(notice.contains("application/pdf"), "{notice}");
    assert!(notice.contains("1234 bytes"), "{notice}");
    let unknown = non_text_notice(None, None);
    assert!(unknown.contains("unknown"), "{unknown}");
}

#[test]
fn a_declared_header_charset_is_used_to_decode() {
    // "café" in ISO-8859-1: the é is the single byte 0xE9.
    let (_, text) = extract(b"caf\xe9", PAGE_URL, Some("text/plain; charset=ISO-8859-1"));
    assert_eq!(text, "café");
}

#[test]
fn a_meta_charset_is_used_when_the_header_has_none() {
    let html = b"<html><head><meta charset=\"windows-1252\"><title>Caf\xe9</title></head><body><p>x</p></body></html>";
    let (title, _) = extract(html, PAGE_URL, Some("text/html"));
    assert_eq!(title.as_deref(), Some("Café"));
}

#[test]
fn a_meta_like_string_in_plain_text_does_not_change_the_decoding() {
    let text = "the header charset=latin1 is mentioned here: café";
    let (_, out) = extract(text.as_bytes(), PAGE_URL, Some("text/plain"));
    assert_eq!(out, text);
}

#[test]
fn a_utf16_body_with_a_bom_is_decoded() {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "hello".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let (_, text) = extract(&bytes, PAGE_URL, Some("text/plain"));
    assert_eq!(text, "hello");
}
