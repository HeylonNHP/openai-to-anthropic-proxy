# CONFIGURATION.md

Plain-English reference for setting the proxy up: every `proxy.json` option, and
the Claude Code settings that point it at the proxy. For the full details (env
vars, edge cases, deep dives), see [`proxy.json-guide.md`](./proxy.json-guide.md).

## Minimal config

```json
{
  "upstream_base_url": "https://api.openai.com",
  "upstream_api_key": "sk-..."
}
```

That's enough to run. Everything else has a sensible default.

## Upstream connection

| Option | Default | What it does |
| --- | --- | --- |
| `upstream_base_url` | (required) | Address of your OpenAI-compatible API. |
| `upstream_api_key` | (required) | Your API key for that service. |
| `upstream_path` | `/v1/responses` | Don't change unless your provider uses a different endpoint. |
| `request_timeout_secs` | 600 | How long to wait before giving up on one request (10 minutes). |
| `listen_addr` | `0.0.0.0:8085` | Where the proxy listens locally. |
| `max_request_body_bytes` | 33554432 | Biggest request Claude Code can send (32 MB). Leave alone. |
| `proxy_key` | (none) | A shared secret clients must send with `X-Proxy-Key`. Recommended if anyone else can reach the proxy. |

## Model routing

| Option | Default | What it does |
| --- | --- | --- |
| `model_aliases.map` | empty | Rewrites names like `claude-sonnet-5` to your upstream model names. |
| `model_aliases.default_model` | (none) | Fallback used if the upstream rejects a model. |

## Reasoning (extended thinking)

| Option | Default | What it does |
| --- | --- | --- |
| `reasoning.default` | `none` | Reasoning effort when the client doesn't ask. |
| `reasoning.models` | empty | Per-model reasoning effort (keys are upstream model names). |
| `reasoning.effort_map` | (shipped map) | Translates Claude Code's effort levels (`low`/`medium`/`high`/`xhigh`/`max`). |
| `reasoning.thinking_disabled` | maps `disabled` to `none` | What happens when Claude Code turns thinking off. |
| `reasoning_effort` | (none) | A blunt fallback effort for every request. `reasoning.default` is usually better. |

## Replaying the model's own prior thinking

| Option | Default | What it does |
| --- | --- | --- |
| `reasoning_replay.default` | `text` | How a prior turn's thinking is sent back: `text` (safe, works everywhere), `off` (drop it), or `native` (OpenAI's reasoning item - provider-specific, opt in per model). |
| `reasoning_replay.models` | empty | Per-model override of the mode above. |
| `reasoning_replay.max_chars` | 0 | Cap on replayed reasoning text per turn (`0` = unlimited). |

## Prompt caching

| Option | Default | What it does |
| --- | --- | --- |
| `prompt_caching.models` | empty (off) | Upstream models that support prompt caching. |
| `prompt_caching.cache_key` | (none) | Optional tag to help the upstream group similar prompts. |

## System messages

| Option | Default | What it does |
| --- | --- | --- |
| `system_message_placement` | `inline` | `inline` keeps the start of the prompt stable so more is cached. `instructions` is safer for strict upstreams. |

## Documents (PDFs and files Claude attaches)

| Option | Default | What it does |
| --- | --- | --- |
| `forward_documents` | false | Forward PDFs and base64 file blocks. Text files always go through. |
| `tool_result_images` | `array` | How images inside tool results are sent: `array`, `lift`, or `drop`. |

## Server tools (Claude Code-style tools the proxy runs for the model)

| Option | Default | What it does |
| --- | --- | --- |
| `server_tools.max_rounds` | 10 | How many rounds one request may run (`0` = no limit). |
| `server_tools.pause_turn` | false | At the limit, end with no answer (Anthropic style) instead of refusing the next tool call. Leave false for Claude Code. |

## Advisor (optional second opinion)

Off by default. Only enable if you want the model to consult another model mid-task.

| Option | Default | What it does |
| --- | --- | --- |
| `advisor.enabled` | false | Turn the advisor on. |
| `advisor.model` | (required if on) | Which model advises (must be cheap - the advisor re-runs conversations). |
| `advisor.effort` | (none) | Reasoning effort for the advisor only. |
| `advisor.max_uses` | 2 | Consultations per turn (`0` = no limit). |
| `advisor.max_tokens` | 4096 | Cap on the advisor's answer. |
| `advisor.reasoning_max_tokens` | 16384 | Cap on the advisor's thinking. |
| `advisor.timeout_secs` | 0 | Per-request timeout (`0` = none; bounded by global). |

## Web fetch

| Option | Default | What it does |
| --- | --- | --- |
| `web_fetch.enabled` | false | Let the model fetch web pages. |
| `web_fetch.timeout_secs` | 20 | Total fetch timeout. |
| `web_fetch.max_bytes` | null (unlimited) | Max bytes downloaded per fetch. |
| `web_fetch.max_content_bytes` | null (unlimited) | Max extracted text handed to the model. |

## Web search

| Option | Default | What it does |
| --- | --- | --- |
| `web_search.enabled` | false | Let the model search the web (DuckDuckGo by default). |
| `web_search.max_queries` | 3 | Queries per call (max 10). |
| `web_search.max_results_per_query` | 5 | Pages fetched per query (max 10). |
| `web_search.compaction_model` | null | Model used to summarise long pages; null truncates instead. |
| `web_search.compaction_threshold_chars` | 5000 | Page length above which the summary/truncation kicks in. |
| `web_search.search_timeout_secs` | 15 | Time budget for the search request itself. |
| `web_search.page_timeout_secs` | 15 | Time budget for fetching one result page. |
| `web_search.max_page_bytes` | 2097152 | Max bytes read from one result page (2 MB). |
| `web_search.search_endpoint` | DuckDuckGo HTML | Which search endpoint to use. Rarely needs changing. |
| `web_search.search_min_interval_ms` | 1500 | Minimum gap between searches (rate limiting). |
| `web_search.block_wait_minutes` | 10 | How long to tell the model to wait after a CAPTCHA. |
| `web_search.state_dir` | `proxy-state` | Where cookies and caches are stored. |
| `web_search.cache_ttl_secs` | 600 | Cache lifetime. |
| `web_search.cache_max_entries` | 128 | Cache size. |
| `web_search.session_ttl_hours` | 168 | Cookie session lifetime (~1 week). |

## Logging

| Option | Default | What it does |
| --- | --- | --- |
| `log_to_disk` | false | Write a log file to disk. |

## Model picker

| Option | Default | What it does |
| --- | --- | --- |
| `prefix_models_for_discovery` | false | Prefix every model ID the proxy lists with `claude-`, so Claude Code's `/model` picker discovers non-Claude model names. The proxy strips the prefix back off. |

## Claude Code settings (`~/.claude/settings.json`)

Everything above configures **the proxy**. This section configures **Claude Code**
so it talks to the proxy instead of Anthropic.

Put these in the `env` block of your user-level settings file
(`~/.claude/settings.json`, or `%USERPROFILE%\.claude\settings.json` on
Windows). Claude Code re-applies this `env` block on every request, so it
overrides whatever is in your shell.

### Environment variables (go in the `env` block)

| Variable | Example | What it does |
| --- | --- | --- |
| `ANTHROPIC_BASE_URL` | `http://localhost:8085` | Points Claude Code at the proxy instead of Anthropic. **Required.** |
| `ANTHROPIC_API_KEY` | `any` | The proxy ignores it, but Claude Code needs an API key set. Any value works. |
| `ANTHROPIC_CUSTOM_HEADERS` | `X-Proxy-Key: your-secret-here` | Only needed if you set `proxy_key` in `proxy.json`. Without it the proxy answers 401. |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | `1000000` | The upstream model's **real** context window. Without it Claude Code assumes 200K and auto-compacts too early. |
| `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY` | `1` | Fills Claude Code's `/model` picker from the proxy's `/v1/models` instead of typing model names by hand. |

### Top-level keys (go outside the `env` block)

| Key | Example | What it does |
| --- | --- | --- |
| `model` | `"claude-sonnet-5"` | The model Claude Code asks for. Should be a key in your `model_aliases.map`, or the upstream will get a name it does not serve. |
| `advisorModel` | `"opus"` | Which model Claude Code consults as advisor. `"opus"`, `"sonnet"` or `"fable"`, or a full model ID. Leave unset to keep the advisor off. |
| `autoCompactWindow` | `1000000` | A settings-file alternative to `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (the token count at which Claude Code compacts). |

A complete example:

```json
{
  "model": "claude-sonnet-5",
  "advisorModel": "opus",
  "autoCompactWindow": 1000000,
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8085",
    "ANTHROPIC_API_KEY": "any",
    "ANTHROPIC_CUSTOM_HEADERS": "X-Proxy-Key: your-secret-here",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1"
  }
}
```

### Only if you use the advisor

| Variable | Example | What it does |
| --- | --- | --- |
| `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL` | `1` | Lets Claude Code declare the advisor tool. **Needed behind a proxy**, because a non-Anthropic model has no "advisor rank" in Claude Code's model catalog - without this, Claude Code refuses to offer the advisor. |
| `CLAUDE_CODE_DISABLE_ADVISOR_TOOL` | `1` | The opposite: turns the advisor tool back off. |

The advisor also needs `advisor.enabled` in `proxy.json` - until then Claude
Code declares the tool but no consultation runs.

### Picking the model

The model name Claude Code sends must be a key in `model_aliases.map`,
otherwise the proxy passes it through unchanged and the upstream probably
rejects it (the proxy logs a warning naming the exact alias to add).

Any of these will do it:

- Set the `model` key in `settings.json`, or pick from the `/model` picker once
  gateway model discovery is on.
- `ANTHROPIC_MODEL` - the model to use.
- `ANTHROPIC_DEFAULT_MODEL` - the model for new sessions.
- `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`,
  `ANTHROPIC_DEFAULT_HAIKU_MODEL` - what the `opus` / `sonnet` / `haiku`
  aliases resolve to.

### Optional extras

| Variable | What it does |
| --- | --- |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` | Stops Claude Code's background traffic (updates, tips, error reporting). Handy behind a proxy. |
| `CLAUDE_CODE_GATEWAY_MODEL_DISCOVERY_TIMEOUT_MS` | How long Claude Code waits for the proxy's `/v1/models` (default 3000). |
| `ANTHROPIC_CUSTOM_MODEL_OPTION` | Adds an extra entry to the `/model` picker. |

### Things to avoid

- **Do not leave `ANTHROPIC_AUTH_TOKEN` set.** Claude Code sends it as an
  `Authorization: Bearer` header and it can take precedence over the
  `ANTHROPIC_API_KEY` above. The launch scripts remove it for you.
- **Do not set `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_ENTRYPOINT`,
  `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_PARENT_SESSION_ID` or
  `CLAUDE_CODE_SSE_PORT` yourself.** Claude Code sets these when it spawns a
  sub-agent. If they leak into a fresh `claude` invocation, that invocation is
  treated as a sub-agent of the parent session. The launch scripts clear them.

### Launch scripts

`scripts/start-claude-code.sh` (macOS/Linux) and `scripts/start-claude-code.ps1`
(Windows) set `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` for you, forward
`PROXY_KEY` as the `X-Proxy-Key` header, and pass `--setting-sources=project,local`
so your user-level `settings.json` cannot override the base URL. Use them if
Claude Code keeps hitting Anthropic instead of the proxy.

## Tips

- Env vars override `proxy.json` and use the same names (uppercase). Useful for one-off overrides.
- `0` means no limit for the limit-style options - but leaving the key out keeps the default, which is not "no limit".
- Unknown or misspelled fields are rejected at startup, so typos fail fast.
- Leaving a key out keeps its default, so you only need to set what you change.

