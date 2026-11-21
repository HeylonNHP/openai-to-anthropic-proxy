# CLAUDE.md

Guidance for Claude Code (and other agents) working in this repository.

## What this is

A Rust proxy that lets **Claude Code** run against **any OpenAI-compatible
model**: it translates Anthropic Messages requests into OpenAI Responses API
calls, and the answers back. Rust edition 2024, `tokio` + `axum`.

Read [`README.md`](./README.md) for what it does and how to run it;
[`REFERENCE.md`](./REFERENCE.md) for architecture and the full reference.

## The gate — run these before you claim anything works

```bash
cargo fmt --all -- --check   # must be clean
cargo clippy --all-targets    # must be warning-free
cargo test                    # unit + e2e (953 tests as of this writing)
```

CI (`.github/workflows/ci.yml`) runs exactly these three on every push and pull
request, plus a build check on macOS and Windows. Nothing merges without them
passing.

## Conventions

### One unit-test module per file — enforced

Every unit-test module lives in **its own file**, next to the code it tests:

```text
src/translate.rs          ← the code
src/translate/tests.rs    ← the unit tests
```

The source file declares it, at the bottom:

```rust
#[cfg(test)]
mod tests;
```

- `src/thing.rs` or `src/thing/mod.rs` → module `NAME` lives in `src/thing/NAME.rs`.
- A crate root (`src/lib.rs`, `src/main.rs`) → module `NAME` lives in `src/NAME.rs`.
- The test module is a **child** module, so `use super::*;` works and private
  items stay testable.
- `tests/` is for **integration tests only** — separate crates, public API only
  (`tests/proxy_e2e.rs`). `tests/project_layout.rs` is a repo-hygiene test that
  enforces the rule above: an inline `#[cfg(test)] mod X { … }` fails the run.

There is no "is it big enough to split?" judgement. The rule is uniform.

### Clippy

`clippy.toml` sets `correctness` and `suspicious` to **deny**; `complexity`,
`perf`, `style` and `nursery` to **warn**; `restriction` to allow. Two
project-wide exceptions are set in `Cargo.toml`: `unsafe_derive_deserialize`
is denied, and `missing_const_for_fn` is allowed (it fires on `fn main`).

Do not add `#![allow(...)]` to silence a lint — fix the code, or, if the lint
is genuinely wrong here, add a narrow, commented `#[allow]` at the call site.

### No printing in library code

`clippy::print_stdout` is a no-op in the binary's `main`, but library modules
must not print to stdout — stdout is reserved for the proxy's own protocol
output. Log with `tracing` instead.

### Docs live in specific files

| File | Holds |
|---|---|
| `README.md` | Visitor-facing: what it is, how to run it, feature list |
| `REFERENCE.md` | Architecture, translation overview, config reference, out-of-scope list |
| `proxy.json-guide.md` | Every `proxy.json` key, explained |
| `proxy.json.example` | A filled-in example config (keep it in sync with the guide) |
| `src/tools/server_loop/README.md` | The shared server-tool loop |
| `src/tools/web_search/README.md` | The proxy-native web search |

Change the file that owns the topic; don't spread one fact across several.

## Conventions in the code

- **Config precedence:** environment variables override `proxy.json`. Unknown
  fields in `proxy.json` **fail fast at startup** rather than being ignored.
  Both are implemented in `src/config.rs`; the resolved struct and the warning
  list live there too.
- **Aliases:** `advisor.model` is an **explicit upstream model name** and is
  never passed through `model_aliases` / `default_model` resolution. Inbound
  client-supplied model names are the only ones alias-resolved.
- **Server tools** (`web_search`, `web_fetch`, `advisor`) share one loop in
  `src/tools/server_loop/`. They honour the client's `max_uses`, domain lists
  and similar options, and each round streams to the client as it finishes.
- **Model rejection** (upstream says "no such model" / "parameter unsupported")
  is handled in `src/model_rejection.rs`, which distinguishes a *model*
  rejection from a *tool* rejection before retrying.

## Working on this repo

- `dev` and `master` currently point at the same commit; `dev` is where new
  work lands, `master` is the stable line.
- Commit messages: `feat:`, `fix:`, `docs:`, `refactor:`, `chore:`, `perf:` —
  lowercase, imperative, one line of summary.
- Don't commit `proxy.json` or any other `proxy*.json` — they are gitignored
  and hold real credentials. `proxy.json.example` is the tracked one.
- `.claude/` is **part of the repo** (shared settings, skills, agents). Only
  per-developer state (`settings.local.json`, `history.jsonl`, `debug/`,
  `worktrees/`) is ignored — see `.gitignore`.
- `target/` is build output and is gitignored. It grows large; `cargo clean` is
  safe.

## Open work

[`ADVISOR_REMAINING_FAULTS.md`](./ADVISOR_REMAINING_FAULTS.md) records the
advisor faults still to fix, with enough detail to pick them up cold. Fix them
in the order listed and update the table as you go.
