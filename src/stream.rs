//! Streaming translation: OpenAI SSE chunks → Anthropic SSE events.
//!
//! The translator is a state machine. The client feeds it OpenAI chunks
//! one at a time via [`StreamTranslator::feed_chunk`]; the translator
//! yields zero or more Anthropic events for each chunk. When the
//! upstream closes, the client calls [`StreamTranslator::finish`] to
//! flush any remaining state.
//!
//! ## State
//!
//! - `text_block`: whether we have an open text content block, plus its
//!   Anthropic content index.
//! - `tool_blocks`: per OpenAI tool-call index, the state of the
//!   in-flight `tool_use` Anthropic block (id, name, accumulated args,
//!   Anthropic content index). Stored in a `BTreeMap` so iteration in
//!   `finalize` is in index order, giving deterministic
//!   `content_block_stop` ordering for parallel tool calls.
//! - `next_index`: next free Anthropic content index.
//! - `stop_reason`: the final stop reason, set when a `finish_reason`
//!   chunk arrives.
//! - `usage`: token usage, set from the terminal usage chunk.
//!
//! The state is private; tests assert on the emitted events, not the
//! internal state.

use std::collections::BTreeMap;

use crate::anthropic::{
    ApiErrorBody, ContentBlockKind, ContentDelta, Message, MessageDeltaPayload, StopReason,
    StreamEvent, Usage as AnthropicUsage,
};
use crate::openai::{ChatCompletionChunk, ChunkChoice, Usage as OpenAiUsage};
use serde_json::Value;

/// Stateful translator for one streaming request.
#[derive(Debug)]
pub struct StreamTranslator {
    msg_id: String,
    model: String,
    started: bool,
    text_block: Option<TextBlockState>,
    tool_blocks: BTreeMap<u32, ToolBlockState>,
    next_index: u32,
    stop_reason: Option<StopReason>,
    usage: Option<AnthropicUsage>,
    finished: bool,
}

#[derive(Debug, Clone, Copy)]
struct TextBlockState {
    index: u32,
}

#[derive(Debug, Clone)]
struct ToolBlockState {
    index: u32,
    id: String,
    name: String,
    /// Arguments accumulated before `content_block_start` could be
    /// emitted (because id/name were not yet both known). Flushed as
    /// `input_json_delta` events right after the start fires.
    pending_args: String,
    /// True from `is_new` until id AND name are both non-empty and the
    /// deferred `content_block_start` has been emitted. While true,
    /// any incoming arguments are buffered in `pending_args` rather
    /// than emitted.
    pending_start: bool,
    arguments: String,
}

impl StreamTranslator {
    /// Build a translator. `model` is the model name to put in
    /// `message_start.model`; it's the upstream's resolved model, if
    /// available, falling back to the client's requested model.
    #[must_use]
    pub fn new(msg_id: String, model: String) -> Self {
        Self {
            msg_id,
            model,
            started: false,
            text_block: None,
            tool_blocks: BTreeMap::new(),
            next_index: 0,
            stop_reason: None,
            usage: None,
            finished: false,
        }
    }

    /// Feed one OpenAI chunk. Returns the Anthropic events it produces.
    /// The translator is tolerant: chunks that lack expected fields are
    /// treated as no-ops, and a `finish_reason` chunk that arrives
    /// without a prior text/tool event is still handled correctly.
    pub fn feed_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamEvent> {
        // Some upstreams (notably OpenAI with stream_options.include_usage)
        // send a usage-only chunk *after* the finish_reason chunk. If
        // we already finalized, we still need to capture that usage
        // before dropping the chunk. Order matters: record usage first,
        // then decide whether to keep processing.
        if let Some(usage) = &chunk.usage {
            self.usage = Some(map_usage(usage));
        }

        if self.finished {
            return Vec::new();
        }

        let mut events = Vec::new();

        // 1. Emit message_start on the first non-empty chunk.
        if !self.started && !chunk.choices.is_empty() {
            events.push(self.build_message_start());
            self.started = true;
        }

        // 2. Process each choice.
        for choice in &chunk.choices {
            self.process_choice(&mut events, choice);
        }

        // 3. If any choice produced a finish_reason, finalize.
        let any_finish = chunk.choices.iter().any(|c| c.finish_reason.is_some());
        if any_finish {
            self.finalize(&mut events);
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
            // We never emitted message_start; emit one so the client's
            // view of the world is well-formed.
            events.push(self.build_message_start());
            self.started = true;
        }
        if !self.finished {
            self.finalize(&mut events);
        }
        // The `message_delta` is the place the client learns the
        // final stop_reason and usage. It's built here, after any
        // late-arriving usage chunk has been recorded, so a usage-only
        // chunk that arrived after the finish_reason chunk is
        // reflected correctly.
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
        events.push(StreamEvent::Error {
            error: ApiErrorBody {
                r#type: kind.into(),
                message: message.into(),
            },
        });
        events.push(StreamEvent::MessageStop {});
        events
    }

    // ─── internal helpers ─────────────────────────────────────────────

    fn build_message_start(&self) -> StreamEvent {
        StreamEvent::MessageStart {
            message: Message {
                id: self.msg_id.clone(),
                r#type: "message",
                role: "assistant",
                content: Vec::new(),
                model: self.model.clone(),
                stop_reason: None,
                stop_sequence: None,
                usage: AnthropicUsage {
                    input_tokens: 0,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens_5m: 0,
                    cache_creation_input_tokens_1h: 0,
                    thinking_tokens: 0,
                },
            },
        }
    }

    fn process_choice(&mut self, events: &mut Vec<StreamEvent>, choice: &ChunkChoice) {
        if let Some(content) = &choice.delta.content {
            self.process_content_delta(events, content);
        }
        if let Some(tool_calls) = &choice.delta.tool_calls {
            for tc in tool_calls {
                self.process_tool_call_delta(events, tc);
            }
        }
        if let Some(reason) = &choice.finish_reason {
            self.stop_reason = Some(map_finish_reason(reason));
        }
    }

    fn process_content_delta(&mut self, events: &mut Vec<StreamEvent>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.text_block.is_none() {
            // The Anthropic streaming spec says opening a new content
            // block implicitly closes the previous one. If a tool_use
            // block is still open (text → tool_use → text), close it
            // first so the indices stay contiguous. BTreeMap iteration
            // is in ascending index order, which matches the order the
            // blocks were opened.
            let open_tools: Vec<u32> = self.tool_blocks.values().map(|s| s.index).collect();
            for idx in open_tools {
                events.push(StreamEvent::ContentBlockStop { index: idx });
            }
            self.tool_blocks.clear();

            let index = self.allocate_index();
            self.text_block = Some(TextBlockState { index });
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Text {
                    text: String::new(),
                },
            });
        }
        // The block was just (re)opened on the line above, so it must
        // be Some — use `let` rather than unwrap to keep the lints
        // happy without a panic.
        let Some(text_state) = self.text_block else {
            tracing::error!("text block state unexpectedly None after open");
            return;
        };
        events.push(StreamEvent::ContentBlockDelta {
            index: text_state.index,
            delta: ContentDelta::TextDelta {
                text: text.to_owned(),
            },
        });
    }

    fn process_tool_call_delta(
        &mut self,
        events: &mut Vec<StreamEvent>,
        tc: &crate::openai::ChunkToolCall,
    ) {
        // Was this tool index already known? If not, we'll need to
        // emit a content_block_start before any delta — but we may
        // not have id and name yet, so the start is deferred.
        let is_new = !self.tool_blocks.contains_key(&tc.index);

        if is_new {
            // Per the Anthropic streaming spec, opening a new content
            // block implicitly closes the previous one. Close the
            // text block if it's still open so the indices stay
            // contiguous and the client sees a clean transition.
            if let Some(text) = self.text_block.take() {
                events.push(StreamEvent::ContentBlockStop { index: text.index });
            }

            let index = self.allocate_index();
            self.tool_blocks.insert(
                tc.index,
                ToolBlockState {
                    index,
                    id: tc.id.clone().unwrap_or_default(),
                    name: tc
                        .function
                        .as_ref()
                        .and_then(|f| f.name.clone())
                        .unwrap_or_default(),
                    pending_args: String::new(),
                    pending_start: true,
                    arguments: String::new(),
                },
            );
        }

        // From here on we need mutable access to the state. The
        // invariant from the `is_new` branch above guarantees the
        // entry exists; we still avoid `expect` so a future refactor
        // can't panic the worker on a stray chunk.
        let Some(state) = self.tool_blocks.get_mut(&tc.index) else {
            tracing::error!(
                index = tc.index,
                "tool block state missing in process_tool_call_delta"
            );
            return;
        };

        // Update id/name if the upstream supplied them on this delta
        // (they may arrive split across chunks — the OpenAI spec
        // allows id on one chunk, name on a later one).
        if let Some(id) = &tc.id
            && !id.is_empty()
        {
            state.id.clone_from(id);
        }
        if let Some(name) = tc.function.as_ref().and_then(|f| f.name.as_ref())
            && !name.is_empty()
        {
            state.name.clone_from(name);
        }

        // If we were waiting for id/name before emitting start, and
        // they're now both present, emit start and flush any buffered
        // arguments as input_json_delta events.
        if state.pending_start && !state.id.is_empty() && !state.name.is_empty() {
            events.push(StreamEvent::ContentBlockStart {
                index: state.index,
                content_block: ContentBlockKind::ToolUse {
                    id: state.id.clone(),
                    name: state.name.clone(),
                    input: Value::Object(Default::default()),
                },
            });
            if !state.pending_args.is_empty() {
                let buffered = std::mem::take(&mut state.pending_args);
                state.arguments.push_str(&buffered);
                events.push(StreamEvent::ContentBlockDelta {
                    index: state.index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: buffered,
                    },
                });
            }
            state.pending_start = false;
        }

        // Buffer or emit the arguments fragment, depending on whether
        // the start has fired yet.
        if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref())
            && !args.is_empty()
        {
            if state.pending_start {
                state.pending_args.push_str(args);
            } else {
                state.arguments.push_str(args);
                events.push(StreamEvent::ContentBlockDelta {
                    index: state.index,
                    delta: ContentDelta::InputJsonDelta {
                        partial_json: args.clone(),
                    },
                });
            }
        }
    }

    fn finalize(&mut self, events: &mut Vec<StreamEvent>) {
        // Close the text block if open.
        if let Some(text) = self.text_block.take() {
            events.push(StreamEvent::ContentBlockStop { index: text.index });
        }
        // Close any open tool_use blocks.
        let tool_indices: Vec<u32> = self.tool_blocks.values().map(|s| s.index).collect();
        for idx in tool_indices {
            events.push(StreamEvent::ContentBlockStop { index: idx });
        }
        // `message_delta` is emitted by `finish()` instead, so that any
        // usage chunk that arrives AFTER `finalize` (a real OpenAI
        // behaviour) is reflected in the final delta.
        self.finished = true;
    }

    fn allocate_index(&mut self) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        idx
    }
}

fn map_usage(u: &OpenAiUsage) -> AnthropicUsage {
    AnthropicUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens_5m: 0,
        cache_creation_input_tokens_1h: 0,
        thinking_tokens: 0,
    }
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::EndTurn,
        other => {
            tracing::warn!(finish_reason = %other, "unknown finish_reason; defaulting to EndTurn");
            StopReason::EndTurn
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{ChunkDelta, ChunkFunction, ChunkToolCall};

    fn msg_id() -> String {
        "msg_test".into()
    }

    fn model() -> String {
        "test-model".into()
    }

    fn chunk(
        id: &str,
        choices: Vec<ChunkChoice>,
        usage: Option<OpenAiUsage>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: model(),
            choices,
            usage,
        }
    }

    fn text_choice(content: &str) -> ChunkChoice {
        ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(content.into()),
                tool_calls: None,
            },
            finish_reason: None,
        }
    }

    fn finish_choice(reason: &str) -> ChunkChoice {
        ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some(reason.into()),
        }
    }

    fn usage_chunk() -> OpenAiUsage {
        OpenAiUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        }
    }

    fn first_role_chunk() -> ChunkChoice {
        ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some("assistant".into()),
                content: None,
                tool_calls: None,
            },
            finish_reason: None,
        }
    }

    fn tool_choice(tool_calls: Vec<ChunkToolCall>) -> ChunkChoice {
        ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: None,
                tool_calls: Some(tool_calls),
            },
            finish_reason: None,
        }
    }

    #[test]
    fn text_stream_emits_message_start_then_text_deltas_then_stop() {
        let mut t = StreamTranslator::new(msg_id(), model());

        // Chunk 1: role marker
        let events = t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));

        // Chunk 2: text
        let events = t.feed_chunk(&chunk("c2", vec![text_choice("Hello")], None));
        assert_eq!(events.len(), 2);
        match &events[0] {
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(*index, 0);
                assert!(
                    matches!(content_block, ContentBlockKind::Text { text } if text.is_empty())
                );
            }
            _ => panic!("expected content_block_start text"),
        }
        match &events[1] {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 0);
                assert!(matches!(delta, ContentDelta::TextDelta { text } if text == "Hello"));
            }
            _ => panic!("expected text_delta"),
        }

        // Chunk 3: more text
        let events = t.feed_chunk(&chunk("c3", vec![text_choice(" world")], None));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::ContentBlockDelta { .. }));

        // Chunk 4: finish_reason + usage. finalize() closes the
        // content block; the message_delta is deferred to finish()
        // so any late-arriving usage can update the totals.
        let events = t.feed_chunk(&chunk(
            "c4",
            vec![finish_choice("stop")],
            Some(usage_chunk()),
        ));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));

        // finish() emits message_delta (with the captured usage)
        // followed by message_stop.
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
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));

        // First tool call delta: id + name, no args yet.
        let tc1 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: Some("call_1".into()),
            kind: Some("function".into()),
            function: Some(ChunkFunction {
                name: Some("get_weather".into()),
                arguments: None,
            }),
        }]);
        let events = t.feed_chunk(&chunk("c2", vec![tc1], None));
        // Should emit only content_block_start (no delta yet — args are empty).
        assert_eq!(events.len(), 1);
        match &events[0] {
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

        // Second tool delta: arguments fragment.
        let tc2 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: None,
            kind: None,
            function: Some(ChunkFunction {
                name: None,
                arguments: Some(r#"{"loc"#.into()),
            }),
        }]);
        let events = t.feed_chunk(&chunk("c3", vec![tc2], None));
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 0);
                assert!(
                    matches!(delta, ContentDelta::InputJsonDelta { partial_json } if partial_json == r#"{"loc"#)
                );
            }
            _ => panic!("expected input_json_delta"),
        }

        // Third tool delta: rest of args.
        let tc3 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: None,
            kind: None,
            function: Some(ChunkFunction {
                name: None,
                arguments: Some(r#""ation":"SF"}"#.into()),
            }),
        }]);
        let events = t.feed_chunk(&chunk("c4", vec![tc3], None));
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ContentBlockDelta { delta, .. } => {
                assert!(
                    matches!(delta, ContentDelta::InputJsonDelta { partial_json } if partial_json == r#""ation":"SF"}"#)
                );
            }
            _ => panic!("expected input_json_delta"),
        }

        // Finish with tool_calls reason. feed_chunk returns just the
        // content_block_stop; message_delta is deferred to finish().
        let events = t.feed_chunk(&chunk(
            "c5",
            vec![finish_choice("tool_calls")],
            Some(usage_chunk()),
        ));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));

        let closing = t.finish();
        assert_eq!(closing.len(), 2);
        match &closing[0] {
            StreamEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
            }
            _ => panic!("expected message_delta"),
        }
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    #[test]
    fn text_then_tool_call_in_same_stream() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        t.feed_chunk(&chunk("c2", vec![text_choice("Let me check.")], None));

        // Open a tool use.
        let events = t.feed_chunk(&chunk(
            "c3",
            vec![tool_choice(vec![ChunkToolCall {
                index: 0,
                id: Some("call_1".into()),
                kind: Some("function".into()),
                function: Some(ChunkFunction {
                    name: Some("get_weather".into()),
                    arguments: Some("{}".into()),
                }),
            }])],
            None,
        ));
        // expect: content_block_stop(text=0), content_block_start(tool=1), content_block_delta(args)
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        match &events[1] {
            StreamEvent::ContentBlockStart { index, .. } => assert_eq!(*index, 1),
            _ => panic!(),
        }
        assert!(matches!(
            events[2],
            StreamEvent::ContentBlockDelta { index: 1, .. }
        ));

        let _ = t.feed_chunk(&chunk(
            "c4",
            vec![finish_choice("tool_calls")],
            Some(usage_chunk()),
        ));
        let closing = t.finish();
        // [MessageDelta, MessageStop]
        assert!(matches!(closing[0], StreamEvent::MessageDelta { .. }));
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    #[test]
    fn finish_without_start_emits_message_start_first() {
        let t = StreamTranslator::new(msg_id(), model());
        // No chunks fed. finish() should still emit a well-formed close.
        let events = t.finish();
        // message_start, message_delta (no stop_reason), message_stop
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(events[1], StreamEvent::MessageDelta { .. }));
        assert!(matches!(events[2], StreamEvent::MessageStop {}));
    }

    #[test]
    fn feed_after_finish_is_noop() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        t.feed_chunk(&chunk(
            "c2",
            vec![finish_choice("stop")],
            Some(usage_chunk()),
        ));
        // After this, t is finished. A subsequent chunk produces no events.
        let events = t.feed_chunk(&chunk("c3", vec![text_choice("ignored")], None));
        assert!(events.is_empty());
    }

    #[test]
    fn length_finish_reason_maps_to_max_tokens() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        t.feed_chunk(&chunk("c2", vec![text_choice("a bit")], None));
        let events = t.feed_chunk(&chunk(
            "c3",
            vec![finish_choice("length")],
            Some(usage_chunk()),
        ));
        // finalize() returns just the content_block_stop; the
        // message_delta is built in finish().
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        let closing = t.finish();
        match &closing[0] {
            StreamEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_reason, Some(StopReason::MaxTokens));
            }
            _ => panic!("expected message_delta"),
        }
    }

    /// Bug 1 regression: the upstream sends a finish-reason chunk
    /// *without* usage, then a separate usage-only chunk. The
    /// translator must capture the late usage and surface it in the
    /// final `message_delta` (which is emitted by the bridge on
    /// `[DONE]`).
    #[test]
    fn usage_chunk_after_finish_reason_is_captured() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        t.feed_chunk(&chunk("c2", vec![text_choice("hi")], None));

        // Finish-reason chunk with NO usage. finalize() returns just
        // the content_block_stop; message_delta is deferred to finish().
        let events = t.feed_chunk(&chunk("c3", vec![finish_choice("stop")], None));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));

        // Now a usage-only chunk arrives (no choices, no finish_reason).
        // The translator must accept it and update its internal usage.
        let late = OpenAiUsage {
            prompt_tokens: 42,
            completion_tokens: 7,
            total_tokens: 49,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        let events = t.feed_chunk(&chunk("c4", vec![], Some(late)));
        // No new events (no choices, no finish) but the translator
        // state must reflect the captured usage.
        assert!(events.is_empty());

        // finish() closes the stream with a fresh message_delta that
        // carries the captured usage.
        let closing = t.finish();
        assert_eq!(closing.len(), 2);
        assert!(matches!(closing[0], StreamEvent::MessageDelta { .. }));
        match &closing[0] {
            StreamEvent::MessageDelta { usage, .. } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 42);
                assert_eq!(u.output_tokens, 7);
            }
            _ => panic!("expected message_delta"),
        }
        assert!(matches!(closing[1], StreamEvent::MessageStop {}));
    }

    /// Bug 4 regression: when a tool call's id and name arrive in
    /// separate chunks, the deferred `content_block_start` must
    /// fire only once both are non-empty, and any arguments that
    /// arrived before the start must be flushed as `input_json_delta`
    /// right after.
    #[test]
    fn defer_tool_use_start_until_id_and_name_known() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));

        // Chunk 2: id only, no name yet. With the deferred-start
        // behaviour, this produces NO events. The arguments fragment
        // is buffered in `pending_args`.
        let tc1 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: Some("call_1".into()),
            kind: Some("function".into()),
            function: Some(ChunkFunction {
                name: None,
                arguments: Some("ignored-args".into()),
            }),
        }]);
        let events = t.feed_chunk(&chunk("c2", vec![tc1], None));
        assert!(events.is_empty(), "no events before name is known");

        // Chunk 3: name only (id was already set). Start fires now,
        // and the buffered args are flushed as an input_json_delta.
        let tc2 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: None,
            kind: None,
            function: Some(ChunkFunction {
                name: Some("get_weather".into()),
                arguments: None,
            }),
        }]);
        let events = t.feed_chunk(&chunk("c3", vec![tc2], None));
        assert_eq!(events.len(), 2);
        match &events[0] {
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
        match &events[1] {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 0);
                assert!(
                    matches!(delta, ContentDelta::InputJsonDelta { partial_json } if partial_json == "ignored-args")
                );
            }
            _ => panic!("expected buffered input_json_delta"),
        }

        // Chunk 4: more args after start. Emits another input_json_delta.
        let tc3 = tool_choice(vec![ChunkToolCall {
            index: 0,
            id: None,
            kind: None,
            function: Some(ChunkFunction {
                name: None,
                arguments: Some(r#"{"loc":"SF"}"#.into()),
            }),
        }]);
        let events = t.feed_chunk(&chunk("c4", vec![tc3], None));
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::InputJsonDelta { .. },
                ..
            }
        ));
    }

    /// Bug 9 regression: text → tool_use → text must close the tool
    /// block before opening the new text block, otherwise the indices
    /// skip and the client sees dangling blocks.
    #[test]
    fn text_after_tool_call_closes_tool_block_first() {
        let mut t = StreamTranslator::new(msg_id(), model());
        t.feed_chunk(&chunk("c1", vec![first_role_chunk()], None));
        t.feed_chunk(&chunk("c2", vec![text_choice("Let me check. ")], None));

        // Open a tool use (single chunk: id + name + args).
        let events = t.feed_chunk(&chunk(
            "c3",
            vec![tool_choice(vec![ChunkToolCall {
                index: 0,
                id: Some("call_1".into()),
                kind: Some("function".into()),
                function: Some(ChunkFunction {
                    name: Some("get_weather".into()),
                    arguments: Some("{}".into()),
                }),
            }])],
            None,
        ));
        // text-stop, tool-start, tool-delta
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        assert!(matches!(
            events[1],
            StreamEvent::ContentBlockStart { index: 1, .. }
        ));
        assert!(matches!(
            events[2],
            StreamEvent::ContentBlockDelta { index: 1, .. }
        ));

        // Now a text delta follows the tool use. The tool block (index 1)
        // must be closed before the new text block (index 2) opens.
        let events = t.feed_chunk(&chunk("c4", vec![text_choice("Done.")], None));
        assert_eq!(events.len(), 3);
        assert!(
            matches!(events[0], StreamEvent::ContentBlockStop { index: 1 }),
            "tool block must close before text reopens"
        );
        match &events[1] {
            StreamEvent::ContentBlockStart { index, .. } => assert_eq!(*index, 2),
            _ => panic!("expected content_block_start for new text block"),
        }
        match &events[2] {
            StreamEvent::ContentBlockDelta { index, .. } => assert_eq!(*index, 2),
            _ => panic!("expected content_block_delta"),
        }
    }
}
