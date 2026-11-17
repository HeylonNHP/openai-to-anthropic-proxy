//! The `web_fetch` loop: run the executor, serve its fetch call, re-run.
//!
//! ## The shape of the loop
//!
//! ```text
//! execute ──▶ executor response
//!                │
//!                ├─ no web_fetch call ──▶ done
//!                │
//!                └─ web_fetch call
//!                       │
//!                       ├─ fetch the URL (the proxy is the server)
//!                       ├─ emit server_tool_use + web_fetch_tool_result
//!                       ├─ inject the page text as the tool's output
//!                       └─ re-run the executor
//! ```
//!
//! The executor MUST be re-run because the fetched text has to reach it:
//! the model asked for the page, and the model is what produces the
//! answer. The proxy is the server for the tool, so the proxy is the one
//! that satisfies the call.
//!
//! ## Fail closed
//!
//! A fetch failure NEVER fails the client request. The failure becomes a
//! `web_fetch_tool_result` error block, the executor is told the fetch
//! failed, and the turn continues — exactly the contract Anthropic's own
//! server-side tools have (errors come back as HTTP 200 with the error
//! in the body).
//!
//! ## Unlimited fetches
//!
//! Anthropic's tool declaration carries a `max_uses`, but the operator
//! asked for it to be **unlimited**: the executor may fetch as many URLs
//! as it likes in one turn. The loop is therefore bounded only by the
//! executor's own behaviour. A runaway loop is made visible in the log
//! (and to the operator) rather than capped.

use serde_json::Value;

use crate::anthropic::ResponseContentBlock;
use crate::config::WebFetchConfig;
use crate::proxy::{Attempt, Outcome};
use crate::responses::{Input, InputItem, OutputItem, ResponsesResponse};
use crate::tools::web_fetch::{fetch, protocol};
use crate::tui::{LogKind, LogLine};

/// What the loop produced.
pub(crate) enum SessionOutcome {
    /// A fully buffered, assembled final response ready for rendering.
    Complete(Box<SessionResult>),
    /// `prepare` forces `stream: false`, but if an upstream somehow
    /// answers with a stream anyway, degrade to the pre-existing streaming
    /// path rather than erroring the client.
    UnexpectedStreaming(reqwest::Response),
}

/// The buffered result of the whole turn.
pub(crate) struct SessionResult {
    /// The LAST executor response — the one that answered the user.
    pub final_resp: ResponsesResponse,
    /// Ordered prefix: the translated content of every INTERMEDIATE
    /// executor sampling (minus the internal `web_fetch` tool call)
    /// interleaved with the `server_tool_use` / `web_fetch_tool_result`
    /// pairs, in the order they actually happened.
    pub prefix_blocks: Vec<ResponseContentBlock>,
}

/// A turn that fetches more than this many URLs is a runaway loop.
///
/// The proxy does NOT cap it (the operator asked for unlimited fetches),
/// but it makes the hazard visible in the log and to the operator.
const RUNAWAY_FETCH_THRESHOLD: usize = 10;

/// Text injected into the executor's transcript when the fetch failed.
///
/// It must NOT look like page content — the client sees a
/// `web_fetch_tool_result` error block, and the executor must be told the
/// fetch failed rather than handed an empty page.
const FETCH_FAILED: &str =
    "The web_fetch request failed. The page could not be retrieved. Continue without it.";

/// Run the whole detection-triggered turn.
pub(crate) async fn run(
    attempt: &mut Attempt,
    cfg: &WebFetchConfig,
) -> Result<SessionOutcome, crate::error::AppError> {
    let url = attempt.url();
    let mut prefix_blocks: Vec<ResponseContentBlock> = Vec::new();
    let mut warned = false;
    let mut fetches = 0usize;

    loop {
        // The executor's own pipe: capabilities, parameter-strip retry,
        // one-shot default_model fallback, error mapping, corrective retry.
        let resp = match attempt.execute(&url).await {
            Ok(Outcome::Buffered(body)) => body,
            Ok(Outcome::Streaming(raw)) => {
                tracing::warn!(
                    "web_fetch path expected a buffered response; rendering the upstream stream directly"
                );
                return Ok(SessionOutcome::UnexpectedStreaming(raw));
            }
            Err(err) => return Err(err),
        };

        let Some(call) = find_web_fetch_call(&resp.output) else {
            return Ok(SessionOutcome::Complete(Box::new(SessionResult {
                final_resp: resp,
                prefix_blocks,
            })));
        };

        // The executor asked for a fetch. Everything it said in this
        // sampling is an INTERMEDIATE block: keep all of it except the
        // internal web_fetch tool_use.
        prefix_blocks.extend(intermediate_blocks(&resp, attempt));

        fetches += 1;
        if fetches == RUNAWAY_FETCH_THRESHOLD {
            tracing::warn!(
                fetches,
                "the executor has fetched 10 URLs in one turn; web_fetch is unlimited by \
                 configuration, so this will keep going until the executor stops asking"
            );
            attempt.state.output.emit(LogLine {
                kind: LogKind::Warning,
                text: format!(
                    "  \u{26a0} web_fetch: {fetches} URLs fetched in one turn (unlimited by configuration)"
                ),
            });
        }

        let id = protocol::new_id();
        let url_arg = call.url.clone();

        // The server_tool_use block goes out with the request that
        // follows; the result block closes the pair.
        prefix_blocks.push(ResponseContentBlock::Raw(protocol::server_tool_use(
            &id, &url_arg,
        )));

        let fetched = fetch::fetch(&url_arg, cfg).await;
        let (result_block, injection) = match fetched {
            Ok(page) => {
                let block = protocol::web_fetch_result(
                    &id,
                    &page.final_url,
                    page.title.as_deref(),
                    &page.text,
                );
                (block, page.text)
            }
            Err(err) => {
                if !warned {
                    warned = true;
                    tracing::warn!(
                        url = %url_arg,
                        error_code = err.code,
                        detail = %err.detail,
                        "web_fetch failed; continuing without the page"
                    );
                    attempt.state.output.emit(LogLine {
                        kind: LogKind::Warning,
                        text: format!("  \u{26a0} web_fetch failed ({}): {}", err.code, err.detail),
                    });
                }
                let block = protocol::error_result(&id, err.code);
                (block, FETCH_FAILED.to_owned())
            }
        };
        prefix_blocks.push(ResponseContentBlock::Raw(result_block));

        // Feed the page text back to the executor. The upstream needs
        // the `function_call` item and its matching `function_call_output`
        // so the call is never left dangling.
        inject(attempt, &call.call_id, &call.arguments, &injection);
    }
}

/// One `web_fetch` call emitted by the executor.
struct WebFetchCall {
    /// The upstream's call id, used to pair the output with the call.
    call_id: String,
    /// The raw arguments JSON, exactly as the executor produced it.
    arguments: String,
    /// The `url` argument, parsed out of the arguments.
    url: String,
}

/// Find the FIRST `function_call{name:"web_fetch"}` in an executor
/// response.
fn find_web_fetch_call(output: &[OutputItem]) -> Option<WebFetchCall> {
    output.iter().find_map(|item| match item {
        OutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } if name == crate::tools::web_fetch::WEB_FETCH_TOOL_NAME => {
            let url = parse_url_argument(arguments)?;
            Some(WebFetchCall {
                call_id: call_id.clone(),
                arguments: arguments.clone(),
                url,
            })
        }
        _ => None,
    })
}

/// Pull the `url` out of the executor's arguments JSON.
///
/// A call with no usable `url` is not a fetch: it is skipped, so a
/// malformed call cannot spin the loop.
fn parse_url_argument(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    let url = parsed.get("url")?.as_str()?.trim().to_owned();
    if url.is_empty() {
        return None;
    }
    Some(url)
}

/// Translated content of one executor sampling, minus the internal
/// `web_fetch` tool_use. Kept in order so intermediate text and sibling
/// client tool uses survive to the final Anthropic message.
fn intermediate_blocks(resp: &ResponsesResponse, attempt: &Attempt) -> Vec<ResponseContentBlock> {
    let mut blocks = crate::translate::responses_to_anthropic(resp, &attempt.registry).content;
    blocks.retain(|block| {
        !matches!(
            block,
            ResponseContentBlock::ToolUse { name, .. }
                if name == crate::tools::web_fetch::WEB_FETCH_TOOL_NAME
        )
    });
    blocks
}

/// Feed the fetched text back to the executor as the tool's output.
///
/// The `function_call` item is replayed alongside its
/// `function_call_output` — the Responses API requires the call to be
/// present in the input for the output to pair with it.
fn inject(attempt: &mut Attempt, call_id: &str, arguments: &str, output: &str) {
    if let Input::Items(items) = &mut attempt.outbound.input {
        items.push(InputItem::FunctionCall {
            call_id: call_id.to_owned(),
            name: crate::tools::web_fetch::WEB_FETCH_TOOL_NAME.to_owned(),
            arguments: arguments.to_owned(),
        });
        items.push(InputItem::FunctionCallOutput {
            call_id: call_id.to_owned(),
            output: output.to_owned().into(),
        });
    }
}

#[cfg(test)]
mod tests;
