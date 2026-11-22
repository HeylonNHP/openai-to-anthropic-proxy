use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::tools::web_search::ddg;

const PAGE: &str = r#"<html><body><div id="links"><div class="web-result">
<h2><a href="https://example.com/a">Title A</a></h2>
<a class="result__snippet">Snippet A</a></div></div></body></html>"#;

/// A search engine that records every request it gets.
struct FakeEngine {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeEngine {
    async fn start() -> Self {
        Self::start_with(200, PAGE).await
    }

    /// An engine that answers every search with `status` and `page`, setting a
    /// `kl` cookie.
    async fn start_with(status: u16, page: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}/html/", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => raw.extend_from_slice(&chunk[..n]),
                        }
                    }
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&raw).into_owned());
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: text/html\r\nretry-after: 30\r\nset-cookie: kl=wt-wt; Max-Age=3600\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                        page.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self { endpoint, requests }
    }

    fn request(&self, index: usize) -> String {
        self.requests.lock().unwrap()[index].to_lowercase()
    }
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        Self(std::env::temp_dir().join(format!(
            "web-search-session-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(endpoint: &str, state_dir: Option<&std::path::Path>) -> WebSearchConfig {
    WebSearchConfig {
        search_endpoint: endpoint.to_owned(),
        state_dir: state_dir.map(std::path::Path::to_path_buf),
        ..WebSearchConfig::default()
    }
}

async fn search(session: &SearchSession, endpoint: &str, query: &str) {
    let searched = ddg::search(
        &session.client,
        endpoint,
        query,
        "wt-wt",
        5,
        Duration::from_secs(5),
    )
    .await
    .expect("search");
    assert_eq!(searched.hits.len(), 1);
}

#[tokio::test]
async fn the_session_sends_back_the_cookie_the_engine_set() {
    let engine = FakeEngine::start().await;
    let session = SearchSession::open(&config(&engine.endpoint, None)).expect("session");

    search(&session, &engine.endpoint, "one").await;
    search(&session, &engine.endpoint, "two").await;

    assert!(!engine.request(0).contains("\r\ncookie:"), "first visit");
    assert!(
        engine.request(1).contains("\r\ncookie: kl=wt-wt\r\n"),
        "{}",
        engine.request(1)
    );
}

#[tokio::test]
async fn the_search_posts_like_a_same_origin_form_submit() {
    let engine = FakeEngine::start().await;
    let session = SearchSession::open(&config(&engine.endpoint, None)).expect("session");
    search(&session, &engine.endpoint, "rust lang").await;

    let origin = engine.endpoint.trim_end_matches("/html/");
    let request = engine.request(0);
    assert!(request.starts_with("post /html/ "), "{request}");
    assert!(request.contains(&format!("\r\norigin: {origin}\r\n")));
    assert!(request.contains(&format!("\r\nreferer: {origin}/\r\n")));
    assert!(request.contains("\r\nsec-fetch-site: same-origin\r\n"));
    assert!(request.contains("\r\ncontent-type: application/x-www-form-urlencoded\r\n"));
}

#[tokio::test]
async fn the_cookie_survives_a_restart() {
    let engine = FakeEngine::start().await;
    let dir = TempDir::new();
    let cfg = config(&engine.endpoint, Some(&dir.0));

    let first = SearchSession::open(&cfg).expect("session");
    search(&first, &engine.endpoint, "one").await;
    drop(first);

    // A new process: a new session that loads the saved one.
    let second = SearchSession::open(&cfg).expect("session");
    search(&second, &engine.endpoint, "two").await;

    assert!(
        engine.request(1).contains("\r\ncookie: kl=wt-wt\r\n"),
        "{}",
        engine.request(1)
    );
}

#[tokio::test]
async fn the_same_endpoint_shares_one_session() {
    let a = session_for(&config("http://127.0.0.1:1/shared/", None)).expect("session");
    let b = session_for(&config("http://127.0.0.1:1/shared/", None)).expect("session");
    let other = session_for(&config("http://127.0.0.1:1/other/", None)).expect("session");
    assert!(Arc::ptr_eq(&a, &b));
    assert!(!Arc::ptr_eq(&a, &other));
}

#[tokio::test]
async fn the_cache_follows_the_config() {
    let cfg = WebSearchConfig {
        cache_ttl_secs: 5,
        cache_max_entries: 1,
        ..config("http://127.0.0.1:1/x/", None)
    };
    let session = SearchSession::open(&cfg).expect("session");
    let one = ddg::SearchHit {
        title: "t".into(),
        url: "https://example.com/".into(),
        snippet: String::new(),
    };
    session
        .cache
        .put("wt-wt", "a", 5, std::slice::from_ref(&one));
    session
        .cache
        .put("wt-wt", "b", 5, std::slice::from_ref(&one));
    assert!(
        session.cache.get("wt-wt", "a", 5).is_none(),
        "capacity is 1"
    );
    assert!(session.cache.get("wt-wt", "b", 5).is_some());
}

const CHALLENGE_PAGE: &str = r#"<html><head><title>DuckDuckGo</title></head><body>
<form id="challenge-form"><div class="anomaly-modal__title">Unfortunately, bots use DuckDuckGo too.</div></form></body></html>"#;

async fn run(engine: &FakeEngine) -> Result<ddg::Searched, ddg::SearchError> {
    let session = SearchSession::open(&config(&engine.endpoint, None)).expect("session");
    ddg::search(
        &session.client,
        &engine.endpoint,
        "rust",
        "wt-wt",
        5,
        Duration::from_secs(5),
    )
    .await
}

#[tokio::test]
async fn a_successful_search_reports_the_exchange() {
    let engine = FakeEngine::start().await;
    let searched = run(&engine).await.expect("search");
    let r = searched.report;
    assert_eq!(r.status, Some(200));
    assert_eq!(r.http_version, "HTTP/1.1");
    assert_eq!(r.content_type.as_deref(), Some("text/html"));
    assert_eq!(r.set_cookie_names, ["kl"]);
    assert_eq!(r.body_bytes, PAGE.len());
    assert!(r.markers().contains(&"web-result"));
}

#[tokio::test]
async fn a_challenge_is_a_block_that_carries_its_page() {
    let engine = FakeEngine::start_with(202, CHALLENGE_PAGE).await;
    let error = run(&engine).await.expect_err("challenged");
    assert!(error.blocked);
    let r = error.report.expect("report");
    assert_eq!(r.status, Some(202));
    assert!(r.markers().contains(&"challenge-form"));
    assert_eq!(r.body, CHALLENGE_PAGE);
}

#[tokio::test]
async fn a_rate_limit_status_is_a_block_that_carries_its_headers() {
    let engine = FakeEngine::start_with(429, "slow down").await;
    let error = run(&engine).await.expect_err("limited");
    assert!(error.blocked);
    let r = error.report.expect("report");
    assert_eq!(r.status, Some(429));
    assert_eq!(r.retry_after.as_deref(), Some("30"));
    assert_eq!(r.body, "slow down");
}

#[tokio::test]
async fn a_dead_endpoint_is_a_failure_with_the_transport_kind() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!("http://{}/html/", listener.local_addr().expect("addr"));
    drop(listener);
    let session = SearchSession::open(&config(&endpoint, None)).expect("session");
    let error = ddg::search(
        &session.client,
        &endpoint,
        "q",
        "wt-wt",
        5,
        Duration::from_secs(5),
    )
    .await
    .expect_err("refused");
    assert!(!error.blocked);
    let transport = error.report.expect("report").transport.expect("transport");
    assert!(transport.starts_with("connect:"), "{transport}");
}

#[tokio::test(start_paused = true)]
async fn the_stats_say_how_many_searches_succeeded_before_a_block() {
    let session = SearchSession::open(&config("http://127.0.0.1:1/stats/", None)).expect("session");
    for _ in 0..3 {
        session.note_ok();
    }
    tokio::time::advance(Duration::from_secs(20)).await;
    let first = session.note_failure(true);
    assert_eq!(first.searches, 4);
    assert_eq!(first.ok_streak, 3, "three good searches before the block");
    assert_eq!(first.secs_since_last_ok, Some(20));
    assert_eq!(first.secs_since_last_block, None, "the first block");

    tokio::time::advance(Duration::from_secs(40)).await;
    let second = session.note_failure(true);
    assert_eq!(
        second.ok_streak, 0,
        "the streak was reset by the first block"
    );
    assert_eq!(second.secs_since_last_block, Some(40));
}

#[tokio::test]
async fn the_context_carries_the_session_and_the_request() {
    let engine = FakeEngine::start().await;
    let session = SearchSession::open(&config(&engine.endpoint, None)).expect("session");
    search(&session, &engine.endpoint, "one").await;
    session.note_ok();
    let ctx = session.context(&engine.endpoint, "wt-wt", "two", session.snapshot());
    assert_eq!(ctx.query, "two");
    assert_eq!(ctx.cookies_sent, 1, "the kl cookie the engine set");
    assert_eq!(ctx.searches, 1);
    assert_eq!(ctx.ok_streak, 1);
}
