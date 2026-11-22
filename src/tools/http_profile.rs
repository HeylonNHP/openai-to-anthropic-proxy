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

/// What one request claims to be, which decides the headers after the
/// identity block.
struct Shape<'a> {
    /// `Origin` and `Content-Type`, for a form POST.
    form_origin: Option<&'a str>,
    site: FetchSite,
    referer: Option<&'a str>,
    user_gesture: bool,
}

/// Apply browser headers plus Chrome 157 / Windows navigation metadata, for a
/// top-level navigation (a link click, a typed URL, a redirect hop).
pub fn apply(request: RequestBuilder, navigation: Option<Navigation<'_>>) -> RequestBuilder {
    build(
        request,
        &Shape {
            form_origin: None,
            site: navigation.map_or(FetchSite::None, |n| n.site),
            referer: navigation.and_then(|n| n.referer),
            user_gesture: navigation.is_none_or(|n| n.user_gesture),
        },
    )
}

/// A POST of `fields` to `endpoint` as a browser sends a search form that sits
/// on the endpoint's own site: a same-origin, user-initiated navigation with
/// the page's origin as `Origin` and as the (origin-only) referrer, which is
/// what the engine's `referrer-policy: origin` makes a browser send.
///
/// The body is encoded here and the `Content-Type` is set in its browser
/// position, so the caller adds nothing but the timeout.
pub fn apply_form_submit(
    request: RequestBuilder,
    endpoint: &str,
    fields: &[(&str, &str)],
) -> RequestBuilder {
    let origin = url::Url::parse(endpoint)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .filter(|o| o != "null");
    let referer = origin.as_ref().map(|o| format!("{o}/"));
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    build(
        request,
        &Shape {
            form_origin: origin.as_deref(),
            site: FetchSite::SameOrigin,
            referer: referer.as_deref().or(Some(endpoint)),
            user_gesture: true,
        },
    )
    .body(body)
}

/// The version the identity below claims.
const CHROME_MAJOR: u32 = 157;

/// Add the headers in the order Chrome sends them: client hints, `Origin` /
/// `Content-Type` (forms), `Upgrade-Insecure-Requests`, `User-Agent`,
/// `Accept`, Fetch Metadata, `Referer`, `Accept-Encoding`,
/// `Accept-Language`, `Priority`. Header ORDER is part of what an anti-bot
/// system compares, and `reqwest` keeps the order they are added in. (The
/// `Cookie` header is added last by the cookie jar, where Chrome puts it just
/// before `Priority`; that one position cannot be controlled from here.)
fn build(mut request: RequestBuilder, shape: &Shape<'_>) -> RequestBuilder {
    let major = CHROME_MAJOR.to_string();
    let user_agent = format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    );
    let sec_ch_ua = format!(
        "\"Chromium\";v=\"{major}\", \"Google Chrome\";v=\"{major}\", \"Not_A Brand\";v=\"24\""
    );
    request = add_header(request, "sec-ch-ua", &sec_ch_ua);
    request = add_header(request, "sec-ch-ua-mobile", "?0");
    request = add_header(request, "sec-ch-ua-platform", "\"Windows\"");
    if let Some(origin) = shape.form_origin {
        request = add_header(request, "origin", origin);
    }
    if shape.form_origin.is_some() {
        request = add_header(request, "content-type", "application/x-www-form-urlencoded");
    }
    request = add_header(request, "upgrade-insecure-requests", "1");
    request = add_header(request, "user-agent", &user_agent);
    request = add_header(
        request,
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    );
    request = add_header(
        request,
        "sec-fetch-site",
        match shape.site {
            FetchSite::None => "none",
            FetchSite::SameOrigin => "same-origin",
            FetchSite::CrossSite => "cross-site",
        },
    );
    request = add_header(request, "sec-fetch-mode", "navigate");
    if shape.user_gesture {
        request = add_header(request, "sec-fetch-user", "?1");
    }
    request = add_header(request, "sec-fetch-dest", "document");
    if let Some(referer) = shape.referer {
        request = request.header(reqwest::header::REFERER, referer);
    }
    // Only encodings this crate can decode (reqwest's gzip, brotli, deflate).
    request = add_header(request, "accept-encoding", "gzip, deflate, br");
    request = add_header(request, "accept-language", "en-US,en;q=0.9");
    add_header(request, "priority", "u=0, i")
}

fn add_header(request: RequestBuilder, name: &str, value: &str) -> RequestBuilder {
    request.header(
        HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
        HeaderValue::from_str(value).expect("valid header value"),
    )
}

#[cfg(test)]
mod tests;
