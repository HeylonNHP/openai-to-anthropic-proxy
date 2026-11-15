# proxy.json guide

This is the human-readable walkthrough for the on-disk config file. The checked-in template is `proxy.json.example`; copy it to `proxy.json` and edit the values for your setup. The real `proxy.json` stays local so it can hold secrets and provider-specific settings.

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

- `upstream_base_url` is the gateway URL the proxy sends requests to. The default upstream path is `/v1/responses`; the proxy appends it for you.
- `upstream_api_key` is the upstream bearer token. Keep it out of docs and out of git — `proxy.json` is in `.gitignore`.
- `request_timeout_secs` controls how long the proxy waits before giving up on a single request. The default is 600 (10 minutes).
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

### Advisor (`advisor`)

Optional settings for the server-side advisor (a second-opinion consultation inside one assistant message). **The whole feature is opt-in**: `enabled` defaults to `false`, and when it is off the proxy makes no extra upstream calls and emits no advisor wire fields. This section only stores the resolved policy; it does not change request behaviour by itself.

```json
"advisor": {
  "enabled": false,
  "model": "beam",
  "effort": "medium",
  "max_uses": 2,
  "max_tokens": 2048,
  "timeout_secs": 45
}
```

- `model` is an **explicit upstream model name**, not an inbound alias. It is written straight into the advisor's own upstream request; `model_aliases.map` is not applied to it. Keep it separate from the executor's alias so the advisor's effort and cost do not couple to the executor's `reasoning.models` entry.
- **`beam` is the cheap default** ($0.125/M input, $0.40/M output — roughly 25× cheaper on output than `kimi-k3.1-pro`). An advisor consult re-runs the executor, so a wrong model choice multiplies cost. Never use a kimi model as the advisor.
- `effort` is an optional reasoning effort string for the advisor only. Omit it to send none.
- `max_uses` caps consultations per turn (Claude Code never sets one). `max_tokens` caps the advisor's answer. `timeout_secs` bounds the extra upstream call. A value of `0` for any of these falls back to the default with a WARN rather than failing startup.
- Every field has an env override with the same **env > file > default** precedence used elsewhere: `ADVISOR_ENABLED`, `ADVISOR_MODEL`, `ADVISOR_EFFORT`, `ADVISOR_MAX_USES`, `ADVISOR_MAX_TOKENS`, `ADVISOR_TIMEOUT_SECS`. `ADVISOR_ENABLED` accepts `1`/`true`/`yes`/`on` (anything else is `false`).
- The section is safe to omit entirely. The TUI rewrites `proxy.json` from the same schema, and the new key is only written when it is actually configured — so a config saved by this version still starts on an older binary.

### Model aliases

This object rewrites the model name Claude Code asks for into the model your upstream actually serves.

- Matching is exact. There are no wildcards.
- If a model is not listed, the proxy passes it through unchanged.
- `default_model` is a safety net. If the upstream rejects the **model** (as opposed to the request), the proxy retries once with this fallback and logs at WARN.
  - The proxy recognises a model rejection from the upstream's error body: a message that names the model as the subject of a negative (`model "claude-sonnet-5" not found. Try glm-5.3-flash:cloud.`), or an error code such as `model_not_found` / `model_not_supported`. It deliberately does **not** fire when the error is about the *request* rather than the model — a tool rejection (`the web_search tool is not supported`), a bad parameter, a schema error. An error that merely mentions the model in passing (`the web_search tool is not supported for model claude-sonnet-5`) is passed straight through too: the fallback answers with a different model, so it must only fire when the upstream really is refusing the model. To keep that separation reliable, the proxy only recognises the *absence* words — "not found", "does not exist", "doesn't exist", "deprecated" — when the model name sits between "model" and the negative. A message where "model" sits immediately before the negative ("model not supported") is recognised too.
  - The retry happens **before** any bytes reach the client, so it is safe for streaming and non-streaming alike. Exactly one retry: if the fallback model is rejected too, the client gets that second error.
  - If the fallback model is the same one that was just rejected, no retry happens. `default_model` may be written as an inbound name or an upstream name — it is resolved through the alias map either way.
  - The WARN carries the upstream's own suggestion as `suggested_model`. That is **telemetry only** — the proxy never routes to it, because it is untrusted input and it may name a model your alias map does not use. But it is the most useful line in the log: it tells you exactly which alias to add.
- An inbound model name that is **not** in `map` is passed through to the upstream unchanged. That is sometimes right — the upstream may serve the name itself — but it is also how a request ends up 404ing. The proxy warns about it once per distinct name, in the log and in the TUI's log pane (it remembers the first 256 distinct names, then stops warning).
- **Important:** renaming a model can confuse Claude Code's context-window detection. Claude Code estimates when to auto-compact from the model name, not from the API response, and an unknown name falls back to 200K. If your upstream supports a larger window, set `CLAUDE_CODE_MAX_CONTEXT_TOKENS` in your environment or `~/.claude/settings.json` (see [REFERENCE.md](./REFERENCE.md#context-window-workaround)).

In the redacted example:

- `claude-opus-4-8` → `gpt-5.6-luna`
- `claude-sonnet-5` → `gpt-5.4-mini`
- `claude-haiku-4-5` → `gpt-5.4-nano`

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
