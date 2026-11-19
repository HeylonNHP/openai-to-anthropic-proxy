use super::*;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::net::TcpListener;

/// A tiny local HTTP server that serves one canned response.
///
/// Used to exercise the REAL fetch path (connect, read, extract) without
/// touching the public internet. It binds to loopback, so the tests use
/// [`AddressPolicy::AllowPrivate`] — the production path always uses
/// [`AddressPolicy::Strict`].
struct TestServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
}

impl TestServer {
    /// Start a server that answers every request with `status` and `body`.
    async fn start(status: u16, body: &'static str) -> Self {
        Self::start_with_headers(status, body, Vec::new()).await
    }

    /// Start a server that answers every request with `status`, `body` and
    /// extra headers (used for the redirect test).
    async fn start_with_headers(
        status: u16,
        body: &'static str,
        headers: Vec<(&'static str, String)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let headers = headers.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    // Read the request (headers at least) so the client
                    // sees a well-formed exchange.
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;

                    let mut response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str("\r\n");
                    response.push_str(body);
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { addr, hits }
    }

    /// Start a server that accepts connections and reads the request but never
    /// answers, so a client can only get out by timing out.
    async fn start_stalling() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_clone.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    // Hold the connection open, silent, for as long as the
                    // test runs.
                    std::future::pending::<()>().await;
                });
            }
        });

        Self { addr, hits }
    }

    /// The URL of a path on this server.
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn test_config() -> WebFetchConfig {
    WebFetchConfig {
        enabled: true,
        timeout_secs: Some(10),
        max_bytes: None,
        max_content_bytes: None,
    }
}

#[tokio::test]
async fn fetches_and_extracts_a_page() {
    let server = TestServer::start(
        200,
        r#"<!DOCTYPE html><html><head><title>Hello</title></head><body><article><p>The body of the page.</p></article></body></html>"#,
    )
    .await;

    let page = fetch_with_policy(
        &server.url("/"),
        &test_config(),
        AddressPolicy::AllowPrivate,
    )
    .await
    .expect("the fetch must succeed");

    assert_eq!(page.title.as_deref(), Some("Hello"));
    assert!(
        page.text.contains("The body of the page."),
        "got {:?}",
        page.text
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn a_non_2xx_response_is_an_error_not_a_page() {
    let server = TestServer::start(404, "not found").await;

    let err = fetch_with_policy(
        &server.url("/"),
        &test_config(),
        AddressPolicy::AllowPrivate,
    )
    .await
    .expect_err("a 404 must be an error");

    assert_eq!(err.code, "url_not_accessible");
    assert!(err.detail.contains("404"), "got {}", err.detail);
}

#[tokio::test]
async fn the_byte_cap_truncates_the_download() {
    let server = TestServer::start(200, "0123456789ABCDEF").await;

    let mut cfg = test_config();
    cfg.max_bytes = Some(4);
    let page = fetch_with_policy(&server.url("/"), &cfg, AddressPolicy::AllowPrivate)
        .await
        .expect("the fetch must succeed");

    assert!(
        page.text.len() <= 4,
        "the download must be capped, got {:?}",
        page.text
    );
}

#[tokio::test]
async fn the_content_cap_truncates_the_extracted_text() {
    let body = "First paragraph.\n\nSecond paragraph.";
    let server = TestServer::start(200, body).await;

    let mut cfg = test_config();
    cfg.max_content_bytes = Some(20);
    let page = fetch_with_policy(&server.url("/"), &cfg, AddressPolicy::AllowPrivate)
        .await
        .expect("the fetch must succeed");

    assert!(
        page.text.contains("[content truncated]"),
        "got {:?}",
        page.text
    );
    assert!(page.text.len() < body.len() + 40, "got {:?}", page.text);
}

#[tokio::test]
async fn a_redirect_is_followed() {
    let server = TestServer::start_with_headers(
        302,
        "",
        vec![("Location", "http://example.invalid/redirected".to_owned())],
    )
    .await;
    // The redirect target does not resolve, so the fetch FAILS — but it
    // must fail AFTER following the redirect, which the hit count proves.
    let err = fetch_with_policy(
        &server.url("/"),
        &test_config(),
        AddressPolicy::AllowPrivate,
    )
    .await
    .expect_err("the redirect target does not resolve");

    assert_eq!(server.hits(), 1, "the redirect must have been served");
    assert_eq!(err.code, "url_not_accessible");
}

#[tokio::test]
async fn a_private_address_is_refused_under_the_strict_policy() {
    // The strict policy is what the proxy always uses: a loopback URL
    // (the test server) must be refused.
    let server = TestServer::start(200, "body").await;
    let err = fetch(&server.url("/"), &test_config())
        .await
        .expect_err("the strict policy must refuse a loopback address");
    assert_eq!(err.code, "url_not_allowed");
    assert_eq!(server.hits(), 0, "no connection may be made");
}

#[tokio::test]
async fn a_timeout_is_reported_as_an_error() {
    let mut cfg = test_config();
    cfg.timeout_secs = Some(1);
    // A server that never answers: the only way out is the timeout. (A
    // zero-second budget against a server that answers at once is a RACE: the
    // timer does not fire before the next millisecond tick, so a fast
    // loopback response could win.)
    let server = TestServer::start_stalling().await;

    let started = std::time::Instant::now();
    let err = fetch_with_policy(&server.url("/"), &cfg, AddressPolicy::AllowPrivate)
        .await
        .expect_err("the request must time out");

    assert_eq!(err.code, "url_not_accessible");
    assert!(err.detail.contains("timed out after 1s"), "{}", err.detail);
    assert_eq!(server.hits(), 1, "the request did reach the server");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the budget must bound the fetch, took {:?}",
        started.elapsed()
    );
}
