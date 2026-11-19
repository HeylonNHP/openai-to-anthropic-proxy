# `server_loop` — the loop behind every proxy-served server tool

Anthropic's server-side tools (`web_search`, `web_fetch`, `advisor`) run
**inside the API**: the model calls one, the server satisfies the call, and the
model carries on, all in one request. The upstream this proxy talks to has no
such tools, so the proxy stands in for the server. This module is that
stand-in, written **once** for all of them.

A request may declare any mix of the tools, and the model can use all of them
in one turn.

## Contents

- [How a turn runs](#how-a-turn-runs)
- [What the client receives](#what-the-client-receives)
- [What the operator sees](#what-the-operator-sees)
- [The round limit](#the-round-limit)
- [Streaming](#streaming)
- [Failure handling](#failure-handling)
- [Adding or removing a tool](#adding-or-removing-a-tool)
- [Known limitations](#known-limitations)
- [Module map](#module-map)

## How a turn runs

```text
build_outbound   every tool that is enabled AND declared adds its internal
                 function tool; the request is forced to stream: false
      │
      ▼
 ┌─▶ run the executor (the upstream model)         Attempt::execute
 │        │
 │        ├─ no call to any server tool ──────────▶ done
 │        │
 │        └─ calls, in the order the model made them
 │               ├─ each tool serves its own call   ServerTool::serve
 │               ├─ the client gets the result blocks
 │               ├─ the output is injected as function_call_output
 │               └─ round complete
 └──────────────────────────────────────────────────┘
```

* **Internal tool names.** The model calls `proxy_web_search`, `web_fetch` and
  `advisor`. The names cannot collide, so all of them can be offered at once.
  (`web_search` is deliberately not called `web_search`: the translator treats
  that name as the upstream's built-in search.) The client never sees these
  calls; it sees `server_tool_use` and result blocks.
* **Every call in a response is served**, in order, not just the first.
* **Calls to different tools are independent.** A tool that has hit its own
  limit (the advisor's `max_uses`) stops being served, and the others carry on.

## What the client receives

One assistant message containing, in the order they happened: the model's
intermediate text, each tool's `server_tool_use` / result pair, any client
`tool_use` the model asked for, and the final answer.

* `stop_reason` is `end_turn`, or `tool_use` if a *client* tool is pending.
  A terminal reason from the model (`max_tokens`, `refusal`, `stop_sequence`)
  is kept. `pause_turn` is only sent when [enabled](#the-round-limit).
* `usage` is the LAST executor response's, plus what each tool adds:
  `server_tool_use.web_search_requests` / `web_fetch_requests` (a zero count is
  left out) and, for the advisor, `iterations[]`.

## What the operator sees

Every served call becomes one row in the TUI's RECENT REQUESTS pane. The
client never sees the call itself — it sees a `server_tool_use` block — so
this is the only place it shows up:

```text
↳  web_fetch  |  https://example.com/docs  |  ok  |  1.24s  |  18.43k chars
```

| column | what it holds |
|---|---|
| tool | the client-facing tool name (`web_fetch`, `web_search`, `advisor`) |
| subject | what the call acted on: a URL, a query, a model |
| outcome | `ok`, `error`, `refused` or `partial` |
| elapsed | how long the call took |
| detail | the error code, or the size of what came back |

Every served call has one: a tool that has nothing much to say still says
what it did, so no call can go unseen.

## The round limit

A **round** is one executor run whose calls were served. Every round costs an
upstream call, so a request runs at most `server_tools.max_rounds` of them
(**10** by default, like Anthropic's own loop). `0` means no limit, as it does
for every other limit in `proxy.json` — and it is needed to make an unlimited
`advisor.max_uses` or `web_fetch` truly unlimited.

At the limit, one of two things happens, chosen by `server_tools.pause_turn`:

| `pause_turn` | at the limit |
|---|---|
| `false` (default) | The model's next call is **refused** and it is told the limit was reached. It gets one last run to answer with what it has. The client sees an ordinary finished turn. |
| `true` | The response is returned **as it stands** with `stop_reason: "pause_turn"` and no further upstream call is made, exactly as Anthropic's API does. The client resends the conversation to carry on. |

**Why `pause_turn` is off by default.** The Anthropic SDKs resume a paused
turn; **Claude Code does not**. (Checked by searching its binary: nothing in
its own turn loop reacts to `pause_turn`, so a paused turn would simply end,
with no answer.) Turn it on for clients that resume.

A client tool pending in the paused round still wins: the stop reason is
`tool_use`, since the client must run the tool, and sending its result resumes
the turn. A resumed conversation (the paused assistant message as the last
message) is accepted; the earlier results are put back in front of the model.

## Streaming

A streaming client is sent each round as it finishes instead of the whole turn
at the end:

```text
first run done ─▶ message_start
each round     ─▶ content blocks (the model's text, server_tool_use, result)
(quiet)        ─▶ ping every 10 s
last run done  ─▶ final blocks, message_delta, message_stop
```

* Each executor run is still fetched **whole**: a function call cannot be
  served until it is complete. So a turn where the model simply answers, with
  no tool call, is still delivered in one burst. Streaming a plain answer token
  by token would need the call detection to work on a live upstream stream;
  that is not built.
* The stream only starts once the **first** run has succeeded. A failure there
  is an ordinary HTTP error with its real status, which clients retry on.
* The loop runs in its own task, talking to the response over a channel. A
  client that disconnects stops the loop before its next upstream call.

## Failure handling

A tool never fails the client request. It turns its own failure into an error
result block for the client and error text for the model, and the turn goes on.

Only the executor can fail the request:

* **The first run fails:** an ordinary upstream error (HTTP status and body),
  after each tool has had a chance to scrub it (the advisor removes its beta
  and tool names, which would otherwise make Claude Code switch the feature
  off for the whole process).
* **A later run fails:** the work already done is returned, not an error. Each
  tool may add to it (`on_rerun_failure`); the advisor appends an
  `unavailable` pair if a consultation was pending.

## Adding or removing a tool

A tool is a struct implementing [`handler::ServerTool`](handler.rs):

| method | purpose |
|---|---|
| `executor_name` | the internal function name the model calls |
| `serve` | serve one call: the client's blocks and the model's output text |
| `on_executor_response` | *(optional)* see every executor response (the advisor's `iterations[]`) |
| `exhausted` | *(optional)* it has refused a call: stop serving it, so a model that ignores the refusal cannot keep the turn alive |
| `on_rerun_failure` | *(optional)* close any half-finished pair when a later run fails |
| `contribute_usage` | *(optional)* add to the response's `usage` |
| `scrub_error` | *(optional)* clean a first-run error before the client sees it |

What `serve` returns also carries a one-line `summary` of the call (see
[What the operator sees](#what-the-operator-sees)). A new tool says what it
did in it; the loop writes the row and times the call.

To **add** one: write the module (a `prepare` that adds its function tool, an
`active` predicate, and the `ServerTool`), call its `prepare` in
`proxy::build_outbound`, and add an `if its::active(..)` line in
[`handlers`](mod.rs). Nothing else in the loop changes.

To **remove** one: delete its module and those two lines.

## Known limitations

* **Plain answers are not streamed token by token** (see [Streaming](#streaming)).
* **Usage is the last executor run's**, not the sum of the rounds (the advisor's
  tokens are in `iterations[]`; compaction tokens are not recorded).
* **Internal calls use the model's own function-calling**, not Anthropic's
  server tools, so the model's behaviour (when it chooses to search, how it
  writes queries) is the upstream model's, not Claude's.

## Module map

| file | role |
|---|---|
| `mod.rs` | `handlers`: which tools serve this request |
| `handler.rs` | the `ServerTool` interface |
| `run.rs` | the loop, the round limit, the streaming sink |
| `render.rs` | assembling the final message (`assemble`) and the streaming tail |
| `stream.rs` | the streaming response: task, channel, pings, SSE events |
| `summary.rs` | what the operator sees of one served call |
| `*/tests.rs` | unit tests, one file per module (enforced by `tests/project_layout.rs`) |
