use super::*;
use reqwest::header::HeaderMap;

#[test]
fn browser_profile_is_complete_and_coherent() {
    let req = apply(
        reqwest::Client::new().get("https://example.invalid"),
        Some(Navigation {
            site: FetchSite::None,
            referer: None,
            user_gesture: true,
        }),
    )
    .build()
    .expect("request");
    let headers: &HeaderMap = req.headers();
    for name in [
        "user-agent",
        "accept",
        "accept-language",
        "accept-encoding",
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
        "sec-fetch-dest",
        "sec-fetch-mode",
        "sec-fetch-site",
        "sec-fetch-user",
        "upgrade-insecure-requests",
    ] {
        assert!(headers.contains_key(name), "missing {name}");
    }
    let ua = headers["user-agent"].to_str().expect("UA");
    let hints = headers["sec-ch-ua"].to_str().expect("CH");
    assert!(ua.contains("Chrome/157."));
    assert!(hints.contains("v=\"157\""));
    assert!(ua.contains("Windows NT 10.0; Win64; x64"));
    assert_eq!(headers["sec-ch-ua-platform"], "\"Windows\"");
    assert_eq!(headers["accept-encoding"], "gzip, deflate, br");
    assert_eq!(headers["sec-fetch-user"], "?1");
}

#[test]
fn headers_are_added_in_chromes_order() {
    let req = apply(reqwest::Client::new().get("https://example.invalid"), None)
        .build()
        .expect("request");
    let order: Vec<&str> = req.headers().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        order,
        [
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "upgrade-insecure-requests",
            "user-agent",
            "accept",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-user",
            "sec-fetch-dest",
            "accept-encoding",
            "accept-language",
            "priority",
        ]
    );
}

#[test]
fn form_submit_is_a_same_origin_navigation_with_origin_and_referrer() {
    let req = apply_form_submit(
        reqwest::Client::new().post("https://example.invalid/html/"),
        "https://example.invalid/html/",
        &[("q", "a b"), ("kl", "wt-wt")],
    )
    .build()
    .expect("request");
    let headers = req.headers();
    assert_eq!(headers["origin"], "https://example.invalid");
    assert_eq!(
        headers[reqwest::header::REFERER],
        "https://example.invalid/",
        "origin-only, as the engine's referrer-policy makes a browser send"
    );
    assert_eq!(
        headers[reqwest::header::CONTENT_TYPE],
        "application/x-www-form-urlencoded"
    );
    assert_eq!(headers["sec-fetch-site"], "same-origin");
    assert_eq!(headers["sec-fetch-mode"], "navigate");
    assert_eq!(headers["sec-fetch-dest"], "document");
    assert_eq!(headers["sec-fetch-user"], "?1");
    assert_eq!(headers["upgrade-insecure-requests"], "1");
    let body = req.body().and_then(|b| b.as_bytes()).expect("body");
    assert_eq!(body, b"q=a+b&kl=wt-wt");
    let order: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
    let at = |name: &str| order.iter().position(|n| *n == name).expect(name);
    assert!(at("origin") < at("content-type"));
    assert!(at("content-type") < at("user-agent"));
    assert!(at("referer") < at("accept-encoding"));
}

#[test]
fn only_decodable_encodings_are_advertised() {
    let req = apply(reqwest::Client::new().get("https://example.invalid"), None)
        .build()
        .expect("request");
    assert_eq!(req.headers()["accept-encoding"], "gzip, deflate, br");
}
