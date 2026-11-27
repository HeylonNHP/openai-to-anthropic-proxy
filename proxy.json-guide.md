# proxy.json guide

This is the human-readable walkthrough for the on-disk config file. The checked-in template is `proxy.json.example`; copy it to `proxy.json` and edit the values for your setup. The real `proxy.json` stays local so it can hold secrets and provider-specific settings.

This file explains **what each setting means and what goes wrong at the edges** — invalid values, precedence, and per-tool semantics. For the flat list of every option with its environment variable and default, see [`CONFIGURATION.md`](./CONFIGURATION.md); for the architecture behind the translation, see [`REFERENCE.md`](./REFERENCE.md).

## The short version

Config is resolved in this order:

1. Environment variables
2. `proxy.json`
3. Built-in defaults

That means `proxy.json` is the right place for your normal setup, while env vars are handy for one-off overrides (for example, rotating the API key without editing the file).

## A redacted example

This is a slimmed-down version of a real `proxy.json`. The structure is the same — only the secret values are missing.

```json
{
  "upstream_base_url": "https://prodaus.gateway.airia.ai",
  "upstream_api_key":  "<your airia key here>",
  "request_timeout_secs": 600,

  "reasoning": {
    "default": "none",
    "models": {
      "gpt-5.4-mini": "xhigh",
      "gpt-5.6-luna": "xhigh",
      "gpt-5.6-terra": "xhigh",
      "gpt-5.6-sol": "xhigh"
    }
  },

  "model_aliases": {
    "default_model": "gpt-5.4-mini",
    "map": {
      "claude-opus-4-8": "gpt-5.6-luna",
      "claude-sonnet-5": "gpt-5.4-mini",
      "claude-haiku-4-5": "gpt-5.4-nano"
    }
  }
}
```

Every field is optional. If you only need the basics, a two-field file is enough:

```json
{
  "upstream_base_url": "https://api.openai.com",
  "upstream_api_key":  "sk-..."
}
```

## What each section is doing

### Upstream connection

- `max_request_body_bytes` limits the inbound `/v1/messages` body to a positive whole number of bytes. The default is **33,554,432 bytes (32 MiB)**; set this lower or higher for your deployment. `MAX_REQUEST_BODY_BYTES` overrides the file value. A numeric value of `0`, a negative or fractional value, or a value outside the platform's `usize` range produces a warning and falls back to the default without stopping startup. Non-numeric environment text is ignored, so the file value/default applies. Requests are buffered in memory before handling; raising this limit increases the memory a single request can consume, and concurrent large requests multiply that cost.
- `upstream_base_url` is the gateway URL the proxy sends requests to. The default upstream path is `/v1/responses`; the proxy appends it for you.
- `upstream_api_key` is the upstream bearer token. Keep it out of docs and out of git — `proxy.json` is in `.gitignore`.
- `request_timeout_secs` controls how long the proxy waits before giving up on a single request. The default is 600 (10 minutes).
- `min_output_tokens` is the smallest `max_output_tokens` the proxy sends upstream. The default is **16**; `MIN_OUTPUT_TOKENS` overrides the file value, and `0` turns the floor off. OpenAI-style servers reportedly reject anything below 16 with a 400, but Claude Code is reported to probe with `max_tokens: 1`, so a client `max_tokens` below the floor is raised to it. The reply is the upstream's real answer, so a probe may carry up to 16 tokens instead of 1. Set it to `0` for an upstream you know has no floor. Non-numeric environment text is ignored.
- `max_concurrent_requests` caps how many requests the proxy has in flight to the upstream at once. It is **unset by default, which means no limit**; `0` means the same, and `MAX_CONCURRENT_REQUESTS` overrides the file value. Requests past the cap wait in a first-come queue and are sent as slots free up. Queue time does not count against `request_timeout_secs`. For a **streaming** request the wait is no longer silence: once it passes ten seconds the proxy starts the stream with an empty `message_start` and sends a `ping` every ten seconds until the reply arrives, so the wait cannot trip Claude Code's stream-idle watchdog. A failure within those first ten seconds is still delivered as an HTTP status (so Claude Code's own `429` backoff-retry applies); a failure after the stream has started arrives as an in-stream `error` event instead, which Claude Code retries less aggressively — so keep `rate_limit_retries` in mind when raising the queue limit. A **non-streaming** request has no channel for mid-wait bytes and simply waits. A slot is held until the reply is finished, which for a streamed reply is when the stream ends, because that is how long the upstream counts it as in flight. Every call to the upstream model counts: the main request, token counts, and the advisor and web-search calls. Use it with an upstream that answers `429` with a message like "too many concurrent requests", which Claude Code's parallel subagents can trigger. Pick a number at or just under the upstream's own cap. Non-numeric environment text is ignored.
- `rate_limit_retries` is how many times the proxy re-sends a request the upstream refused with HTTP `429`. The default is **3**; `0` turns the retry off, and `RATE_LIMIT_RETRIES` overrides the file value. The waits double (about 0.5 s, 1 s, 2 s, then up to 8 s), each scaled by a random factor between a half and one so that refused requests do not come back together. If the upstream sent a `Retry-After`, the wait is at least that long (and never longer than `max_retry_wait_secs`). The slot is released while a request waits. The `429` is passed to Claude Code, as a `rate_limit_error`, only when the retries are used up. Only the HTTP status is retried: other errors, and an error event inside a stream that has already started, are not. The proxy cannot tell a short-lived concurrency `429` from a spent quota (`insufficient_quota` is also a `429`), so a quota error is retried too and reported after the retries; set `rate_limit_retries` to `0` if you would rather see it at once.
- `max_retry_wait_secs` caps any single wait the proxy itself takes between two attempts of one request: the exponential backoff above, and any `Retry-After` the upstream asked for. The default is **60** — exactly what the proxy already does (the exponential part never exceeds 8 s and a `Retry-After` was already capped at a minute), so the default changes nothing, and values above 60 do the same because no wait is ever longer. Set it lower to reach a client faster when the upstream keeps refusing: with `5`, a non-streaming client waits at most a few seconds before the `429` is passed on instead of up to a minute, and the proxy retries sooner than the upstream asked (possibly to be refused again) — the trade is gentler upstream pacing versus a faster, silent answer for the client, and a streaming client gets its placeholder `message_start` and pings after ten seconds either way. `MAX_RETRY_WAIT_SECS` overrides the file value; `0` is not usable and falls back to 60 with a warning (`rate_limit_retries: 0` is the knob for no waiting at all). Non-numeric environment text is ignored. It does not change the `retry-after` value passed through to the client — that stays capped at 60 s, as Claude Code expects.
- **`max_tokens: 0`** is Anthropic's prompt-cache pre-warm. The proxy asks the upstream for the floor, so its automatic prefix cache is primed, then throws the output away and answers with empty `content`, `stop_reason: "max_tokens"` and `output_tokens: 0` (`input_tokens` still shows the prompt was processed). Server tools (`web_search`, `web_fetch`, the advisor) are skipped for such a request. With `stream: true` it is refused with a 400, as Anthropic does. The other combinations Anthropic refuses with 0 (extended thinking, forced `tool_choice`, `output_config.format`) are not checked.
- `listen_addr` is the local address the proxy binds to. The default is `0.0.0.0:8085`, so you only need to set it when you want a different host or port.
- `upstream_path` is the upstream route. The default is `/v1/responses`. Change it only if your provider uses a different endpoint.

### Client auth (`proxy_key`)

The proxy binds `0.0.0.0:8085` by default, which means anyone on the network can reach it. If you don't set `proxy_key`, every reachable client can spend your upstream API key — the proxy prints a startup warning but otherwise allows all traffic. To require auth, set `proxy_key` to any shared secret string:

```json
{
  "proxy_key": "any-shared-secret-string"
}
```

Clients must then send an `X-Proxy-Key: any-shared-secret-string` header on every `/v1/messages` request, or the proxy returns 401. The `/healthz` endpoint is always open for liveness checks. The header value is compared in constant time to avoid timing leaks. Environment variable: `PROXY_KEY` — wins over the `proxy.json` value.

**Wiring `X-Proxy-Key` into Claude Code.** Add an `ANTHROPIC_CUSTOM_HEADERS` entry to the `env` block in `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8085",
    "ANTHROPIC_API_KEY": "any",
    "ANTHROPIC_CUSTOM_HEADERS": "X-Proxy-Key: any-shared-secret-string"
  }
}
```

`ANTHROPIC_CUSTOM_HEADERS` is the Anthropic SDK's standard way to inject arbitrary HTTP headers on every request. The value is in `Name: Value` format (one header per line for multiple). The `scripts/start-claude-code.sh` and `.ps1` helper scripts do this forwarding automatically when `PROXY_KEY` is set in the calling shell.

### Logging (`log_to_disk`)

By default the proxy **drops** every `tracing` event — nothing reaches the terminal or a file. The terminal shows only the explicit `println!` / `eprintln!` lines (startup banner, per-request `→` / `←` summary, shutdown notice). Set `log_to_disk: true` in `proxy.json`, or `LOG_TO_DISK=1` in the env, to write a rotating file at `target/logs/proxy.log` for postmortem inspection. The file path is unchanged from earlier versions. Off-by-default keeps request and response bodies off the terminal and off disk on every upstream error.

```json
{
  "log_to_disk": true
}
```

`RUST_LOG` chooses **which** events are recorded. It is the standard `tracing-subscriber` filter variable, and the proxy reads it at startup:

- Unset, the filter is `info,openai_to_anthropic_proxy=debug` — the proxy's own events at debug, everything else at info.
- Set it to raise or lower the level, or to target one module: `RUST_LOG=debug`, `RUST_LOG=openai_to_anthropic_proxy=trace`, `RUST_LOG=warn`.
- **`RUST_LOG` alone shows you nothing.** It picks the events; `log_to_disk` decides whether they are written anywhere. With `log_to_disk` off, the events go to a sink and are discarded whatever the filter says. To see anything you need **both**: `LOG_TO_DISK=1` and `RUST_LOG=...`, then read `target/logs/proxy.log`.

These two are the only logging knobs the proxy has. There is **no** way to dump a request or response body. The log records an INFO summary per request (inbound and upstream model, stream flag, tool count, thinking and effort settings), the raw upstream **response** body at DEBUG wherever the response is buffered rather than streamed (a non-streaming `/v1/messages`, an advisor consult, a `web_search` compaction), and the upstream error body at WARN. For the request side, add a temporary `tracing::debug!` around `send_upstream_once` in `src/proxy.rs`, or put a logging proxy between the proxy and the upstream.

### Reasoning

The proxy selects the upstream `reasoning.effort` for each request from two
independent surfaces:

**1. Fixed per-model effort** (used when the client sends no explicit
reasoning signal):

1. `reasoning.models[resolved_model]`
2. `reasoning.default`
3. top-level `reasoning_effort`
4. built-in `none`

**2. Claude Code request-driven effort** (used when the client explicitly
requests an effort or disables thinking):

- `reasoning.effort_map` — translates Claude Code's `output_config.effort`
  (`low`/`medium`/`high`/`xhigh`/`max`) into the upstream model's effort
  vocabulary. Each entry is a Claude-effort → upstream-effort object, with
  a `default` for all models and per-model `models` overrides. A value of
  `null` omits the upstream `reasoning` object entirely.
- `reasoning.thinking_disabled` — translates Claude Code's
  `thinking: {"type": "disabled"}` request. It uses the same shape as
  `effort_map`, keyed by the `disabled` sentinel (normally mapping to
  `none` so the upstream does no reasoning). Disabled thinking takes
  precedence over both `output_config.effort` and the fixed per-model
  config.

Resolution precedence for a single request:

1. `thinking.type = "disabled"` → `thinking_disabled` map
2. `output_config.effort` → `effort_map` map
3. no explicit signal → fixed per-model resolution above

A useful rule: if a model gets renamed by `model_aliases.map`, key the
reasoning entries by the renamed upstream model, not the Claude-facing name.
That way an aliased request lands on the right effort automatically.

#### Reasoning summary

An OpenAI-style upstream only returns reasoning text when the request asks for
a summary (`reasoning.summary`, one of `auto`, `concise`, `detailed`). That text
is what the proxy turns into the Anthropic `thinking` block, and what
[`reasoning_replay`](#reasoning-replay) sends back on the next turn. Without it
the thinking block is empty and replay does nothing.

So the proxy sends `reasoning.summary` by default (`"auto"`) on every request
that has reasoning on. It sends none when the effort is `none`, or when the
reasoning object is omitted.

- `reasoning.summary` (env `REASONING_SUMMARY`): `auto` (default), `concise`,
  `detailed`, or `off` to send no `summary` field. Env wins over the file. An
  unrecognised value warns at startup and uses `auto`.
- `reasoning.summary_models`: per-upstream-model override, same values. Use
  `"off"` for an upstream that rejects the field with a 400.
- The advisor follows the same setting, using the advisor model's entry. Its
  reply is read as plain text, so it never shows the summary; the setting just
  keeps the advisor's request consistent with the main one.

Not every model supports every value (`concise` in particular), and OpenAI may
require organisation verification before returning summaries for its newest
reasoning models. If an upstream rejects the request after this was introduced,
set that model to `off`. This does not help an upstream that returns its
reasoning in a different field (DeepSeek's first-party API uses `content`
parts); that is a separate limitation.

```json
{
  "reasoning": {
    "default": "none",
    "models": {
      "gpt-5.4-mini": "high",
      "gpt-5.6-luna": "medium"
    },
    "effort_map": {
      "default": {
        "low": "none",
        "medium": "low",
        "high": "medium",
        "xhigh": "high",
        "max": "high"
      }
    },
    "thinking_disabled": {
      "default": {
        "disabled": "none"
      }
    }
  }
}
```

In the redacted example above, `claude-opus-4-8` and `claude-sonnet-5` both
route to models with explicit `reasoning.models` entries. `claude-haiku-4-5`
routes to `gpt-5.4-nano`, which falls back to `reasoning.default = "none"`
because there is no dedicated entry for that model.

### Prompt caching

`prompt_caching` is opt-in and defaults to disabled (empty `models` list).

- `models` is a list of upstream model names that support prompt caching. Only requests sent to these models will get `prompt_cache_breakpoint` markers and the `prompt_cache_key` field. Models not in the list get no prompt-caching fields at all.
- `cache_key` is optional; if set, it is forwarded as `prompt_cache_key` to help the upstream route similar prompt prefixes to the same cache bucket.

```json
{
  "prompt_caching": {
    "models": ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"],
    "cache_key": "my-app"
  }
}
```

Environment variable: `PROMPT_CACHING_MODELS` — a comma-separated list (e.g. `gpt-5.6-luna,gpt-5.6-terra`). It overrides the `models` field from the file. Use `PROMPT_CACHE_KEY` to override `cache_key`.

Because unknown JSON fields are silently ignored by most OpenAI-compatible endpoints, enabling this is safe even for Ollama, OpenRouter, or vLLM upstreams. Those endpoints will simply ignore the extra fields.

### System messages inside the conversation (`system_message_placement`)

Claude Code does not only send a system prompt at the top. It also puts `role: "system"` messages **inside** `messages`, each where it applies: the environment block and skills list near the start, and later "The user hasn't heard from you in a while", hook output and similar notes. (Captured from the Claude Code CLI itself; each of those is a separate `system` message, and a tool announcement arrives as a text-less `tool_addition` block next to them.)

An upstream's prompt cache can only reuse a prompt whose **start is unchanged**. By default the proxy folds every one of these messages into `instructions`, which is the very start of the prompt, so each time one arrives the whole conversation after it is read again at full price. In a day of real traffic this was **46% of all uncached input** (13 misses, 2.8M tokens, some on 470k-token conversations).

- `"inline"` (**default**): send each one as a `system` message at its own position in the conversation. The start of the prompt then stays identical and only the new note is uncached.
- `"instructions"`: fold them into `instructions`, as the proxy did before. Every upstream accepts this, but it changes the start of the prompt each time one arrives.

```json
{
  "system_message_placement": "instructions"
}
```

Environment variable: `SYSTEM_MESSAGE_PLACEMENT` (`inline` or `instructions`, case-insensitive; the env var wins over the file; anything else warns and uses the default).

**`inline` is the default because it needs nothing to be set up, but it does depend on the upstream.** It needs the upstream to accept a `system` message after the first turn. The OpenAI Responses API does. An upstream that applies a chat template might reject one with a 400, or ignore it. It has not been tried against every upstream. **If requests start failing after an upgrade, or the model seems to ignore the notes, set `"system_message_placement": "instructions"`.** To see whether `inline` helps with your upstream, compare the cached-token figures in the proxy's log.

The `# Advisor Tool` section Claude Code puts in one of these messages is removed from the inline copy too, under the same rule as for the instructions, and the advisor's reviewer never sees it there either.

### Documents and PDFs (`forward_documents`)

Claude Code attaches files as Anthropic `document` blocks, in a user message or
inside a `tool_result` (its Read tool returns a PDF that way). What the proxy
does depends on the block's `source`:

- `text` and `content` sources are **always** forwarded, as plain text
  (`input_text`; a `content` source's images go as `input_image`). A `title` and
  `context`, when present, lead the text. This needs no setting.
- `base64` and `url` sources (PDFs) are controlled by `forward_documents`:
  - `false` (**default**) — the file is not sent. The model gets a short note,
    such as `[attached document omitted: report.pdf (application/pdf)]`, so it
    knows a file existed instead of seeing nothing.
  - `true` — the file is sent as an `input_file` part (`file_data` for
    `base64`, `file_url` for `url`).
- Any other source (such as a Files API `file` reference) cannot be resolved by
  the proxy and always becomes the note.

```json
{
  "forward_documents": true
}
```

Environment variable: `FORWARD_DOCUMENTS` (`1` / `true` / `yes` / `on` turn it
on; anything else is off). Env wins over the file.

It is off by default because `input_file` is an OpenAI feature that many
OpenAI-compatible gateways and local servers reject with a 400, which would fail
the whole request, and every later turn that repeats the attachment. Turn it on
only for an upstream you have confirmed accepts `input_file`.

Notes:

- Inside a `tool_result`, a file part is sent only when `tool_result_images` is
  `array` (the default), because that is the only form that can carry it. With
  `lift` or `drop` the block becomes the note.
- `POST /v1/messages/count_tokens` estimates a forwarded file as a single
  image-sized item, not by its size, so a large PDF is under-counted.

### Tool result images (`tool_result_images`)

A tool can return images (a screenshot from a browser tool, an image produced by
a script, …). Anthropic packs those `image` blocks into the `tool_result` content,
and this setting decides what the proxy does with them:

- `array` (**default**) — send the tool output as an **array of content parts**
  (`input_text` + `input_image`), in the order the client sent them. This is the
  form the Responses API documents for `function_call_output.output`, and
  OpenAI and DeepSeek accept it.
- `lift` — for gateways that only accept the **plain string** form of
  `function_call_output.output` and reject the array form with a 400. The tool
  output carries only the text; the images are moved into a follow-up `user`
  message sent immediately after the tool result.
- `drop` — discard the images and keep only their text. This is the behaviour
  before this option existed.

```json
{
  "tool_result_images": "array"
}
```

Environment variable: `TOOL_RESULT_IMAGES` (accepts `array`, `lift` or `drop`,
case-insensitive). Env wins over the file. An unrecognised value does **not**
stop the proxy — it falls back to `array` with a warning, the same house rule
as the advisor limits.

Notes:

- Images in **user** messages are always forwarded as `input_image` parts
  (base64 `data:` URIs or passthrough URLs) and do not depend on this setting.
- Images in **assistant** turns are dropped with a warning — the Responses API
  has no assistant-image content part to map them to.
- This setting only governs images **inside `tool_result` blocks**. If your
  gateway is not vision-capable at all, the images are still transmitted;
  whether the upstream can read them is an upstream question, not a proxy one.
- An `image` block whose `source` the proxy cannot decode (unknown type,
  missing fields) is dropped with a warning; the rest of the tool result is
  unaffected.
- `is_error: true` still prefixes the text with `[TOOL_ERROR]` in all three
  modes.
- Tool results carried in **text only** look exactly the same as before this
  option existed — a bare string `output`, byte for byte.
- Requests default to a **32 MiB (33,554,432-byte)** cap; configure it with
  `max_request_body_bytes` or `MAX_REQUEST_BODY_BYTES`. A base64 image inflates
  ~37% over its binary size, so the default allows roughly a 24 MB screenshot
  in one tool result. This cap applies to the inbound `/v1/messages` body; it
  does not change the fake upstream test server's separate test-only limit.

### Reasoning replay

`reasoning_replay` controls how the proxy sends a prior turn's **extended thinking** back to the upstream.

Claude Code (and any Anthropic client) echoes the model's `thinking` blocks back on the next request. The upstream, however, is an OpenAI-Responses-API endpoint, which has no equivalent of the Anthropic `thinking` block — so the proxy has to re-encode it. Before this option existed, the proxy **dropped those blocks entirely**, which meant the model never saw any of its own prior reasoning.

- `default` is the mode for every upstream model not listed in `models`. One of:
  - `"text"` (default) — fold the reasoning **text** into the assistant message, wrapped in `<thinking>` markers, ahead of the visible answer. This is ordinary assistant text, so it is safe for *any* upstream: it cannot introduce a new input-item type, and therefore cannot trip an upstream's unknown-item rejection.
  - `"off"` — drop the thinking blocks (the old behaviour). Only useful if the extra input tokens are not worth paying for.
  - `"native"` — emit a Responses `reasoning` input item. This is OpenAI's documented replay mechanism and DeepSeek's Responses API *requires* it, but it is a provider-specific wire shape: OpenAI pairs a reasoning item with the item that immediately follows it, and some endpoints reject a reasoning item that has no `id` (the proxy cannot recover the upstream's `rs_…` id — the Anthropic `thinking` block has no id field). **Opt in per model and verify against your upstream first.**
- `models` is a flat object of upstream model name → mode, overriding `default` for that model. Keys are the **alias-resolved upstream** names (the same convention as `prompt_caching.models`).
- `max_chars` caps how many characters of reasoning text are replayed **per assistant turn** — and a request replays *every* prior turn, so the effective cap is `turns × max_chars`. It is a **single global value**, not per-model: you cannot give one upstream a tighter cap than another. **`0` means unlimited, not "replay nothing"** — it is the default. Replayed reasoning is re-sent on **every** subsequent turn and Claude Code echoes *every* prior thinking block, so the replayed text grows with the session. Set an explicit cap if the input-token cost matters to you. Note the cap applies to the reasoning *text* only; the opaque `signature` is forwarded in full in `"native"` mode.

```json
{
  "reasoning_replay": {
    "default": "text",
    "models": {
      "gpt-5.6-luna": "native"
    },
    "max_chars": 4000
  }
}
```

Notes:

- The `signature` field of a thinking block carries the upstream's encrypted chain-of-thought. It is only meaningful to the upstream that issued it, so `"text"` mode drops it and `"native"` mode forwards it as `encrypted_content`.
- A `redacted_thinking` block is an opaque Anthropic-encrypted blob with no readable text. It is never replayed upstream; the proxy logs a warning instead.
- In `"text"` mode the reasoning is folded into the assistant message's **single** `output_text` part, so the visible answer is never displaced. Any `</thinking>` inside the reasoning is escaped, so quoted content cannot close the fence early and be read back as the assistant's own words.
- The reasoning always precedes the visible answer, matching Anthropic's own ordering (`thinking` then `text`).
- The replayed amount is reported on the `→ upstream` log line as `replayed_chars`. **If that number drops to zero across every request, the upstream has stopped producing reasoning** and the replay has silently become a no-op — that is the signal to look for after a provider change.
- The proxy reads the upstream's reasoning from the `summary` field of its `reasoning` output item (non-streaming) and from `response.reasoning_summary_text.delta` events (streaming). **How much reasoning you actually get depends on the upstream**, and it varies a lot:

  | upstream | thinking blocks with text |
  |---|---|
  | `deepseek-v4.1-flash:1017` | **60 / 60** (p50 ≈ 1 100 chars, max ≈ 17 000) |
  | `openai/gpt-5.6-luna` | 46 / 437 (p50 = 0 — this upstream generates no reasoning summary) |

  An upstream that puts the reasoning only in a `content` array (DeepSeek's *first-party* Responses API uses `content` with `reasoning_text` parts and generates no summary) would also deliver an empty thinking block. In every case the reasoning is dropped rather than replayed, so the fix is a no-op for that upstream — check `replayed_chars` before assuming it is working.

### What it costs

Measured against the 2026-11-12 production traffic (`deepseek-v4.1-flash` behind the
airia gateway), whose reasoning summaries are 97–17 072 characters (median 1 054):

| | before | after |
|---|---|---|
| reasoning replayed per request | 0 | 153 → 138 081 chars as the session grows |
| extra input tokens per request | 0 | ≈ +23 700 average, ≈ +34 500 at end-of-session |
| as a share of the prompt | — | ≈ +6.6 % of the session's median context (358 726 tokens) — ≈ +6.5 % at the largest context seen that day (530 794 tokens) |

That is the price of the model keeping its own reasoning. Set `max_chars` if you want
less of it — but remember the cap is per turn, so the real bound is `turns × max_chars`.

### `"native"` mode caveats

`"native"` is **experimental**. It emits a Responses `reasoning` input item, which is OpenAI's documented replay mechanism and is *required* by DeepSeek's Responses API — but it is a provider-specific wire shape. Before enabling it for a production model, send a real request through it and check the upstream accepts the result. Known limits:

- The upstream's own `rs_…` item id cannot be recovered — the Anthropic `thinking` block carries no id — so the proxy sends a **synthetic** id derived from the reasoning content. It is stable across turns (so the upstream's prompt cache is not churned), but it is not the id the upstream originally issued.
- Anthropic's *interleaved* thinking (one thinking block per tool call) is merged into a **single** reasoning item, because the Responses API pairs a reasoning item with the item that immediately follows it. The proxy logs a WARN when it merges more than one block.
- The reasoning item always carries a `summary` field, even when empty, because the upstream's schema marks it required.
- A reasoning item that carries only a signature (no text) is still emitted, with an empty `summary`. Some endpoints may reject it.

### Web fetch (`web_fetch`)

Optional settings for the server-side `web_fetch` tool — Anthropic's "fetch this URL for me" server tool, which the proxy implements itself. **The whole feature is opt-in**: `enabled` defaults to `false`, and when it is off the proxy fetches nothing, synthesizes no tool, and every ordinary request is byte-for-byte unchanged.

When enabled, a fetch happens only if the client declared the **server-side** `web_fetch` tool (a tool named `web_fetch` with **no** `input_schema`) **and** the executor emits `function_call{name:"web_fetch"}`. The proxy performs the HTTP GET itself, extracts the readable text, and hands it back to the executor so the model can answer with the page in context.

```json
"web_fetch": {
  "enabled": false,
  "timeout_secs": 20,
  "max_bytes": null,
  "max_content_bytes": null
}
```

- `timeout_secs` bounds the **whole** fetch — connect, read and extract — as one wall-clock budget. The default is 20 seconds.
- `max_bytes` caps how many bytes are read from the network for one fetch. **`null` (the default) means unlimited.**
- `max_content_bytes` caps the size of the **extracted text** that is handed to the model, truncating at a paragraph boundary. **`null` (the default) means no truncation.**
- The operator sets **no domain allow-list and no URL-length limit**: the proxy fetches any `http`/`https` URL the model asks for, unless the *client* put `allowed_domains` / `blocked_domains` on its tool declaration (then those apply, and every redirect hop is checked against them too).
- There **is** an SSRF guard, and it is not configurable: only `http`/`https` are allowed, a URL may not carry credentials, every resolved address is checked against the private/reserved ranges (loopback, RFC1918, link-local including the cloud metadata address, CGNAT, unique-local, multicast, …), the checked address is the one the connection uses (so DNS rebinding cannot smuggle a different one past the check), and every redirect hop is re-validated the same way.
- A failed fetch never fails the request. It becomes a `web_fetch_tool_result` error block (with an Anthropic error code such as `url_not_allowed` or `url_not_accessible`), the executor is told the fetch failed, and the turn continues — the same contract Anthropic's own server-side tools have.
- The number of fetches is **not** capped by `web_fetch` itself. If the client declares `max_uses`, the proxy enforces it (a fetch over the limit gets a `max_uses_exceeded` result); with none, fetches are unlimited, and a turn that fetches ten URLs is logged with a warning. The shared loop's round limit (see [Server tools](#server-tools-server_tools)) still bounds the turn unless it is set to `0`.
- The client's `max_content_tokens` (about 4 characters per token) can tighten `max_content_bytes` for a request, never loosen it.
- Every field has an env override with the same **env > file > default** precedence used elsewhere: `WEB_FETCH_ENABLED`, `WEB_FETCH_TIMEOUT_SECS`, `WEB_FETCH_MAX_BYTES`, `WEB_FETCH_MAX_CONTENT_BYTES`. `WEB_FETCH_ENABLED` accepts `1`/`true`/`yes`/`on`.
- The section is safe to omit entirely. The TUI rewrites `proxy.json` from the same schema, and the new key is only written when it is actually configured — so a config saved by this version still starts on an older binary.

### Web search (`web_search`)

Optional proxy-native replacement for the upstream's built-in web search. **Opt-in**: `enabled` defaults to `false`, and when it is off the client's `web_search` tool goes to the upstream's own search exactly as before. When enabled, the proxy answers the tool itself — it scrapes DuckDuckGo for each query the model submits, fetches each result page, and compacts long pages — and hands the text to the executor.

```json
"web_search": {
  "enabled": false,
  "compaction_model": null,
  "compaction_threshold_chars": 5000,
  "max_queries": 3,
  "max_results_per_query": 5,
  "search_timeout_secs": 15,
  "page_timeout_secs": 15,
  "max_page_bytes": 2097152,
  "search_min_interval_ms": 1500,
  "block_wait_minutes": 10,
  "state_dir": "proxy-state",
  "cache_ttl_secs": 600,
  "cache_max_entries": 128,
  "session_ttl_hours": 168
}
```

- `compaction_model` is an **explicit upstream model name** (no alias resolution, like `advisor.model`). Pages longer than `compaction_threshold_chars` are summarised by it (up to 3 passes). **Unset / `null` means no model call:** long pages are truncated at a paragraph boundary and carry a note saying so.
- `max_queries` (default 3, max 10) is how many queries the model may run per call; `max_results_per_query` (default 5, max 10) is how many result pages are fetched per query.
- **Nothing is unlimited.** Unlike `web_fetch`, `0`, negative and fractional values are invalid: the default is used with a startup warning. Values above the maximum are clamped, with a warning.
- Result pages are fetched through the same SSRF-hardened fetcher as `web_fetch`.
- A blocked search (CAPTCHA, HTTP error) or an unreachable page is reported in the tool result and never fails the request. The proxy does not fall back to the upstream's search.
- **Searches are paced.** DuckDuckGo is scraped, and it answers a burst of queries from one address with a bot challenge (CAPTCHA). So searches run **one at a time**, across every request the proxy is serving, with at least `search_min_interval_ms` (default 1500, max 30000) between the end of one search and the start of the next. Several queries in one call simply queue.
- **When a search is blocked** (CAPTCHA, or an HTTP error that means rate limiting) the model is told what happened and to wait about `block_wait_minutes` (default 10, max 120) before searching again. The proxy does not refuse later searches: if the model comes back sooner and is blocked again, the message also says how long ago the previous blocked search was, in seconds and minutes, so the model can see how fast it is retrying. A search that returns results clears that record. The record lives in memory and starts empty after a restart.
- **The search keeps a session.** The cookies DuckDuckGo sets are sent back on later searches and saved in `state_dir` (default `proxy-state`, beside `proxy.json`, created on demand and gitignored), so a restart continues the same session. Expired cookies are never sent or saved, and a session older than `session_ttl_hours` (default 168, max 720) is dropped and restarted. A file that is missing, corrupt or unwritable never fails a search.
- **Results are cached.** The same query (same region, case and spacing ignored) within `cache_ttl_secs` (default 600, max 86400) is answered from memory and never reaches DuckDuckGo; `cache_max_entries` (default 128, max 4096) bounds the cache. Blocked and failed searches are never cached. The cache lives in memory and starts empty after a restart.
- **Failed searches are logged in detail** (one `warn`, `web_search search failed`): the HTTP status and headers, what kind of page came back, how many searches had succeeded before the block, whether cookies were sent, and more. The blocked page itself is saved under `<state_dir>/blocked-pages/` (newest 5 kept). See the web search README's "Diagnosing failures".
- `search_endpoint` overrides where searches are POSTed (default DuckDuckGo's HTML interface) — mainly useful for tests.
- Env overrides use the same env > file > default precedence: `WEB_SEARCH_ENABLED`, `WEB_SEARCH_COMPACTION_MODEL`, `WEB_SEARCH_COMPACTION_THRESHOLD_CHARS`, `WEB_SEARCH_MAX_QUERIES`, `WEB_SEARCH_MAX_RESULTS_PER_QUERY`, `WEB_SEARCH_SEARCH_TIMEOUT_SECS`, `WEB_SEARCH_PAGE_TIMEOUT_SECS`, `WEB_SEARCH_MAX_PAGE_BYTES`, `WEB_SEARCH_ENDPOINT`, `WEB_SEARCH_SEARCH_MIN_INTERVAL_MS`, `WEB_SEARCH_BLOCK_WAIT_MINUTES`, `WEB_SEARCH_STATE_DIR`, `WEB_SEARCH_CACHE_TTL_SECS`, `WEB_SEARCH_CACHE_MAX_ENTRIES`, `WEB_SEARCH_SESSION_TTL_HOURS`.
- It works alongside `web_fetch` and the advisor in the same turn. The client's own options on its `web_search` tool are applied: `max_uses` (queries per request), `allowed_domains` / `blocked_domains`, and `user_location` (the search region). What the model read is carried into later turns, so it still knows what it found.

Full documentation: [`src/tools/web_search/README.md`](./src/tools/web_search/README.md).

### Server tools (`server_tools`)

The limits of the loop that `web_search`, `web_fetch` and the advisor share. **Optional**: the defaults match Anthropic's own server-side loop, and the section is safe to omit.

```json
"server_tools": {
  "max_rounds": 10,
  "pause_turn": false
}
```

- `max_rounds` is how many **rounds** one request may run. A round is one executor run whose server-tool calls were served, and every round costs an upstream call. The default is `10`, as in Anthropic's own loop. **`0` means no limit**, like every other limit in this file. Negative and fractional values fall back to the default with a warning.
- **An unlimited tool needs `max_rounds: 0` too.** `advisor.max_uses: 0` and the unlimited `web_fetch` are only truly unlimited if this is `0` as well; with the default, the turn ends after 10 rounds whatever the tool allows.
- `pause_turn` chooses what happens at the limit. **`false` (the default):** the model's next tool call is refused, it is told the limit was reached, and it gets one last run to answer, so the client sees an ordinary finished turn. **`true`:** the response is returned as it stands with `stop_reason: "pause_turn"` and no further upstream call is made, as Anthropic's API does; the client resends the conversation to carry on. **Claude Code does not resume a `pause_turn`** (the Anthropic SDKs do), so leave it `false` for Claude Code, or a long research turn would simply end with no answer.
- Env overrides: `SERVER_TOOLS_MAX_ROUNDS`, `SERVER_TOOLS_PAUSE_TURN` (accepts `1`/`true`/`yes`/`on`), with the usual env > file > default precedence.
- More: [`src/tools/server_loop/README.md`](./src/tools/server_loop/README.md).

### Advisor (`advisor`)

Optional settings for the server-side advisor (a second-opinion consultation inside one assistant message). **The whole feature is opt-in**: `enabled` defaults to `false`, and when it is off the proxy makes no extra upstream calls, synthesizes no tool, and emits no advisor wire fields — every ordinary request is byte-for-byte unchanged. When enabled, a consultation happens only if the client declared a tool named `advisor` **and** the executor emits `function_call{name:"advisor"}`; it is bounded by `max_uses` (set `max_uses` to `0` to remove that bound — see below) and degrades to `error_code: "unavailable"` on any advisor failure without failing the client request.

This section only covers the **proxy** side. The client side is separate and is set in Claude Code's own settings file (`advisorModel` plus `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1` in `~/.claude/settings.json`) — see [the proxy's own server tools](./README.md#optional-the-proxys-own-server-tools).

```json
"advisor": {
  "enabled": false,
  "model": "beam",
  "effort": "medium",
  "max_uses": 2,
  "max_tokens": 4096,
  "reasoning_max_tokens": 16384,
  "timeout_secs": 0
}
```

- `model` is an **explicit upstream model name**, not an inbound alias. It is written straight into the advisor's own upstream request; `model_aliases.map` is not applied to it. Keep it separate from the executor's alias so the advisor's effort and cost do not couple to the executor's `reasoning.models` entry.
- **`beam` is the cheap default** ($0.125/M input, $0.40/M output — roughly 25× cheaper on output than `kimi-k3.1-pro`). An advisor consult re-runs the executor, so a wrong model choice multiplies cost. Never use a kimi model as the advisor.
- `effort` is an optional reasoning effort string for the advisor only. Omit it to send none.
- `max_uses` caps consultations per turn (Claude Code never sets one). `max_tokens` caps the advisor's **answer** (reasoning is not counted against it) and `reasoning_max_tokens` is the extra allowance for its **reasoning**. `timeout_secs` bounds the extra upstream call, and **by default there is none**: a reasoning advisor reading a long conversation is slow, and cutting it off only turns a good answer into a failure. Set it to a number of seconds if you want one. **Set any of the others to `0` to remove that limit** — a `0` means "no limit"; leaving a key out keeps its shipped default (for `timeout_secs` the default is no limit).
- Every field has an env override with the same **env > file > default** precedence used elsewhere: `ADVISOR_ENABLED`, `ADVISOR_MODEL`, `ADVISOR_EFFORT`, `ADVISOR_MAX_USES`, `ADVISOR_MAX_TOKENS`, `ADVISOR_REASONING_MAX_TOKENS`, `ADVISOR_TIMEOUT_SECS`. `ADVISOR_ENABLED` accepts `1`/`true`/`yes`/`on` (anything else is `false`).
- The section is safe to omit entirely. The TUI rewrites `proxy.json` from the same schema, and the new key is only written when it is actually configured — so a config saved by this version still starts on an older binary.

#### `0` means "no limit" — an ABSENT key means the default

These four keys follow one rule, in `proxy.json` and in the env vars alike:

| value | meaning |
| --- | --- |
| key **absent** | the shipped default — `max_uses` 2, `max_tokens` 4096, `reasoning_max_tokens` 16384; `timeout_secs` has no limit by default. For the other three an absent key is **not** "no limit". |
| **`0`** | **no limit** |
| `n > 0` | that bound |
| invalid (negative, fractional, too large) | the shipped default, **with a warning** — the proxy still starts (see below) |

`0` = unlimited follows the house precedent of `reasoning_replay.max_chars`, where `0` means unlimited. **"No limit" means something different per field**, so read the caveats before setting it:

- **`max_uses: 0` — the consultation loop loses its bound** (but see `server_tools.max_rounds`: the shared loop still stops a turn after 10 rounds unless that is `0` too). The loop is then bounded only by the executor's behaviour: the executor may call `advisor` as often as it likes. This is a real hazard — every iteration costs **two** upstream calls (the executor re-run *and* the advisor), so an executor that keeps asking will spin and burn tokens. The proxy imposes no ceiling of its own (you asked for no limit), but it makes the hazard visible: one WARN at startup when the advisor is enabled with an unlimited `max_uses`, and one WARN per turn that passes 10 consultations.
- **`max_tokens: 0` or `reasoning_max_tokens: 0` — the proxy stops capping the advisor's output, the upstream does not.** The proxy then **omits** `max_output_tokens` from the advisor's own upstream request, so the **upstream's own default** applies. That default may be **smaller** than the shipped one, so "no limit" here means *the proxy imposes no cap*, not *the answer may be long*. If your upstream's default is too small, set explicit numbers instead.

#### `max_tokens` is the answer, `reasoning_max_tokens` is the thinking

The upstream's `max_output_tokens` counts reasoning and answer together and cannot count only the answer. A reasoning advisor such as `beam` given a small limit can spend all of it thinking and return nothing (the proxy log showed exactly this: `incomplete / max_output_tokens` at 2048). So the proxy sends the **sum**, `max_tokens + reasoning_max_tokens` (by default 4096 + 16384 = 20480), and then holds the *answer* to `max_tokens` itself: an answer longer than about `max_tokens × 4` characters is cut and marked `[…]`. This is an estimate, since the proxy cannot count tokens exactly.

If the advisor still stops at the output limit but had already written advice, that advice is kept (marked as cut off) instead of being thrown away. If it stopped before writing any, the consultation fails and the log says its reasoning used the whole budget; raise `reasoning_max_tokens` or lower `effort`.
- **`timeout_secs: 0` (or absent, the default) — no per-request timeout override.** The advisor's own call is then bounded only by the **global `request_timeout_secs`** (the reqwest client is built with that timeout, so every request is bounded by it anyway). A *bounded* `timeout_secs` is always clamped to the global value, so the advisor can never extend your global bound.

The env vars follow the same `0` = no limit rule (`ADVISOR_MAX_USES=0`, `ADVISOR_MAX_TOKENS=0`, `ADVISOR_REASONING_MAX_TOKENS=0`, `ADVISOR_TIMEOUT_SECS=0`).

#### The advisor prompt is trimmed to a fixed size

The advisor is sent the executor's whole conversation, which grows without bound in a long session. To keep it inside the advisor model's context window, the proxy holds that prompt to a **fixed budget of 1,200,000 characters (roughly 400,000 tokens)**. This is a built-in constant, **not** a `proxy.json` key or an env var. A prompt within the budget is sent whole; a larger one is trimmed in stages, stopping at the first stage that fits:

| stage | what is trimmed |
| --- | --- |
| always | inline `data:` blobs (images) are replaced by a short placeholder, and the JSON is compact (not pretty-printed) |
| 1 | **tool results** are shortened, gentlest cap first: 16,000, then 4,000, then 1,000 bytes each |
| 2 | **every remaining long string** is capped at 20,000 bytes |
| 3 | the **oldest items** are dropped, in blocks of 16, and replaced by one marker line. The first item (the user's task) and the most recent items are always kept |

The executor's own instructions are capped at a quarter of the budget (300,000 characters) so they cannot crowd out the conversation. Each cut states how many bytes it removed, and a trimmed prompt is logged at `info` with its size before and after. If you point `advisor.model` at a model whose context window is much smaller than about 400,000 tokens, this budget is too large for it and its calls will still be rejected as "prompt is too long"; the constant is `MAX_TRANSCRIPT_CHARS` in `src/tools/advisor/tool.rs`.

#### Prompt caching for the advisor

Every consultation sends the whole conversation again, but each one is the previous one plus a few new items, so the start of the prompt is nearly identical from one consultation to the next. An upstream with prompt caching charges much less for a repeated start. To get that for the advisor, **list the advisor's model in `prompt_caching.models`** (for example `"models": ["beam"]`); with the default empty list the advisor request carries no caching fields, as before.

- The prompt is laid out with the unchanging parts first: the reviewer's instructions, then the assistant's own instructions, then the conversation, oldest item first, with the new text at the end.
- The advisor request carries a `prompt_cache_key`. If you set `prompt_caching.cache_key` it is used as given; otherwise the proxy derives one from the conversation's first item (the user's task), so every consultation of one conversation shares a key and a different conversation gets a different one.
- When the conversation is long enough to be trimmed (step 3 above), the oldest items are dropped **in blocks of 16**, rounded up. Dropping exactly as many as needed would move the cut on every consultation and change the start of the prompt each time, so no cache could hit. In blocks, the cut stays put for several consultations in a row. The price is that up to 15 more old items than strictly necessary are left out.
- The Claude Code advisor line now shows how many input tokens were served from the cache (`576k in (480k cached) · 1.2k out`). Check that number on the second consultation of a turn to see whether the upstream is honouring the cache.

Whether the upstream gives a discount at all depends on the upstream and the model, and it has not been checked against the real service. A cache also expires after a few minutes without use, so a consultation after a long pause starts cold.

#### Failed consultations: one retry, and they do not use up `max_uses`

A consultation that fails for a reason that could be transient (a network error or timeout, an HTTP 429 or 5xx, or a response with no usable advice) is **retried once**, after a quarter of a second, before it is reported as `unavailable`. A client-side error such as "prompt is too long" would fail again the same way, so it is not retried. The log line carries both failures when the retry fails too.

**Only successful consultations count toward `max_uses`.** With the default of 2, one flaky response no longer uses up half the allowance. Failures are bounded separately: after **two** failed consultations in one turn (each already retried) the advisor is treated as down for the rest of that turn. The executor is told it will not be consulted again and given one last run to answer, and any further advisor call is dropped without another upstream request. This limit is built in, not a `proxy.json` key.

#### Why a consultation failed: the error code and the executor's message

Claude Code shows a failed consultation as "Advisor unavailable (`code`)", so the code is the only reason you see there. The proxy now picks the most specific code Claude Code knows, and tells the executor the same reason in words (so it can adapt). The full detail, including the upstream's own message, is in the proxy log.

| the advisor failed because… | code shown in Claude Code | retried once? |
| --- | --- | --- |
| the conversation is too long for it (a 4xx naming "too long" or the context length) | `prompt_too_long` | no |
| it is rate limited (HTTP 429) | `too_many_requests` | yes |
| it is overloaded or unreachable (HTTP 5xx, connection failure) | `overloaded` | yes |
| it did not answer in time (timeout, HTTP 504) | `execution_time_exceeded` | yes |
| it answered with no usable advice (no text, a tool call, or the output limit hit before any text) | `unavailable` | yes |
| the request to it was refused (any other 4xx, such as a bad model name) | `unavailable` | no |

Claude Code only knows six codes, and an unknown one renders as "unknown", so no other code is ever sent. After two failures in a turn the executor is told the advisor will not be consulted again, whatever the reason.

#### What the log records about advisor failures

**Every** failed consultation is a `WARN` line in the proxy log (`advisor consultation failed; continuing without advice`), with the failure code, how many consultations have failed so far this turn (`failures_this_turn`), the prompt size in characters (`prompt_chars`), whether the advisor is now being given up on (`giving_up`) and the error text. A failed attempt that is then retried is logged too (`advisor consultation attempt failed; retrying once`). The transcript itself is never logged. Only the one-line warning shown in the Claude Code/TUI is limited to once per turn, since repeats there are noise; the log is the reliable record of how often the advisor fails.

#### How the advisor is prompted

The advisor reads another assistant's instructions and conversation, and a model shown those can mistake them for its own and carry on as that assistant. A reasoning advisor did exactly that in practice: it answered with a call to `advisor` and no text, so the consultation failed. The request is therefore framed as a review. The advisor's system prompt says it is **not** the assistant, has **no tools** (and must not call `advisor`), and must reply with plain-text advice only. The assistant's instructions are labelled as not written for the reviewer, the executor's own `# Advisor Tool` section ("call advisor before substantive work") is removed from them, and the request ends by repeating that the reviewer should write advice and call no tool. If an advisor model still replies with a tool call, the log now says "advisor called a tool instead of writing advice".

#### An invalid value is a warning, not a startup failure

A limit must be a **whole number that is not negative**. Anything else is invalid, and an invalid value never stops the proxy: it **starts anyway**, falls back to that limit's shipped default, and prints a warning. (The judgement is on the VALUE: `2` and `2.0` are the same valid limit; `-1` and `2.5` are not.)

| written value | what happens |
| --- | --- |
| `"max_uses": -1` (negative) | starts; `max_uses` falls back to the default (`2`); warning |
| `"max_uses": 2.5` (fractional) | starts; `max_uses` falls back to the default (`2`); warning |
| `"max_uses": 4294967296` (bigger than the 32-bit value the proxy uses internally) | starts; `max_uses` falls back to the default (`2`); warning |

```
WARNING: advisor.max_uses = -1 is not a valid limit (it is negative); using the default 2
WARNING: advisor.max_uses = 2.5 is not a valid limit (it is not a whole number); using the default 2
WARNING: advisor.max_uses = 4294967296 is out of range (...); using the default 2
```

Each limit is judged on its own, so one bad value does not affect the others. The env vars are judged exactly like the file (`ADVISOR_MAX_USES=-1`, `ADVISOR_MAX_USES=2.5`, `ADVISOR_MAX_USES=4294967296` all warn and fall back the same way). This follows the house rule: a typo in one advisor limit must not take the whole deployment down. The warning is printed on stderr (so you see it even without file logging) and written to `target/logs/proxy.log` when `log_to_disk` is on.

Two things are still configuration **errors** — the file is not a valid config, so the proxy refuses to start:

* a limit that is not a number at all (`"max_uses": "2"` — a string);
* a number that is not representable as a JSON number at all (`1e400`).

One env-var case is different: `ADVISOR_MAX_USES=abc` is not a number, so the variable is **ignored** (the file value or the default wins) and nothing is printed. That is the long-standing behaviour for these variables.

### Model aliases

This object rewrites the model name Claude Code asks for into the model your upstream actually serves.

- Matching is exact. There are no wildcards.
- If a model is not listed, the proxy passes it through unchanged.
- `default_model` is a safety net. If the upstream rejects the **model** (as opposed to the request), the proxy retries once with this fallback and logs at WARN.
  - The proxy recognises a model rejection from the upstream's error body: a message that names the model as the subject of a negative (`model "claude-sonnet-5" not found. Try glm-5.3-flash:cloud.`), or an error code such as `model_not_found` / `model_not_supported`. It deliberately does **not** fire when the error is about the *request* rather than the model — a tool rejection (`the web_search tool is not supported`), a bad parameter, a schema error. An error that merely mentions the model in passing (`the web_search tool is not supported for model claude-sonnet-5`) is passed straight through too: the fallback answers with a different model, so it must only fire when the upstream really is refusing the model. To keep that separation reliable, the proxy only recognises the *absence* words — "not found", "does not exist", "doesn't exist", "deprecated" — when the model name sits between "model" and the negative. A message where "model" sits immediately before the negative ("model not supported") is recognised too.
  - The retry happens **before** any bytes reach the client, so it is safe for streaming and non-streaming alike. Exactly one retry: if the fallback model is rejected too, the client gets that second error.
  - If the fallback model is the same one that was just rejected, no retry happens. `default_model` may be written as an inbound name or an upstream name — it is resolved through the alias map either way.
  - If the fallback is refused too (model not found, or 410 for a retired model), there is no third attempt and the error names `default_model` as the problem, with the upstream's own reason. A retired `default_model` therefore fails every request whose model is unknown, so keep it pointing at a live model.
  - The WARN carries the upstream's own suggestion as `suggested_model`. That is **telemetry only** — the proxy never routes to it, because it is untrusted input and it may name a model your alias map does not use. But it is the most useful line in the log: it tells you exactly which alias to add.
- An inbound model name that is **not** in `map` is passed through to the upstream unchanged. That is sometimes right — the upstream may serve the name itself — but it is also how a request ends up 404ing. The proxy warns about it once per distinct name, in the log and in the TUI's log pane (it remembers the first 256 distinct names, then stops warning).
- **Important:** renaming a model can confuse Claude Code's context-window detection. Claude Code estimates when to auto-compact from the model name, not from the API response, and an unknown name falls back to 200K. If your upstream supports a larger window, set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` in your environment or `~/.claude/settings.json` (see [REFERENCE.md](./REFERENCE.md#context-window-workaround)).

In the redacted example:

- `claude-opus-4-8` → `gpt-5.6-luna`
- `claude-sonnet-5` → `gpt-5.4-mini`
- `claude-haiku-4-5` → `gpt-5.4-nano`

### Model discovery (`prefix_models_for_discovery`)

Claude Code's `/model` picker only lists model ids that begin with `claude` or
`anthropic`, so an upstream whose models are called `gpt-5.6-luna` or
`glm-5.3-flash` shows nothing to pick from. This flag makes the proxy present
those models under a `claude-` name, and strip the prefix again when a request
comes back in.

- **Off by default.** Set `prefix_models_for_discovery: true` in `proxy.json`,
  or `PREFIX_MODELS_FOR_DISCOVERY=1` in the env (`1` / `true` / `yes` / `on`),
  to turn it on.
- **On the way out** (`GET /v1/models`): every upstream model id that does not
  already start with `claude` or `anthropic` is listed with a `claude-` prefix
  in front of it. The `display_name` keeps the upstream's own name, so the
  picker shows the real model behind the alias.
- **On the way back in**: when the client asks for a `claude-`- or
  `anthropic-`-prefixed model that is **not** in `model_aliases.map`, the proxy
  strips the prefix and sends the remainder upstream — `claude-gpt-5.6-luna`
  goes to the upstream as `gpt-5.6-luna`. A name that **is** in the alias map
  is resolved through the map instead, so the alias wins and the prefix is
  left alone.
- This only works with gateway model discovery switched on in Claude Code
  too: `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` in your
  `~/.claude/settings.json` (see [CONFIGURATION.md](./CONFIGURATION.md)).
- The alias map is still the better tool when you want specific inbound names.
  This flag is for "show me everything the upstream has, under names Claude
  Code will accept".
- **Watch out for the context window.** Claude Code estimates when to
  auto-compact from the model name, so a prefixed name it has never seen falls
  back to 200K. Set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` if your upstream supports
  a larger window (see [REFERENCE.md](./REFERENCE.md#context-window-workaround)).

### Switching providers

If you move from Airia to another upstream, usually change these first:

1. `upstream_base_url`
2. `upstream_api_key`
3. `model_aliases.map`
4. `reasoning.models`
5. `advisor.model` (if the advisor is enabled)

If the upstream already accepts the model names Claude Code sends, you can leave the alias object empty (omit it entirely).

## Good defaults

- Leave `listen_addr` alone unless you need a different local port.
- Leave `upstream_path` alone unless your provider uses a different endpoint.
- Start with `request_timeout_secs: 600` unless you want faster failures.
- Keep `default_model` set if you want an automatic retry when the upstream says a model is missing.

## Keep secrets local

The real `proxy.json` should stay on your machine. The file is already in `.gitignore`, so it won't be committed by accident. If you want to share an example, use `proxy.json.example` or this redacted guide instead.

## Editing tips

- The file is strict JSON — no comments and no trailing commas. Most editors will highlight syntax errors as you type.
- Unknown fields are rejected at startup with a list of accepted field names, so a typo will fail fast instead of being silently ignored.
- Use any JSON formatter (`jq`, your editor's "format document", etc.) to keep the file readable. The proxy doesn't care about whitespace.
