//! Shared browser-shaped request headers for web fetching and search.

use reqwest::RequestBuilder;
use reqwest::header::{HeaderName, HeaderValue};

/// Navigation origin for Fetch Metadata headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSite {
    /// Initial top-level navigation.
    None,
    /// Redirect within one host.
    SameOrigin,
    /// Redirect across hosts.
    CrossSite,
}

/// Per-hop browser navigation information.
#[derive(Debug, Clone, Copy)]
pub struct Navigation<'a> {
    pub site: FetchSite,
    pub referer: Option<&'a str>,
    pub user_gesture: bool,
}

/// Apply shared browser headers plus Chrome 157 / Windows navigation metadata.
pub fn apply(mut request: RequestBuilder, navigation: Option<Navigation<'_>>) -> RequestBuilder {
    request = apply_shared(request);
    request = add_header(request, "upgrade-insecure-requests", "1");
    request = add_header(request, "sec-fetch-dest", "document");
    request = add_header(request, "sec-fetch-mode", "navigate");
    let site = navigation.map_or(FetchSite::None, |n| n.site);
    request = add_header(
        request,
        "sec-fetch-site",
        match site {
            FetchSite::None => "none",
            FetchSite::SameOrigin => "same-origin",
            FetchSite::CrossSite => "cross-site",
        },
    );
    if navigation.is_none_or(|n| n.user_gesture) {
        request = add_header(request, "sec-fetch-user", "?1");
    }
    if let Some(referer) = navigation.and_then(|n| n.referer) {
        request = request.header(reqwest::header::REFERER, referer);
    }
    request
}

/// Apply the shared browser identity/content headers to a form POST without
/// claiming that it is a top-level document navigation. Keep the caller's
/// explicit referrer, but omit Fetch Metadata and upgrade-insecure-requests:
/// those describe navigation requests, not this tool's search submission.
pub fn apply_form_submit(mut request: RequestBuilder, referer: &str) -> RequestBuilder {
    request = apply_shared(request);
    request.header(reqwest::header::REFERER, referer)
}

fn apply_shared(mut request: RequestBuilder) -> RequestBuilder {
    const CHROME_MAJOR: u32 = 157;
    let major = CHROME_MAJOR.to_string();
    let user_agent = format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    );
    let sec_ch_ua = format!(
        "\"Chromium\";v=\"{major}\", \"Google Chrome\";v=\"{major}\", \"Not_A Brand\";v=\"24\""
    );
    let headers = [
        ("user-agent", user_agent),
        ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".to_owned()),
        ("accept-language", "en-US,en;q=0.9".to_owned()),
        ("accept-encoding", "gzip, deflate, br".to_owned()),
        ("sec-ch-ua", sec_ch_ua),
        ("sec-ch-ua-mobile", "?0".to_owned()),
        ("sec-ch-ua-platform", "\"Windows\"".to_owned()),
    ];
    for (name, value) in headers {
        request = request.header(
            HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
            HeaderValue::from_str(&value).expect("valid header value"),
        );
    }
    request
}

fn add_header(request: RequestBuilder, name: &str, value: &str) -> RequestBuilder {
    request.header(
        HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
        HeaderValue::from_str(value).expect("valid header value"),
    )
}

#[cfg(test)]
mod tests;
