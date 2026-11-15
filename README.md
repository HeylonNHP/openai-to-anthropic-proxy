# openai-to-anthropic-proxy

A small Rust proxy that lets Claude Code talk to OpenAI-compatible upstreams by translating Anthropic requests into OpenAI Responses API calls and back.

## Quick start

1. Set `UPSTREAM_BASE_URL` and `UPSTREAM_API_KEY`, or copy `proxy.json.example` to `proxy.json` and fill them in.
   - For a fuller walkthrough of `proxy.json`, see [proxy.json guide](./proxy.json-guide.md).
2. Start the proxy:

   ```bash
   cargo run --release
   ```

3. Launch Claude Code against it:

   - macOS/Linux: `./scripts/start-claude-code.sh`
   - Windows: `./scripts/start-claude-code.ps1`

The proxy listens on `http://localhost:8085` by default.

## Configure Claude Code once

If you want Claude Code to use the proxy every time, update your user-level settings file instead of launching through the helper scripts. On Windows, that file is typically `%USERPROFILE%\.claude\settings.json` — for example, `C:\Users\hwhite\.claude\settings.json`.

If your file already has an `env` block, merge these values into it:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8085",
    "ANTHROPIC_API_KEY": "any",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000"
  }
}
```

`CLAUDE_CODE_MAX_CONTEXT_TOKENS` tells Claude Code the context window of the upstream model. Without it, Claude Code assumes the default Anthropic 200K window and auto-compacts conversations too early when the upstream actually supports more (for example, GPT-5.4-mini and GPT-5.6-luna both support 1M tokens). Set this to whatever your upstream model really supports.

After that, you can run `claude` directly and it will use the proxy without the start scripts.

For config details, launch-script behavior, and protocol notes, see [`REFERENCE.md`](./REFERENCE.md).

### If the proxy is configured with `proxy_key`

The proxy accepts a `proxy_key` setting in `proxy.json` (or a `PROXY_KEY` env var) that, when set, requires every `/v1/messages` request to carry a matching `X-Proxy-Key` header. Without it, the proxy returns 401 before doing any other work. Add the secret to your `~/.claude/settings.json` so Claude Code sends it on every request:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8085",
    "ANTHROPIC_API_KEY": "any",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000",
    "ANTHROPIC_CUSTOM_HEADERS": "X-Proxy-Key: your-secret-here"
  }
}
```

`ANTHROPIC_CUSTOM_HEADERS` is a Claude Code / Anthropic SDK environment variable that injects custom HTTP headers on every API request. The value uses the standard `Name: Value` HTTP header format. For multiple headers, separate them with newlines (a single line is fine for `X-Proxy-Key`).

If you launch via the helper scripts (`scripts/start-claude-code.sh` / `.ps1`) and have `PROXY_KEY` set in your environment, the script forwards it to the child automatically — you don't need to edit `settings.json` in that case.

### Enabling the advisor tool

The proxy's advisor support is being implemented; see
[`ADVISOR_TOOL_RESEARCH.md`](./ADVISOR_TOOL_RESEARCH.md). Once `advisor.enabled`
is turned on in `proxy.json`, Claude Code also needs a **persistent** advisor
model in its user settings. Add these two keys to `~/.claude/settings.json`
(`%USERPROFILE%\.claude\settings.json` on Windows). Merge into the existing
`env` block rather than replacing it — that block already holds
`ANTHROPIC_BASE_URL` and your keys:

```json
{
  "advisorModel": "opus",
  "env": {
    "CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL": "1"
  }
}
```

Why both are required:

- `advisorModel` is the persistent setting (equivalent to `/advisor opus`).
  Unset it to turn the advisor off; `--advisor <model>` overrides it for one
  session only.
- `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1` is **required behind a
  custom `ANTHROPIC_BASE_URL`**. Without it Claude Code waits for the
  `tengu_sage_compass2` feature flag fetched from Anthropic, which a proxy
  cannot supply; with a gateway token and no API key there may be no
  credential to fetch it at all. Measured on Claude Code 2.1.322: with no
  fetched flag, `advisorModel` alone — and even `--advisor` alone — sends no
  tool declaration. This env var is an internal Claude Code knob and is not
  listed in the official env-vars reference.

To disable the advisor completely, remove `advisorModel`, run `/advisor off`,
or set `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1` (that wins over everything).

> **Status caveat.** Until `advisor.enabled` is on in `proxy.json`, setting
> this makes Claude Code declare the advisor tool and send its beta header,
> but the proxy has no advisor to run yet, so no consultation occurs. Leave
> `advisorModel` unset (or set `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1`) if you do
> not want that interim state.

Gateway note: never return a 400/422 naming `advisor_20260301` or
`advisor-tool-2026-03-01`. Claude Code treats that as "the server doesn't know
this tool", retries once without the advisor, and omits it for every later
request in the session.
