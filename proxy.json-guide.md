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

The proxy chooses `reasoning_effort` in this order for each request:

1. `reasoning.models[resolved_model]`
2. `reasoning.default`
3. top-level `reasoning_effort`
4. built-in `none`

A useful rule: if a model gets renamed by `model_aliases.map`, key the reasoning entry by the renamed upstream model, not the Claude-facing name. That way an aliased request lands on the right effort automatically.

In the redacted example above, `claude-opus-4-8` and `claude-sonnet-5` both route to models with explicit `reasoning.models` entries. `claude-haiku-4-5` routes to `gpt-5.4-nano`, which falls back to `reasoning.default = "none"` because there is no dedicated entry for that model.

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

### Model aliases

This object rewrites the model name Claude Code asks for into the model your upstream actually serves.

- Matching is exact. There are no wildcards.
- If a model is not listed, the proxy passes it through unchanged.
- `default_model` is a safety net. If the upstream rejects a model, the proxy retries once with this fallback and logs at WARN.
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
