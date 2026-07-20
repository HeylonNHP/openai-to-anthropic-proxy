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

use std::collections::BTreeMap;

use serde_json::Value;

use crate::anthropic::{
    ApiErrorBody, ContentBlockKind, ContentDelta, Message, MessageDeltaPayload, StopReason,
    StreamEvent, Usage as AnthropicUsage,
};
use crate::responses::{IncompleteDetails, OutputItem, ResponsesResponse, ResponsesStreamEvent};

/// Stateful translator for one streaming request.
#[derive(Debug)]
pub struct StreamTranslator {
    msg_id: String,
    model: String,
    started: bool,
    text_block: Option<TextBlockState>,
    /// Per upstream `output_index`. Responses uses the `output_index`
    /// from the event to identify items.
    tool_blocks: BTreeMap<u32, ToolBlockState>,
    /// Same as `tool_blocks`, for `Reasoning` output items. We open a
    /// Thinking content block and emit ThinkingDelta events.
    thinking_blocks: BTreeMap<u32, ThinkingBlockState>,
    next_index: u32,
    stop_reason: Option<StopReason>,
    usage: Option<AnthropicUsage>,
    /// Input-side usage captured from the earliest `response.created` /
    /// `response.in_progress` event so `message_start` can report real
    /// Anthropic-shaped token counts instead of a placeholder.
    input_tokens: u32,
    cache_read_input_tokens: u32,
    cache_creation_input_tokens: u32,
    input_usage_seen: bool,
    finished: bool,
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
    /// `function_call_arguments.delta` events. Tracked so we can emit a
    /// final `input_json_delta` if the upstream's terminal `done` event
    /// sends a complete `arguments` we haven't seen before.
    arguments: String,
}

#[derive(Debug, Clone)]
struct ThinkingBlockState {
    index: u32,
    /// Concatenated summary text from `reasoning_summary_text.delta`
    /// events. Set on the block's `signature` field at `output_item.done`
    /// from `encrypted_content`.
    thinking: String,
    signature: String,
}

impl StreamTranslator {
    /// Build a translator. `msg_id` is the message id to emit in
    /// `message_start.message.id`; `model` is the model name.
    #[must_use]
    pub fn new(msg_id: String, model: String) -> Self {
        Self {
            msg_id,
            model,
            started: false,
            text_block: None,
            tool_blocks: BTreeMap::new(),
            thinking_blocks: BTreeMap::new(),
            next_index: 0,
            stop_reason: None,
            usage: None,
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            input_usage_seen: false,
            finished: false,
        }
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
            ResponsesStreamEvent::OutputTextAnnotationAdded { .. } => {
                // No-op; we don't propagate annotations today.
            }
            ResponsesStreamEvent::Completed { response }
            | ResponsesStreamEvent::Incomplete { response } => {
                self.capture_terminal(&mut events, response);
            }
            ResponsesStreamEvent::Failed { response } => {
                // Surface the upstream error. The translator doesn't
                // own the bridge's terminal-error path (the bridge
                // does), but if for some reason it gets here we still
                // emit a clean error event + message_stop.
                let msg = response
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "upstream response failed".to_owned());
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
                events.push(StreamEvent::Error {
                    error: ApiErrorBody {
                        r#type: "upstream_error".to_string(),
                        message: msg,
                    },
                });
                events.push(StreamEvent::MessageStop {});
                self.finished = true;
            }
            ResponsesStreamEvent::Error { message, .. } => {
                // Top-level error event: same path.
                if !self.started {
                    events.push(self.build_message_start());
                    self.started = true;
                }
                events.push(StreamEvent::Error {
                    error: ApiErrorBody {
                        r#type: "upstream_error".to_string(),
                        message: message.clone(),
                    },
                });
                events.push(StreamEvent::MessageStop {});
                self.finished = true;
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
        let usage = self.usage.clone().or(Some(AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
        }));
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
                input_tokens: self.input_tokens,
                output_tokens: 0,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
                cache_creation_input_tokens_5m: 0,
                cache_creation_input_tokens_1h: 0,
                thinking_tokens: 0,
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
            OutputItem::FunctionCall { call_id, name, .. } => {
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
                        thinking: String::new(),
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
                // may carry a *complete* arguments blob. If so, and if
                // it differs from what we accumulated, emit a final
                // `input_json_delta` so the client sees the truth.
                if let Some(state) = self.tool_blocks.get(&output_index)
                    && state.arguments != *arguments
                    && !arguments.is_empty()
                {
                    // The accumulated string is missing the tail.
                    // Emit the difference as one final delta.
                    if let Some(tail) = arguments.strip_prefix(&state.arguments) {
                        events.push(StreamEvent::ContentBlockDelta {
                            index: state.index,
                            delta: ContentDelta::InputJsonDelta {
                                partial_json: tail.to_owned(),
                            },
                        });
                    } else {
                        // Total mismatch — emit the whole thing.
                        events.push(StreamEvent::ContentBlockDelta {
                            index: state.index,
                            delta: ContentDelta::InputJsonDelta {
                                partial_json: arguments.clone(),
                            },
                        });
                    }
                }
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
            OutputItem::Message { .. } | OutputItem::Unknown => {
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

    fn emit_tool_args_delta(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        args: &str,
    ) {
        if args.is_empty() {
            return;
        }
        let Some(state) = self.tool_blocks.get_mut(&output_index) else {
            // Dangling delta with no parent tool_use block — drop.
            tracing::warn!(output_index, "dangling function_call_arguments delta");
            return;
        };
        state.arguments.push_str(args);
        events.push(StreamEvent::ContentBlockDelta {
            index: state.index,
            delta: ContentDelta::InputJsonDelta {
                partial_json: args.to_owned(),
            },
        });
    }

    fn maybe_finalize_tool_args(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        arguments: &str,
    ) {
        let Some(state) = self.tool_blocks.get(&output_index) else {
            return;
        };
        if state.arguments != arguments && !arguments.is_empty() {
            // The done event carries a complete arguments blob. Emit
            // a final delta with the trailing portion.
            if let Some(tail) = arguments.strip_prefix(&state.arguments) {
                events.push(StreamEvent::ContentBlockDelta {
                    index: state.index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: tail.to_owned(),
                    },
                });
            } else {
                events.push(StreamEvent::ContentBlockDelta {
                    index: state.index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: arguments.to_owned(),
                    },
                });
            }
            self.tool_blocks.get_mut(&output_index).unwrap().arguments = arguments.to_owned();
        }
    }

    fn emit_thinking_delta(
        &mut self,
        events: &mut Vec<StreamEvent>,
        output_index: u32,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        let Some(state) = self.thinking_blocks.get(&output_index) else {
            tracing::warn!(output_index, "dangling reasoning_summary_text delta");
            return;
        };
        let index = state.index;
        self.thinking_blocks
            .get_mut(&output_index)
            .unwrap()
            .thinking
            .push_str(text);
        events.push(StreamEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::ThinkingDelta {
                thinking: text.to_owned(),
            },
        });
    }

    fn capture_terminal(&mut self, events: &mut Vec<StreamEvent>, response: &ResponsesResponse) {
        // Capture usage first so `finish()` includes it in message_delta.
        if let Some(usage) = &response.usage {
            self.usage = Some(map_usage(usage));
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
        // before the message_delta.
        self.finalize(events);
    }

    fn close_open_tool_blocks(&mut self, events: &mut Vec<StreamEvent>) {
        if self.tool_blocks.is_empty() {
            return;
        }
        let indices: Vec<u32> = self.tool_blocks.values().map(|s| s.index).collect();
        for idx in indices {
            events.push(StreamEvent::ContentBlockStop { index: idx });
        }
        self.tool_blocks.clear();
    }

    fn close_open_thinking_blocks(&mut self, events: &mut Vec<StreamEvent>) {
        if self.thinking_blocks.is_empty() {
            return;
        }
        let indices: Vec<u32> = self.thinking_blocks.values().map(|s| s.index).collect();
        for idx in indices {
            events.push(StreamEvent::ContentBlockStop { index: idx });
        }
        self.thinking_blocks.clear();
    }

    fn finalize(&mut self, events: &mut Vec<StreamEvent>) {
        if let Some(text) = self.text_block.take() {
            events.push(StreamEvent::ContentBlockStop { index: text.index });
        }
        self.close_open_tool_blocks(events);
        self.close_open_thinking_blocks(events);
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
}

fn map_usage(u: &crate::responses::ResponsesUsage) -> AnthropicUsage {
    AnthropicUsage {
        input_tokens: u.input_tokens,
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
mod tests {
    use super::*;
    use crate::responses::{
        IncompleteDetails, InputTokensDetails, OutputItem, ResponsesResponse, ResponsesUsage,
    };

    fn msg_id() -> String {
        "msg_test".into()
    }

    fn model() -> String {
        "test-model".into()
    }

    fn base_response(status: &str) -> ResponsesResponse {
        ResponsesResponse {
            id: "resp_test".into(),
            object: "response".into(),
            created_at: 0,
            status: status.into(),
            model: model(),
            output: vec![],
            usage: Some(ResponsesUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                input_tokens_details: None,
                output_tokens_details: None,
            }),
            error: None,
            incomplete_details: None,
        }
    }

    fn created_event() -> ResponsesStreamEvent {
        ResponsesStreamEvent::Created {
            response: base_response("in_progress"),
        }
    }

    fn completed_event(status: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::Completed {
            response: base_response(status),
        }
    }

    fn completed_event_with_output(status: &str, output: Vec<OutputItem>) -> ResponsesStreamEvent {
        let mut r = base_response(status);
        r.output = output;
        ResponsesStreamEvent::Completed { response: r }
    }

    fn incomplete_event(reason: &str) -> ResponsesStreamEvent {
        let mut r = base_response("incomplete");
        r.incomplete_details = Some(IncompleteDetails {
            reason: reason.into(),
        });
        ResponsesStreamEvent::Incomplete { response: r }
    }

    fn message_item_added(output_index: u32) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputItemAdded {
            output_index,
            item: OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("in_progress".into()),
                role: "assistant".into(),
                content: vec![],
            },
        }
    }

    fn function_call_added(output_index: u32, call_id: &str, name: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputItemAdded {
            output_index,
            item: OutputItem::FunctionCall {
                id: Some(format!("fc_{output_index}")),
                status: Some("in_progress".into()),
                call_id: call_id.into(),
                name: name.into(),
                arguments: String::new(),
            },
        }
    }

    fn function_call_args_delta(output_index: u32, delta: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::FunctionCallArgumentsDelta {
            item_id: format!("fc_{output_index}"),
            output_index,
            delta: delta.into(),
        }
    }

    fn function_call_args_done(output_index: u32, arguments: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::FunctionCallArgumentsDone {
            item_id: format!("fc_{output_index}"),
            output_index,
            arguments: arguments.into(),
        }
    }

    fn reasoning_item_added(output_index: u32) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputItemAdded {
            output_index,
            item: OutputItem::Reasoning {
                id: Some(format!("rs_{output_index}")),
                summary: vec![],
                encrypted_content: String::new(),
            },
        }
    }

    fn reasoning_summary_delta(output_index: u32, delta: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::ReasoningSummaryTextDelta {
            item_id: format!("rs_{output_index}"),
            output_index,
            summary_index: 0,
            delta: delta.into(),
        }
    }

    fn reasoning_item_done(output_index: u32, encrypted: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputItemDone {
            output_index,
            item: OutputItem::Reasoning {
                id: Some(format!("rs_{output_index}")),
                summary: vec![],
                encrypted_content: encrypted.into(),
            },
        }
    }

    fn output_text_delta(output_index: u32, text: &str) -> ResponsesStreamEvent {
        ResponsesStreamEvent::OutputTextDelta {
            item_id: "msg_x".into(),
            output_index,
            content_index: 0,
            delta: text.into(),
        }
    }

    #[test]
    fn text_stream_emits_message_start_then_text_deltas_then_stop() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        let evs = t.feed_event(&output_text_delta(0, "Hello"));
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::TextDelta { .. },
                ..
            }
        ));
        let evs = t.feed_event(&output_text_delta(0, " world"));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockDelta { .. }));

        let evs = t.feed_event(&completed_event("completed"));
        // content_block_stop (text)
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));

        let closing = t.finish();
        assert_eq!(closing.len(), 2);
        match &closing[0] {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
            }
            _ => panic!("expected message_delta"),
        }
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    #[test]
    fn tool_call_stream_emits_tool_use_block() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());

        let evs = t.feed_event(&function_call_added(0, "call_1", "get_weather"));
        // content_block_start (tool)
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 0);
                match content_block {
                    ContentBlockKind::ToolUse { id, name, .. } => {
                        assert_eq!(id, "call_1");
                        assert_eq!(name, "get_weather");
                    }
                    _ => panic!("expected tool_use"),
                }
            }
            _ => panic!("expected content_block_start"),
        }

        // Args delta.
        let evs = t.feed_event(&function_call_args_delta(0, r#"{"loc"#));
        assert_eq!(evs.len(), 1);
        assert!(matches!(
            evs[0],
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::InputJsonDelta { .. },
                ..
            }
        ));

        let evs = t.feed_event(&function_call_args_delta(0, r#"ation":"SF"}"#));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockDelta { .. }));

        // Terminal done with full args; matches what we accumulated, so
        // no extra delta is emitted.
        let evs = t.feed_event(&function_call_args_done(0, r#"{"location":"SF"}"#));
        assert!(evs.is_empty());

        // The terminal Completed event should carry the function call
        // in its output so the translator can detect has_tool_use and
        // set stop_reason = ToolUse.
        let evs = t.feed_event(&completed_event_with_output(
            "completed",
            vec![OutputItem::FunctionCall {
                id: Some("fc_0".into()),
                status: Some("completed".into()),
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments: r#"{"location":"SF"}"#.into(),
            }],
        ));
        // content_block_stop (tool)
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));

        let closing = t.finish();
        match &closing[0] {
            StreamEvent::MessageDelta { delta, .. } => {
                // has_tool_use → stop_reason = ToolUse
                assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
            }
            _ => panic!("expected message_delta"),
        }
    }

    #[test]
    fn text_then_tool_call_closes_text_first() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        t.feed_event(&output_text_delta(0, "Let me check."));

        // Open a tool use. Expect: text-stop, tool-start.
        let evs = t.feed_event(&function_call_added(0, "call_1", "get_weather"));
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
        assert!(matches!(
            evs[1],
            StreamEvent::ContentBlockStart { index: 1, .. }
        ));

        // Args delta goes to index 1.
        let evs = t.feed_event(&function_call_args_delta(0, "{}"));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::ContentBlockDelta { index, .. } => assert_eq!(*index, 1),
            _ => panic!(),
        }

        let _ = t.feed_event(&completed_event("completed"));
        let closing = t.finish();
        assert!(matches!(closing[0], StreamEvent::MessageDelta { .. }));
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    #[test]
    fn parallel_tool_calls_close_in_output_index_order() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&function_call_added(0, "call_a", "tool_a"));
        let _ = t.feed_event(&function_call_added(1, "call_b", "tool_b"));

        // Both blocks are open at output_index 0 and 1, Anthropic
        // indices 0 and 1.
        let evs = t.feed_event(&completed_event("completed"));
        // content_block_stop(0), content_block_stop(1) — BTreeMap order.
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
        assert!(matches!(evs[1], StreamEvent::ContentBlockStop { index: 1 }));
    }

    #[test]
    fn reasoning_item_emits_thinking_block() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());

        let evs = t.feed_event(&reasoning_item_added(0));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 0);
                assert!(matches!(content_block, ContentBlockKind::Thinking { .. }));
            }
            _ => panic!("expected content_block_start Thinking"),
        }

        let evs = t.feed_event(&reasoning_summary_delta(0, "step 1 "));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            StreamEvent::ContentBlockDelta { delta, .. } => {
                assert!(matches!(
                    delta,
                    ContentDelta::ThinkingDelta { thinking } if thinking == "step 1 "
                ));
            }
            _ => panic!("expected thinking_delta"),
        }

        let evs = t.feed_event(&reasoning_summary_delta(0, "step 2"));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockDelta { .. }));

        // Reasoning item done carries the signature; the translator
        // records it but does not emit a delta (no signature-delta on
        // the wire).
        let evs = t.feed_event(&reasoning_item_done(0, "opaque-sig"));
        assert!(evs.is_empty(), "no events for reasoning done");

        let _ = t.feed_event(&completed_event("completed"));
        let closing = t.finish();
        // MessageDelta + MessageStop
        assert!(matches!(closing[0], StreamEvent::MessageDelta { .. }));
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    #[test]
    fn late_reasoning_item_after_text_closes_text_first() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        t.feed_event(&output_text_delta(0, "intro"));

        // Reasoning item after the text block.
        let evs = t.feed_event(&reasoning_item_added(1));
        // text-stop, thinking-start.
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
        match &evs[1] {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 1);
                assert!(matches!(content_block, ContentBlockKind::Thinking { .. }));
            }
            _ => panic!("expected content_block_start Thinking"),
        }
    }

    #[test]
    fn incomplete_response_max_tokens_maps_to_max_tokens() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        t.feed_event(&output_text_delta(0, "a bit"));
        let evs = t.feed_event(&incomplete_event("max_output_tokens"));
        // content_block_stop(text)
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
        let closing = t.finish();
        match &closing[0] {
            StreamEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason, Some(StopReason::MaxTokens));
            }
            _ => panic!("expected message_delta"),
        }
    }

    #[test]
    fn failed_response_surfaces_error() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        let mut r = base_response("failed");
        r.error = Some(crate::responses::ResponsesError {
            message: "upstream rejected".into(),
            kind: "server_error".into(),
            code: None,
            param: None,
        });
        let evs = t.feed_event(&ResponsesStreamEvent::Failed { response: r });
        // The translator calls emit_error internally; we get back the
        // events. The exact shape depends on whether message_start
        // fired — it has, so just the error + stop.
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Error { .. })));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::MessageStop {})));
    }

    #[test]
    fn usage_in_completed_event_is_captured() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        t.feed_event(&output_text_delta(0, "x"));
        let _ = t.feed_event(&completed_event("completed"));
        let closing = t.finish();
        match &closing[0] {
            StreamEvent::MessageDelta { usage, .. } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
            }
            _ => panic!("expected message_delta"),
        }
    }

    #[test]
    fn completed_event_usage_includes_cache_fields() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        t.feed_event(&output_text_delta(0, "x"));

        let mut r = base_response("completed");
        r.usage = Some(ResponsesUsage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 20,
                cache_write_tokens: 7,
            }),
            output_tokens_details: None,
        });
        let _ = t.feed_event(&ResponsesStreamEvent::Completed { response: r });

        let closing = t.finish();
        match &closing[0] {
            StreamEvent::MessageDelta { usage, .. } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.output_tokens, 50);
                assert_eq!(u.cache_read_input_tokens, 20);
                assert_eq!(u.cache_creation_input_tokens, 7);
            }
            _ => panic!("expected message_delta"),
        }
    }

    #[test]
    fn finish_without_start_emits_message_start_first() {
        let t = StreamTranslator::new(msg_id(), model());
        let evs = t.finish();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(evs[1], StreamEvent::MessageDelta { .. }));
        assert!(matches!(evs[2], StreamEvent::MessageStop {}));
    }

    #[test]
    fn feed_after_finish_is_noop() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        let _ = t.feed_event(&completed_event("completed"));
        // After this, t is finished. A subsequent event produces no events.
        let evs = t.feed_event(&output_text_delta(0, "ignored"));
        assert!(evs.is_empty());
    }

    #[test]
    fn created_event_replaces_msg_id_and_model() {
        let mut t = StreamTranslator::new("synthetic".into(), "inbound".into());
        let ev = ResponsesStreamEvent::Created {
            response: ResponsesResponse {
                id: "resp_real".into(),
                object: "response".into(),
                created_at: 0,
                status: "in_progress".into(),
                model: "gpt-5.6-luna".into(),
                output: vec![],
                usage: None,
                error: None,
                incomplete_details: None,
            },
        };
        let evs = t.feed_event(&ev);
        // message_start with the upstream id (prefixed msg_) and model.
        match &evs[0] {
            StreamEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_resp_real");
                assert_eq!(message.model, "gpt-5.6-luna");
            }
            _ => panic!("expected message_start"),
        }
    }

    #[test]
    fn created_event_usage_surfaces_in_message_start() {
        let mut t = StreamTranslator::new("synthetic".into(), "inbound".into());
        let ev = ResponsesStreamEvent::Created {
            response: ResponsesResponse {
                id: "resp_real".into(),
                object: "response".into(),
                created_at: 0,
                status: "in_progress".into(),
                model: "gpt-5.6-luna".into(),
                output: vec![],
                usage: Some(ResponsesUsage {
                    input_tokens: 42,
                    output_tokens: 5,
                    total_tokens: 47,
                    input_tokens_details: Some(InputTokensDetails {
                        cached_tokens: 30,
                        cache_write_tokens: 12,
                    }),
                    output_tokens_details: None,
                }),
                error: None,
                incomplete_details: None,
            },
        };
        let evs = t.feed_event(&ev);
        match &evs[0] {
            StreamEvent::MessageStart { message } => {
                assert_eq!(message.usage.input_tokens, 42);
                assert_eq!(message.usage.cache_read_input_tokens, 30);
                assert_eq!(message.usage.cache_creation_input_tokens, 12);
                assert_eq!(message.usage.output_tokens, 0);
                assert_eq!(message.usage.thinking_tokens, 0);
            }
            _ => panic!("expected message_start"),
        }
    }

    #[test]
    fn dangling_args_delta_is_dropped() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        // No tool use opened yet.
        let evs = t.feed_event(&function_call_args_delta(0, "ignored"));
        assert!(evs.is_empty());
    }

    #[test]
    fn dangling_thinking_delta_is_dropped() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        let evs = t.feed_event(&reasoning_summary_delta(0, "ignored"));
        assert!(evs.is_empty());
    }

    #[test]
    fn empty_text_delta_is_dropped() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        let evs = t.feed_event(&output_text_delta(0, ""));
        assert!(evs.is_empty());
    }

    #[test]
    fn empty_args_delta_is_dropped() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&function_call_added(0, "call_1", "tool"));
        let evs = t.feed_event(&function_call_args_delta(0, ""));
        assert!(evs.is_empty());
    }

    #[test]
    fn unknown_event_is_dropped() {
        let mut t = StreamTranslator::new(msg_id(), model());
        let evs = t.feed_event(&ResponsesStreamEvent::Unknown);
        assert!(evs.is_empty());
    }

    #[test]
    fn top_level_error_event_routes_through_emit_error() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        let evs = t.feed_event(&ResponsesStreamEvent::Error {
            code: Some("server_error".into()),
            message: "boom".into(),
            param: None,
        });
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Error { .. })));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::MessageStop {})));
    }

    #[test]
    fn emit_error_closes_open_content_blocks_before_error() {
        // Regression: an upstream error SSE that arrives after a
        // content_block_start must emit content_block_stop before the
        // error event, otherwise the client sees an unbalanced block.
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&message_item_added(0));
        let _ = t.feed_event(&output_text_delta(0, "partial"));

        let evs = t.emit_error("api_error", "upstream SSE error");

        // Expected order: content_block_stop, error, message_stop.
        assert!(
            matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }),
            "expected content_block_stop first, got {:?}",
            evs[0]
        );
        assert!(
            matches!(evs[1], StreamEvent::Error { .. }),
            "expected error event second, got {:?}",
            evs[1]
        );
        assert!(
            matches!(evs[2], StreamEvent::MessageStop {}),
            "expected message_stop third, got {:?}",
            evs[2]
        );
    }

    #[test]
    fn emit_error_after_open_tool_block_closes_it_first() {
        // Same regression, but for an in-flight tool_use block.
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_event(&created_event());
        t.feed_event(&function_call_added(0, "call_1", "get_weather"));
        let _ = t.feed_event(&function_call_args_delta(0, r#"{"loc"#));

        let evs = t.emit_error("api_error", "upstream SSE error");

        // content_block_stop for the open tool block must precede error.
        let stop_idx = evs
            .iter()
            .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }));
        let error_idx = evs
            .iter()
            .position(|e| matches!(e, StreamEvent::Error { .. }));
        assert!(
            stop_idx.is_some() && error_idx.is_some() && stop_idx.unwrap() < error_idx.unwrap(),
            "content_block_stop must precede error, got {:?}",
            evs
        );
    }
}
