//! Assemble the final Anthropic response for a server-tool turn.
//!
//! A client that asked for the response whole gets it from [`render`] (every
//! run was buffered: `prepare` forces `stream: false`); one that asked for
//! SSE is streamed to as the turn goes (see
//! [`crate::tools::server_loop::stream`]) and finished by [`finish_streaming`].

use axum::Json;
use axum::response::{IntoResponse, Response};

use crate::anthropic::{Message, ResponseContentBlock, StopReason};
use crate::error::AppError;
use crate::proxy::{Attempt, record_json_stats};
use crate::responses::ResponsesResponse;
use crate::tools::server_loop::handler::ServerTool;
use crate::tools::server_loop::run::{SessionResult, StopSignals};
use crate::translate;

/// Render the assembled turn.
pub(crate) fn render(attempt: &Attempt, result: SessionResult) -> Result<Response, AppError> {
    let SessionResult {
        final_resp,
        prefix_blocks,
        tools,
        paused,
        signals,
        emitted: _,
        live_final: _,
    } = result;

    let mut anth = translate_final(attempt, &final_resp, signals);
    assemble(&mut anth, prefix_blocks, &tools, paused);

    // Records the turn's usage exactly once: `run` has already replaced the
    // final response's usage with the sum over every executor run.
    record_json_stats(attempt, &final_resp);

    Ok(Json(anth).into_response())
}

/// What a streaming client still has to be sent when the turn is over: the
/// blocks it has not seen, and the closing `message_delta`.
pub(crate) struct Tail {
    pub blocks: Vec<ResponseContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: crate::anthropic::Usage,
}

/// Finish a turn whose client has been streamed to: the same assembly as
/// [`render`], keeping only what has not been sent yet.
pub(crate) fn finish_streaming(attempt: &Attempt, result: SessionResult) -> Tail {
    let SessionResult {
        final_resp,
        prefix_blocks,
        tools,
        paused,
        signals,
        emitted,
        live_final,
    } = result;

    let prefix_len = prefix_blocks.len();
    let mut anth = translate_final(attempt, &final_resp, signals);
    assemble(&mut anth, prefix_blocks, &tools, paused);
    record_json_stats(attempt, &final_resp);

    // The blocks already sent are the first `emitted` of the assembled
    // message: `assemble` puts the prefix, which they came from, in front.
    // When the last run was streamed live, its content (everything after the
    // prefix) is with the client too; `assemble` still read it, for the stop
    // reason, but none of it is sent again.
    if live_final {
        anth.content.truncate(prefix_len);
    }
    let unsent = anth.content.split_off(emitted.min(anth.content.len()));
    Tail {
        blocks: unsent,
        stop_reason: anth.stop_reason,
        stop_sequence: anth.stop_sequence,
        usage: anth.usage,
    }
}

/// Translate the final response, ranking in the stop signals its output may
/// not show.
fn translate_final(
    attempt: &Attempt,
    final_resp: &ResponsesResponse,
    signals: StopSignals,
) -> Message {
    let mut anth = translate::responses_to_anthropic(final_resp, &attempt.registry);
    anth.stop_reason = if signals.refused {
        Some(StopReason::Refusal)
    } else {
        translate::stop_reason_for_cut_short_call(
            anth.stop_reason,
            &final_resp.status,
            signals.cut_short,
        )
    };
    anth
}

/// Turn the translated final response into what the client must see.
pub(crate) fn assemble(
    anth: &mut Message,
    prefix_blocks: Vec<ResponseContentBlock>,
    tools: &[Box<dyn ServerTool>],
    paused: bool,
) {
    // The client must see `server_tool_use` + result blocks, never a client
    // `tool_use` for an internal function tool.
    anth.content.retain(|block| {
        !matches!(block, ResponseContentBlock::ToolUse { name, .. }
            if tools.iter().any(|t| t.executor_name() == name))
    });
    anth.content.splice(0..0, prefix_blocks);

    for tool in tools {
        tool.contribute_usage(&mut anth.usage);
    }

    // The translator's reason is the last run's, ranked as on the plain path:
    // the client must not run the tools of a refused turn, and a turn cut off
    // at `max_tokens` is retried with a bigger budget, not acted on. After
    // those, a pending client tool_use wins (the client must run it), then a
    // loop stopped at its round limit is `pause_turn`.
    let has_client_tool_use = anth
        .content
        .iter()
        .any(|b| matches!(b, ResponseContentBlock::ToolUse { .. }));
    anth.stop_reason = Some(match anth.stop_reason {
        Some(StopReason::Refusal) => StopReason::Refusal,
        // A paused turn's reason comes from a round already served, so with
        // nothing for the client to run it can still resume.
        Some(StopReason::MaxTokens) if has_client_tool_use || !paused => StopReason::MaxTokens,
        _ if has_client_tool_use => StopReason::ToolUse,
        _ if paused => StopReason::PauseTurn,
        Some(StopReason::StopSequence) => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    });
}

#[cfg(test)]
mod tests;
