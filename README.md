# openai-to-anthropic-proxy

A small Rust proxy that lets Claude Code talk to OpenAI-compatible upstreams by translating Anthropic requests into OpenAI Responses API calls and back.

## Quick start

1. Set `UPSTREAM_BASE_URL` and `UPSTREAM_API_KEY`, or copy `proxy.toml.example` to `proxy.toml` and fill them in.
   - For a fuller walkthrough of `proxy.toml`, see [proxy.toml guide](./proxy-toml-guide.md).
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
    "ANTHROPIC_API_KEY": "any"
  }
}
```

After that, you can run `claude` directly and it will use the proxy without the start scripts.

For config details, launch-script behavior, and protocol notes, see [`REFERENCE.md`](./REFERENCE.md).
