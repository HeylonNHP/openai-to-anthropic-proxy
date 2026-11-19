//! The loop: run the executor, serve its server-tool calls, re-run.
//!
//! ## Bounded
//!
//! The loop runs at most `server_tools.max_rounds` ROUNDS, a round being one
//! executor run whose calls were served (each costs an upstream call). At the
//! limit it either asks the model to answer with what it has (the default), or,
//! with `server_tools.pause_turn`, returns the response as it stands with
//! `stop_reason: "pause_turn"` for the client to resend, as Anthropic's API
//! does. See [`RoundPolicy`].
//!
//! ## Fail closed
//!
//! A tool never fails the client request (see [`ServerTool`]). Only the
//! executor can: the FIRST executor failure is an ordinary upstream failure
//! and propagates; a failure on a LATER run does not discard the work already
//! done, so the response built so far is returned instead.

use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::anthropic::{Message, ResponseContentBlock};
use crate::config::{ReasoningReplay, ReasoningReplayMode};
use crate::error::AppError;
use crate::proxy::{Attempt, Outcome};
use crate::repair::ToolSchemaRegistry;
use crate::responses::{
    Input, InputContentPart, InputItem, InputTokensDetails, OutputContentPart, OutputItem,
    OutputTokensDetails, ReasoningSummaryPart, ReasoningTextPart, ResponsesResponse,
    ResponsesUsage,
};
use crate::tools::server_loop::handler::{Call, ServerTool};
use crate::tui::{LogKind, LogLine};

/// What the loop tells a streaming client's response as it goes, and what the
/// task running it adds at the end (see [`crate::tools::server_loop::stream`]).
pub(crate) enum LoopEvent {
    /// The first executor response arrived: the stream can start. Carries
    /// the message skeleton for `message_start`.
    Started(Message),
    /// More content blocks are final, in order.
    Blocks(Vec<ResponseContentBlock>),
    /// The turn is complete: what is left to send.
    Finished(crate::tools::server_loop::render::Tail),
    /// The turn failed. Before `Started` this is an ordinary HTTP error;
    /// after it, an SSE `error` event.
    Failed(AppError),
    /// The upstream streamed although the request was buffered: hand the
    /// request back to be rendered as an ordinary stream.
    Fallback(Box<Attempt>, reqwest::Response),
}

/// Sends a streaming client's view of the loop to its response, and counts
/// what was sent. Does nothing when there is no sink (a buffered client).
struct Emitter<'a> {
    sink: Option<&'a UnboundedSender<LoopEvent>>,
    /// Prefix blocks already sent.
    emitted: usize,
    started: bool,
}

impl<'a> Emitter<'a> {
    const fn new(sink: Option<&'a UnboundedSender<LoopEvent>>) -> Self {
        Self {
            sink,
            emitted: 0,
            started: false,
        }
    }

    /// The first executor response arrived. Sent once.
    fn start(&mut self, skeleton: impl FnOnce() -> Message) {
        if let Some(sink) = self.sink
            && !self.started
        {
            self.started = true;
            let _ = sink.send(LoopEvent::Started(skeleton()));
        }
    }

    /// Send every prefix block not yet sent.
    fn flush(&mut self, prefix: &[ResponseContentBlock]) {
        if let Some(sink) = self.sink
            && self.emitted < prefix.len()
        {
            let _ = sink.send(LoopEvent::Blocks(prefix[self.emitted..].to_vec()));
            self.emitted = prefix.len();
        }
    }

    /// The client is still there (always true without a sink).
    fn client_present(&self) -> bool {
        self.sink.is_none_or(|sink| !sink.is_closed())
    }
}

/// What the loop produced.
pub(crate) enum SessionOutcome {
    /// A fully buffered, assembled final response ready for rendering.
    Complete(Box<SessionResult>),
    /// `prepare` forces `stream: false`, but if an upstream answers with a
    /// stream anyway, degrade to the ordinary streaming path rather than
    /// erroring the client.
    UnexpectedStreaming(reqwest::Response),
}

/// Text injected for calls made after the round limit: the model must answer.
const ROUND_LIMIT_REACHED: &str = "The server tool limit for this request was reached, so this \
     call was not run. Answer with the information you already have, and say if it is incomplete.";

/// How many rounds the loop may run, and what happens at the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoundPolicy {
    /// Rounds that may be served. At least 1; `usize::MAX` is "no limit".
    pub max_rounds: usize,
    /// At the limit, return `pause_turn` instead of asking for an answer.
    pub pause_turn: bool,
}

impl RoundPolicy {
    pub(crate) fn from_config(cfg: &crate::config::ServerToolsConfig) -> Self {
        Self {
            // `None` (no limit) is a limit nothing can reach. A limit of 0 can
            // only come from a hand-built config; it would forbid all work, so
            // it is raised to 1.
            max_rounds: cfg.max_rounds.map_or(usize::MAX, |m| m.max(1)),
            pause_turn: cfg.pause_turn,
        }
    }

    /// What to do after `rounds_served` rounds, once the latest has been served.
    pub(crate) const fn after_round(self, rounds_served: usize) -> AfterRound {
        if rounds_served < self.max_rounds {
            AfterRound::Continue
        } else if self.pause_turn {
            AfterRound::Pause
        } else {
            AfterRound::AnswerNext
        }
    }
}

/// The decision [`RoundPolicy::after_round`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterRound {
    /// Run the executor again as usual.
    Continue,
    /// Stop now and return `pause_turn`.
    Pause,
    /// Run the executor again, but refuse any further server-tool call: its
    /// next response is the final one.
    AnswerNext,
}

/// The buffered result of the whole turn.
pub(crate) struct SessionResult {
    /// The LAST executor response, the one that answered the user. Its
    /// `usage` is the whole turn's: the sum over every executor run.
    pub final_resp: ResponsesResponse,
    /// Intermediate executor content interleaved with each tool's result
    /// blocks, in the order they happened.
    pub prefix_blocks: Vec<ResponseContentBlock>,
    /// The tools, handed back so each can add its numbers to `usage`.
    pub tools: Vec<Box<dyn ServerTool>>,
    /// The loop stopped at the round limit with work still to do: the
    /// response ends with `stop_reason: "pause_turn"` (unless a client tool
    /// call is pending).
    pub paused: bool,
    /// How many `prefix_blocks` a streaming client already has (0 when the
    /// client is buffered).
    pub emitted: usize,
}

/// Run the whole turn for `tools`, for a client that gets the response whole.
///
/// # Errors
///
/// The first executor run failed (scrubbed by the tools before it is
/// returned).
pub(crate) async fn run(
    attempt: &mut Attempt,
    tools: Vec<Box<dyn ServerTool>>,
) -> Result<SessionOutcome, AppError> {
    drive(attempt, tools, None).await
}

/// The loop. With a `sink`, a streaming client's response is told about the
/// turn as it happens; without one, nothing is sent and the result is simply
/// returned.
///
/// # Errors
///
/// The first executor run failed (scrubbed by the tools before it is
/// returned), or the streaming client went away.
pub(crate) async fn drive(
    attempt: &mut Attempt,
    mut tools: Vec<Box<dyn ServerTool>>,
    sink: Option<&UnboundedSender<LoopEvent>>,
) -> Result<SessionOutcome, AppError> {
    let mut out = Emitter::new(sink);
    let url = attempt.url();
    let policy = RoundPolicy::from_config(&attempt.state.config.server_tools);
    let names: Vec<&'static str> = tools.iter().map(|t| t.executor_name()).collect();
    let mut prefix_blocks: Vec<ResponseContentBlock> = Vec::new();
    // The last executor response whose content is already in
    // `prefix_blocks`. Once present, a failed re-run renders THIS (minus its
    // output, which is not repeated) instead of failing the client.
    let mut last_good: Option<ResponsesResponse> = None;
    // Rounds served so far. `limit_hit`: the round limit was reached, so any
    // further call is refused. `terminal`: such a refusal was just sent, so
    // the next response is final whatever it contains.
    let mut rounds = 0usize;
    let mut limit_hit = false;
    let mut terminal = false;
    // Every executor run in the turn is billed, not just the last: the
    // response's top-level usage is the sum of them all, as Anthropic
    // reports for a server-tool turn.
    let mut total_usage: Option<ResponsesUsage> = None;

    loop {
        // Nobody is listening any more: stop spending upstream calls.
        if !out.client_present() {
            return Err(AppError::Internal(
                "the client disconnected during a server-tool turn".to_owned(),
            ));
        }
        let outcome = attempt.execute(&url).await;
        // An upstream that streams although the request was buffered can only
        // be handed on as a stream when nothing has been done yet; later, it
        // is treated like any other failed re-run.
        let outcome = match outcome {
            Ok(Outcome::Streaming(raw)) if last_good.is_none() => {
                tracing::warn!(
                    "server-tool path expected a buffered response; rendering the upstream \
                     stream directly"
                );
                return Ok(SessionOutcome::UnexpectedStreaming(raw));
            }
            Ok(Outcome::Streaming(_)) => Err(AppError::Internal(
                "the upstream streamed a response that was requested whole".to_owned(),
            )),
            other => other,
        };
        let resp = match outcome {
            Ok(Outcome::Buffered(body)) => {
                add_usage(&mut total_usage, body.usage.as_ref());
                body
            }
            Ok(Outcome::Streaming(_)) => unreachable!("handled above"),
            Err(err) => {
                let Some(last) = last_good.take() else {
                    return Err(scrub(&tools, err));
                };
                warn_rerun_failed(attempt, &err);
                if !terminal {
                    for tool in &mut tools {
                        tool.on_rerun_failure(&mut prefix_blocks);
                    }
                }
                return Ok(complete(
                    last,
                    total_usage,
                    prefix_blocks,
                    tools,
                    false,
                    out.emitted,
                ));
            }
        };
        out.start(|| {
            let mut skeleton = crate::translate::responses_to_anthropic(&resp, &attempt.registry);
            skeleton.content.clear();
            skeleton
        });

        for tool in &mut tools {
            tool.on_executor_response(&resp);
        }

        // Calls to a tool that is already exhausted are dropped, not served.
        let calls: Vec<(usize, Call)> = find_calls(&resp.output, &names)
            .into_iter()
            .filter(|(index, _)| !tools[*index].exhausted())
            .collect();
        if terminal || calls.is_empty() {
            // The turn is over. A call that was dropped, or that came after
            // a refusal, must not survive into the final response.
            let mut resp = resp;
            if terminal {
                strip_calls(&mut resp, &names);
            } else {
                strip_exhausted(&mut resp, &tools);
            }
            return Ok(complete(
                resp,
                total_usage,
                prefix_blocks,
                tools,
                false,
                out.emitted,
            ));
        }

        // Everything the executor said in this sampling is an INTERMEDIATE
        // block: keep all of it except the internal tool calls.
        prefix_blocks.extend(intermediate_blocks(&resp, &attempt.registry, &names));
        out.flush(&prefix_blocks);

        let replay_mode = attempt
            .state
            .config
            .reasoning_replay_for_model(&attempt.outbound.model);

        if limit_hit {
            // The round limit was reached and the model still asks for more:
            // refuse, and give it one last run to answer.
            let answered: Vec<(String, String)> = calls
                .iter()
                .map(|(_, call)| (call.call_id.clone(), ROUND_LIMIT_REACHED.to_owned()))
                .collect();
            continue_with(&mut attempt.outbound.input, &resp, &answered, &replay_mode);
            terminal = true;
            last_good = Some(without_output(&resp));
            continue;
        }

        // `(call_id, output)` of every call answered this round. A call
        // skipped below gets no output, so it is not replayed either.
        let mut answered: Vec<(String, String)> = Vec::new();
        for (tool_index, call) in &calls {
            // An earlier call in this same response may have exhausted it.
            if tools[*tool_index].exhausted() {
                continue;
            }
            let started = Instant::now();
            let served = tools[*tool_index].serve(attempt, call).await;
            // The call the client never sees: show it in the request log, so
            // a turn that fetched pages is not a turn that did nothing.
            attempt.state.output.emit(LogLine {
                kind: LogKind::Tool,
                text: served.summary.row(started.elapsed()),
            });
            prefix_blocks.extend(served.blocks);
            out.flush(&prefix_blocks);
            answered.push((call.call_id.clone(), served.output));
        }
        continue_with(&mut attempt.outbound.input, &resp, &answered, &replay_mode);
        rounds += 1;

        match policy.after_round(rounds) {
            AfterRound::Continue => {}
            AfterRound::Pause => {
                tracing::info!(
                    rounds,
                    "server-tool round limit reached; returning pause_turn for the client to resume"
                );
                return Ok(complete(
                    without_output(&resp),
                    total_usage,
                    prefix_blocks,
                    tools,
                    true,
                    out.emitted,
                ));
            }
            AfterRound::AnswerNext => {
                tracing::warn!(
                    rounds,
                    "server-tool round limit reached; asking the model to answer"
                );
                attempt.state.output.emit(LogLine {
                    kind: LogKind::Warning,
                    text: format!(
                        "  \u{26a0} server tools: round limit ({rounds}) reached; asking the model to answer"
                    ),
                });
                limit_hit = true;
            }
        }
        last_good = Some(without_output(&resp));
    }
}

fn complete(
    mut final_resp: ResponsesResponse,
    total_usage: Option<ResponsesUsage>,
    prefix_blocks: Vec<ResponseContentBlock>,
    tools: Vec<Box<dyn ServerTool>>,
    paused: bool,
    emitted: usize,
) -> SessionOutcome {
    // The response the client gets (and the stats) carry the whole turn's
    // usage, not only the last run's.
    if total_usage.is_some() {
        final_resp.usage = total_usage;
    }
    SessionOutcome::Complete(Box::new(SessionResult {
        final_resp,
        prefix_blocks,
        tools,
        paused,
        emitted,
    }))
}

/// Add one executor run's usage to the turn's running total.
///
/// Every field is a count, so the turn's usage is the field-wise sum of its
/// runs (saturating: a turn cannot overflow `u32` in practice, but must not
/// panic if it did). A run that reported no usage adds nothing.
pub(crate) fn add_usage(total: &mut Option<ResponsesUsage>, run: Option<&ResponsesUsage>) {
    let Some(run) = run else {
        return;
    };
    let Some(sum) = total.as_mut() else {
        *total = Some(run.clone());
        return;
    };
    sum.input_tokens = sum.input_tokens.saturating_add(run.input_tokens);
    sum.output_tokens = sum.output_tokens.saturating_add(run.output_tokens);
    sum.total_tokens = sum.total_tokens.saturating_add(run.total_tokens);
    if let Some(run_in) = &run.input_tokens_details {
        let d = sum.input_tokens_details.get_or_insert(InputTokensDetails {
            cached_tokens: 0,
            cache_write_tokens: 0,
        });
        d.cached_tokens = d.cached_tokens.saturating_add(run_in.cached_tokens);
        d.cache_write_tokens = d
            .cache_write_tokens
            .saturating_add(run_in.cache_write_tokens);
    }
    if let Some(run_out) = &run.output_tokens_details {
        let d = sum
            .output_tokens_details
            .get_or_insert(OutputTokensDetails {
                reasoning_tokens: 0,
            });
        d.reasoning_tokens = d.reasoning_tokens.saturating_add(run_out.reasoning_tokens);
    }
}

/// Let every tool clean an upstream error before it can reach the client.
fn scrub(tools: &[Box<dyn ServerTool>], err: AppError) -> AppError {
    tools.iter().fold(err, |err, tool| tool.scrub_error(err))
}

fn warn_rerun_failed(attempt: &Attempt, err: &AppError) {
    tracing::warn!(
        error = %err,
        "executor re-run failed after server-tool work; returning what was done"
    );
    attempt.state.output.emit(LogLine {
        kind: LogKind::Warning,
        text: "  \u{26a0} upstream failed after the server tools ran; returning the work so far"
            .to_owned(),
    });
}

/// Every call in `output` to one of the tools in `names`, in the order the
/// executor made them, each with the index of the tool that serves it.
pub(crate) fn find_calls(output: &[OutputItem], names: &[&'static str]) -> Vec<(usize, Call)> {
    output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => names.iter().position(|n| n == name).map(|index| {
                (
                    index,
                    Call {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                )
            }),
            _ => None,
        })
        .collect()
}

/// Translated content of one executor sampling, minus the internal tool
/// calls. Order is kept so intermediate text and sibling client tool uses
/// survive to the final message.
pub(crate) fn intermediate_blocks(
    resp: &ResponsesResponse,
    registry: &ToolSchemaRegistry,
    names: &[&'static str],
) -> Vec<ResponseContentBlock> {
    let mut blocks = crate::translate::responses_to_anthropic(resp, registry).content;
    blocks.retain(|block| {
        !matches!(block, ResponseContentBlock::ToolUse { name, .. } if names.iter().any(|n| n == name))
    });
    blocks
}

/// Remove the calls to exhausted tools from a terminal response, so none
/// is left dangling in the response or anything built from it.
pub(crate) fn strip_exhausted(resp: &mut ResponsesResponse, tools: &[Box<dyn ServerTool>]) {
    resp.output.retain(|item| {
        !matches!(item, OutputItem::FunctionCall { name, .. }
            if tools.iter().any(|t| t.exhausted() && t.executor_name() == name))
    });
}

/// Remove every call to one of the loop's own tools from a final response.
pub(crate) fn strip_calls(resp: &mut ResponsesResponse, names: &[&'static str]) {
    resp.output.retain(
        |item| !matches!(item, OutputItem::FunctionCall { name, .. } if names.iter().any(|n| n == name)),
    );
}

/// The response kept as `last_good` while the loop continues: its content is
/// already in the prefix, so rendering it verbatim would repeat it. Keep the
/// metadata (id, model, status, usage) and drop only `output`.
pub(crate) fn without_output(resp: &ResponsesResponse) -> ResponsesResponse {
    let mut clone = resp.clone();
    clone.output.clear();
    clone
}

/// Extend the executor's input so its re-run continues where this round left
/// off: what it said this round ([`replay_round`]), then the output of every
/// call that was answered.
pub(crate) fn continue_with(
    input: &mut Input,
    resp: &ResponsesResponse,
    answered: &[(String, String)],
    replay: &ReasoningReplay,
) {
    let answered_ids: Vec<&str> = answered.iter().map(|(id, _)| id.as_str()).collect();
    let mut items = replay_round(resp, &answered_ids, replay);
    for (call_id, output) in answered {
        items.push(InputItem::FunctionCallOutput {
            call_id: call_id.clone(),
            output: output.clone().into(),
        });
    }
    if let Input::Items(existing) = input {
        existing.extend(items);
    }
}

/// What the executor said in one round, as input items for its re-run, in
/// the order it said them.
///
/// Without this the re-run would see only its tool calls and their outputs:
/// its own text from the round (often the plan the calls were part of) and
/// its reasoning would be gone, and an upstream that requires a function
/// call's reasoning item to be passed back with it would reject the request.
///
/// * An assistant message is replayed as an assistant message.
/// * A function call is replayed only if it is answered (`answered` holds
///   its `call_id`): a call with no output is rejected by the upstream. That
///   leaves out a sibling CLIENT tool call (the client runs it after the
///   turn) and a call to an exhausted tool.
/// * Reasoning follows the operator's `reasoning_replay` mode for the
///   model, as on a later turn: `native` replays the reasoning item itself
///   (with its real id and encrypted content), `text` folds its summary
///   into the next assistant message between `<thinking>` markers, `off`
///   drops it. A reasoning item belongs to the item that follows it, and the
///   upstream rejects one whose following item is missing, so a reasoning
///   item whose next item is not replayed is not replayed either.
/// * Anything else (a built-in `web_search_call`, an unknown item) is
///   upstream-internal and is not replayed.
pub(crate) fn replay_round(
    resp: &ResponsesResponse,
    answered: &[&str],
    replay: &ReasoningReplay,
) -> Vec<InputItem> {
    let mut items = Vec::new();
    // The reasoning waiting for the item it belongs to.
    let mut pending: Option<PendingReasoning> = None;
    for item in &resp.output {
        match item {
            OutputItem::Reasoning {
                id,
                summary,
                encrypted_content,
            } => {
                let text = summary
                    .iter()
                    .map(|part| part.text.as_str())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                pending = Some(PendingReasoning {
                    id: id.clone(),
                    text: cap_chars(text, replay.max_chars),
                    encrypted_content: encrypted_content.clone(),
                });
            }
            OutputItem::Message { role, content, .. } if role == "assistant" => {
                let text: String = content
                    .iter()
                    .filter_map(|part| match part {
                        OutputContentPart::OutputText { text, .. } => Some(text.as_str()),
                        OutputContentPart::OutputRefusal { refusal } => Some(refusal.as_str()),
                        _ => None,
                    })
                    .collect();
                let thinking = take_reasoning(&mut pending, replay, &mut items);
                push_assistant_text(&mut items, thinking, &text);
            }
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } if answered.contains(&call_id.as_str()) => {
                if let Some(thinking) = take_reasoning(&mut pending, replay, &mut items) {
                    push_assistant_text(&mut items, Some(thinking), "");
                }
                items.push(InputItem::FunctionCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            // Not replayed, so the reasoning it followed is not either.
            _ => pending = None,
        }
    }
    items
}

/// A reasoning item seen in the output, not yet replayed.
struct PendingReasoning {
    id: Option<String>,
    text: String,
    encrypted_content: String,
}

/// The item that `pending` reasoning belongs to is being replayed: in
/// `native` mode push the reasoning item now (it must come first); in `text`
/// mode return its text to fold into the assistant message.
fn take_reasoning(
    pending: &mut Option<PendingReasoning>,
    replay: &ReasoningReplay,
    items: &mut Vec<InputItem>,
) -> Option<String> {
    let reasoning = pending.take()?;
    match replay.mode {
        ReasoningReplayMode::Off => None,
        ReasoningReplayMode::Text => (!reasoning.text.is_empty()).then(|| {
            replay
                .replayed_chars
                .set(replay.replayed_chars.get() + reasoning.text.chars().count() as u64);
            reasoning.text
        }),
        ReasoningReplayMode::Native => {
            if !reasoning.text.is_empty() {
                replay
                    .replayed_chars
                    .set(replay.replayed_chars.get() + reasoning.text.chars().count() as u64);
            }
            let has_text = !reasoning.text.is_empty();
            items.push(InputItem::Reasoning {
                id: reasoning.id,
                // OpenAI reads `summary`, DeepSeek reads `content`: both carry
                // the text, as the translator does for a later turn.
                content: if has_text {
                    vec![ReasoningTextPart {
                        kind: "reasoning_text".into(),
                        text: reasoning.text.clone(),
                    }]
                } else {
                    Vec::new()
                },
                summary: if has_text {
                    vec![ReasoningSummaryPart {
                        kind: "summary_text".into(),
                        text: reasoning.text,
                    }]
                } else {
                    Vec::new()
                },
                encrypted_content: (!reasoning.encrypted_content.is_empty())
                    .then_some(reasoning.encrypted_content),
            });
            None
        }
    }
}

/// Push an assistant message: the folded reasoning (if any) between
/// `<thinking>` markers, then the visible text. Nothing at all is pushed
/// for a message with neither.
fn push_assistant_text(items: &mut Vec<InputItem>, thinking: Option<String>, text: &str) {
    let body = thinking.map_or_else(
        || text.to_owned(),
        |thinking| {
            format!(
                "{}{thinking}{}{text}",
                crate::translate::THINKING_PREFIX,
                crate::translate::THINKING_SUFFIX
            )
        },
    );
    if body.is_empty() {
        return;
    }
    items.push(InputItem::Message {
        role: "assistant".to_owned(),
        content: vec![InputContentPart::OutputText { text: body }],
    });
}

/// Cut reasoning text to the operator's `reasoning_replay.max_chars`
/// (`0` is no limit).
fn cap_chars(text: String, max_chars: usize) -> String {
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text;
    }
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests;
