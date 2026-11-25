//! Streaming translation: OpenAI Responses SSE events → Anthropic SSE events.
//!
//! The translator is a state machine. The client feeds it
//! `ResponsesStreamEvent`s one at a time via [`StreamTranslator::feed_event`];
//! the translator yields zero or more Anthropic events for each input.
//! When the upstream closes, the client calls
//! [`StreamTranslator::finish`] to flush any remaining state.
//!
//! ## State
//!
//! - `msg_id`, `model`: identity carried through to `message_start`.
//! - `started`: whether we've emitted `message_start` yet.
//! - `text_block`: whether we have an open text content block, plus its
//!   Anthropic content index.
//! - `tool_blocks`: per Responses `output_index`, the state of the
//!   in-flight `tool_use` Anthropic block (call_id, name, accumulated
//!   args, Anthropic content index). Stored in a `BTreeMap` so iteration
//!   in `finalize` is in `output_index` order, giving deterministic
//!   `content_block_stop` ordering for parallel tool calls.
//! - `thinking_blocks`: same shape as `tool_blocks`, but for Reasoning
//!   items — we open a `Thinking` block and emit `ThinkingDelta` events.
//! - `next_index`: next free Anthropic content index.
//! - `stop_reason`: the final stop reason, set when a terminal
//!   `Completed`/`Incomplete` event arrives.
//! - `usage`: token usage, captured from the terminal `Completed` event.
//! - `finished`: set after the terminal event fires; later events are
//!   no-ops.
//!
//! Public surface:
//! - [`StreamTranslator::new`] — build a translator.
//! - [`StreamTranslator::feed_event`] — feed one upstream event, return
//!   zero-or-more Anthropic events.
//! - [`StreamTranslator::finish`] — flush the closing
//!   `message_delta` + `message_stop` (idempotent).
//! - [`StreamTranslator::emit_error`] — terminal error path; emits an
//!   `error` event followed by `message_stop`.
//!
//! The state is private; tests assert on the emitted events, not the
//! internal state.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::anthropic::{
    ApiErrorBody, ContentBlockKind, ContentDelta, Message, MessageDeltaPayload, ServerToolUseUsage,
    StopReason, StreamEvent, Usage as AnthropicUsage, WebSearchResult, WebSearchToolResultBlock,
};
use crate::responses::{IncompleteDetails, OutputItem, ResponsesResponse, ResponsesStreamEvent};

/// Stateful translator for one streaming request.
#[derive(Debug)]
pub struct StreamTranslator {
    msg_id: String,
    model: String,
    started: bool,
    /// Original (client-supplied) tool schemas for this request. Used
    /// to repair buffered tool-call arguments before they are shipped
    /// downstream (see `emit_repaired_args`).
    registry: crate::repair::ToolSchemaRegistry,
    text_block: Option<TextBlockState>,
    /// Per upstream `output_index`. Responses uses the `output_index`
    /// from the event to identify items.
    tool_blocks: BTreeMap<u32, ToolBlockState>,
    /// Same as `tool_blocks`, for `Reasoning` output items. We open a
    /// Thinking content block and emit ThinkingDelta events.
    thinking_blocks: BTreeMap<u32, ThinkingBlockState>,
    next_index: u32,
    stop_reason: Option<StopReason>,
    /// Whether a tool call's arguments were cut short by the upstream
    /// (it ran out of output tokens mid-call). Such a call is
    /// incomplete, so the turn must not be reported as a finished one.
    truncated_tool_call: bool,
    usage: Option<AnthropicUsage>,
    /// Number of web-search calls represented in the emitted Anthropic blocks.
    web_search_count: u32,
    /// `tool_use_id`s of web searches detected in the stream whose
    /// `web_search_tool_result` block has not yet been emitted. Airia
    /// emits the `web_search_call` item *before* the message text and its
    /// `url_citation` annotations, so we defer the result block until the
    /// citations are available (message done / finalize).
    pending_web_search_ids: Vec<String>,
    /// `url_citation` (url, title) pairs collected from
    /// `response.output_text.annotation.added` events (and the completed
    /// message content) during streaming.
    collected_citations: Vec<(String, String)>,
    /// Input-side usage captured from the earliest `response.created` /
    /// `response.in_progress` event so `message_start` can report real
    /// Anthropic-shaped token counts instead of a placeholder.
    input_tokens: u32,
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
    input_usage_seen: bool,
    finished: bool,
    /// True only when a terminal completed/incomplete response event arrived.
    /// A stream that merely closes or errors is not countable.
    completed: bool,
    /// Names of function calls that must never reach the client (the proxy's
    /// own server-tool calls, see [`Self::continuation`]). Empty for an
    /// ordinary translator.
    hidden_tools: Vec<String>,
    /// `output_index`es of the hidden calls seen so far, so their argument
    /// events are dropped without the "dangling delta" warning.
    hidden_outputs: BTreeSet<u32>,
}

/// Summary stats extracted from a `StreamTranslator` after the stream ends.
/// Used by the proxy to print a user-readable response summary line.
#[derive(Debug, Clone)]
pub struct StreamStats {
    pub usage: Option<AnthropicUsage>,
    pub stop_reason: Option<StopReason>,
    pub model: String,
    pub input_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub completed: bool,
}

#[derive(Debug, Clone, Copy)]
struct TextBlockState {
    index: u32,
}

#[derive(Debug, Clone)]
struct ToolBlockState {
    index: u32,
    /// Tool call id. Set on `output_item.added`; kept for debugging and
    /// future use (e.g. the bridge might want to verify that the
    /// `id` it sees in `response.completed` matches the one it
    /// shipped downstream).
    #[allow(dead_code)]
    id: String,
    /// Tool name. Same rationale as `id`.
    #[allow(dead_code)]
    name: String,
    /// JSON-encoded arguments accumulated across the
    /// `function_call_arguments.delta` events. Buffered (NOT streamed
    /// through) so the complete blob can be repaired against the
    /// original tool schema before it is shipped downstream.
    arguments: String,
    /// Whether the (repaired) argument blob has already been emitted
    /// as an `input_json_delta`. Makes the emit path idempotent across
    /// `output_item.done`, `function_call_arguments.done`, and block
    /// close.
    emitted: bool,
}

#[derive(Debug, Clone)]
struct ThinkingBlockState {
    index: u32,
    /// Encrypted content blob from the upstream's `Reasoning` output
    /// item. Carried on the wire as the `signature` field of the
    /// Anthropic Thinking block so the client can verify the block
    /// on the next turn.
    signature: String,
}

impl StreamTranslator {
    /// Build a translator. `msg_id` is the message id to emit in
    /// `message_start.message.id`; `model` is the model name;
    /// `registry` carries the original tool schemas captured at
    /// request-translation time (empty when none were declared).
    #[must_use]
    pub fn new(msg_id: String, model: String, registry: crate::repair::ToolSchemaRegistry) -> Self {
        Self {
            msg_id,
            model,
            started: false,
            registry,
            text_block: None,
            tool_blocks: BTreeMap::new(),
            thinking_blocks: BTreeMap::new(),
            next_index: 0,
            stop_reason: None,
            truncated_tool_call: false,
            usage: None,
            web_search_count: 0,
            pending_web_search_ids: Vec::new(),
            collected_citations: Vec::new(),
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            input_usage_seen: false,
            finished: false,
            completed: false,
            hidden_tools: Vec::new(),
            hidden_outputs: BTreeSet::new(),
        }
    }

    /// Build a translator for ONE executor run inside a larger message.
    ///
    /// The proxy's server-tool loop streams every run of the executor to the
    /// client as it is generated, but the client sees a single assistant
    /// message made of several runs. So a continuation translator:
    ///
    /// * never sends `message_start` (the loop sent it, once);
    /// * numbers its content blocks from `first_index`, where the previous
    ///   run's blocks left off (read the end with [`Self::next_index`]);
    /// * drops every call to a function named in `hidden_tools`: the loop
    ///   serves those itself, so the client must not see them as `tool_use`;
    /// * must be closed with [`Self::close_blocks`], never [`Self::finish`],
    ///   which would end the whole message.
    #[must_use]
    pub fn continuation(
        msg_id: String,
        model: String,
        registry: crate::repair::ToolSchemaRegistry,
        first_index: u32,
        hidden_tools: Vec<String>,
    ) -> Self {
        let mut translator = Self::new(msg_id, model, registry);
        translator.started = true;
        translator.next_index = first_index;
        translator.hidden_tools = hidden_tools;
        translator
    }

    /// The content index the next block would get: where a following run (or
    /// a block the loop sends itself) must continue.
    #[must_use]
    pub const fn next_index(&self) -> u32 {
        self.next_index
    }

    /// Close every open content block and stop, without the closing
    /// `message_delta` / `message_stop`. Idempotent. The loop calls this when
    /// a run ends, however it ended, so the client never sees a
    /// `content_block_start` without its `content_block_stop`.
    pub fn close_blocks(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.finished {
            self.finalize(&mut events);
        }
        events
    }

    /// Feed one upstream event. Returns the Anthropic events it produces.
    /// The translator is tolerant: events that lack expected fields are
    /// treated as no-ops, and a terminal `Completed`/`Incomplete` event
    /// that arrives after content is handled correctly.
    pub fn feed_event(&mut self, event: &ResponsesStreamEvent) -> Vec<StreamEvent> {
        // Once finished, drop everything. (Terminal `Completed` events
        // can be followed by `Done` markers; we don't care.)
        if self.finished {
            return Vec::new();
        }

        let mut events = Vec::new();

        match event {
            ResponsesStreamEvent::Created { response } => {
                // First non-`Created` event opens message_start. The
                // `response.created` event carries the real upstream id,
                // model, and (often) input-side usage; capture them so the
                // downstream `message_start` reports accurate counts.
                self.capture_response_metadata(response);
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
            }
            ResponsesStreamEvent::InProgress { response } => {
                // Capture the same metadata/usage from the in-progress
                // snapshot. If we somehow haven't opened message_start yet,
                // do it now.
                self.capture_response_metadata(response);
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
            }
            ResponsesStreamEvent::OutputItemAdded { output_index, item } => {
                self.handle_output_item_added(&mut events, *output_index, item);
            }
            ResponsesStreamEvent::OutputItemDone { output_index, item } => {
                self.handle_output_item_done(&mut events, *output_index, item);
            }
            ResponsesStreamEvent::ContentPartAdded { .. }
            | ResponsesStreamEvent::ContentPartDone { .. } => {
                // No-op: the parent output_item.added already opened the
                // content block.
            }
            ResponsesStreamEvent::OutputTextDelta {
                output_index,
                delta,
                ..
            } => {
                self.emit_text_delta(&mut events, *output_index, delta);
            }
            ResponsesStreamEvent::OutputTextDone { .. } => {
                // No-op; the block closes on the next item or on stream close.
            }
            ResponsesStreamEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => {
                self.emit_tool_args_delta(&mut events, *output_index, delta);
            }
            ResponsesStreamEvent::FunctionCallArgumentsDone {
                output_index,
                arguments,
                ..
            } => {
                // The terminal `done` may carry a *complete* arguments
                // blob. If it doesn't match what we accumulated, we
                // emit a final delta so the client sees the truth.
                self.maybe_finalize_tool_args(&mut events, *output_index, arguments);
            }
            ResponsesStreamEvent::ReasoningSummaryTextDelta {
                output_index,
                delta,
                ..
            } => {
                self.emit_thinking_delta(&mut events, *output_index, delta);
            }
            ResponsesStreamEvent::ReasoningSummaryTextDone { .. } => {
                // No-op; we use output_item.done for the closing signature.
            }
            ResponsesStreamEvent::RefusalDelta {
                output_index,
                delta,
                ..
            } => {
                // Refusal text: surface as a text delta. (We always
                // open a text block for the parent Message item; if
                // the model emits both output_text and refusal parts,
                // refusal deltas land on the same text block.)
                self.emit_text_delta(&mut events, *output_index, delta);
            }
            ResponsesStreamEvent::RefusalDone { .. } => {
                // No-op.
            }
            ResponsesStreamEvent::OutputTextAnnotationAdded { annotation, .. } => {
                // Airia streams the web search's `url_citation` annotations
                // via this event (typically while a `Message` output_text is
                // streaming). Collect them so the deferred
                // `web_search_tool_result` block carries the real result
                // links to Claude Code. Previously this was a no-op, so the
                // result block was always empty and Claude Code reported
                // "No links found."
                collect_url_citations(
                    &mut self.collected_citations,
                    std::slice::from_ref(annotation),
                );
            }
            ResponsesStreamEvent::Completed { response }
            | ResponsesStreamEvent::Incomplete { response } => {
                self.completed = true;
                self.capture_terminal(&mut events, response);
            }
            ResponsesStreamEvent::Failed { response } => {
                // Surface the upstream error. The translator doesn't
                // own the bridge's terminal-error path (the bridge
                // does), but if for some reason it gets here we still
                // emit a clean error event + message_stop. We MUST
                // call `finalize` first so any open text/tool/thinking
                // block is closed with a matching `content_block_stop`
                // before the terminal `MessageStop` — otherwise the
                // Anthropic client sees a half-streamed response.
                let (kind, msg) = response.error.as_ref().map_or_else(
                    || {
                        (
                            "api_error".to_owned(),
                            "upstream response failed".to_owned(),
                        )
                    },
                    |e| {
                        crate::context_overflow::stream_error(
                            &crate::responses::code_text(e.code.as_ref()),
                            &e.message,
                        )
                    },
                );
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
                self.finalize(&mut events);
                events.push(StreamEvent::Error {
                    error: ApiErrorBody {
                        r#type: kind,
                        message: msg,
                    },
                });
                events.push(StreamEvent::MessageStop {});
            }
            ResponsesStreamEvent::Error { message, code, .. } => {
                let (kind, message) = crate::context_overflow::stream_error(
                    code.as_deref().unwrap_or_default(),
                    message,
                );
                // Top-level error event: same path — close any open
                // blocks before the terminal `MessageStop` so the
                // client sees a well-formed sequence.
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
                self.finalize(&mut events);
                events.push(StreamEvent::Error {
                    error: ApiErrorBody {
                        r#type: kind,
                        message,
                    },
                });
                events.push(StreamEvent::MessageStop {});
            }
            ResponsesStreamEvent::Unknown => {
                // Round-trip only.
            }
        }

        events
    }

    /// Mark the stream as done. Returns the closing events: a
    /// `message_delta` carrying the final stop_reason + usage, then
    /// `message_stop`, plus any `content_block_stop` events for blocks
    /// that the upstream forgot to close. Idempotent.
    pub fn finish(mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.started {
            events.push(self.build_message_start());
            self.started = true;
        }
        if !self.finished {
            self.finalize(&mut events);
        }
        // The `message_delta` is the place the client learns the
        // final stop_reason and usage. Build it here, after any
        // late-arriving usage from the terminal `Completed` event has
        // been recorded.
        let usage = self.usage.clone().or_else(|| {
            Some(AnthropicUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens_5m: 0,
                cache_creation_input_tokens_1h: 0,
                thinking_tokens: 0,
                server_tool_use: if self.web_search_count > 0 {
                    Some(ServerToolUseUsage {
                        web_search_requests: self.web_search_count,
                        ..ServerToolUseUsage::default()
                    })
                } else {
                    None
                },
                iterations: None,
            })
        });
        events.push(StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: self.stop_reason.take(),
                stop_sequence: None,
            },
            usage,
        });
        events.push(StreamEvent::MessageStop {});
        events
    }

    /// Build a final `error` event, plus any closing events. Used when
    /// the upstream sends an error SSE or when something goes wrong
    /// locally.
    pub fn emit_error(
        mut self,
        kind: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.started {
            events.push(self.build_message_start());
            self.started = true;
        }
        // Close any open content blocks before emitting the terminal
        // error, so the downstream Anthropic event sequence stays
        // well-formed (every content_block_start is matched by a
        // content_block_stop). The `finalize` helper also marks the
        // translator as finished.
        if !self.finished {
            self.finalize(&mut events);
        }
        events.push(StreamEvent::Error {
            error: ApiErrorBody {
                r#type: kind.into(),
                message: message.into(),
            },
        });
        events.push(StreamEvent::MessageStop {});
        events
    }

    /// Return the final stats from this translator: usage, stop_reason, and model.
    /// Useful for the proxy to print a summary line after a streaming response completes.
    #[must_use]
    pub fn stats(&self) -> StreamStats {
        StreamStats {
            usage: self.usage.clone(),
            stop_reason: self.stop_reason,
            model: self.model.clone(),
            input_tokens: self.input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            completed: self.completed,
        }
    }

    // ─── internal helpers ─────────────────────────────────────────────

    fn capture_response_metadata(&mut self, response: &ResponsesResponse) {
        if !response.id.is_empty() && response.id.starts_with("resp_") {
            // Translate to Anthropic-shaped `msg_...` id.
            self.msg_id = ensure_msg_prefix(&response.id);
        }
        if !response.model.is_empty() {
            self.model = response.model.clone();
        }
        if let Some(u) = &response.usage {
            self.input_tokens = u.input_tokens;
            self.cache_read_input_tokens = u
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens);
            self.cache_creation_input_tokens = u
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cache_write_tokens);
            self.input_usage_seen = true;
        }
    }

    fn build_message_start(&self) -> StreamEvent {
        let usage = if self.input_usage_seen {
            AnthropicUsage {
                // `self.input_tokens` stays gross (stats and the TUI want the
                // upstream's figure); the client gets Anthropic's net one.
                input_tokens: crate::responses::net_input_tokens(
                    self.input_tokens,
                    self.cache_read_input_tokens,
                    self.cache_creation_input_tokens,
                ),
                output_tokens: 0,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
                cache_creation_input_tokens_5m: 0,
                cache_creation_input_tokens_1h: 0,
                thinking_tokens: 0,
                server_tool_use: None,
                iterations: None,
            }
        } else {
            // Legacy placeholder: downstream expects a message_start before
            // any upstream usage is available.
            AnthropicUsage {
                input_tokens: 0,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens_5m: 0,
                cache_creation_input_tokens_1h: 0,
                thinking_tokens: 0,
                server_tool_use: None,
                iterations: None,
            }
        };
        StreamEvent::MessageStart {
            message: Message {
                id: self.msg_id.clone(),
                r#type: "message",
                role: "assistant",
                content: Vec::new(),
                model: self.model.clone(),
                stop_reason: None,
                stop_sequence: None,
                usage,
            },
        }
    }

    fn handle_output_item_added(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        item: &OutputItem,
    ) {
        tracing::debug!(output_index, ?item, "handle_output_item_added");
        if !self.started {
            events.push(self.build_message_start());
            self.started = true;
        }
        match item {
            OutputItem::Message { .. } => {
                // Open a text content block. A text message after
                // tool calls / reasoning implies those are done.
                // Close them in BTreeMap order so the indices stay
                // contiguous.
                self.close_open_tool_blocks(events);
                self.close_open_thinking_blocks(events);

                // NOTE: We deliberately do NOT scan the message's
                // `content` for `url_citation` annotations here. In the
                // streaming path, `content` arrives empty at
                // `output_item.added` time — the text and annotations
                // come later via `output_text.delta` /
                // `output_text.annotation.added` events. Scanning here
                // never fires, so we rely on the upstream
                // `WebSearchCall` / `FunctionCall { name: "WebSearch" }`
                // detection (handled in their own match arms) to inject
                // the `server_tool_use` + `web_search_tool_result`
                // blocks. The non-streaming path (`build_content_blocks`
                // in translate.rs) still scans annotations as a
                // fallback for fully-populated responses.
                if self.text_block.is_none() {
                    let index = self.allocate_index();
                    self.text_block = Some(TextBlockState { index });
                    events.push(StreamEvent::ContentBlockStart {
                        index,
                        content_block: ContentBlockKind::Text {
                            text: String::new(),
                        },
                    });
                }
            }
            OutputItem::WebSearchCall { .. } => {
                // The upstream used its built-in `web_search` tool.
                // Emit a `server_tool_use` block so Claude Code's search
                // counter/progress registers the search immediately, and
                // defer the `web_search_tool_result` block until the
                // `url_citation` annotations arrive (message done). The
                // actual result links come back as `url_citation`
                // annotations on the message text, which arrive AFTER
                // this item in the stream.
                self.emit_server_tool_use(events);
            }
            OutputItem::FunctionCall { name, .. }
                if self.hidden_tools.iter().any(|hidden| hidden == name) =>
            {
                // One of the loop's own tools: the client never sees it. No
                // block, no index; its argument events are dropped.
                self.hidden_outputs.insert(output_index);
            }
            OutputItem::FunctionCall { call_id, name, .. } => {
                // The model may invoke web search as a function tool
                // (name == "WebSearch" or "web_search") instead of via
                // a `web_search_call` output item. Detect that here and
                // emit the `server_tool_use` block FIRST, then fall
                // through to the normal tool_use handling. The matching
                // `web_search_tool_result` block is emitted later when
                // annotations are available.
                if crate::translate::is_web_search_function_call(name, &self.registry) {
                    self.emit_server_tool_use(events);
                }

                // Close any open text block.
                if let Some(text) = self.text_block.take() {
                    events.push(StreamEvent::ContentBlockStop { index: text.index });
                }
                // Close any open thinking block (a tool call
                // interleaves with reasoning on a different
                // output_index, but a single response can have
                // multiple parallel tool calls; we must NOT close
                // existing tool blocks here).
                self.close_open_thinking_blocks(events);
                // Allocate a fresh Anthropic index for this tool.
                let index = self.allocate_index();
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::ToolUse {
                        id: call_id.clone(),
                        name: name.clone(),
                        input: Value::Object(Default::default()),
                    },
                });
                self.tool_blocks.insert(
                    output_index,
                    ToolBlockState {
                        index,
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        emitted: false,
                    },
                );
            }
            OutputItem::Reasoning { .. } => {
                // Close any open text block.
                if let Some(text) = self.text_block.take() {
                    events.push(StreamEvent::ContentBlockStop { index: text.index });
                }
                self.close_open_tool_blocks(events);
                self.close_open_thinking_blocks(events);
                let index = self.allocate_index();
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                });
                self.thinking_blocks.insert(
                    output_index,
                    ThinkingBlockState {
                        index,
                        signature: String::new(),
                    },
                );
            }
            OutputItem::Unknown => {
                // Round-trip only.
            }
        }
    }

    fn handle_output_item_done(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        item: &OutputItem,
    ) {
        match item {
            OutputItem::FunctionCall { arguments, .. } => {
                // The terminal `output_item.done` for a FunctionCall
                // may carry a *complete* arguments blob; adopt it and
                // emit the repaired arguments exactly once.
                self.maybe_finalize_tool_args(events, output_index, arguments);
            }
            OutputItem::Reasoning {
                encrypted_content, ..
            } => {
                if self.thinking_blocks.contains_key(&output_index) && !encrypted_content.is_empty()
                {
                    // The signature blob is opaque to the client but
                    // must be carried on the wire. We update internal
                    // state but don't emit a delta; the Anthropic
                    // Thinking block has no signature-delta event in
                    // the current wire format, and the client renders
                    // the block based on its `thinking` text.
                    if let Some(state) = self.thinking_blocks.get_mut(&output_index) {
                        state.signature = encrypted_content.clone();
                    }
                }
            }
            OutputItem::Message { content, .. } => {
                // The message is done; its `output_text` parts carry the
                // final `url_citation` annotations (Airia includes them on
                // the completed content even when the intermediate
                // `content_part.added`/`done` snapshots omit them). Merge
                // them into the collected citations; the deferred
                // `web_search_tool_result` blocks are emitted in
                // `finalize`/`finish`, after the text block is closed, so
                // the Anthropic content-block sequence stays well-ordered.
                for part in content {
                    if let crate::responses::OutputContentPart::OutputText { annotations, .. } =
                        part
                    {
                        collect_url_citations(&mut self.collected_citations, annotations);
                    }
                }
            }
            OutputItem::Unknown | OutputItem::WebSearchCall { .. } => {
                // Nothing to do; the next item or stream close will
                // close the text block.
            }
        }
    }

    fn emit_text_delta(&mut self, events: &mut Vec<StreamEvent>, _output_index: u32, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(text_state) = self.text_block else {
            // Dangling text delta with no open text block — the parent
            // Message item wasn't opened. Open one now so the client
            // sees the text.
            self.close_open_tool_blocks(events);
            self.close_open_thinking_blocks(events);
            let index = self.allocate_index();
            self.text_block = Some(TextBlockState { index });
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Text {
                    text: String::new(),
                },
            });
            let Some(s) = self.text_block else { return };
            events.push(StreamEvent::ContentBlockDelta {
                index: s.index,
                delta: ContentDelta::TextDelta {
                    text: text.to_owned(),
                },
            });
            return;
        };
        events.push(StreamEvent::ContentBlockDelta {
            index: text_state.index,
            delta: ContentDelta::TextDelta {
                text: text.to_owned(),
            },
        });
    }

    /// Accumulate a tool-argument delta *without* shipping it. The
    /// full argument blob is repaired against the original tool schema
    /// and emitted once, when the tool block closes (see
    /// [`Self::emit_repaired_args`]). Buffering is what makes repair
    /// possible at all: the sanitizer's mutations (null sentinels,
    /// string-encoded JSON, fabricated optional values) can only be
    /// inverted on complete JSON.
    fn emit_tool_args_delta(
        &mut self,
        _events: &mut Vec<StreamEvent>,
        output_index: u32,
        args: &str,
    ) {
        if args.is_empty() || self.hidden_outputs.contains(&output_index) {
            return;
        }
        let Some(state) = self.tool_blocks.get_mut(&output_index) else {
            // Dangling delta with no parent tool_use block — drop.
            tracing::warn!(output_index, "dangling function_call_arguments delta");
            return;
        };
        state.arguments.push_str(args);
    }

    /// The terminal `done` event carries the upstream's view of the
    /// complete arguments. Adopt it when non-empty (it is
    /// authoritative over a possibly-truncated delta accumulation),
    /// then emit the repaired blob.
    fn maybe_finalize_tool_args(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        arguments: &str,
    ) {
        let Some(state) = self.tool_blocks.get_mut(&output_index) else {
            return;
        };
        if !arguments.is_empty() && state.arguments != arguments {
            if !state.arguments.is_empty() && arguments.strip_prefix(&state.arguments).is_none() {
                // Total mismatch between delta accumulation and the
                // done blob. The old code re-emitted the whole done
                // blob *on top of* the deltas, producing concatenated
                // garbage on the client. Trust the authoritative done
                // blob instead.
                tracing::warn!(
                    output_index,
                    "function_call done arguments diverge from accumulated deltas; using done blob"
                );
            }
            // Extension of what we saw, first sight of the blob, or a
            // divergence resolved in favour of the authoritative done
            // blob.
            state.arguments = arguments.to_owned();
        }
        self.emit_repaired_args(events, output_index);
    }

    /// Emit the (repaired) argument JSON for a tool block as a single
    /// `input_json_delta`, exactly once. Idempotent: a block whose
    /// arguments were already emitted is a no-op.
    fn emit_repaired_args(&mut self, events: &mut Vec<StreamEvent>, output_index: u32) {
        let Some(state) = self.tool_blocks.get_mut(&output_index) else {
            return;
        };
        if state.emitted {
            return;
        }
        state.emitted = true;
        let raw = std::mem::take(&mut state.arguments);
        let repaired = repair_arguments(&self.registry, &state.name, &raw);
        if repaired.truncated {
            // The upstream ran out of output tokens mid-call. The call
            // is incomplete, so the turn must not be reported as a
            // finished one — see `capture_terminal`.
            self.truncated_tool_call = true;
        }
        if repaired.json.is_empty() {
            return;
        }
        events.push(StreamEvent::ContentBlockDelta {
            index: state.index,
            delta: ContentDelta::InputJsonDelta {
                partial_json: repaired.json,
            },
        });
    }

    fn emit_thinking_delta(&self, events: &mut Vec<StreamEvent>, output_index: u32, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(state) = self.thinking_blocks.get(&output_index) else {
            tracing::warn!(output_index, "dangling reasoning_summary_text delta");
            return;
        };
        let index = state.index;
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::ThinkingDelta {
                thinking: text.to_owned(),
            },
        });
    }

    fn capture_terminal(&mut self, events: &mut Vec<StreamEvent>, response: &ResponsesResponse) {
        // Capture usage first so `finish()` includes it in message_delta.
        // The terminal snapshot is authoritative: created/in-progress
        // snapshots may omit usage or contain only provisional counts.
        if let Some(usage) = &response.usage {
            self.input_tokens = usage.input_tokens;
            self.cache_read_input_tokens = usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens);
            self.cache_creation_input_tokens = usage
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cache_write_tokens);
            self.input_usage_seen = true;
            let mut u = map_usage(usage);
            if self.web_search_count > 0 {
                u.server_tool_use = Some(ServerToolUseUsage {
                    web_search_requests: self.web_search_count,
                    ..ServerToolUseUsage::default()
                });
            }
            self.usage = Some(u);
        }
        // Map the response's status to a stop reason.
        let has_tool_use = response
            .output
            .iter()
            .any(|i| matches!(i, OutputItem::FunctionCall { .. }));
        let status_stop =
            map_status_to_stop_reason(&response.status, response.incomplete_details.as_ref());
        self.stop_reason = Some(
            if status_stop == Some(StopReason::EndTurn) && has_tool_use {
                StopReason::ToolUse
            } else {
                status_stop.unwrap_or(StopReason::EndTurn)
            },
        );
        // Close open blocks so the client sees a well-formed sequence
        // before the message_delta. This is also where the arguments of
        // a tool block that was never closed explicitly are flushed —
        // and so where a truncated argument blob is noticed.
        self.finalize(events);
        // A tool call the upstream cut short is an *incomplete* call:
        // the client must not run it. Anthropic reports `max_tokens`
        // for one, and that is the signal the client keys on to retry
        // the turn with a bigger budget.
        //
        // Only a reply that claims to be complete is overridden. A
        // reply the upstream marked `incomplete` carries its own
        // reason, and that reason wins — `max_output_tokens` already
        // maps to `max_tokens`, and `content_filter` maps to
        // `refusal`, which this must not overwrite.
        if self.truncated_tool_call
            && response.status == "completed"
            && matches!(
                self.stop_reason,
                Some(StopReason::EndTurn) | Some(StopReason::ToolUse)
            )
        {
            self.stop_reason = Some(StopReason::MaxTokens);
        }
    }

    fn close_open_tool_blocks(&mut self, events: &mut Vec<StreamEvent>) {
        if self.tool_blocks.is_empty() {
            return;
        }
        // Flush any tool block whose arguments were never emitted
        // (e.g. an upstream that sends deltas but no terminal done
        // event), then close it. Per block: args delta, then stop, in
        // BTreeMap (output_index) order so parallel calls close
        // deterministically.
        let ordered: Vec<u32> = self.tool_blocks.keys().cloned().collect();
        for output_index in ordered {
            self.emit_repaired_args(events, output_index);
            if let Some(state) = self.tool_blocks.get(&output_index) {
                events.push(StreamEvent::ContentBlockStop { index: state.index });
            }
        }
        self.tool_blocks.clear();
    }

    fn close_open_thinking_blocks(&mut self, events: &mut Vec<StreamEvent>) {
        if self.thinking_blocks.is_empty() {
            return;
        }
        // Snapshot the entries so we can mutate `self.thinking_blocks`
        // while iterating. Each block is closed by emitting the
        // captured `signature` as a `SignatureDelta` (when non-empty)
        // followed by `ContentBlockStop`. Anthropic's wire format
        // requires the signature delta before the stop, otherwise the
        // client can't verify the thinking block on the next turn.
        let snapshot: Vec<(u32, String)> = self
            .thinking_blocks
            .values()
            .map(|state| (state.index, state.signature.clone()))
            .collect();
        for (block_index, signature) in snapshot {
            if !signature.is_empty() {
                events.push(StreamEvent::ContentBlockDelta {
                    index: block_index,
                    delta: ContentDelta::SignatureDelta { signature },
                });
            }
            events.push(StreamEvent::ContentBlockStop { index: block_index });
        }
        self.thinking_blocks.clear();
    }

    fn finalize(&mut self, events: &mut Vec<StreamEvent>) {
        if let Some(text) = self.text_block.take() {
            events.push(StreamEvent::ContentBlockStop { index: text.index });
        }
        self.close_open_tool_blocks(events);
        self.close_open_thinking_blocks(events);
        // Safety net: if the stream ends (e.g. a tool_use-only turn, or an
        // incomplete/failed response) before the message content that would
        // carry citations, still emit the deferred `web_search_tool_result`
        // blocks so every `server_tool_use` has a matching result block.
        self.emit_pending_web_search_results(events);
        // `message_delta` is emitted by `finish()` instead, so any
        // usage that arrives AFTER `finalize` (a real Responses
        // behaviour, e.g. a late `Completed`) is reflected in the
        // final delta.
        self.finished = true;
    }

    fn allocate_index(&mut self) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        idx
    }

    /// Inject a `server_tool_use` (start+stop) and `web_search_tool_result`
    /// (start+stop) content block pair into the Anthropic event stream.
    /// Called when the upstream signals a web search via a
    /// `web_search_call` output item or a `FunctionCall` whose name is
    /// `WebSearch`/`web_search`. Increments `web_search_count` so the
    /// terminal `message_delta` includes the right `server_tool_use`
    /// usage.
    ///
    /// `citations` is no longer used; the result block is populated
    /// later by `emit_pending_web_search_results` once `url_citation`
    /// the result block with. When `None`, an empty result list is used —
    /// sufficient for Claude Code's search counter to register the
    /// search, even without the actual URLs/titles (those arrive later
    /// as `url_citation` annotations on the text, which the client
    /// renders inline rather than from this block).
    fn emit_server_tool_use(&mut self, events: &mut Vec<StreamEvent>) {
        self.web_search_count = self.web_search_count.saturating_add(1);
        let tool_use_id = format!("stoolu_web_search_{:02}", self.web_search_count);
        self.pending_web_search_ids.push(tool_use_id.clone());

        let index = self.allocate_index();
        events.push(StreamEvent::ContentBlockStart {
            index,
            content_block: ContentBlockKind::ServerToolUse {
                id: tool_use_id,
                name: "web_search".to_string(),
                input: serde_json::json!({"query": "Web search"}),
            },
        });
        events.push(StreamEvent::ContentBlockStop { index });
    }

    fn emit_pending_web_search_results(&mut self, events: &mut Vec<StreamEvent>) {
        let ids = std::mem::take(&mut self.pending_web_search_ids);
        let citations = std::mem::take(&mut self.collected_citations);
        for (i, tool_use_id) in ids.into_iter().enumerate() {
            let result_content: Vec<WebSearchResult> = if i == 0 {
                citations
                    .iter()
                    .map(|(url, title)| WebSearchResult {
                        url: url.clone(),
                        title: title.clone(),
                        encrypted_content: String::new(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let index = self.allocate_index();
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::WebSearchToolResult {
                    block: WebSearchToolResultBlock {
                        tool_use_id,
                        content: result_content,
                    },
                },
            });
            events.push(StreamEvent::ContentBlockStop { index });
        }
    }
}

/// Extract `url_citation` (url, title) pairs from annotation JSON values,
/// as emitted by `response.output_text.annotation.added` and carried on
/// `OutputContentPart::OutputText.annotations`. Airia marks grounded
/// claims with `{ "type": "url_citation", "url": ..., "title": ... }`.
fn collect_url_citations(target: &mut Vec<(String, String)>, annotations: &[Value]) {
    for ann in annotations {
        if ann.get("type").and_then(|v| v.as_str()) == Some("url_citation")
            && let (Some(url), Some(title)) = (
                ann.get("url").and_then(|v| v.as_str()),
                ann.get("title").and_then(|v| v.as_str()),
            )
        {
            let citation = (url.to_string(), title.to_string());
            if !target.iter().any(|existing| existing.0 == citation.0) {
                target.push(citation);
            }
        }
    }
}

/// The JSON text to ship for a tool call, and whether the upstream cut
/// the arguments short.
struct RepairedArguments {
    /// The JSON text to ship downstream.
    json: String,
    /// Whether the arguments were cut short — the call is incomplete.
    truncated: bool,
}

/// Parse + repair a raw upstream tool-argument blob against the original
/// schema for `tool` (when registered). Returns the JSON text to ship
/// downstream. Empty/whitespace input becomes `{}` (Anthropic
/// `tool_use.input` must be an object); input that is not JSON at all is
/// logged and shipped as `{}` rather than as a synthetic `_raw_arguments`
/// key the client's strict schema would reject outright. Input the
/// upstream cut short keeps the parameters that were finished — the one
/// that was in flight is dropped, as Anthropic's own API drops it.
fn repair_arguments(
    registry: &crate::repair::ToolSchemaRegistry,
    tool: &str,
    raw: &str,
) -> RepairedArguments {
    if raw.trim().is_empty() {
        return RepairedArguments {
            json: "{}".to_owned(),
            truncated: false,
        };
    }
    let (parsed, truncated) = match crate::repair::read_arguments(raw) {
        crate::repair::Arguments::Whole(value) => (value, false),
        crate::repair::Arguments::Truncated(value) => {
            tracing::warn!(
                tool = %tool,
                "upstream tool arguments were cut short; keeping the parameters that were finished"
            );
            (value, true)
        }
        crate::repair::Arguments::Malformed => {
            tracing::warn!(
                tool = %tool,
                "upstream tool arguments are not valid JSON; shipping empty input"
            );
            (Value::Object(Default::default()), false)
        }
    };
    let Some(original) = registry.get(tool) else {
        // No original schema (tool not declared inbound, e.g. a
        // server-injected call): pass through untouched.
        return RepairedArguments {
            json: serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_owned()),
            truncated,
        };
    };
    let repaired = crate::repair::repair(parsed, original);
    if let Some(violation) = crate::repair::validate(&repaired, original) {
        tracing::warn!(
            tool = %tool,
            violation = %violation,
            "tool input still violates the original schema after repair"
        );
    }
    RepairedArguments {
        json: serde_json::to_string(&repaired).unwrap_or_else(|_| "{}".to_owned()),
        truncated,
    }
}

fn map_usage(u: &crate::responses::ResponsesUsage) -> AnthropicUsage {
    AnthropicUsage {
        input_tokens: u.net_input_tokens(),
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u
            .input_tokens_details
            .as_ref()
            .map_or(0, |d| d.cache_write_tokens),
        cache_read_input_tokens: u
            .input_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens),
        cache_creation_input_tokens_5m: 0,
        cache_creation_input_tokens_1h: 0,
        thinking_tokens: u
            .output_tokens_details
            .as_ref()
            .map_or(0, |d| d.reasoning_tokens),
        server_tool_use: None,
        iterations: None,
    }
}

fn map_status_to_stop_reason(
    status: &str,
    incomplete: Option<&IncompleteDetails>,
) -> Option<StopReason> {
    match status {
        "completed" | "cancelled" => Some(StopReason::EndTurn),
        "incomplete" => Some(match incomplete.map(|d| d.reason.as_str()).unwrap_or("") {
            "max_output_tokens" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        }),
        "failed" => Some(StopReason::EndTurn),
        _ => {
            tracing::warn!(status = %status, "unknown Responses status; defaulting to EndTurn");
            Some(StopReason::EndTurn)
        }
    }
}

fn ensure_msg_prefix(id: &str) -> String {
    if id.starts_with("msg_") {
        id.to_owned()
    } else {
        format!("msg_{id}")
    }
}

#[cfg(test)]
mod tests;
