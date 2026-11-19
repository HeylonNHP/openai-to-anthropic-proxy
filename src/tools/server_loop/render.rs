//! Assemble the final Anthropic response for a server-tool turn.
//!
//! The chain was buffered (`prepare` forced `stream: false`). A client that
//! asked for the response whole gets it from [`render`]; one that asked for
//! SSE is streamed to as the turn goes (see
//! [`crate::tools::server_loop::stream`]) and finished by [`finish_streaming`].

use axum::Json;
use axum::response::{IntoResponse, Response};

use crate::anthropic::{Message, ResponseContentBlock, StopReason};
use crate::error::AppError;
use crate::proxy::{Attempt, record_json_stats};
use crate::tools::server_loop::handler::ServerTool;
use crate::tools::server_loop::run::SessionResult;
use crate::translate;

/// Render the assembled turn.
pub(crate) fn render(attempt: &Attempt, result: SessionResult) -> Result<Response, AppError> {
    let SessionResult {
        final_resp,
        prefix_blocks,
        tools,
        paused,
        emitted: _,
    } = result;

    let mut anth: Message = translate::responses_to_anthropic(&final_resp, &attempt.registry);
    assemble(&mut anth, prefix_blocks, &tools, paused);

    // Reads the LAST executor response's authoritative usage exactly once.
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
        emitted,
    } = result;

    let mut anth: Message = translate::responses_to_anthropic(&final_resp, &attempt.registry);
    assemble(&mut anth, prefix_blocks, &tools, paused);
    record_json_stats(attempt, &final_resp);

    // The blocks already sent are the first `emitted` of the assembled
    // message: `assemble` puts the prefix, which they came from, in front.
    let unsent = anth.content.split_off(emitted.min(anth.content.len()));
    Tail {
        blocks: unsent,
        stop_reason: anth.stop_reason,
        stop_sequence: anth.stop_sequence,
        usage: anth.usage,
    }
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

    // Preserve a TERMINAL stop reason from the translator. A pending client
    // tool_use always wins (the client must run it). Otherwise a loop that
    // stopped at its round limit is `pause_turn`, and anything else that is
    // not terminal is `end_turn`.
    let has_client_tool_use = anth
        .content
        .iter()
        .any(|b| matches!(b, ResponseContentBlock::ToolUse { .. }));
    if has_client_tool_use {
        anth.stop_reason = Some(StopReason::ToolUse);
    } else if paused {
        anth.stop_reason = Some(StopReason::PauseTurn);
    } else if !matches!(
        anth.stop_reason,
        Some(StopReason::MaxTokens | StopReason::Refusal | StopReason::StopSequence)
    ) {
        anth.stop_reason = Some(StopReason::EndTurn);
    }
}

#[cfg(test)]
mod tests;
