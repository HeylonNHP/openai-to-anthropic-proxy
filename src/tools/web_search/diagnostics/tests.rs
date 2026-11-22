use super::*;
use std::sync::{Arc, Mutex};

const CHALLENGE: &str = r#"<html><head><title>DuckDuckGo</title><style>.x{}</style></head><body>
<form id="challenge-form" action="//duckduckgo.com/anomaly.js"><div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div></form>
<script>var secret = 1;</script></body></html>"#;

fn report(body: &str) -> Report {
    let mut r = Report::default();
    r.set_body(body, 12);
    r
}

fn ctx() -> Context {
    Context {
        query: "how does rustls work".into(),
        region: "wt-wt".into(),
        endpoint: "https://html.duckduckgo.com/html/".into(),
        cookies_sent: 1,
        session_age_secs: 90,
        searches: 9,
        ok_streak: 7,
        secs_since_last_ok: Some(3),
        secs_since_last_block: None,
    }
}

#[test]
fn a_challenge_page_is_recognised_by_its_markers() {
    let markers = report(CHALLENGE).markers();
    assert!(markers.contains(&"challenge-form"));
    assert!(markers.contains(&"anomaly-modal"));
    assert!(markers.contains(&"bots-use-duckduckgo"));
    assert!(!markers.contains(&"web-result"));
}

#[test]
fn a_no_results_page_says_so() {
    assert!(report(r#"<div class="no-results">No results.</div>"#).says_no_results());
    assert!(!report("<html><body>hello</body></html>").says_no_results());
    assert!(!report(CHALLENGE).says_no_results());
}

#[test]
fn the_head_is_the_visible_text_without_scripts_or_styles() {
    let head = report(CHALLENGE).head();
    assert!(
        head.contains("Unfortunately, bots use DuckDuckGo too."),
        "{head}"
    );
    assert!(!head.contains("secret"), "scripts are not text: {head}");
    assert!(!head.contains(".x{}"), "styles are not text: {head}");
    assert_eq!(Report::default().head(), "");
}

#[test]
fn the_head_is_capped() {
    let head = report(&format!("<body>{}</body>", "word ".repeat(1000))).head();
    assert_eq!(head.chars().count(), HEAD_CHARS);
}

#[test]
fn the_body_is_capped_but_its_full_length_is_kept() {
    let big = "é".repeat(MAX_BODY_CHARS + 100);
    let r = report(&big);
    assert_eq!(r.body.chars().count(), MAX_BODY_CHARS, "char-safe cap");
    assert_eq!(r.body_bytes, big.len(), "the real size");
    assert_eq!(r.elapsed_ms, 12);
}

/// A fresh empty directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("web-search-diag-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_page_is_saved_under_the_state_dir() {
    let dir = TempDir::new("save");
    let path = save_page(Some(&dir.0), &report(CHALLENGE)).expect("saved");
    assert!(path.starts_with(dir.0.join(SAVED_DIR)));
    assert_eq!(std::fs::read_to_string(&path).expect("read"), CHALLENGE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn nothing_is_saved_without_a_state_dir_or_a_page() {
    let dir = TempDir::new("none");
    assert_eq!(save_page(None, &report(CHALLENGE)), None);
    assert_eq!(save_page(Some(&dir.0), &Report::default()), None);
    assert!(!dir.0.exists());
}

#[test]
fn only_the_newest_pages_are_kept() {
    let dir = TempDir::new("prune");
    let saved = dir.0.join(SAVED_DIR);
    for n in 0..(MAX_SAVED_PAGES as u128 + 3) {
        write_page(&saved, "page", 1_000 + n).expect("write");
    }
    let mut names: Vec<_> = std::fs::read_dir(&saved)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), MAX_SAVED_PAGES);
    let newest = 1_000 + MAX_SAVED_PAGES as u128 + 2;
    assert_eq!(names.last().unwrap(), &format!("search-{newest:016}.html"));
    let oldest_kept = newest + 1 - MAX_SAVED_PAGES as u128;
    assert_eq!(names[0], format!("search-{oldest_kept:016}.html"));
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// What `f` logs, as one string.
fn logged(f: impl FnOnce()) -> String {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn a_block_is_logged_with_the_engine_and_the_session() {
    let mut r = report(CHALLENGE);
    r.status = Some(202);
    r.http_version = "HTTP/2.0".into();
    r.set_cookie_names = vec!["kl".into()];
    let saved = PathBuf::from("/state/blocked-pages/search-1.html");
    let line = logged(|| {
        log_failure(
            &ctx(),
            true,
            "the search engine returned a bot challenge",
            Some(&r),
            Some(&saved),
        );
    });
    for expected in [
        "WARN",
        "kind=\"blocked\"",
        "status=Some(202)",
        "http_version=HTTP/2.0",
        "challenge-form",
        "bots-use-duckduckgo",
        "query=how does rustls work",
        "cookies_sent=1",
        "session_age_secs=90",
        "searches_in_session=9",
        "ok_before_this=7",
        "secs_since_last_ok=Some(3)",
        "secs_since_last_block=None",
        "set_cookies=[\"kl\"]",
        "Unfortunately, bots use DuckDuckGo too.",
        "blocked-pages/search-1.html",
    ] {
        assert!(line.contains(expected), "missing {expected:?} in:\n{line}");
    }
}

#[test]
fn a_failure_without_a_response_is_logged_too() {
    let r = Report {
        transport: Some("timeout: operation timed out".into()),
        elapsed_ms: 15_000,
        ..Report::default()
    };
    let line = logged(|| log_failure(&ctx(), false, "search request failed", Some(&r), None));
    assert!(line.contains("kind=\"failed\""), "{line}");
    assert!(line.contains("timeout: operation timed out"), "{line}");
    assert!(line.contains("elapsed_ms=15000"), "{line}");
    let line = logged(|| log_failure(&ctx(), false, "x", None, None));
    assert!(
        line.contains("kind=\"failed\""),
        "no report is fine: {line}"
    );
}

#[test]
fn an_unexplained_empty_page_is_logged() {
    let line = logged(|| log_unexplained_empty(&ctx(), &report("<body>hi</body>"), None));
    assert!(line.contains("soft block or changed markup"), "{line}");
    assert!(line.contains("page_head=hi"), "{line}");
}
