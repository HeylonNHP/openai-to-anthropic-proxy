# `web_search` — the proxy's own web search

An **optional** replacement for the upstream's built-in web search. When
enabled in `proxy.json`, the proxy answers the client's server-side
`web_search` tool itself: it scrapes DuckDuckGo, fetches the result pages,
shortens long pages, and hands the text to the model. When disabled (the
default) nothing changes and the client's `web_search` tool goes to the
upstream's own search, as before.

The design follows Locopilot's `web_search` tool: the model submits several
queries, every query is searched, and every result page is fetched.

> **This module is built to be removable.** See [How to remove it](#how-to-remove-it).

## Contents

- [Enabling it](#enabling-it)
- [How it works](#how-it-works)
- [Configuration](#configuration)
- [Design decisions](#design-decisions)
- [Known limitations and deferred work](#known-limitations-and-deferred-work)
- [How to remove it](#how-to-remove-it)
- [Module map](#module-map)

## Enabling it

```json
{
  "web_search": {
    "enabled": true,
    "compaction_model": "some-cheap-upstream-model"
  }
}
```

It only activates for a request that declares the **server-side** tool: a tool
named `web_search` with no `input_schema` (what Claude Code and the Anthropic
API send). A client function tool that merely happens to be called
`web_search` carries a schema and is left alone.

## How it works

```text
client ──▶ proxy ──▶ upstream executor model
                         │ calls proxy_web_search({queries: [...]})
              ┌──────────┘
              ▼
   normalise queries (trim, dedup, cap at max_queries)
   search each query on DuckDuckGo     (staggered, ≤ 2 at a time)
   fetch each UNIQUE result URL once   (≤ 4 at a time, SSRF-guarded)
   fit each page to compaction_threshold_chars
              │
              ▼
   results text ──▶ back to the executor, which is re-run
```

1. **Inbound (`prepare`).** The translator turned the client's `web_search`
   declaration into the upstream's *built-in* search tool. `prepare` removes
   that and adds an ordinary function tool, `proxy_web_search`, taking
   `queries: string[]`. It also forces `stream: false`: the executor has to be
   re-run with the results, so the whole chain is buffered.
2. **The loop (`server_loop`, shared).** The executor is run. If it calls
   `proxy_web_search`, `tool.rs` runs the queries and the shared loop feeds the
   formatted results back as the call's output and re-runs the executor. It
   repeats until the executor answers without searching. If a re-run fails
   after searching, the client still gets the work already done.
3. **What the client sees.** One `server_tool_use` / `web_search_tool_result`
   pair per query (titles and URLs), exactly the shape Anthropic's own search
   produces, then the executor's answer. `usage.server_tool_use.web_search_requests`
   counts the queries that ran. A streaming client gets each pair as the search
   finishes (see the shared loop's README).
4. **What the executor sees.** Plain `key: value` text: for each query its
   results (title, URL, snippet, page text) and a numbered SOURCES list so it
   can cite real URLs.

### What the client's tool options do

The client sets options on its `web_search` declaration; the proxy applies them:

| option | effect |
|---|---|
| `max_uses` | Counts **queries** across the whole request. A query over the limit is not run: the client gets a `max_uses_exceeded` result for it and the model is told. |
| `allowed_domains` / `blocked_domains` | Each query is sent with `site:` / `-site:` operators, **and** the hits are filtered afterwards (the engine may ignore an operator). A hit outside the rules is never fetched, and a page that redirects outside them is refused. Setting both lists is a 400, as on Anthropic. An empty list is no list. |
| `user_location` | The country picks DuckDuckGo's region (`kl`). Anything unknown is "no region". |

A domain rule covers its subdomains (`example.com` matches `docs.example.com`);
a subdomain rule matches only that subdomain; `example.com/blog` matches by path
prefix. The matching lives in `src/tools/domains.rs`, shared with `web_fetch`.

### Results stay in context

Anthropic puts each result's page content in an opaque `encrypted_content`, and
the model sees it again when the client sends the message back. The proxy does
the same with its own format: `"oap1:" + base64(what the model read)`. It is not
encrypted (the client is the user), only opaque. `history.rs` writes and reads
it; the translator unpacks an echoed `web_search_tool_result` into the model's
context on the next turn, delimited so it is not mistaken for the assistant's
own words.

* Each result is capped at 12,000 characters when written.
* A result the proxy did not write (the upstream's built-in search, another
  server) cannot be read, so its **title and URL** are replayed instead.
* A failed or empty search adds nothing.
* A blob is only understood by this proxy: moving a conversation to the real
  Anthropic API loses the page content of earlier results.

### Compaction

A page longer than `compaction_threshold_chars` is shortened:

- With a `compaction_model`: up to 3 summarisation passes (a side call to the
  upstream, like the advisor's), each asking for a shorter result. The prompt
  preserves code, tables, URLs and quotations.
- Without one, or if the call fails, or the model cannot get under the limit:
  the text is **truncated** at a paragraph boundary and the page carries a note
  saying it was *not* summarised, so the model never mistakes the first N
  characters for the whole page.

### Failure handling

Nothing here fails the client request. A blocked search (CAPTCHA, HTTP error),
an unreachable page, a failed compaction: each is reported in the tool result
(`search_error:` / `(failed to fetch page: …)` / a truncation note, and an
`unavailable` error block for the client) and the turn continues. One bad query
or page never discards the others. If you see CAPTCHAs, DuckDuckGo is
rate-limiting your IP; the proxy does **not** silently fall back to the
upstream's search (see below).

### Pacing and blocks (`gate.rs`)

Searches are serialised and spaced by `search_min_interval_ms`, process-wide
and per endpoint, so a burst of queries cannot reach the engine at once. A
blocked search (bot challenge, or the HTTP status DuckDuckGo uses for a flagged
client) tells the model to wait `block_wait_minutes`. Searches are never
refused on the proxy's side; if the model is blocked again, the message adds how
long ago the previous blocked search was (seconds and minutes). A search that
returns results clears that record. The wording deliberately only asks the
model to wait, never to stop searching.

### The session: cookies, cache and fingerprint (`session.rs`, `cookies.rs`, `cache.rs`)

A browser keeps its connection and cookies and does not repeat a search it just
ran. So the search client's state outlives a request, process-wide and per
endpoint, like the gate:

- **One HTTP client** is reused, so connections are reused.
- **Cookies are kept** (`cookies.rs`). DuckDuckGo sets a year-long `kl` cookie.
  Cookies are sent back to their own site only and are written to
  `<state_dir>/search-session.json` (default `proxy-state/`, beside
  `proxy.json`; owner-only on Unix; written through a temp file and renamed).
  A missing, corrupt or wrong-version file just starts an empty session; a
  failed save is logged and the session stays in memory. The jar is never given
  to the page-fetching client.
- **Two expiry checks, on every use.** A cookie's own `Expires` / `Max-Age`:
  an expired cookie is not sent, saved or loaded. And the session's age
  (`session_ttl_hours`): past it, every cookie (session cookies included) is
  dropped and a new session starts, on load and while running. A start time in
  the future (the clock moved back) is not trusted.
- **Results are cached** (`cache.rs`) by region and normalised query for
  `cache_ttl_secs`, at most `cache_max_entries` of them. The lookup happens
  before the gate, so a repeat neither waits nor reaches the engine. Only
  non-empty successes are stored; blocks and errors never are. Entries are
  checked for expiry on every lookup and swept on every insert.
- **Requests look like a browser's.** The form POST carries `Origin`, an
  origin-only `Referer`, same-origin Fetch Metadata and Chrome's header order
  (`http_profile.rs`). The HTTP/2 connection settings are tuned to Chrome's:
  stream window, connection window and header-list size.

**What this does not match.** The client is `reqwest` over `rustls`, so the TLS
ClientHello (JA3 / JA4: cipher and extension lists and order), the HTTP/2
`HEADER_TABLE_SIZE` / `MAX_FRAME_SIZE` settings and the HTTP/2 pseudo-header
order are those of `rustls` and `hyper`, not Chrome's. Matching them needs a
different TLS stack (BoringSSL, e.g. the `wreq` crate), which is a heavy
dependency and was not added. If DuckDuckGo still challenges you, the likely
cause is the address (a datacenter or VPN IP) rather than anything here.

### Diagnosing failures (`diagnostics.rs`)

A failed search is logged as ONE `warn` event, `web_search search failed`, with
what it takes to tell the causes apart:

| field | tells you |
|---|---|
| `kind` | `blocked` (bot challenge or a rate-limiting status) or `failed` (transport) |
| `status`, `http_version`, `content_type`, `server`, `retry_after`, `set_cookies` (names only) | the engine's answer. DuckDuckGo's challenge is a **202** with an image puzzle |
| `markers`, `page_head`, `body_bytes` | what the page is: `challenge-form`, `bots-use-duckduckgo`, `web-result`, … and the start of its visible text |
| `transport`, `elapsed_ms` | for no response: `timeout` / `connect` / … plus the error chain |
| `cookies_sent`, `session_age_secs` | whether this request carried the session |
| `searches_in_session`, `ok_before_this` | **blocked after how many good searches**: 0 is the address or fingerprint, 40 is the rate |
| `secs_since_last_ok`, `secs_since_last_block` | how fast it is being retried |
| `saved_page` | the page itself, see below |

A block also saves the page it got to `<state_dir>/blocked-pages/search-<ms>.html`
(the newest 5 are kept, owner-only), so the challenge can be read after the
fact. A search that returns **no results and no "no results" message** is
logged (`returned no results and no 'no results' message`) and saved the same
way: a silent soft block, or changed markup, looks like that. Successful
searches log at `debug`. None of this changes what a search returns.

## Configuration

All keys are optional; every one has an env override
(`WEB_SEARCH_<KEY>`, uppercased). Precedence is env > `proxy.json` > default.

| `proxy.json` key | Default | Ceiling | Meaning |
|---|---|---|---|
| `enabled` | `false` | — | Use the proxy's search instead of the upstream's. |
| `compaction_model` | *(unset)* | — | Explicit **upstream** model name used to summarise long pages (alias resolution is bypassed, like `advisor.model`). Unset = truncate, no model call. |
| `compaction_threshold_chars` | `5000` | 100000 | Pages longer than this are summarised (or truncated) to fit. |
| `max_queries` | `3` | 10 | Queries the model may run in one tool call. Extra queries are not run, and the model is told so. |
| `max_results_per_query` | `5` | 10 | Result pages fetched per query. |
| `search_timeout_secs` | `15` | 120 | Budget for one search request. |
| `page_timeout_secs` | `15` | 120 | Budget for fetching one page. |
| `max_page_bytes` | `2097152` | 16 MiB | Network read cap per page. |
| `search_endpoint` | DuckDuckGo's HTML endpoint | — | Where searches are POSTed. Mostly for tests. |
| `search_min_interval_ms` | `1500` | 30000 | Searches run one at a time; this is the minimum gap between the end of one and the start of the next. |
| `block_wait_minutes` | `10` | 120 | Minutes the model is told to wait after a blocked search. |
| `state_dir` | `proxy-state` | — | Directory where the search session (cookies) is saved, relative to the working directory (where `proxy.json` is read from). Blank = the default. |
| `cache_ttl_secs` | `600` | 86400 | How long a search result is cached. |
| `cache_max_entries` | `128` | 4096 | Most searches kept in the cache. |
| `session_ttl_hours` | `168` | 720 | A session's cookies are dropped and a new session started after this long. |

**There is no "unlimited".** Unlike `web_fetch`, `0`, negative or fractional
values are invalid: the default is used and a startup warning is printed.
Values above the ceiling are clamped, with a warning. Beyond the config, a
request is bounded by the shared loop's round limit (`server_tools.max_rounds`,
default 10) and by the client's own `max_uses` on its `web_search` tool.

## Design decisions

- **Same shape as the other tools.** `mod.rs` (activation + `prepare`),
  `tool.rs` (the `ServerTool` the shared loop calls), `protocol.rs` (wire
  builders) — mirroring `web_fetch`, so anyone who knows one knows the other.
  The loop and the response assembly are not here: they are the shared
  `src/tools/server_loop/`, which every proxy-served server tool uses.
- **One internal tool name, `proxy_web_search`.** The translator and stream
  layers treat any function call named `web_search` as the upstream's built-in
  search (`translate::extract_web_search_citations`). A distinct internal name
  keeps the two from being confused; the client still sees `web_search`.
- **A single I/O seam (`backend::Backend`).** Search, page fetch and
  compaction sit behind one trait; the orchestration (`run`) is generic over
  it and does no I/O itself. That is what makes the multi-query logic unit
  testable without a network or an upstream. `LiveBackend` is the only
  implementation that does I/O.
- **Result pages go through `web_fetch`'s fetcher.** Result URLs come from a
  web page, so they are untrusted. Reusing `web_fetch::fetch` gives the same
  SSRF protection (private addresses refused, DNS pinned, redirects
  re-validated) without duplicating security-critical code. This is the one
  and only dependency, and it points one way: `web_search → web_fetch`.
- **First results page only.** DuckDuckGo's first HTML page has ~10 results,
  which is also the `max_results_per_query` ceiling, so Locopilot's `vqd`
  pagination dance (a frequent source of CAPTCHAs) is unnecessary.
- **Each unique URL is fetched once**, even when several queries return it.
- **Searches are staggered and few-at-a-time.** DuckDuckGo answers a burst
  with a CAPTCHA; page fetches and compaction calls run in parallel (≤ 4).
- **Truncation is the default, not a model call.** With no `compaction_model`
  the proxy never spends tokens on its own; the operator opts into that cost
  by naming a model (ideally a cheap one).
- **Bounded by construction.** Every limit has a default *and* a ceiling, plus
  the shared loop's round limit. A runaway model cannot cause unbounded outbound
  requests.
- **No page cache, no JavaScript rendering (v1).** Locopilot has both
  (Playwright, an in-memory cache). Both can be added behind `Backend` later
  without touching the rest.
- **Errors, not fallback, when DuckDuckGo blocks us.** The alternative — silently
  using the upstream's native search for that request — is more resilient but
  makes `enabled` mean "sometimes". For now a CAPTCHA is an error in the tool
  result and the model carries on.
- **Replayed results cost context on every later turn.** That is the price of
  the model remembering what it read, as with Anthropic's own search; the cap
  per result bounds it.
- **Compaction token usage is not recorded** in the TUI session stats (the
  advisor's isn't either). Only the executor calls are.

## Known limitations and deferred work

### Server tools combine freely

`web_search`, `web_fetch` and the advisor all run on the shared
`src/tools/server_loop/`, so a request may declare any mix of them and the
model can use all of them in one turn. There is no precedence rule: each tool
that is enabled and declared is offered, and a call to any of them is served in
the order the model made it. (Claude Code's own `WebFetch` is a client-side tool
and is unaffected.)

### Other deferred items

- Pagination (more than ~10 results per query).
- A page cache (search results are cached; fetched pages are not).
- Matching Chrome's TLS and HTTP/2 fingerprint (see [the session](#the-session-cookies-cache-and-fingerprint-sessionrs-cookiesrs-cachers)).
- Playwright / JavaScript rendering for SPA pages.
- Extracting outbound links from pages (Locopilot returns them).
- A fallback search engine (or fallback to the upstream's) when DuckDuckGo
  blocks the proxy.
- Recording compaction token usage in the TUI stats.

## How to remove it

Everything specific to this tool is in `src/tools/web_search/` plus a handful
of small, clearly marked hooks. `web_fetch` and the advisor never import from
`web_search`.

1. **Delete the module:**
   ```bash
   git rm -r src/tools/web_search
   ```
2. **`src/tools/mod.rs`** — remove `pub mod web_search;` (and its line in the
   module doc comment).
3. **`src/proxy.rs`** — two hooks, both marked with `web_search`:
   - in `build_outbound`: the `web_search::prepare` call;
   - revert `pub(crate)` on `Attempt::req` and `record_json_stats` (optional;
     harmless to leave).
   And in `src/tools/server_loop/mod.rs`: the `if web_search::active(…)` push
   in `handlers` (`Attempt::run` itself is generic and needs no change).
4. **`src/config.rs`** — delete, each clearly named: the `DEFAULT_/MAX_WEB_SEARCH_*`
   constants, `WebSearchConfig` (+ `Default`), the `web_search` field on
   `Config` and its resolution block, `resolve_bounded_limit`, the
   `web_search_*` fields in `EnvInputs` / `capture()`, `JsonConfig::web_search`
   and `JsonWebSearchConfig`.
5. **`src/translate.rs`** — remove the echoed-search handling: the
   `web_search_queries` list, the `server_tool_use{web_search}` branch and the
   `ContentBlockParam::WebSearchToolResult` arm (and its name in the user-message
   arm), then `WebSearchToolResultBlockParam` and its enum variant in
   `src/anthropic.rs`. Also `native_web_search_tool`'s use of options is
   independent of this module (it maps the *upstream's* built-in search) — keep it.
   `src/tools/domains.rs` is shared with `web_fetch` — keep it.
6. **Tests** — remove the `web_search` tests in `src/config/tests.rs`, the
   `tests/proxy_e2e/web_search.rs` module (and its `mod web_search;` line in
   `tests/proxy_e2e/main.rs`), the `// ─── Proxy-native web_search` helpers in
   `tests/proxy_e2e/harness.rs`, the `web_search:` lines in the `Config`
   literals across `tests/proxy_e2e/*.rs` and in `src/proxy/tests.rs`, and the
   `WebSearchConfig` imports in `tests/proxy_e2e/*.rs`.
7. **Docs** — remove the `web_search` rows/sections from `REFERENCE.md`,
   `README.md`, `proxy.json.example` and `proxy.json-guide.md`.
8. **Verify:** `cargo build && cargo clippy --all-targets && cargo test`.

A `proxy.json` that still has a `web_search` section will be **rejected** after
removal (the config schema denies unknown fields) — delete that section too.

## Module map

| file | role |
|---|---|
| `mod.rs` | activation (`active`) and the inbound rewrite (`prepare`) |
| `tool.rs` | the `ServerTool` the shared loop calls: serves a call; applies the client's `max_uses` |
| `run.rs` | orchestrates one tool call, generic over `Backend`; no I/O |
| `backend.rs` | the `Backend` trait and `LiveBackend` (the only I/O) |
| `ddg.rs` | DuckDuckGo HTML parsing (pure) and the search request |
| `session.rs` | the shared search HTTP client, cookie jar and result cache, per endpoint |
| `cookies.rs` | the persistent cookie jar: expiry, session rotation, atomic save |
| `cache.rs` | the result cache: ttl, capacity |
| `diagnostics.rs` | the failure log line, the engine-side `Report`, saved blocked pages |
| `gate.rs` | pacing and the record of blocked searches |
| `compact.rs` | threshold, summarisation passes, truncation fallback |
| `format.rs` | the result text the executor reads, and the text carried into later turns |
| `history.rs` | `encrypted_content`: write a result's text, read it back on a later turn |
| `protocol.rs` | the wire blocks the client sees (pure) |
| `*/tests.rs` | unit tests, one file per module (enforced by `tests/project_layout.rs`) |
