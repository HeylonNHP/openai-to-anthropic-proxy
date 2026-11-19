//! The interface every proxy-served server tool implements.

use futures_util::future::BoxFuture;

use crate::anthropic::{ResponseContentBlock, Usage};
use crate::error::AppError;
use crate::proxy::Attempt;
use crate::responses::ResponsesResponse;
use crate::tools::server_loop::summary::CallSummary;

/// One call the executor model made to a server tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// The upstream's call id; the output is paired to it.
    pub call_id: String,
    /// The internal function name ([`ServerTool::executor_name`]).
    pub name: String,
    /// The arguments exactly as the model produced them. Each tool parses
    /// its own; a malformed call is the tool's to answer, never to skip.
    pub arguments: String,
}

/// What serving one call produced.
pub(crate) struct Served {
    /// What the client sees for this call: the `server_tool_use` block and
    /// its result block, in order.
    pub blocks: Vec<ResponseContentBlock>,
    /// What the executor model reads as the call's output. It must never
    /// look like content when the call failed.
    pub output: String,
    /// What the operator sees: the call in one line, for the request log.
    /// Every served call has one, so no call can go unseen.
    pub summary: CallSummary,
}

/// A server tool the proxy serves itself.
///
/// A tool serves a call and says how it shows up in the final response; the
/// loop owns everything else (running the executor, finding calls, injecting
/// outputs, assembling the reply).
///
/// Serving never fails the client request: a tool turns its own failures into
/// an error result block plus error text for the model.
pub(crate) trait ServerTool: Send {
    /// The name of the internal function tool the executor calls. Calls to it
    /// are routed here, and its calls are hidden from the client.
    fn executor_name(&self) -> &'static str;

    /// Serve one call.
    fn serve<'a>(&'a mut self, attempt: &'a Attempt, call: &'a Call) -> BoxFuture<'a, Served>;

    /// Called with EVERY buffered executor response, before its calls are
    /// routed. A tool that reports per-sampling numbers (the advisor's
    /// `iterations[]`) records them here.
    fn on_executor_response(&mut self, _resp: &ResponsesResponse) {}

    /// The tool has refused a call because its limit was reached, and has
    /// already told the executor so. From then on the loop does not serve
    /// the tool: a further call to it is dropped (hidden from the client,
    /// answered with nothing) and cannot keep the turn going. That is what
    /// bounds a turn whose executor ignores the refusal.
    fn exhausted(&self) -> bool {
        false
    }

    /// A later executor run failed after this turn had already done work.
    /// The loop returns what was produced so far; a tool that would otherwise
    /// leave a half-finished pair in it closes the pair here.
    fn on_rerun_failure(&mut self, _prefix: &mut Vec<ResponseContentBlock>) {}

    /// Add this tool's numbers to the response's `usage`.
    fn contribute_usage(&self, _usage: &mut Usage) {}

    /// Clean an upstream error before it reaches the client.
    fn scrub_error(&self, err: AppError) -> AppError {
        err
    }
}

#[cfg(test)]
mod tests;
