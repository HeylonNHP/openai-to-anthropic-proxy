# `web_fetch` resilience plan

**Scope:** make the proxy's server-side `web_fetch` (and `web_search`'s page
fetches) resilient against anti-bot blocking, without defeating access
controls, and without losing the diagnostics that explain a failure.

**Repo:** `C:\git\openai-to-anthropic-proxy` (Rust edition 2024, reqwest 0.12.28)
**Plan date:** 2026-11-19
**Status:** proposal — nothing in this plan has been implemented. Every file
path, function name, config field and URL below was read out of the working
tree on the plan date; every line number is a pointer, not a promise.

---

## 0. TL;DR

Three separate defects are stacked behind the reported `403`:

1. **The request does not look like a client the site is willing to serve.**
   `src/tools/web_fetch/fetch.rs` sends only `user-agent:
   openai-to-anthropic-proxy/0.1.0 (web_fetch)` and `accept: */*`. No
   `Accept-Language`, no client hints, no `Sec-Fetch-*`, HTTP/1.1 only.
   BOM's WAF rejects that shape.
2. **The failure is silent.** `fetch.rs` drops the response body on any
   non-2xx, so the caller sees "0 bytes" and the operator sees
   `server returned 403 Forbidden` with no reason.
3. **The URL the model asked for is the wrong URL.** The BOM town forecast
   page is a rendered page; the authoritative content is an official
   machine-readable product (`IDQ11296.xml`, containing `QLD_PT022` =
   Townsville). No amount of header tuning makes the HTML page the right
   source.

The plan fixes them in that order (diagnostics → identity → source), because
each phase is independently shippable and the first two are prerequisites for
verifying the third.

**The honesty question, answered up front:** ship `honest` as the default
(self-identifying UA), add an **opt-in, per-domain `browser` profile**, and
never escalate automatically. The operator decides which sites get the Chrome
persona; the proxy never silently changes identity mid-session. Details in
§2.

---

## 1. Diagnosis — what the wire actually looks like today

`src/tools/web_fetch/fetch.rs`:

| Element | Line (approx.) | Today |
|---|---|---|
| `const USER_AGENT` | 44 | `openai-to-anthropic-proxy/0.1.0 (web_fetch)` |
| `build_client(cfg, host, pinned)` | 508 | `.redirect(Policy::none())`, `.user_agent(USER_AGENT)`, `.timeout(...)`, `.resolve(host, pinned)` |
| request build | 204 | `client.get(target.to_url()).send()` — **no per-request headers at all** |
| redirect loop | 210 | manual, `MAX_REDIRECTS = 5` (line 40), re-validates SSRF + `DomainRules` per hop, re-pins DNS per hop |
| non-2xx handling | 241 | `FetchError::new("url_not_accessible", format!("server returned {status}"))` — **the body is never read** |
| `read_capped(response, max_bytes)` | 546 | streams `bytes_stream()`, success path only |

So the only headers on the wire are `user-agent` (above) and reqwest's own
`accept: */*`. There is no `Accept`, no `Accept-Language`, no `Accept-Encoding`
beyond reqwest's automatic one, no `sec-ch-ua*`, no `Sec-Fetch-*`, no
`Upgrade-Insecure-Requests`.

Dependency facts (verified against `Cargo.toml` / `Cargo.lock`):

* `reqwest` is declared `default-features = false` with
  `["json","stream","rustls-tls","gzip","brotli","deflate","charset"]`.
* Therefore **absent**: `http2` (so the `h2` crate is absent and ALPN
  advertises only `http/1.1`), `cookies` (no jar), `zstd`, `socks`,
  `system-proxy` (so `HTTP_PROXY`/`HTTPS_PROXY` are silently ignored).
* `native-tls` and `rustls-native-certs` are absent, so the trust store is
  the bundled `webpki-roots` 1.0.8 (Mozilla list), **not** the Windows
  certificate store.
* `encoding_rs` **is** present (the `charset` feature), but
  `src/tools/web_fetch/extract.rs` decodes with `std::str::from_utf8` and
  `String::from_utf8_lossy` only — the `charset=` parameter of `Content-Type`
  is parsed nowhere.

### Why the manual `curl` worked

The same machine, the same URL, `curl` with browser-like headers → HTTP 200,
~41 KB. `curl`'s TLS ClientHello is OpenSSL/Schannel, **not** Chrome's
BoringSSL fingerprint. Therefore BOM's block is **header-based, not
TLS-fingerprint-based**. That is the single most important fact in this
document: it puts the fix in the cheap tier (HTTP hygiene) and rules out the
expensive tier (TLS impersonation).

---

## 2. Decision A — the honesty / impersonation tension

### 2.1 The tension, stated honestly

The project has a stated stance (`src/tools/web_fetch/fetch.rs`, the
`USER_AGENT` doc comment):

> A real, honest UA. Some sites serve a degraded page to an obviously
> bot-like UA, and there is no reason to pretend to be anything else.

The user asked for "resilience against anti-bot measures". Those two pull in
opposite directions. Pretending to be Chrome is a small lie that is invisible
in the code but visible to every site the proxy touches; and BOM's own 403
page says the site "does not support web scraping" and points users at an
anonymous FTP service — i.e. the site has *told us* what the sanctioned
machine interface is.

There is also a third consideration that is easy to miss: **a lying UA
invites a legal problem that an honest UA does not.** Under the Australian
Copyright Act 1969 s 196AB ("circumventing a technological protection
measure") and the target's terms of service, deliberately presenting yourself
as a browser to get past a WAF is materially different from sending a
polite, well-formed request that a site accepts. That asymmetry is the
strongest argument for keeping `honest` as the shipped default and making
the browser profile an explicit, operator-visible, per-domain choice.

### 2.2 The options considered

| Option | Fixes the BOM bug? | Honesty | Cost / risk |
|---|---|---|---|
| **A.** `honest` only, forever | ✗ (BOM still 403s) | Maximum | zero |
| **B.** `browser` everywhere, default | ✓ | Low — every site sees a fake Chrome | Medium (breaks the project's stated stance; a WAF that compares IP reputation with UA will still block) |
| **C.** `honest` default + **per-domain opt-in** `browser` profile | ✓ (operator opts `www.bom.gov.au` in) | High — the lie is scoped, named and visible | Small extra config |
| **D.** Auto-escalate: try `honest`, on 403 retry as `browser` | ✓ | Low, and *deceptive by construction* | Breaks cookie/identity coherence; looks like an attacker probing |

### 2.3 Recommendation — Option C

**Ship `honest` as the default. Add a `browser` profile that the operator
turns on globally or for named domains. Never escalate automatically.**

Concretely:

* `web_fetch.profile`: `"honest"` (default) | `"browser"`.
* `web_fetch.browser_profile_domains`: a list of domains that get the
  `browser` profile **even when `profile` is `honest`**. The list is the
  scoped-lie mechanism, and it is printed in the TUI at startup so the
  operator can see exactly which sites the proxy pretends to be a browser on.
* **Default value of that list: `["html.duckduckgo.com", "duckduckgo.com"]`** —
  and only that. See §2.5.
* **No auto-escalation.** A 403 never causes the proxy to re-send the request
  with a different identity. Reason: a challenge cookie (`cf_clearance`) is
  bound to the (IP, TLS session, UA) triple it was issued to, so a proxy that
  rotates identity between requests invalidates the very cookies it is trying
  to collect, and a WAF watching for "one client, two identities" sees an
  attacker. Identity is therefore resolved **per host, once, from config**,
  and is stable for the life of the process.
* **One module, one identity.** `src/tools/web_search/ddg.rs` currently
  hard-codes its own Chrome UA (`BROWSER_USER_AGENT`, line ~34,
  `Chrome/124.0.0.0` — two years stale) with no configuration knob. The plan
  moves both tools onto one shared module (`src/tools/http_profile.rs`), so
  there is exactly one place where the proxy's identity is decided.

### 2.4 What each profile sends

The `honest` profile deliberately does **not** send `sec-ch-ua*` or
`Sec-Fetch-*`: those are browser-only client hints, and sending them
alongside a self-identifying UA is both a lie and an *inconsistency* a WAF
can cross-check. The `honest` profile is a small, complete, consistent
client:

```
user-agent: openai-to-anthropic-proxy/0.1.0 (+https://github.com/<owner>/openai-to-anthropic-proxy; web_fetch)
accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8
accept-language: en-US,en;q=0.9
accept-encoding: gzip, deflate, br
```

The `browser` profile is the full Chrome set in §4.3.

### 2.5 The `web_search` constraint (important)

DuckDuckGo serves a CAPTCHA challenge page to obvious bots, which is why
`ddg.rs` hard-codes a Chrome UA today. If `honest` becomes the global
default and the DDG request follows it, **`web_search` breaks**. That is why
the default `browser_profile_domains` list contains the DuckDuckGo hosts: the
search endpoint keeps exactly the identity it has today, and nothing else
changes. The UA value moves from a hard-coded constant in `ddg.rs` to the
shared module, but the *behaviour* of `web_search` is unchanged.

If the operator sets `profile: "browser"` globally, the list becomes
redundant and the DDG entry is harmless.

### 2.6 The provenance rule

When the proxy uses the browser profile, it says so:

* the TUI prints one line at startup:
  `outbound identity: honest (openai-to-anthropic-proxy/0.1.0) · browser profile for: html.duckduckgo.com, www.bom.gov.au`;
* every fetch made with the browser profile adds a `tracing::debug!` line
  naming the host and the profile used.

The proxy never claims to be a human; it claims to be a browser-shaped client
on named hosts, and the operator can see which hosts those are.

---

## 3. Decision B — the source-preference strategy (the BOM answer)

### 3.1 The requirement

"Resilience against anti-bot measures" must **not** mean "defeat the WAF".
The BOM site has told us its sanctioned machine interface. The correct fix is
to fetch that interface.

### 3.2 Why the three obvious mechanisms are wrong (or insufficient)

**(a) A hard-coded per-domain table inside the binary — rejected.**
It couples a general-purpose proxy to one government department; every new
site needs a code change and a release; the mapping data (town → product
code) is *data*, not logic, and belongs in configuration.

**(b) Generic feed/JSON-LD discovery — rejected as the primary mechanism.**
It is attractive, but it cannot solve the problem that is actually in front
of us: the canonical machine-readable source for a BOM town forecast is
identified by a *product ID* (`IDQ11296`) and an *area code* (`QLD_PT022`),
and no `<link rel="alternate">` on the HTML page points at it. Discovery
finds *published feeds*; it cannot find a product-ID XML package that the
page merely renders. Two secondary problems: discovery costs an extra
request on every fetch (latency + one more untrusted URL to validate), and
its results are a moving target.

*Honest caveat:* whether the BOM town page advertises any feed has **not**
been verified (no network access was used for this plan). If it does, generic
discovery is a reasonable *additional* Phase 7, but it is not a substitute for
the mapping table.

**(c) A generic "try the FTP mirror" rule — not possible as stated.**
The one BOM endpoint that works with *no* headers at all is
`ftp://ftp.bom.gov.au/anon/gen/fwo/IDQ11296.xml`. reqwest cannot speak FTP,
and the SSRF guard deliberately allows only `http`/`https`. Adding an FTP
client is a large, attack-surface-heavy change for one site.

### 3.3 Recommendation — config-driven **source hints**

Add `web_fetch.source_hints`: an ordered list of operator-supplied rules that
say *"when the model asks for a URL matching this pattern, fetch this
canonical machine-readable source instead (or first)"*.

```json
"web_fetch": {
  "source_hints": [
    {
      "match": "www.bom.gov.au/places/qld/townsville-city/forecast",
      "urls": ["https://www.bom.gov.au/fwo/IDQ11296.xml"],
      "contains": "QLD_PT022",
      "note": "Bureau of Meteorology Town Forecast XML Package QLD (product IDQ11296), area QLD_PT022."
    }
  ]
}
```

Semantics:

| field | meaning |
|---|---|
| `match` | host, or host + path prefix — the same syntax `DomainRules` already uses (`src/tools/domains.rs`). Matched against the requested URL. |
| `urls` | ordered candidate URLs. The first that succeeds wins. If all fail, the fetcher falls back to the originally requested URL and reports the hint failure in the log. |
| `contains` | optional marker. After fetching a candidate, only the section of the document containing this marker is kept (a bounded byte window around the marker), so the model gets the Townsville area rather than the whole state. |
| `note` | optional provenance line. It is prepended to the extracted text so the **model knows where the content actually came from** — a different source than the URL it asked for. This is a correctness requirement, not decoration. |

Why config-driven rather than built-in:

* the mapping data (URL pattern → product ID → area code) is per-site data
  that changes when BOM reorganises its products; it must be fixable by
  editing `proxy.json`, not by shipping a binary;
* it keeps the default behaviour of the proxy **unchanged** (the shipped
  default list is empty), so shipping this feature is a no-op for every
  existing user;
* it generalises: the same mechanism covers a news site with an RSS feed, a
  government API, a docs site with a `.json` index, etc.

The BOM example is documented in `proxy.json-guide.md` with the exact URLs and
product IDs (below) so an operator can copy it into their `proxy.json`.

### 3.4 The exact BOM facts (verified live on the plan date)

| Thing | Value |
|---|---|
| The page that 403s | `https://www.bom.gov.au/places/qld/townsville-city/forecast` |
| `IDSS40301.94822` (the product ID in the original screenshot) | **Does not exist** in any current BOM product set. Almost certainly garbled. Do not chase it. |
| Town Forecast XML Package QLD | **`IDQ11296`** — contains `QLD_PT022` = "Townsville", with the full extended wording |
| Précis Forecast Package QLD | `IDQ11295` |
| The product ID printed on the Townsville City web page | `IDQ10150` (précis + the `QLD_PW005` district text) |
| Coastal Waters | `IDQ11290` |
| QLD observations | `IDQ60920` (Townsville Aero = WMO `94294`, `bom-id 032040`) |
| Naming conventions that **no longer exist** | there is no `latest_` prefix; there is no per-station `/fwo/IDx60901/<ID>.<wmo>.json` directory shape (those 404/550 now) |
| The HTTP mirror of the anonymous FTP tree | **flat**: `https://www.bom.gov.au/fwo/IDQ11296.xml` — works with a browser UA, **403 with a default curl UA** |
| The anonymous FTP original | `ftp://ftp.bom.gov.au/anon/gen/fwo/IDQ11296.xml` — works with **no headers at all**, no WAF, byte-identical (SHA-256) to the HTTPS copy. Not fetchable by this proxy (no FTP support, and the SSRF guard allows only http/https). |
| JSON API | `https://api.bom.gov.au/apikey/v1/forecasts/texts?aac=QLD_PT022&timezone=Australia/Brisbane` — fields `issue_time_utc`, `issue_time_next_utc`, per-day `precis_text` and `locality_text` (the verbatim sentence), `advice_summary`, `fire_danger`. Works with a browser UA, **403 with a default curl UA**. No API key needed in testing; no published licence field in the payload. |
| Legacy geohash API | `https://api.weather.bom.gov.au/v1/locations/rk86kvk/forecasts/daily` — works with a **plain** UA, CORS-open, returns `extended_text` and `issue_time`. **Every response carries "You must not use, copy or share it."** |
| Licence | Anonymous FTP products are free for personal use / use within your organisation; **not** for supply to another person and **not** for commercial use. Secondary distribution requires formal permission. Attribution: "Bureau of Meteorology, © Commonwealth of Australia". |

Two consequences the operator must be told about:

1. **The BOM HTTP mirror and the JSON API also need browser headers.** The
   source hint does not remove the need for Phase 2 — it removes the need to
   scrape a rendered page, and it moves the fetch onto an official
   product with a documented licence.
2. **Licence.** The `api.weather.bom.gov.au` endpoint's payload forbids
   sharing, so shipping it inside an AI agent's context is a licence
   question, not just a technical one. It is therefore **not** in the
   recommended default hint; if the operator wants it, they add it
   explicitly and take responsibility. The BOM licence also forbids
   commercial use and onward supply, which matters if this proxy is used
   commercially.

### 3.5 The end-to-end BOM story after the plan

1. Operator sets `web_fetch.profile: "browser"` (or adds
   `www.bom.gov.au` to `browser_profile_domains`).
2. Operator adds the source hint above (copy-paste from the guide).
3. The model asks for
   `https://www.bom.gov.au/places/qld/townsville-city/forecast`.
4. The proxy fetches `https://www.bom.gov.au/fwo/IDQ11296.xml` with the
   Chrome persona, keeps the `QLD_PT022` section, and hands the model the
   Townsville forecast text prefixed with the provenance note.
5. The model answers with the forecast.

No CAPTCHA, no TLS impersonation, no third party in the path, and the
content comes from the interface BOM publishes for machines.

---

## 4. The shared identity module

### 4.1 New file: `src/tools/http_profile.rs` (+ `src/tools/http_profile/tests.rs`)

House rule: every unit-test module lives in its own `tests.rs` next to the
module it owns, declared `#[cfg(test)] mod NAME;` (enforced by
`tests/project_layout.rs`).

```rust
//! One place that decides what the proxy looks like on the wire.
//!
//! Both `web_fetch` and `web_search` use this module, so there is exactly
//! one User-Agent string and one client-hint set in the binary.

/// How the proxy presents itself to the sites it fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Self-identifying. The shipped default.
    #[default]
    Honest,
    /// A consistent, recent Chrome persona. Operator opt-in only.
    Browser,
}

/// What a request is for. It selects the `Sec-Fetch-*` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A top-level navigation to a document (the `web_fetch` case).
    Navigation,
    /// A form POST to a search engine (the `web_search` case).
    FormSubmit,
}

/// Which hop of a redirect chain a request is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hop<'a> {
    /// The URL this request goes to.
    pub to: &'a str,
    /// The URL that redirected here, if this is a redirect hop.
    pub from: Option<&'a str>,
}
```

Public API:

```rust
/// The profile to use for `host` under this configuration.
///
/// STABLE for the life of the process: the answer never depends on the
/// outcome of a previous request, so a cookie jar stays coherent with the
/// identity that obtained it.
#[must_use]
pub fn profile_for_host(profile: Profile, browser_domains: &[String], host: &str) -> Profile;

/// The `User-Agent` this profile sends.
#[must_use]
pub fn user_agent(profile: Profile) -> String;

/// The full header set for one request.
#[must_use]
pub fn headers_for(profile: Profile, purpose: Purpose, hop: Hop<'_>) -> Vec<(HeaderName, HeaderValue)>;

/// Apply `headers_for` to a reqwest request builder.
pub fn apply(builder: RequestBuilder, profile: Profile, purpose: Purpose, hop: Hop<'_>) -> RequestBuilder;
```

### 4.2 The single-source-of-truth rule (UA ↔ `sec-ch-ua`)

**Rule:** the Chrome major version appears in exactly ONE constant, and both
the `User-Agent` and the `sec-ch-ua` client hint are derived from it. A
WAF that cross-checks `User-Agent: … Chrome/<major>` against
`sec-ch-ua: "Chromium";v="<major>"` will reject a mismatch, and a mismatch
is the single easiest way to look like a bot.

```rust
/// The Chrome major version the browser profile impersonates.
///
/// ONE SOURCE OF TRUTH: the `User-Agent` and the `sec-ch-ua` client hint
/// are BOTH built from this constant, so they can never disagree.
pub const CHROME_MAJOR: u32 = 157;

/// The Chrome build the persona was captured from.
///
/// Metadata only (it dates the persona for the staleness test). The
/// `User-Agent` uses Chrome's REDUCED form — `<major>.0.0.0` — which is
/// what Chrome itself has sent since Chrome 110.
pub const CHROME_BUILD: &str = "157.0.8103.79";

/// The day the persona was captured.
pub const PERSONA_TAKEN_ON: &str = "2026-11-19";
```

*(157.0.8103.79 is the Chrome installed on the machine this plan was written
on: `C:\Program Files\Google\Chrome\Application\chrome.exe`. It must be
re-captured when the persona is refreshed — see §4.5.)*

```rust
/// The `User-Agent` of the browser profile.
///
/// Built from `CHROME_MAJOR`, never written out literally.
#[must_use]
pub fn browser_user_agent() -> String {
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{CHROME_MAJOR}.0.0.0 Safari/537.36"
    )
}

/// The honest `User-Agent`.
///
/// Self-identifying, and it carries a contact URL so a site operator who is
/// annoyed can reach the operator of the proxy instead of guessing.
#[must_use]
pub fn honest_user_agent() -> String {
    format!(
        "openai-to-anthropic-proxy/{}+ ({})",
        env!("CARGO_PKG_VERSION"),
        CONTACT_URL
    )
}

/// The contact URL in the honest `User-Agent`. Update this to the real
/// repository or contact address before release.
pub const CONTACT_URL: &str = "https://github.com/<owner>/openai-to-anthropic-proxy";
```

The `sec-ch-ua` value:

```rust
/// The GREASE brands Chrome rotates through.
///
/// The exact string changes between Chrome releases. WAFs check the SHAPE
/// (a brand that is not a real brand, rotating) rather than the exact text,
/// so the list is a fixed set and the pick is per request.
pub const GREASE_BRANDS: &[&str] = &[
    "Not_A Brand",
    "Not=A?Brand",
    "Not)A;Brand",
    "Not/A?Brand",
    "Not A;Brand",
    "Not-A.Brand",
];

/// The version the GREASE brand claims.
pub const GREASE_VERSION: &str = "24";

/// The `sec-ch-ua` client hint.
///
/// The two REAL brands carry `CHROME_MAJOR` — the same number as the
/// `User-Agent`. The GREASE brand rotates per request, exactly as Chrome
/// rotates it per connection; the identity does not.
#[must_use]
pub fn sec_ch_ua(grease: &str) -> String {
    format!(
        "\"Chromium\";v=\"{CHROME_MAJOR}\", \"Google Chrome\";v=\"{CHROME_MAJOR}\", \
         \"{grease}\";v=\"{GREASE_VERSION}\""
    )
}
```

The GREASE pick must not use a source of randomness that differs between the
request and the response — it is per *request*, not per session, and it does
not affect cookie validity (the real brands are what matter).

### 4.3 The exact browser header set

Sent by the `browser` profile on a `Navigation` request:

| Header | Value | Notes |
|---|---|---|
| `user-agent` | `Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36` | from `CHROME_MAJOR` |
| `accept` | `text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7` | Chrome's document accept list |
| `accept-language` | `en-US,en;q=0.9` | |
| `accept-encoding` | `gzip, deflate, br` | **only encodings this build can decode — see §4.4** |
| `sec-ch-ua` | `"Chromium";v="157", "Google Chrome";v="157", "Not_A Brand";v="24"` | GREASE brand rotates |
| `sec-ch-ua-mobile` | `?0` | |
| `sec-ch-ua-platform` | `"Windows"` | must agree with the UA's platform token |
| `sec-fetch-dest` | `document` | |
| `sec-fetch-mode` | `navigate` | |
| `sec-fetch-site` | `none` (first hop) / `same-origin` / `cross-site` (redirect hop) | computed per hop |
| `sec-fetch-user` | `?1` (first hop only) | omitted on redirects, as Chrome does |
| `upgrade-insecure-requests` | `1` | |
| `cache-control` | `max-age=0` | Chrome sends this on top-level navigations |
| `referer` | previous URL (same-origin) / origin only (cross-origin) / absent (https→http) | only on redirect hops; Chrome's `strict-origin-when-cross-origin` |

For a `FormSubmit` (the DuckDuckGo POST): identical, except
`sec-fetch-site: same-origin`, `sec-fetch-user: ?1` always, and `referer` set
to the search endpoint (matching today's `ddg.rs` behaviour).

**Deliberately NOT sent:** `connection` (hyper manages it — setting it
manually risks a protocol error), `priority`/`sec-fetch-storage-access`
(newer Chrome signals, optional — add behind a flag if a target needs them),
and any `sec-ch-ua*` in the `honest` profile.

### 4.4 The `Accept-Encoding` trap (must not get this wrong)

reqwest decodes a response based on the **response's** `Content-Encoding`
header, and only if the matching feature is compiled in. If the proxy
advertises `zstd` but the `zstd` feature is off, a server that answers
`Content-Encoding: zstd` is handed to the model **as raw compressed bytes**
— a silent corruption that looks like "0 bytes of text".

Therefore: **the advertised `Accept-Encoding` must list exactly the
encodings this build can decode.** With the current features
(`gzip`, `brotli`, `deflate`) that is `gzip, deflate, br`. If the `zstd`
feature is added, `zstd` may be added to the list in the same commit.

Recommendation: **do not add `zstd` in this round.** `zstd-sys` is a C build
dependency; `gzip, deflate, br` covers ~99 % of the web and no observed
target requires zstd.

A regression test must assert the invariant: *every token in the advertised
`Accept-Encoding` has a matching compiled reqwest feature.*

### 4.5 How the persona gets refreshed

Profiles go stale in roughly 6–10 weeks; Chrome ships every ~4 weeks.

1. The persona is a **single, dated constant block** at the top of
   `src/tools/http_profile.rs` (`CHROME_MAJOR`, `CHROME_BUILD`,
   `PERSONA_TAKEN_ON`).
2. A **staleness test** in `src/tools/http_profile/tests.rs` fails when
   `PERSONA_TAKEN_ON` is more than 70 days old (a compile-time-ish tripwire
   that makes "the persona went stale" a visible test failure rather than a
   silent degradation). It should be `#[ignore]`d by default with a clear
   message, or run only in CI — the choice is the operator's (see §9).
3. A documented refresh procedure in `proxy.json-guide.md`:
   * read the local Chrome version (`chrome://version`, or
     `Get-Item "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"`
     on Windows),
   * set `CHROME_MAJOR` to its major and `CHROME_BUILD` to its full version,
   * set `PERSONA_TAKEN_ON` to today,
   * run the test suite; the UA↔`sec-ch-ua` consistency test will fail if
     the two were edited inconsistently.
4. **Never refresh one half.** The consistency test
   (`the_user_agent_and_the_client_hint_agree_on_the_chrome_version`) is the
   guard.

### 4.6 Header name case (a judgement call, see §9)

Rust's `http` crate lowercases header names on HTTP/1.1. Chrome sends
`User-Agent`, `Sec-Fetch-Dest`, etc. Some WAFs (rare, and usually
mis-implemented ones) match header names case-sensitively.

reqwest 0.12.28 has `ClientBuilder::http1_title_case_headers()` (verified in
the vendored source, `src/async_impl/client.rs:1506`), which restores the
canonical capitalisation.

Recommendation: **enable it in the `browser` profile only** — it costs
nothing, it removes a class of failure, and the `honest` profile does not
care. It is a one-line change in `build_client` and is trivially revertible.

---

## 5. Phases

Effort scale: **S** ≈ half a day, **M** ≈ one day, **L** ≈ two days.
Each phase is independently shippable and independently revertible.

### Phase 0 — Stop throwing the block page away
**Goal:** every non-2xx tells the operator *and* the executor model what
actually happened.
**Value:** high · **Effort:** S · **Risk:** low · **Depends on:** nothing

**Files**
* `src/tools/web_fetch/fetch.rs`
* `src/tools/web_fetch/tool.rs`
* `src/tools/web_fetch/protocol.rs` (documentation only — the wire shape does
  not change)

**Concrete changes**

1. `FetchError` gains two fields:

```rust
pub struct FetchError {
    pub code: &'static str,
    pub detail: String,
    /// The HTTP status, when the failure was a response.
    pub status: Option<u16>,
    /// A short, human-readable excerpt of the error body.
    pub snippet: Option<String>,
}
```

with builder methods `with_status(u16)` and `with_snippet(String)`.

2. New constant and helper in `fetch.rs`:

```rust
/// How many bytes of a non-2xx body are read for the diagnostic.
const ERROR_BODY_MAX_BYTES: u64 = 512;

/// How many characters of the diagnostic reach the model.
pub(crate) const ERROR_SNIPPET_MAX_CHARS: usize = 300;

/// Read a bounded, human-readable excerpt of an error body.
///
/// HTML is stripped (tags and `<script>`/`style` bodies removed), control
/// characters are dropped and whitespace is collapsed, so a WAF's block
/// page becomes one readable sentence instead of 4 KB of markup.
async fn read_error_snippet(response: reqwest::Response, max_bytes: u64) -> String
```

3. In `fetch_inner`, replace the non-2xx branch (currently line ~241):

```rust
if !status.is_success() {
    let snippet = read_error_snippet(response, ERROR_BODY_MAX_BYTES).await;
    return Err(FetchError::new(
        "url_not_accessible",
        format!("server returned {status}"),
    )
    .with_status(status.as_u16())
    .with_snippet(snippet));
}
```

4. `src/tools/web_fetch/tool.rs`: replace the constant `FETCH_FAILED` with a
   function that produces a **bounded** diagnostic for the executor:

```rust
/// Text injected into the executor's transcript when the fetch failed.
pub(crate) fn failure_text(code: &str, status: Option<u16>, snippet: Option<&str>) -> String
```

producing e.g.

```
The web_fetch request failed: the server returned 403 Forbidden (the site
refused the request). Reason: This site does not support web scraping...
The page could not be retrieved; continue without it.
```

with the snippet truncated to 300 characters and the whole string capped at
600 characters. It must never look like page content.

5. The operator log gains the status and the snippet:
   `⚠ web_fetch failed (url_not_accessible): server returned 403 Forbidden — This site does not support web scraping…`
   (`src/tools/web_fetch/tool.rs::warn`, currently ~line 200).

**Verification**
* `src/tools/web_fetch/fetch/live_tests.rs` — new test
  `an_error_body_is_captured_and_reported`: the test server answers 403 with
  a body; assert `err.status == Some(403)` and `err.snippet` contains the
  body text.
* `src/tools/web_fetch/fetch/live_tests.rs` — new test
  `an_error_body_is_capped`: a 1 MB error body must not be read into memory
  (assert the snippet is ≤ 512 bytes and the body was truncated).
* `src/tools/web_fetch/tool/tests.rs` — new test
  `the_failure_text_names_the_status_and_stays_bounded`.
* Existing test `a_non_2xx_response_is_an_error_not_a_page` (in
  `live_tests.rs`) must keep passing — it asserts `err.code ==
  "url_not_accessible"` and `err.detail.contains("404")`.

**Ship it as:** one commit, no config change, no behaviour change other than
better diagnostics.

---

### Phase 1 — One shared identity module
**Goal:** `web_fetch` and `web_search` get their UA/header decisions from one
place, with an `honest` default and an opt-in `browser` profile.
**Value:** high · **Effort:** M · **Risk:** low · **Depends on:** Phase 0

**Files**
* **NEW** `src/tools/http_profile.rs` and `src/tools/http_profile/tests.rs`
* `src/tools/mod.rs` — add `pub mod http_profile;`
* `src/tools/web_fetch/fetch.rs` — delete the local `USER_AGENT` const
  (line ~44); `build_client` takes the profile; the request builder applies
  the shared headers
* `src/tools/web_search/ddg.rs` — delete `BROWSER_USER_AGENT` (line ~34) and
  the two inline `.header(...)` calls; use the shared module
* `src/tools/web_search/backend.rs` — `LiveBackend::new` takes the profile
* `src/tools/web_search/tool.rs` — `WebSearchTool::live` takes the profile
* `src/tools/server_loop/mod.rs` — `handlers()` passes
  `config.web_fetch.profile` / `config.web_fetch.browser_profile_domains`
* `src/config.rs` — the new fields (see §6)

**Concrete changes**

1. `src/tools/http_profile.rs` as specified in §4, with `Profile`,
   `Purpose`, `Hop`, `profile_for_host`, `user_agent`, `headers_for`,
   `apply`.

2. `src/config.rs`: add to `WebFetchConfig`:

```rust
/// How the proxy presents itself to the sites it fetches.
pub profile: FetchProfile,
/// Hosts that get the `browser` profile even when `profile` is `honest`.
pub browser_profile_domains: Vec<String>,
```

with `FetchProfile` living in `src/config.rs` (it is configuration; the
profile *rendering* lives in `http_profile.rs`):

```rust
/// How the proxy identifies itself on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchProfile {
    /// Self-identifying. The shipped default.
    #[default]
    Honest,
    /// A consistent Chrome persona. Operator opt-in.
    Browser,
}
```

3. `src/tools/web_fetch/fetch.rs`:

```rust
/// Build the HTTP client for one hop.
fn build_client(
    cfg: &WebFetchConfig,
    host: &str,
    pinned: std::net::SocketAddr,
    profile: Profile,
) -> Result<reqwest::Client, FetchError>
```

and in `fetch_inner`, the request becomes:

```rust
let response = http_profile::apply(
    client.get(target.to_url()),
    profile,
    Purpose::Navigation,
    Hop { to: &target.to_url(), from: previous.as_deref() },
)
.send()
.await
```

4. `src/tools/web_search/ddg.rs`: the POST becomes

```rust
let response = http_profile::apply(
    client.post(endpoint).form(&[("q", query), ("b", ""), ("kl", region)]),
    profile,
    Purpose::FormSubmit,
    Hop { to: endpoint, from: None },
)
.send()
.await
```

(the `Referer` moves into the shared module; the `Accept` for a form POST
becomes the shared `accept` value).

5. `profile_for_host` semantics, implemented once:

```rust
pub fn profile_for_host(profile: Profile, browser_domains: &[String], host: &str) -> Profile {
    if profile == Profile::Browser {
        return Profile::Browser;
    }
    let host = host.to_ascii_lowercase();
    browser_domains.iter().any(|d| {
        let d = d.trim().to_ascii_lowercase();
        !d.is_empty() && (host == d || host.strip_suffix(d).is_some_and(|r| r.ends_with('.')))
    })
    .then_some(Profile::Browser)
    .unwrap_or(Profile::Honest)
}
```

**Verification**
* `src/tools/http_profile/tests.rs` (new) — `the_user_agent_and_the_client_hint_agree_on_the_chrome_version`,
  `the_honest_profile_sends_no_client_hints`,
  `a_domain_in_the_list_gets_the_browser_profile`,
  `a_subdomain_of_a_listed_domain_gets_the_browser_profile`,
  `the_grease_brand_rotates_but_the_real_brands_do_not`,
  `the_browser_profile_advertises_only_decodable_encodings`.
* `src/tools/web_search/ddg/tests.rs` — the existing parse tests keep
  passing (they do not touch the wire); add a test that `ddg.rs` no longer
  contains its own UA constant (a grep-style assertion is acceptable, or
  simply rely on the shared-module test).
* `src/tools/web_fetch/fetch/live_tests.rs` — unchanged at this phase, plus
  the Phase 2 header assertions.

---

### Phase 2 — The browser profile on the wire (+ HTTP/2)
**Goal:** the `browser` profile is a complete, internally consistent Chrome
request; the transport advertises `h2` like Chrome does.
**Value:** high (this is the phase that actually unblocks BOM) ·
**Effort:** M · **Risk:** medium · **Depends on:** Phase 1

**Files**
* `Cargo.toml` — add the `http2` feature to `reqwest`
* `src/tools/http_profile.rs` — the full header set from §4.3
* `src/tools/web_fetch/fetch.rs` — per-hop `Sec-Fetch-Site` / `Sec-Fetch-User`
  / `Referer`; `http1_title_case_headers()` in the browser profile

**Concrete changes**

1. `Cargo.toml`:

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "gzip", "brotli", "deflate", "charset", "http2"] }
```

This adds the `h2` crate. Chrome advertises `h2,http/1.1` in ALPN; an
h1.1-only client is a "not a browser" signal that some WAFs check before
they even look at the headers.

2. `http_profile.rs` — the header table from §4.3, including per-hop
   `Sec-Fetch-Site` (`none` / `same-origin` / `cross-site`), `Sec-Fetch-User`
   (first hop only) and `Referer`
   (`strict-origin-when-cross-origin`).

3. `fetch.rs` — `build_client` gains:

```rust
if profile == Profile::Browser {
    builder = builder.http1_title_case_headers();
}
```

4. `fetch.rs` — the redirect loop must remember the previous URL so the
   `Referer` and `Sec-Fetch-Site` can be computed. The existing loop already
   keeps `target`; add a `previous: Option<String>`.

**Verification**
* `src/tools/web_fetch/fetch/live_tests.rs` — the harness must now CAPTURE
  the request bytes (see §8); then assert:
  * the browser profile sends `user-agent`, `accept`, `accept-language`,
    `accept-encoding`, `sec-ch-ua`, `sec-ch-ua-mobile`, `sec-ch-ua-platform`,
    `sec-fetch-dest`, `sec-fetch-mode`, `sec-fetch-site`, `sec-fetch-user`,
    `upgrade-insecure-requests`;
  * the honest profile sends `user-agent` and **no** `sec-ch-ua*` /
    `sec-fetch-*`;
  * `sec-ch-ua` contains `v="<the same major as the UA>"`;
  * `accept-encoding` contains only `gzip`, `deflate`, `br`.
* `src/tools/http_profile/tests.rs` — the header-set unit tests.

---

### Phase 3 — Canonical source hints (the BOM answer)
**Goal:** the fetcher can prefer an official machine-readable source for a
matched URL, so the model gets the forecast product rather than a rendered
page.
**Value:** high (this is the phase that makes the reported URL return real
content) · **Effort:** M–L · **Risk:** medium · **Depends on:** Phase 2

**Files**
* `src/config.rs` — `WebFetchConfig.source_hints`, `JsonWebFetchConfig.source_hints`
* **NEW** `src/tools/web_fetch/hints.rs` + `src/tools/web_fetch/hints/tests.rs`
* `src/tools/web_fetch/fetch.rs` — consult the hints before fetching
* `src/tools/web_fetch/extract.rs` — a bounded "window around a marker"
  extractor for the `contains` filter
* `proxy.json-guide.md`, `proxy.json.example`

**Concrete changes**

1. Config:

```rust
/// Preferred machine-readable sources, per URL pattern.
pub source_hints: Vec<SourceHint>,
```

```rust
/// One preferred source for a class of URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceHint {
    /// Host, or host + path prefix (the `DomainRules` syntax).
    pub match_pattern: String,
    /// Candidate URLs, tried in order.
    pub urls: Vec<String>,
    /// Optional marker: only the section containing it is kept.
    pub contains: Option<String>,
    /// Optional provenance line, prepended to the extracted text.
    pub note: Option<String>,
}
```

JSON:

```json
"source_hints": [
  {
    "match": "www.bom.gov.au/places/qld/townsville-city/forecast",
    "urls": ["https://www.bom.gov.au/fwo/IDQ11296.xml"],
    "contains": "QLD_PT022",
    "note": "Bureau of Meteorology Town Forecast XML Package QLD (product IDQ11296), area QLD_PT022."
  }
]
```

2. `src/tools/web_fetch/hints.rs`:

```rust
/// The hint that applies to `url`, if any.
#[must_use]
pub fn hint_for(hints: &[SourceHint], url: &str) -> Option<&SourceHint>;

/// Keep only the section of `text` containing `marker`.
///
/// A bounded window (4 KiB before, 16 KiB after) so a 2 MB state-wide
/// package does not reach the model whole.
#[must_use]
pub fn window_around(text: &str, marker: &str) -> Option<String>;
```

3. `fetch.rs::fetch_inner` — before the fetch loop:

```rust
if let Some(hint) = hints::hint_for(&cfg.source_hints, url) {
    for candidate in &hint.urls {
        match fetch_one(candidate, cfg, policy, rules, profile).await {
            Ok(mut page) => {
                if let Some(marker) = &hint.contains {
                    page.text = hints::window_around(&page.text, marker)
                        .unwrap_or(page.text);
                }
                if let Some(note) = &hint.note {
                    page.text = format!("{note}\n\n{}", page.text);
                }
                page.final_url = candidate.clone();
                return Ok(page);
            }
            Err(e) => { /* log and try the next candidate */ }
        }
    }
    // All candidates failed: fall through to the requested URL.
}
```

**Security requirement (important):** a hint URL is operator-supplied (so it
is trusted), but it must still go through **the same SSRF validation** as any
other URL (`ValidatedUrl::parse` + `resolve_and_check` + DNS pinning) and
**the same client `DomainRules`** — otherwise a hint would become a way to
bypass the client's `allowed_domains`. A hint whose URL is refused by the
client's rules is skipped, not fetched.

**Verification**
* `src/tools/web_fetch/hints/tests.rs` — `a_hint_matches_its_host_and_path`,
  `a_hint_does_not_match_a_different_path`,
  `the_window_keeps_the_marker_and_bounds_the_size`,
  `the_window_returns_none_when_the_marker_is_absent`.
* `src/tools/web_fetch/fetch/live_tests.rs` — a hint pointing at the local
  test server is preferred over the requested URL; a hint whose candidate
  404s falls back to the requested URL; a hint whose candidate is on a
  domain the client's rules block is refused.
* `src/config/tests.rs` — the new `source_hints` JSON parses, round-trips
  through the TUI save, and rejects unknown fields.

---

### Phase 4 — Bounded retry, jitter, `Retry-After`, per-host politeness
**Goal:** transient failures get a second chance; a WAF's 403 does not.
**Value:** medium · **Effort:** M · **Risk:** medium · **Depends on:** Phase 2

**Files**
* **NEW** `src/tools/web_fetch/retry.rs` + `src/tools/web_fetch/retry/tests.rs`
* `src/tools/web_fetch/fetch.rs`
* `src/config.rs` — `WebFetchConfig.max_retries`, `per_host_delay_ms`

**Concrete changes**

1. `retry.rs`:

```rust
/// Whether a status is worth retrying.
///
/// NEVER 403: a WAF's 403 is a DECISION, and retrying it with the same
/// identity escalates the block. Never 404/410/451 either — the answer
/// will not change.
#[must_use]
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// The delay a `Retry-After` header asks for, capped.
#[must_use]
pub fn retry_after(headers: &HeaderMap, cap: Duration) -> Option<Duration>;

/// The backoff for attempt `n` (0-based), with full jitter.
#[must_use]
pub fn backoff(attempt: u32) -> Duration;
```

2. `fetch.rs` — the request becomes a bounded loop:

* attempts = `1 + cfg.max_retries` (default `max_retries = 2` → 3 attempts);
* retry on `is_retryable_status` and on transport errors
  (`err.is_connect()`, `err.is_timeout()`, `err.is_request()`), **never** on
  a 403;
* delay = `retry_after(...)` if present (capped at 10 s), else
  `backoff(attempt)` = `base 250 ms × 2^attempt`, capped at 4 s, with full
  jitter;
* the whole loop stays inside the existing outer `tokio::time::timeout`, so
  the retry budget never extends the fetch's wall-clock budget;
* the DDG POST is **not** retried automatically (a form POST is not
  idempotent by contract).

3. Per-host politeness: a `HostLimiter` (a per-host `tokio::sync::Semaphore`
  of 4 and a per-host "last request" timestamp enforcing
  `per_host_delay_ms`, default 200 ms). It lives in the shared module
  (`src/tools/http_profile.rs`) so `web_fetch` and `web_search` share it.

**Verification**
* `src/tools/web_fetch/retry/tests.rs` — `a_403_is_never_retryable`,
  `a_429_is_retryable`, `a_404_is_not_retryable`,
  `retry_after_honours_the_header_and_caps_it`,
  `the_backoff_grows_and_is_jittered`.
* `src/tools/web_fetch/fetch/live_tests.rs` — `a_500_is_retried_and_then_succeeds`
  (the test server answers 500 once, then 200; assert `hits() == 2`);
  `a_403_is_not_retried` (assert `hits() == 1`).

---

### Phase 5 — A cookie jar shared across hops
**Goal:** a challenge or session cookie survives a redirect chain and across
fetches to the same host.
**Value:** medium · **Effort:** S · **Risk:** low–medium · **Depends on:** Phase 2

**Files**
* `Cargo.toml` — add the `cookies` feature to `reqwest`
* `src/tools/http_profile.rs` — the shared jar
* `src/tools/web_fetch/fetch.rs` — `build_client` passes the jar

**Concrete changes**

1. `Cargo.toml`: add `"cookies"` to the `reqwest` features.

2. A process-wide jar, shared by every per-hop client:

```rust
/// The cookie jar shared by every outbound request.
///
/// The jar is per-PROCESS, not per-client: a challenge cookie is bound to
/// (IP, TLS session, UA), and the profile for a host never changes at
/// runtime (§2.3), so the jar stays coherent with the identity that
/// obtained it.
fn shared_jar() -> &'static Arc<reqwest::cookie::Jar> { … }
```

`reqwest::cookie::Jar` is `Send + Sync` and implements
`reqwest::cookie::CookieStore`, so it can be handed to every per-hop client
with `ClientBuilder::cookie_provider(Arc::clone(jar))`.

**Note:** the current design builds a **new client per hop** (so DNS pinning
works), which means a per-client jar would be thrown away between hops —
that is exactly why the jar must be an `Arc` passed into every
`build_client` call.

**Verification**
* `src/tools/web_fetch/fetch/live_tests.rs` — `a_cookie_set_on_one_hop_is_sent_on_the_next`:
  hop 1 answers 302 with `Set-Cookie: a=b`, hop 2 asserts the request
  carries `Cookie: a=b`. (The harness must capture request headers — §8.)

---

### Phase 6 — Decode with the charset the server declared
**Goal:** a page in ISO-8859-1 or windows-1252 is decoded correctly instead
of being mangled.
**Value:** medium · **Effort:** S · **Risk:** low · **Depends on:** none

**Files**
* `src/tools/web_fetch/extract.rs`
* `src/tools/web_fetch/fetch.rs` (pass the `Content-Type` through — it
  already does)

**Concrete changes**

`extract.rs::extract` currently does `std::str::from_utf8(bytes)` and falls
back to `String::from_utf8_lossy`. Change to:

```rust
/// Decode `bytes` using the `charset=` parameter of `content_type`.
///
/// Falls back to UTF-8, then to a lossy decode. `encoding_rs` is already
/// compiled in (the `charset` feature) but was unused.
#[must_use]
pub fn decode(bytes: &[u8], content_type: Option<&str>) -> String
```

using `encoding_rs::Encoding::for_label(label)`.

**Verification**
* `src/tools/web_fetch/extract/tests.rs` — a windows-1252 page decodes to
  the right characters; a UTF-8 page is unchanged; a page with no charset
  still decodes as UTF-8.

---

## 6. Configuration additions (exact)

### 6.1 New fields on `WebFetchConfig` (`src/config.rs`, struct at ~line 346)

```rust
pub struct WebFetchConfig {
    pub enabled: bool,
    pub timeout_secs: Option<u64>,
    pub max_bytes: Option<u64>,
    pub max_content_bytes: Option<u64>,

    /// How the proxy presents itself to the sites it fetches.
    ///
    /// `Honest` (the default) sends a self-identifying User-Agent.
    /// `Browser` sends a consistent, recent Chrome persona. The operator
    /// opts in; the proxy never escalates on its own.
    pub profile: FetchProfile,
    /// Hosts that get the `browser` profile even when `profile` is
    /// `Honest`.
    ///
    /// Defaults to the DuckDuckGo hosts, because `web_search` scrapes a
    /// search engine's HTML endpoint and that endpoint answers obvious
    /// bots with a CAPTCHA.
    pub browser_profile_domains: Vec<String>,
    /// How many times a retryable failure is retried. `None` = the default
    /// (2 retries, 3 attempts total). `Some(0)` = no retries.
    pub max_retries: Option<u32>,
    /// How many bytes of a non-2xx body are read for the diagnostic.
    /// `None` = the default (512).
    pub error_body_max_bytes: Option<u64>,
    /// Minimum gap between two requests to the same host, in milliseconds.
    /// `None` = the default (200).
    pub per_host_delay_ms: Option<u64>,
    /// Preferred machine-readable sources, per URL pattern.
    pub source_hints: Vec<SourceHint>,
}
```

### 6.2 `impl Default for WebFetchConfig` (~line 363)

```rust
impl Default for WebFetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_secs: Some(DEFAULT_WEB_FETCH_TIMEOUT_SECS),
            max_bytes: None,
            max_content_bytes: None,
            profile: FetchProfile::Honest,
            browser_profile_domains: vec![
                "html.duckduckgo.com".to_owned(),
                "duckduckgo.com".to_owned(),
            ],
            max_retries: Some(DEFAULT_WEB_FETCH_MAX_RETRIES),   // 2
            error_body_max_bytes: Some(DEFAULT_WEB_FETCH_ERROR_BODY_MAX_BYTES), // 512
            per_host_delay_ms: Some(DEFAULT_WEB_FETCH_PER_HOST_DELAY_MS),     // 200
            source_hints: Vec::new(),
        }
    }
}
```

### 6.3 `JsonWebFetchConfig` (JSON shape, ~line 1961)

```rust
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonWebFetchConfig {
    pub enabled: Option<bool>,
    pub timeout_secs: Option<serde_json::Number>,
    pub max_bytes: Option<serde_json::Number>,
    pub max_content_bytes: Option<serde_json::Number>,
    /// "honest" (default) or "browser".
    pub profile: Option<String>,
    /// Comma-separated list of hosts.
    pub browser_profile_domains: Option<String>,
    pub max_retries: Option<serde_json::Number>,
    pub error_body_max_bytes: Option<serde_json::Number>,
    pub per_host_delay_ms: Option<serde_json::Number>,
    pub source_hints: Option<Vec<JsonSourceHint>>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JsonSourceHint {
    pub match_pattern: Option<String>,
    pub urls: Option<Vec<String>>,
    pub contains: Option<String>,
    pub note: Option<String>,
}
```

`proxy.json` shape:

```json
"web_fetch": {
  "enabled": false,
  "timeout_secs": 20,
  "max_bytes": null,
  "max_content_bytes": null,
  "profile": "honest",
  "browser_profile_domains": ["html.duckduckgo.com", "duckduckgo.com"],
  "max_retries": 2,
  "error_body_max_bytes": 512,
  "per_host_delay_ms": 200,
  "source_hints": []
}
```

(`match_pattern` is serialised as `"match"` via
`#[serde(rename = "match")]` if the shorter key is preferred; pick one and
be consistent.)

### 6.4 Env vars

| Env var | JSON field | Default | Meaning |
|---|---|---|---|
| `WEB_FETCH_PROFILE` | `web_fetch.profile` | `honest` | `honest` or `browser`; anything else warns and falls back to `honest` |
| `WEB_FETCH_BROWSER_PROFILE_DOMAINS` | `web_fetch.browser_profile_domains` | `html.duckduckgo.com,duckduckgo.com` | comma-separated host list |
| `WEB_FETCH_MAX_RETRIES` | `web_fetch.max_retries` | `2` | retries for transient failures |
| `WEB_FETCH_ERROR_BODY_MAX_BYTES` | `web_fetch.error_body_max_bytes` | `512` | bytes of a non-2xx body read for the diagnostic |
| `WEB_FETCH_PER_HOST_DELAY_MS` | `web_fetch.per_host_delay_ms` | `200` | minimum gap between requests to one host |
| (Phase 3) `WEB_FETCH_SOURCE_HINTS` | `web_fetch.source_hints` | none | a JSON array string; **or** omit the env override entirely — a nested list is awkward in an env var and the file is the natural place for it |

Recommendation: **no env var for `source_hints`** — it is structured data
and belongs in `proxy.json`.

### 6.5 The five edit sites in `src/config.rs`

| # | Site | Lines (approx.) | What changes |
|---|---|---|---|
| 1 | `pub struct WebFetchConfig` | 346–361 | the new fields (§6.1) |
| 2 | `impl Default for WebFetchConfig` | 363–372 | the new defaults (§6.2) + the new `DEFAULT_WEB_FETCH_*` constants near the existing `DEFAULT_WEB_FETCH_TIMEOUT_SECS` (line 66) |
| 3 | `pub struct JsonWebFetchConfig` | 1961–1971 | the new JSON fields + `JsonSourceHint` (§6.3) |
| 4 | the `web_fetch` block of `Config::resolve` | 1007–1057 | resolve the new fields with the same **env > file > default** order and the same "warn, do not fail" discipline |
| 5 | `EnvInputs` struct (1398–1404) **and** `EnvInputs::capture` (1458–1466) | as above | the `env::var` reads for the new variables |

`FetchProfile` and `SourceHint` are new types in `src/config.rs` (next to
`WebFetchConfig`); `FetchProfile` needs `FromStr`/`Display` so the resolver
can parse the env value and the TUI can round-trip it.

Note the resolver's existing helper `resolve_web_fetch_limit` (~line 1712)
treats `0` as "no limit". For `max_retries`, `0` must mean **no retries**,
not "unlimited" — so `max_retries` needs its own resolution path (or the
existing helper must be generalised). The same applies to
`error_body_max_bytes` (`0` = "do not read the error body", which is a
legitimate setting) and `per_host_delay_ms` (`0` = "no delay").

### 6.6 Documentation to update

| File | What |
|---|---|
| `proxy.json.example` | the `web_fetch` block gains the new keys with their defaults |
| `proxy.json-guide.md` §"Web fetch (`web_fetch`)" (line ~305) | the new fields, the honesty stance and the `browser` profile, the **BOM source-hint example with the exact URLs and product IDs**, and the licence note |
| `REFERENCE.md` env table (lines 43–46) | the new env vars; the "Server-side `web_fetch`" section (line ~191) gains a short "Outbound identity" subsection |
| `src/config/tests.rs` | a new `─── web_fetch ───` section (**there is currently NO `web_fetch` test in this file** — see §8.3) |
| `src/tools/web_search/README.md` | a note that the DDG UA now comes from the shared profile module |

---

## 7. Error diagnostics — the exact design

### 7.1 What is captured

On a non-2xx:

* the **status** (`403 Forbidden`);
* the first **512 bytes** of the body (`error_body_max_bytes`), decoded with
  the charset the response declared, HTML-stripped, whitespace-collapsed,
  and truncated to 300 characters.

For the BOM case this turns "0 bytes" into:

```
server returned 403 Forbidden — This site does not support web scraping.
Use the anonymous FTP service at ftp.bom.gov.au/anon/gen/fwo/ for machine
access.
```

### 7.2 What the model sees

The executor gets the `function_call_output` text. Today that is the
constant `FETCH_FAILED`. The new `failure_text(code, status, snippet)`
produces a bounded, clearly-not-content message:

```
The web_fetch request failed: the server returned 403 Forbidden (the site
refused the request). Reason: This site does not support web scraping. Use
the anonymous FTP service at ftp.bom.gov.au/anon/gen/fwo/ for machine access.
The page could not be retrieved; continue without it.
```

Rules:

* the snippet is truncated to 300 characters and the whole message to 600;
* it is prefixed with a fixed sentence so it can never be mistaken for page
  content;
* the model is told the **status**, so it can reason about "blocked" vs
  "missing" vs "server error";
* it is NOT the raw body: a WAF page can be 40 KB of HTML, and the executor's
  context is the most valuable resource the proxy has.

### 7.3 What the operator sees

`⚠ web_fetch failed (url_not_accessible): server returned 403 Forbidden — This site does not support web scraping…`
plus a `tracing::warn!` with `error_code`, `status` and `snippet` fields.

The existing one-warning-per-turn rule (`WebFetchTool::warned`) is kept, so a
turn that fetches 20 blocked URLs produces one warning, not twenty.

### 7.4 The error-code set — a recommendation

**Recommendation: do NOT add a new wire error code.**

Anthropic's `web_fetch_tool_result_error.error_code` is a fixed enum:
`invalid_tool_input`, `url_not_allowed`, `url_not_accessible`, `unavailable`,
`max_uses_exceeded`. Some client SDKs (the Python SDK types it as a
`Literal`) will fail validation on an unknown value, so inventing
`blocked` or `rate_limited` risks breaking a strict client for a
diagnostic gain.

The proxy's existing mapping stays:

| code | when |
|---|---|
| `invalid_tool_input` | malformed URL, bad scheme, credentials in the URL |
| `url_not_allowed` | private/reserved address, or a domain the client's rules refuse |
| `url_not_accessible` | DNS failure, connection failure, timeout, **any non-2xx (including 403)** |
| `unavailable` | the fetch could not be performed for another reason (e.g. the client could not be built) |
| `max_uses_exceeded` | the client's `max_uses` was used up |

The **status code and the reason live in the executor-facing output and the
operator log**, where there is no schema to break. This is also the smaller
change.

*If* the operator wants a client-visible distinction (Claude Code's UI
showing "blocked" rather than "unreachable"), the safe way is to add an
**optional sibling key** to the error block, e.g.

```json
{ "type": "web_fetch_tool_result_error", "error_code": "url_not_accessible",
  "status": 403 }
```

rather than a new `error_code` value. That is a **decision for the user**
(§9) because a strict SDK may also reject unknown sibling keys — it is a
smaller risk than a new enum value, but it is not zero.

### 7.5 The `FetchError` vocabulary (internal, not on the wire)

The internal `FetchError::code` strings and the executor-facing
vocabulary can be richer, because nothing validates them:

| internal code | meaning | retried? |
|---|---|---|
| `url_not_accessible` | transport failure or non-2xx | only for the retryable statuses |
| `url_not_allowed` | policy refusal (SSRF, domain rules) | never |
| `invalid_tool_input` | the model's URL was unusable | never |
| `unavailable` | the proxy could not perform the fetch | never |
| `max_uses_exceeded` | the client's limit was hit | never |
| *(new)* `blocked` | a 403/429 that a WAF returned, **internally only** | never |

The internal `blocked` code is what the operator log and the executor text
use to say "the site refused us" rather than "the site is down". It never
reaches the wire.

---

## 8. Test plan

### 8.1 The harness must capture request headers

`src/tools/web_fetch/fetch/live_tests.rs` currently discards the request:

```rust
let mut buf = [0u8; 2048];
let _ = stream.read(&mut buf).await;      // ← the bytes are thrown away
```

Change `TestServer` to capture them:

```rust
/// The requests this server received, as raw header blocks.
requests: Arc<Mutex<Vec<String>>>,
```

and in the accept loop:

```rust
let mut buf = [0u8; 8192];
let n = stream.read(&mut buf).await.unwrap_or(0);
let head = String::from_utf8_lossy(&buf[..n]).to_string();
requests.lock().await.push(head);
```

Then a helper on the server:

```rust
/// The header block of the `n`-th request (0-based).
async fn request(&self, n: usize) -> String;

/// The value of one header in the `n`-th request, case-insensitively.
async fn header(&self, n: usize, name: &str) -> Option<String>;
```

The harness must also read until the end of the header block (`\r\n\r\n`),
not just one `read()` — a header set this large can arrive in more than one
TCP segment, and a partial read would make the assertions flaky. Read in a
loop until `\r\n\r\n` is seen or a byte cap (8 KiB) is reached.

`TestServer::start_with_headers` currently takes `Vec<(&'static str, String)>`;
add a variant that serves a **different response per request** (needed for
the retry tests: 500 once, then 200).

### 8.2 Test matrix

| # | Behaviour | Where | Test name |
|---|---|---|---|
| 1 | the browser profile sends the full header set | `src/tools/web_fetch/fetch/live_tests.rs` | `the_browser_profile_sends_a_browser_header_set` |
| 2 | the honest profile sends no client hints | same | `the_honest_profile_sends_no_client_hints` |
| 3 | UA and `sec-ch-ua` agree on the Chrome major | `src/tools/http_profile/tests.rs` | `the_user_agent_and_the_client_hint_agree_on_the_chrome_version` |
| 4 | the advertised `Accept-Encoding` is fully decodable | `src/tools/http_profile/tests.rs` | `the_browser_profile_advertises_only_decodable_encodings` |
| 5 | the error body is captured | `src/tools/web_fetch/fetch/live_tests.rs` | `an_error_body_is_captured_and_reported` |
| 6 | the error body is capped | same | `an_error_body_is_capped` |
| 7 | a 403 is NOT retried | same | `a_403_is_not_retried` |
| 8 | a 500 IS retried, then succeeds | same | `a_500_is_retried_and_then_succeeds` |
| 9 | the redirect hop cap still holds | same | `too_many_redirects_are_refused` (6 hops → error) |
| 10 | a redirect hop changes `Sec-Fetch-Site` / sends `Referer` | same | `a_redirect_hop_sends_a_referer_and_the_right_fetch_site` |
| 11 | a cookie survives a redirect | same | `a_cookie_set_on_one_hop_is_sent_on_the_next` |
| 12 | **the SSRF guard still refuses a loopback address** | same (existing) | `a_private_address_is_refused_under_the_strict_policy` |
| 13 | **the SSRF guard still refuses a redirect to a private address** | `src/tools/web_fetch/fetch/tests.rs` | existing `a_redirect_to_a_private_address_is_resolved_but_refused_at_connect_time` — keep, and add a live variant |
| 14 | **`DomainRules` still refuse a blocked domain before any DNS** | `src/tools/web_fetch/fetch/tests.rs` (existing) | `a_disallowed_domain_is_refused_before_the_network_is_touched` — keep |
| 15 | **a redirect to a blocked domain is refused** | `src/tools/web_fetch/fetch/live_tests.rs` | `a_redirect_to_a_blocked_domain_is_refused` (NEW — this is the one most likely to regress) |
| 16 | the source hint is preferred over the requested URL | `src/tools/web_fetch/fetch/live_tests.rs` | `a_source_hint_is_preferred_over_the_requested_url` |
| 17 | a hint that fails falls back to the requested URL | same | `a_source_hint_falls_back_when_the_candidate_fails` |
| 18 | a hint cannot bypass the client's domain rules | same | `a_source_hint_cannot_bypass_the_client_domain_rules` |
| 19 | the profile for a host is stable | `src/tools/http_profile/tests.rs` | `the_profile_for_a_host_never_changes` |
| 20 | the persona is not stale | `src/tools/http_profile/tests.rs` | `the_browser_persona_is_not_stale` (`#[ignore]`d or CI-only) |
| 21 | the executor's failure text is bounded and names the status | `src/tools/web_fetch/tool/tests.rs` | `the_failure_text_names_the_status_and_stays_bounded` |
| 22 | the new config fields resolve (env > file > default) | `src/config/tests.rs` | `web_fetch_file_values_resolve`, `web_fetch_env_beats_file`, `web_fetch_profile_defaults_to_honest`, `web_fetch_an_unknown_profile_warns_and_falls_back` |
| 23 | the new config round-trips through the TUI save | `src/config/tests.rs` | `the_web_fetch_section_survives_a_save_round_trip` |

**Every one of 12–15 and 18 is a regression guard.** The brief is explicit
that the SSRF guard and `DomainRules` must still hold after these changes;
those tests are the proof.

### 8.3 A gap worth fixing

`src/config/tests.rs` (70 KB, 100 test functions) has **zero** occurrences
of `web_fetch` — there are `web_search_*` tests (lines ~1649–1745) and
`server_tools_*` tests (~1747–1811), but the `web_fetch` resolver is
untested. The Phase 1+ config work should add a `─── web_fetch ───` section
mirroring the `web_search` one. (This corrects the task brief, which listed
`src/config/tests.rs` as a doc file to update: it is a **test** file, and it
has no `web_fetch` coverage today.)

### 8.4 File placement (the house rule)

`tests/project_layout.rs` enforces: every `#[cfg(test)] mod NAME;` points at
its own `NAME.rs` file next to the module, and no inline `mod NAME { … }`.

New test files this plan creates:

| Test file | For |
|---|---|
| `src/tools/http_profile/tests.rs` | `src/tools/http_profile.rs` |
| `src/tools/web_fetch/hints/tests.rs` | `src/tools/web_fetch/hints.rs` |
| `src/tools/web_fetch/retry/tests.rs` | `src/tools/web_fetch/retry.rs` |

Modified test files:

| Test file | Change |
|---|---|
| `src/tools/web_fetch/fetch/live_tests.rs` | capture request headers; new tests 1, 2, 5–11, 15–18 |
| `src/tools/web_fetch/fetch/tests.rs` | keep 13, 14; add the domain-rules redirect unit test |
| `src/tools/web_fetch/tool/tests.rs` | test 21 |
| `src/tools/web_fetch/extract/tests.rs` | the charset decoding tests (Phase 6) |
| `src/config/tests.rs` | test 22, 23 |
| `src/tools/web_search/ddg/tests.rs` | the UA now comes from the shared module |

`tests/proxy_e2e.rs` (332 KB) needs **no** change: it cannot point the
fetcher at a loopback server (the SSRF guard refuses loopback), which is
precisely why `live_tests.rs` exists.

---

## 9. Risks and the decisions you need to make

| # | Decision | Options | **Recommendation** | Risk if wrong |
|---|---|---|---|---|
| 1 | **Default identity** | `honest` / `browser` | **`honest`** (+ per-domain opt-in). It preserves the project's stated stance, keeps the lie scoped and visible, and is the better legal posture. The BOM case is still fixed — via the per-domain list or the source hint. | If you want zero configuration for BOM, set `profile: "browser"` — one line, and the guide documents the trade-off. |
| 2 | **Per-domain vs global browser profile** | global / per-domain / both | **Both** (global `profile` + per-domain override list). The per-domain list is the honesty-preserving mechanism; the global switch is the convenience. | Low. |
| 3 | **Auto-escalation on 403** | yes / no | **No.** It rotates identity, invalidates challenge cookies, and looks like an attacker probing. | Low. |
| 4 | **reqwest 0.12 vs 0.13** | stay 0.12 / move to 0.13 | **Stay on 0.12 for this work.** 0.13 makes rustls the default and adds a `retry` builder method, but `reqwest-retry` 0.9.x requires reqwest `^0.13`, so staying on 0.12 pins `reqwest-retry` to 0.7.x — and the 0.12 retry API is adequate for the bounded-retry policy in Phase 4 (which is ~40 lines and needs no new dependency at all). Revisit the upgrade separately. | Low. Revisit when `reqwest-retry` 0.9 stabilises. |
| 5 | **Enable the `http2` feature** | yes / no | **Yes** (Phase 2). Chrome advertises `h2,http/1.1` in ALPN; an h1.1-only client is a detectable "not a browser" signal. Cost: the `h2` crate (a few hundred KB of binary). | Low. Binary size grows slightly. |
| 6 | **Enable `zstd`** | yes / no | **No, not now.** `zstd-sys` is a C build dependency, and advertising `zstd` without the feature is a silent corruption bug. Keep `gzip, deflate, br` and assert the invariant in a test. | Low. If a target requires zstd, enable the feature and the header in the same commit. |
| 7 | **OS trust store vs `webpki-roots`** | `rustls-tls-native-roots` / keep webpki | **Keep `webpki-roots` for now**, and note it in the guide: the proxy validates against the bundled Mozilla list, **not** the Windows certificate store, so a site with a corporate/enterprise root will fail TLS even though the operator's browser can reach it. If that bites, switch to `rustls-tls-native-roots` (adds `rustls-native-certs`; on Windows it reads the system store). | Medium — this is a plausible cause of "works in the browser, fails in the proxy" that is NOT the 403. Worth knowing about. |
| 8 | **Header name case** | title-case / leave lowercased | **Enable `http1_title_case_headers()` in the browser profile only.** It costs nothing and removes a class of (rare, usually buggy) WAF failures. The honest profile does not care. | Very low. |
| 9 | **`system-proxy` / `socks`** | enable / skip | **Skip for now, but know that `HTTP_PROXY`/`HTTPS_PROXY` are silently ignored today.** If you run the proxy behind a corporate proxy, `web_fetch` will fail in a way that looks like a network error. Enabling `system-proxy` is a one-word change. | Medium if you are behind a proxy — check this before debugging anything else. |
| 10 | **New wire error code** | none / new enum value / optional sibling key | **No new `error_code` value** (Anthropic's enum is fixed and some SDKs validate it). Put the diagnostics in the executor output and the operator log. If you want the client to see it, add an optional `status` sibling key — but test it against your client first. | Low–medium. |
| 11 | **BOM licence** | use BOM products / avoid | **Use them, with attribution.** The anonymous FTP products are free for personal use / use within your organisation but **not** for onward supply and **not** for commercial use. If the proxy is used commercially or serves third parties, get permission or avoid BOM. The `note` field exists for the attribution line. | **Legal, not technical.** Do not skip this. |
| 12 | **Persona staleness tripwire** | test always / `#[ignore]` / CI only | **CI only** (or `#[ignore]`d with a clear message), so a stale persona does not break a local build. | Low. |
| 13 | **Cookie jar scope** | process-wide / per-request | **Process-wide.** A per-request jar cannot survive a redirect chain, which is the main reason to want one. The jar is domain-scoped, so it does not leak across sites. | Low. |

---

## 10. What this plan does **not** recommend (and why)

* **TLS / HTTP2 fingerprint impersonation (BoringSSL, JA3/JA4).** Not
  needed and not justified by the evidence: `curl` uses OpenSSL/Schannel —
  categorically not Chrome's fingerprint — yet the browser-header `curl` got
  HTTP 200 from BOM. The block is header-based. The Rust options
  (`wreq` + `wreq-util`, `impit` (git-only, needs `[patch.crates-io]`),
  `rquest` (yanked), `reqwest-impersonate` (yanked/dead)) carry BoringSSL
  build dependencies, symbol conflicts with rustls in the same binary, and
  a maintenance treadmill (Chrome ships every ~4 weeks; JA4 profiles go
  stale within weeks). `wreq` 6.x is still a release candidate.

* **CAPTCHA solving and residential proxies.** Out of scope on both
  technical and legal grounds. For an Australian Government target, the
  Copyright Act 1969 s 196AB ("circumventing a technological protection
  measure") is directly on point, and the target's terms of service apply.
  And the practical reality is that "proxies fix throughput, not admission":
  residential exits still get 403s on some targets, and Cloudflare now
  ML-detects residential-proxy traffic.

* **A third-party reader service (e.g. `r.jina.ai`) as a silent default
  fallback.** It leaks every user URL to a third party, the free tier is
  non-commercial (CC-BY-NC), it is rate-limited (20 RPM keyless), its cache
  is stale, and it explicitly "respects blocks and does not work around
  them" — so it would not even help against a WAF that blocks Jina. If you
  want it, make it an **explicit, operator-configured, off-by-default**
  source-hint candidate, with the URL leak documented.

* **A headless-browser fallback as the default.** `chromiumoxide` 0.9.1 /
  `thirtyfour` 0.38.3 are viable, but they cost ~1 GB RSS per browser and
  ~0.8–1.2 s of startup, break in containers (sandbox), and Cloudflare's
  JS-detection still flags headless. If it is ever added, it must be an
  **on-demand fallback** for a specific class of target, never the default
  path. `headless_chrome` is dormant and synchronous — avoid it.

* **Automatic profile escalation.** See §2.3.

* **Generic feed/JSON-LD discovery as the primary answer to the BOM
  case.** It is a reasonable *additional* mechanism (Phase 7, optional) but
  it cannot find a product-ID XML package that the HTML page merely
  renders. It is also unverified for BOM.

* **Writing a raw HTTP client with hand-rolled TLS.** The existing
  SSRF-hardened fetcher is good; the problem is the *headers* and the
  *diagnostics*, not the transport. Keep the fetcher, change its identity
  and its reporting.

---

## Appendix A — BOM quick reference (copy-paste ready)

```json
"web_fetch": {
  "enabled": true,
  "profile": "browser",
  "browser_profile_domains": ["html.duckduckgo.com", "duckduckgo.com", "www.bom.gov.au", "api.bom.gov.au"],
  "source_hints": [
    {
      "match": "www.bom.gov.au/places/qld/townsville-city/forecast",
      "urls": ["https://www.bom.gov.au/fwo/IDQ11296.xml"],
      "contains": "QLD_PT022",
      "note": "Source: Bureau of Meteorology Town Forecast XML Package QLD (product IDQ11296), area QLD_PT022. Bureau of Meteorology, © Commonwealth of Australia."
    },
    {
      "match": "www.bom.gov.au/places/qld/townsville-city/forecast",
      "urls": ["https://api.bom.gov.au/apikey/v1/forecasts/texts?aac=QLD_PT022&timezone=Australia/Brisbane"],
      "note": "Source: Bureau of Meteorology forecasts API, area QLD_PT022. Bureau of Meteorology, © Commonwealth of Australia."
    }
  ]
}
```

Product IDs:

| product | id | contains |
|---|---|---|
| Town Forecast XML Package QLD | `IDQ11296` | `QLD_PT022` (Townsville), full extended wording |
| Précis Forecast Package QLD | `IDQ11295` | |
| The id printed on the Townsville City page | `IDQ10150` | `QLD_PW005` district text |
| Coastal Waters | `IDQ11290` | |
| QLD observations | `IDQ60920` | Townsville Aero: WMO `94294`, `bom-id 032040` |
| ~~`IDSS40301.94822`~~ | **does not exist** | — |

Legacy geohash API (works with a plain UA, but its payload says "You must not
use, copy or share it." — treat as licence-restricted):
`https://api.weather.bom.gov.au/v1/locations/rk86kvk/forecasts/daily`

Anonymous FTP (no WAF, byte-identical to the HTTPS copy; not reachable from
this proxy): `ftp://ftp.bom.gov.au/anon/gen/fwo/IDQ11296.xml`

---

## Appendix B — dependency facts (verified against `Cargo.lock` on the plan date)

Present: `reqwest` 0.12.28, `rustls` 0.23.42, `hyper`, `hyper-util`,
`hyper-rustls`, `encoding_rs` (via `charset`), `webpki-roots` 1.0.8,
`quinn`, `serde`, `serde_json`, `tokio`, `futures-util`.

Absent (because `default-features = false`): `h2` (no HTTP/2),
`cookie`/`cookie_store` (no jar), `zstd`/`zstd-sys`, `socks`,
`hyper-util/client-proxy-system` (no `HTTP_PROXY` support),
`native-tls`, `rustls-native-certs` (no OS trust store).

Adding `http2` adds `h2` (plus `hyper/http2`, `hyper-util/http2`,
`hyper-rustls/http2`). Adding `cookies` adds `cookie` 0.18 and
`cookie_store`. Both are pure-Rust and cheap.

---

## Appendix C — file/line index (the plan date's working tree)

| File | Lines | What is there now |
|---|---|---|
| `src/tools/web_fetch/fetch.rs` | 567 | `USER_AGENT` (44), `MAX_REDIRECTS` (40), `transport_error`, `FetchError`, `AddressPolicy`, `fetch`, `fetch_within`, `fetch_with_policy` (test-only), `fetch_guarded`, `fetch_inner`, `ValidatedUrl`, `resolve_and_check`, `is_public_ip`, `build_client` (508), `read_capped` (546), `resolve_redirect`, `truncate` |
| `src/tools/web_fetch/extract.rs` | 133 | `extract`, `document_title`, `looks_like_html`, `readability`; UTF-8-only decoding |
| `src/tools/web_fetch/protocol.rs` | 178 | `ERROR_CODE_*` consts, `server_tool_use`, `web_fetch_result`, `error_result` (code only), `new_id`, `now_iso8601` |
| `src/tools/web_fetch/tool.rs` | 229 | `FETCH_FAILED`, `WebFetchTool`, `Answer`, `answer`, `warn`, `parse_url_argument` |
| `src/tools/web_fetch/mod.rs` | 168 | activation (`has_server_side_web_fetch`, `active`), `prepare` |
| `src/tools/web_fetch/fetch/live_tests.rs` | 219 | `TestServer` (discards request bytes), `start_with_headers`, `start_stalling`, 7 tests |
| `src/tools/web_fetch/fetch/tests.rs` | 184 | 18 unit tests (IP ranges, URL parsing, truncation, domain rules) |
| `src/tools/web_fetch/protocol/tests.rs` | 82 | 8 wire-shape tests |
| `src/tools/web_fetch/tool/tests.rs` | 186 | tool-level tests |
| `src/tools/domains.rs` | — | `DomainRules` (`allows`, `narrow_query`, `from_options`) |
| `src/tools/web_search/backend.rs` | 107 | `LiveBackend::fetch_page` → `page_fetch::fetch_within` |
| `src/tools/web_search/ddg.rs` | 225 | `BROWSER_USER_AGENT` (34), `search` (POST with UA/Accept/Referer), `parse_results` (challenge detection) |
| `src/tools/server_loop/mod.rs` | — | `handlers()` (55) wires `web_search`, `web_fetch`, `advisor` from `attempt.state.config` |
| `src/config.rs` | 2043 | `WebFetchConfig` (346), `Default` (363), `Config::resolve` `web_fetch` block (1007–1057), `EnvInputs` (1398), `EnvInputs::capture` (1458), `resolve_web_fetch_limit` (1712), `JsonWebFetchConfig` (1961), `DEFAULT_WEB_FETCH_TIMEOUT_SECS` (66) |
| `src/config/tests.rs` | ~1811 | `web_search_*` (1649+), `server_tools_*` (1747+) — **no `web_fetch` tests** |
| `tests/project_layout.rs` | — | enforces the "one test module per file" rule |
| `tests/proxy_e2e.rs` | 332 KB | end-to-end tests; cannot use a loopback fetch server |
| `proxy.json-guide.md` | 36 KB | "Web fetch (`web_fetch`)" section at line ~305 |
| `REFERENCE.md` | 30 KB | env-var table lines 43–46; "Server-side `web_fetch`" section line ~191 |
| `proxy.json.example` | 1.7 KB | the `web_fetch` block |
| `Cargo.toml` | 85 | `reqwest` with `default-features = false` |
