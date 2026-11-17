//! The outbound HTTP fetch: SSRF-hardened request, streaming read with an
//! optional byte cap.
//!
//! ## Threat model
//!
//! The proxy fetches URLs a MODEL chose. A model can be talked into
//! fetching `http://169.254.169.254/latest/meta-data/` or
//! `http://10.0.0.5/internal/`. So the fetch layer treats every URL as
//! hostile input:
//!
//! * **scheme allow-list** — `http` and `https` only. No `file:`, no
//!   `gopher:`, no `data:`.
//! * **no credentials in the URL** — `https://user:pass@host/` would leak
//!   the credentials into the request line and into any log.
//! * **IP range checks** — every address the hostname resolves to is
//!   checked against the private/reserved ranges. This is applied to the
//!   RESOLVED addresses, not to the hostname, so `http://localhost/` and
//!   `http://127.0.0.1/` are caught as well as a hostname that points
//!   at `10.0.0.5`.
//! * **DNS pinning** — the checked address is the address the connection
//!   is made to, so a second lookup (a "DNS rebinding" attack, where the
//!   name resolves to a public IP the first time and a private one the
//!   second) cannot slip a different address past the check.
//! * **redirect re-validation** — every hop is re-checked with the same
//!   rules, so a public URL cannot redirect into the private network.
//!
//! ## What is NOT here
//!
//! There is no domain allow-list and no URL-length limit: the operator
//! asked for "fetch any URL the model asks for". The bounds that exist
//! are the total timeout and the optional byte cap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use crate::config::WebFetchConfig;
use crate::tools::web_fetch::extract;

/// The user agent the proxy sends when fetching a page.
///
/// A real, honest UA. Some sites serve a degraded page to an obviously
/// bot-like UA, and there is no reason to pretend to be anything else.
const USER_AGENT: &str = concat!(
    "openai-to-anthropic-proxy/",
    env!("CARGO_PKG_VERSION"),
    " (web_fetch)"
);

/// How many redirects are followed before giving up.
const MAX_REDIRECTS: usize = 5;

/// The outcome of one fetch.
///
/// The error type is a plain `String` (never an `AppError`): a failed
/// fetch must degrade the tool result, not fail the client request.
#[derive(Debug)]
pub struct FetchedPage {
    /// The final URL, after redirects.
    pub final_url: String,
    /// The page title, when the extractor found one.
    pub title: Option<String>,
    /// The extracted readable text.
    pub text: String,
}

/// A failed fetch, with the machine-readable reason.
#[derive(Debug)]
pub struct FetchError {
    /// The `error_code` from Anthropic's `web_fetch` error enum.
    pub code: &'static str,
    /// A human-readable detail, safe to log (never contains credentials).
    pub detail: String,
}

impl FetchError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// Whether private and reserved addresses are refused.
///
/// The proxy ALWAYS uses [`AddressPolicy::Strict`]: the URL comes from a
/// model, so a loopback or private address is an SSRF attempt. The
/// permissive variant exists only so the tests can fetch from a local
/// test server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressPolicy {
    /// Refuse loopback, private, link-local and reserved addresses.
    Strict,
    /// Allow any address.
    ///
    /// TEST USE ONLY.
    #[cfg(test)]
    AllowPrivate,
}

/// Fetch `url` and return its extracted text.
///
/// The whole operation — connect, read, extract — is bounded by
/// `cfg.timeout_secs`. `cfg.max_bytes` caps the number of bytes read from
/// the network; `None` means unlimited. `cfg.max_content_bytes` caps the
/// extracted text; `None` means unlimited.
pub async fn fetch(url: &str, cfg: &WebFetchConfig) -> Result<FetchedPage, FetchError> {
    fetch_with_policy(url, cfg, AddressPolicy::Strict).await
}

/// Fetch `url` under an explicit [`AddressPolicy`].
async fn fetch_with_policy(
    url: &str,
    cfg: &WebFetchConfig,
    policy: AddressPolicy,
) -> Result<FetchedPage, FetchError> {
    let timeout = cfg.timeout_secs.map(Duration::from_secs);

    // The timeout must cover the WHOLE operation, so it wraps everything
    // rather than being handed to the client (which would only bound the
    // request, not the extraction).
    match timeout {
        Some(t) => tokio::time::timeout(t, fetch_inner(url, cfg, policy))
            .await
            .unwrap_or_else(|_| {
                Err(FetchError::new(
                    "url_not_accessible",
                    format!("fetch timed out after {}s", t.as_secs()),
                ))
            }),
        None => fetch_inner(url, cfg, policy).await,
    }
}

async fn fetch_inner(
    url: &str,
    cfg: &WebFetchConfig,
    policy: AddressPolicy,
) -> Result<FetchedPage, FetchError> {
    let mut target = ValidatedUrl::parse(url)?;
    let mut pinned = target.pin(policy).await?;

    let mut hops = 0usize;

    loop {
        let client = build_client(cfg, &target.host, pinned)?;
        let response =
            client.get(target.to_url()).send().await.map_err(|e| {
                FetchError::new("url_not_accessible", format!("request failed: {e}"))
            })?;

        let status = response.status();
        if status.is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(FetchError::new(
                    "url_not_accessible",
                    format!("too many redirects (>{MAX_REDIRECTS})"),
                ));
            }
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Err(FetchError::new(
                    "url_not_accessible",
                    "redirect response had no Location header",
                ));
            };
            // Re-validate the hop with the SAME rules as the first URL:
            // a public URL must not be able to redirect into the private
            // network, or onto another scheme.
            let next = resolve_redirect(&target.to_url(), location)?;
            target = ValidatedUrl::parse(&next)?;
            // Re-resolve and re-check: the redirect may point at a
            // different host entirely.
            pinned = target.pin(policy).await?;
            continue;
        }

        if !status.is_success() {
            return Err(FetchError::new(
                "url_not_accessible",
                format!("server returned {status}"),
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let bytes = read_capped(response, cfg.max_bytes).await?;
        let (title, text) = extract::extract(&bytes, &target.to_url(), content_type.as_deref());
        let text = truncate(text, cfg.max_content_bytes);

        return Ok(FetchedPage {
            final_url: target.to_url(),
            title,
            text,
        });
    }
}

/// A URL that has passed the proxy's validation rules.
#[derive(Debug)]
struct ValidatedUrl {
    scheme: String,
    /// The host, lowercased, WITHOUT IPv6 brackets.
    host: String,
    port: u16,
    /// Path plus query.
    path: String,
}

impl ValidatedUrl {
    /// Parse and validate a URL.
    ///
    /// Rejects anything that is not `http`/`https`, and any URL carrying
    /// credentials.
    fn parse(url: &str) -> Result<Self, FetchError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| FetchError::new("invalid_tool_input", format!("invalid URL: {e}")))?;

        let scheme = parsed.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(FetchError::new(
                "invalid_tool_input",
                format!("unsupported URL scheme '{scheme}'"),
            ));
        }

        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(FetchError::new(
                "invalid_tool_input",
                "URL must not contain credentials",
            ));
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| FetchError::new("invalid_tool_input", "URL has no host"))?
            .to_ascii_lowercase();
        // `Url::host_str` keeps the brackets around an IPv6 literal;
        // strip them so the address parses as an `IpAddr`.
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .map_or_else(|| host.clone(), str::to_owned);

        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| FetchError::new("invalid_tool_input", "URL has no usable port"))?;

        let path = {
            let mut p = parsed.path().to_owned();
            if let Some(q) = parsed.query() {
                p.push('?');
                p.push_str(q);
            }
            p
        };

        Ok(Self {
            scheme,
            host,
            port,
            path,
        })
    }

    /// Resolve the host and pin the address the connection will use.
    ///
    /// The returned address has already been checked against the private
    /// and reserved ranges (unless the policy says otherwise).
    async fn pin(&self, policy: AddressPolicy) -> Result<std::net::SocketAddr, FetchError> {
        let addrs = resolve_and_check(&self.host, self.port, policy).await?;
        addrs.first().copied().ok_or_else(|| {
            FetchError::new("url_not_accessible", "host resolved to no usable address")
        })
    }

    /// Render the URL back to a string.
    fn to_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{}:{}{}", self.scheme, host, self.port, self.path)
    }
}

/// Resolve `host` and keep only the addresses that are safe to connect to.
///
/// Returns the checked addresses in the order they were resolved.
async fn resolve_and_check(
    host: &str,
    port: u16,
    policy: AddressPolicy,
) -> Result<Vec<std::net::SocketAddr>, FetchError> {
    // An IP literal never goes through DNS: check it directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !address_allowed(ip, policy) {
            return Err(FetchError::new(
                "url_not_allowed",
                format!("refusing to fetch a private or reserved address ({ip})"),
            ));
        }
        return Ok(vec![std::net::SocketAddr::new(ip, port)]);
    }

    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| FetchError::new("url_not_accessible", format!("DNS lookup failed: {e}")))?;

    let mut addrs = Vec::new();
    for addr in resolved {
        if !address_allowed(addr.ip(), policy) {
            // The host resolved to a private address: refuse the whole
            // host rather than picking "a good" address, so a rebinding
            // name cannot be used to smuggle one private address past the
            // check.
            return Err(FetchError::new(
                "url_not_allowed",
                format!(
                    "refusing to fetch a private or reserved address ({})",
                    addr.ip()
                ),
            ));
        }
        addrs.push(addr);
    }

    if addrs.is_empty() {
        return Err(FetchError::new(
            "url_not_accessible",
            "host resolved to no addresses",
        ));
    }
    Ok(addrs)
}

/// Whether an address may be connected to under the given policy.
fn address_allowed(ip: IpAddr, policy: AddressPolicy) -> bool {
    match policy {
        AddressPolicy::Strict => is_public_ip(ip),
        #[cfg(test)]
        AddressPolicy::AllowPrivate => true,
    }
}

/// Whether `ip` is a routable public address.
///
/// Everything that is loopback, private, link-local (including the cloud
/// metadata address), unique-local, multicast, unspecified, or otherwise
/// reserved is rejected.
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    // 0.0.0.0/8 — "this network".
    if o[0] == 0 {
        return false;
    }
    // 10.0.0.0/8 — private.
    if o[0] == 10 {
        return false;
    }
    // 100.64.0.0/10 — carrier-grade NAT.
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return false;
    }
    // 127.0.0.0/8 — loopback.
    if o[0] == 127 {
        return false;
    }
    // 169.254.0.0/16 — link-local, INCLUDING the cloud metadata address
    // 169.254.169.254.
    if o[0] == 169 && o[1] == 254 {
        return false;
    }
    // 172.16.0.0/12 — private.
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return false;
    }
    // 192.0.0.0/24 — IETF protocol assignments.
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // 192.168.0.0/16 — private.
    if o[0] == 192 && o[1] == 168 {
        return false;
    }
    // 198.18.0.0/15 — benchmarking.
    if o[0] == 198 && (o[1] & 0xfe) == 18 {
        return false;
    }
    // 198.51.100.0/24, 203.0.113.0/24 — documentation.
    if o[0] == 198 && o[1] == 51 && o[2] == 100 {
        return false;
    }
    if o[0] == 203 && o[1] == 0 && o[2] == 113 {
        return false;
    }
    // 224.0.0.0/4 — multicast (and 240.0.0.0/4, reserved).
    if o[0] >= 224 {
        return false;
    }
    true
}

fn is_public_ipv6(v6: Ipv6Addr) -> bool {
    // ::/128 — unspecified.
    if v6.segments() == [0, 0, 0, 0, 0, 0, 0, 0] {
        return false;
    }
    // ::1/128 — loopback.
    if v6.segments() == [0, 0, 0, 0, 0, 0, 0, 1] {
        return false;
    }
    // ::ffff:0:0/96 — IPv4-mapped. Judge the embedded IPv4 address.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_public_ipv4(v4);
    }
    // fc00::/7 — unique local.
    if (v6.segments()[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // fe80::/10 — link-local.
    if (v6.segments()[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // ff00::/8 — multicast.
    if (v6.segments()[0] & 0xff00) == 0xff00 {
        return false;
    }
    true
}

/// Build the HTTP client for one hop.
///
/// The client is built per hop (not shared) so the DNS override pins the
/// exact address that was validated above. `redirect(Policy::none())`
/// because the proxy follows redirects ITSELF, so it can re-validate
/// every hop.
fn build_client(
    cfg: &WebFetchConfig,
    host: &str,
    pinned: std::net::SocketAddr,
) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        // The per-request timeout is a backstop; the outer
        // `tokio::time::timeout` is the authoritative bound.
        .timeout(
            cfg.timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(300)),
        );

    // Pin the host to the validated address. `resolve` only applies to a
    // NAME; an IP-literal host needs no override because the connection
    // already goes to that literal.
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve(host, pinned);
    }

    builder
        .build()
        .map_err(|e| FetchError::new("unavailable", format!("could not build HTTP client: {e}")))
}

/// Read the response body, stopping at `max_bytes` when one is set.
///
/// The cap is applied to the STREAM, so a page that is far larger than
/// the cap costs only the cap in memory and bandwidth. A body that hits
/// the cap is truncated, not rejected: the model gets the leading part of
/// the page.
async fn read_capped(
    response: reqwest::Response,
    max_bytes: Option<u64>,
) -> Result<Vec<u8>, FetchError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| FetchError::new("url_not_accessible", format!("read failed: {e}")))?;
        if let Some(max) = max_bytes {
            let room = max.saturating_sub(buf.len() as u64);
            if room == 0 {
                break;
            }
            let take = usize::try_from(room.min(chunk.len() as u64)).unwrap_or(chunk.len());
            buf.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                break;
            }
        } else {
            buf.extend_from_slice(&chunk);
        }
    }
    Ok(buf)
}

/// Resolve a `Location` header against the URL it came from, and
/// re-validate it.
///
/// The scheme and credentials are checked here so a redirect cannot move
/// the fetch to `file:` or smuggle credentials in.
fn resolve_redirect(base: &str, location: &str) -> Result<String, FetchError> {
    let base = url::Url::parse(base)
        .map_err(|e| FetchError::new("url_not_accessible", format!("bad base URL: {e}")))?;
    let next = base
        .join(location)
        .map_err(|e| FetchError::new("url_not_accessible", format!("bad redirect target: {e}")))?;
    let next = next.to_string();
    // Re-run the same validation the first URL went through.
    ValidatedUrl::parse(&next)?;
    Ok(next)
}

/// Truncate the extracted text at a paragraph boundary.
///
/// `max_bytes` is `None` for "no limit" (the default). The cut is made at
/// the last paragraph break that fits, so the model never receives half
/// a sentence where it could have received a whole paragraph.
fn truncate(mut text: String, max_bytes: Option<u64>) -> String {
    let Some(max) = max_bytes else {
        return text;
    };
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if text.len() <= max {
        return text;
    }
    // Cut at the last paragraph break inside the budget.
    let head = &text[..max];
    let cut = head.rfind("\n\n").unwrap_or(0);
    text.truncate(cut);
    text.push_str("\n\n[content truncated]");
    text
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
