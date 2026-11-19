use super::*;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
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

// Stateful local harness for resilience-specific tests: capture complete
// request headers through CRLFCRLF and serve one response per connection.
type CapturedResponse = (u16, Vec<(&'static str, &'static str)>, Vec<u8>);

struct CapturingServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
}

impl CapturingServer {
    async fn start(responses: Vec<CapturedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let hits = Arc::new(AtomicUsize::new(0));
        let hit_count = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let index = hit_count.fetch_add(1, Ordering::SeqCst);
                let Some((status, headers, body)) =
                    responses.get(index).or_else(|| responses.last()).cloned()
                else {
                    break;
                };
                let requests = Arc::clone(&request_log);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..n]);
                    }
                    requests
                        .lock()
                        .expect("request lock")
                        .push(String::from_utf8_lossy(&request).into_owned());
                    let mut reply = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        reply.push_str(&format!("{name}: {value}\r\n"));
                    }
                    reply.push_str("\r\n");
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        Self {
            addr,
            requests,
            hits,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn explicit_accept_encoding_still_gets_gzip_decoded() {
    let server = CapturingServer::start(vec![(
        200,
        vec![("Content-Encoding", "gzip"), ("Content-Type", "text/html")],
        vec![
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xb3, 0xc9, 0x28, 0xc9,
            0xcd, 0xb1, 0xb3, 0x49, 0xca, 0x4f, 0xa9, 0xb4, 0x4b, 0x49, 0x4d, 0xce, 0x4f, 0x49,
            0x4d, 0x51, 0x48, 0xaf, 0xca, 0x2c, 0x50, 0xc8, 0x4d, 0x2c, 0xca, 0x4e, 0x2d, 0xb2,
            0xd1, 0x07, 0xcb, 0xd8, 0xe8, 0x83, 0x95, 0x01, 0x00, 0x6e, 0xaa, 0x71, 0xd4, 0x2d,
            0x00, 0x00, 0x00,
        ],
    )])
    .await;
    let page = fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("fetch");
    assert!(page.text.contains("decoded gzip marker"), "{:?}", page.text);
}

#[tokio::test]
async fn browser_profile_is_sent_on_the_wire_with_consistent_chrome_major() {
    let server = CapturingServer::start(vec![(
        200,
        vec![],
        b"<html><body>ok</body></html>".to_vec(),
    )])
    .await;
    fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("fetch");
    let request = server.requests().remove(0).to_ascii_lowercase();
    for header in [
        "user-agent:",
        "accept:",
        "accept-language:",
        "accept-encoding: gzip, deflate, br",
        "sec-ch-ua:",
        "sec-ch-ua-mobile: ?0",
        "sec-ch-ua-platform: \"windows\"",
        "sec-fetch-dest: document",
        "sec-fetch-mode: navigate",
        "sec-fetch-site: none",
        "sec-fetch-user: ?1",
        "upgrade-insecure-requests: 1",
    ] {
        assert!(request.contains(header), "missing {header} in {request}");
    }
    assert!(request.contains("chrome/157."));
    assert!(request.contains("\"chromium\";v=\"157\""));
    assert!(request.contains("\"google chrome\";v=\"157\""));
    assert!(!request.contains("zstd"));
}

#[tokio::test]
async fn cookie_is_retained_between_manually_followed_redirect_hops() {
    let server = CapturingServer::start(vec![
        (
            302,
            vec![("Location", "/next"), ("Set-Cookie", "hop=one; Path=/")],
            Vec::new(),
        ),
        (200, vec![], b"<html><body>ok</body></html>".to_vec()),
    ])
    .await;
    fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("fetch");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].to_ascii_lowercase().contains("cookie: hop=one"),
        "{:?}",
        requests[1]
    );
}

#[tokio::test]
async fn retries_503_but_not_403() {
    let server = CapturingServer::start(vec![
        (503, vec![], b"temporary".to_vec()),
        (200, vec![], b"<html><body>recovered</body></html>".to_vec()),
    ])
    .await;
    fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("retry should recover");
    assert_eq!(server.hits(), 2);

    let forbidden = CapturingServer::start(vec![(403, vec![], b"blocked".to_vec())]).await;
    let error = fetch_with_policy(
        &forbidden.url(),
        &test_config(),
        AddressPolicy::AllowPrivate,
    )
    .await
    .expect_err("403");
    assert!(error.detail.contains("403"));
    assert_eq!(forbidden.hits(), 1);
}

#[tokio::test]
async fn a_declared_non_text_body_is_reported_by_its_type() {
    let server = CapturingServer::start(vec![(
        200,
        vec![("Content-Type", "application/pdf")],
        b"%PDF-1.7\n\x00\x01\x02 binary".to_vec(),
    )])
    .await;
    let page = fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("a non-text page is not an error");
    assert!(page.text.contains("application/pdf"), "{:?}", page.text);
    assert!(
        !page.text.contains("PDF-1.7"),
        "the body is not shown: {:?}",
        page.text
    );
    assert_eq!(page.title, None);
}

#[tokio::test]
async fn an_undeclared_binary_body_is_reported_as_non_text() {
    let server = CapturingServer::start(vec![(
        200,
        vec![("Content-Type", "application/octet-stream")],
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec(),
    )])
    .await;
    let page = fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("fetch");
    assert!(page.text.contains("non-text content"), "{:?}", page.text);
    assert!(
        page.text.contains("application/octet-stream"),
        "{:?}",
        page.text
    );
}

#[tokio::test]
async fn an_octet_stream_that_is_really_text_is_still_read() {
    let server = CapturingServer::start(vec![(
        200,
        vec![("Content-Type", "application/octet-stream")],
        b"just a text file served with the wrong type".to_vec(),
    )])
    .await;
    let page = fetch_with_policy(&server.url(), &test_config(), AddressPolicy::AllowPrivate)
        .await
        .expect("fetch");
    assert_eq!(page.text, "just a text file served with the wrong type");
}

#[tokio::test]
async fn a_name_whose_first_address_is_unreachable_falls_back_to_the_next() {
    // The server listens on IPv4 loopback only. `localhost` resolves to
    // `::1` AND `127.0.0.1` on a typical dual-stack host, often `::1`
    // first; pinning only the first address used to make this fetch fail
    // with "connection refused" although `127.0.0.1` would have answered.
    let server = TestServer::start(200, "reached over the second address").await;
    let addrs: Vec<_> = tokio::net::lookup_host(("localhost", server.addr.port()))
        .await
        .expect("localhost resolves")
        .collect();
    let url = format!("http://localhost:{}/", server.addr.port());
    let page = fetch_with_policy(&url, &test_config(), AddressPolicy::AllowPrivate)
        .await
        .unwrap_or_else(|e| panic!("fetch over {addrs:?} failed: {e:?}"));
    assert_eq!(page.text, "reached over the second address");
}
