# Proxy Bug Hunt Results

This document records the top correctness bugs found in the current proxy diff. It is intended as a handoff for a future agent that will implement the fixes.

## Summary

The current proxy changes introduce five correctness issues that can affect startup, retry behavior, and streaming error handling.

## 1) `proxy.toml` can no longer express a default-only `[reasoning]` section

**Files:** `src/config.rs`, `proxy.toml.example`

**Bug:** `TomlReasoningConfig` requires a `models` map during deserialization, even though the example config shows `[reasoning]` with only `default = "none"` as a valid setup.

**Why it matters:** A user who copies the example config and only enables the default reasoning setting will get a startup failure when the TOML file is parsed.

**Patch direction:**
- Add `#[serde(default)]` to `models` or to the whole `TomlReasoningConfig`.
- Add a regression test that parses a `[reasoning]` table with only `default`.

---

## 2) Fallback retries reuse the original `reasoning_effort`

**File:** `src/proxy.rs`

**Bug:** When the upstream rejects a model and `default_model` is configured, the proxy updates `outbound.model` and retries, but it does not recompute the associated reasoning configuration for the fallback model.

**Why it matters:** The retry may carry a reasoning setting that was chosen for the rejected model, not the fallback model. That can cause the second request to fail again or behave differently than intended.

**Patch direction:**
- After `outbound.model = fallback.to_owned()`, recompute the reasoning field from the new model before sending the retry.
- Prefer rebuilding the outbound request per attempt if that is cleaner.

---

## 3) Streaming requests never use the `default_model` fallback

**File:** `src/proxy.rs`

**Bug:** The retry condition is gated by `!outbound.stream`, so every `stream: true` request skips the `default_model` safety net.

**Why it matters:** The proxy already decides whether to retry before it starts emitting downstream SSE. That means some streaming requests could be retried safely, but currently they fail immediately instead.

**Patch direction:**
- Move the fallback decision before the stream/non-stream split, or
- Remove the `!outbound.stream` guard if the retry still happens before any SSE is written.

---

## 4) Model support detection ignores structured error codes

**File:** `src/proxy.rs`

**Bug:** `is_model_not_supported()` only checks the response body text for a few phrases. It does not inspect structured error fields such as `error.code` / `code`.

**Why it matters:** Some upstreams may return a body like `{"code":"model_not_supported", "message":"..."}` with generic wording that does not match the phrase list. In that case the proxy will not retry even though the error is clearly a model rejection.

**Patch direction:**
- Parse the upstream error envelope when possible.
- Treat codes like `model_not_supported` and `model_not_found` as retryable.
- Keep the phrase matcher as a fallback for providers that only expose text.

---

## 5) Streaming error path can leave content blocks open

**File:** `src/stream.rs`

**Bug:** `StreamTranslator::emit_error()` emits `error` and `message_stop`, but it does not close any open `content_block_start` blocks first.

**Why it matters:** If the upstream fails after the proxy has already opened a text/tool/thinking block, the downstream Anthropic event sequence can become malformed because the client sees a terminal error without matching `content_block_stop` events.

**Patch direction:**
- Reuse the same block-closing logic used by `finish()` / `finalize()` before emitting the error.
- Make sure the error path preserves a well-formed Anthropic event sequence.

---

## Recommended implementation order

1. Fix the TOML parsing bug in `src/config.rs`.
2. Fix the fallback retry logic in `src/proxy.rs` so it recomputes reasoning and applies to streaming where safe.
3. Improve model rejection detection to use structured error codes.
4. Close open stream blocks before emitting terminal errors in `src/stream.rs`.
5. Add regression tests for each case.

## Useful verification ideas

- Parse a `proxy.toml` that contains only `[reasoning] default = "none"`.
- Trigger an upstream model rejection and confirm the retry body changes both `model` and the reasoning setting.
- Run one streaming and one non-streaming fallback case.
- Simulate a streaming upstream failure after a text block has started and confirm the SSE sequence is closed cleanly.
