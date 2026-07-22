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
