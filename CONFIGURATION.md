# CONFIGURATION.md

Plain-English reference for `proxy.json`. For the full details (env vars, edge
cases, deep dives), see [`proxy.json-guide.md`](./proxy.json-guide.md).

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
| `web_search.state_dir` | `proxy-state` | Where cookies and caches are stored. |
| `web_search.cache_ttl_secs` | 600 | Cache lifetime. |
| `web_search.cache_max_entries` | 128 | Cache size. |
| `web_search.session_ttl_hours` | 168 | Cookie session lifetime (~1 week). |

## Logging

| Option | Default | What it does |
| --- | --- | --- |
| `log_to_disk` | false | Write a log file to disk. |

## Tips

- Env vars override `proxy.json` and use the same names (uppercase). Useful for one-off overrides.
- `0` means no limit for the limit-style options - but leaving the key out keeps the default, which is not "no limit".
- Unknown or misspelled fields are rejected at startup, so typos fail fast.

- Leaving a key out keeps its default, so you only need to set what you change.
