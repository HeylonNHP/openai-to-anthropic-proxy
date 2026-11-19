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
fn form_submit_uses_shared_profile_without_navigation_claims() {
    let req = apply_form_submit(
        reqwest::Client::new().post("https://example.invalid/search"),
        "https://example.invalid/",
    )
    .form(&[("q", "query")])
    .build()
    .expect("request");
    let headers = req.headers();
    for name in [
        "user-agent",
        "accept",
        "accept-language",
        "accept-encoding",
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
    ] {
        assert!(headers.contains_key(name), "missing {name}");
    }
    for name in [
        "sec-fetch-dest",
        "sec-fetch-mode",
        "sec-fetch-site",
        "sec-fetch-user",
        "upgrade-insecure-requests",
    ] {
        assert!(!headers.contains_key(name), "unexpected {name}");
    }
    assert_eq!(
        headers[reqwest::header::REFERER],
        "https://example.invalid/"
    );
}
