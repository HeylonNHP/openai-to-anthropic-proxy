//! Keeping a page's text within `web_search.compaction_threshold_chars`.
//!
//! Text at or under the threshold passes through untouched. Longer text goes
//! through up to [`MAX_PASSES`] summarisation passes by a [`Compactor`] (a
//! side call to the configured `compaction_model`). If there is no model, the
//! call fails, or the model cannot get under the limit, the text is
//! TRUNCATED at a paragraph boundary and the result says so — a model must
//! never mistake the first N characters of a page for the whole page.

use std::future::Future;
use std::time::Duration;

use crate::config::WebSearchConfig;
use crate::proxy::{AppState, parse_upstream_body, send_upstream_once};
use crate::responses::{
    Input, InputContentPart, InputItem, OutputContentPart, OutputItem, ResponsesRequest,
    ResponsesResponse,
};

/// How many summarisation passes are attempted before truncating.
pub const MAX_PASSES: u8 = 3;

/// Wall-clock cap on one compaction request (clamped to the global timeout).
/// Long enough to generate the whole output budget, reasoning included, on a
/// model managing about 70 tokens a second.
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Output tokens allowed for a reasoning model's thinking, on top of the
/// summary itself: upstreams count both against `max_output_tokens`, and a
/// model that spends the whole budget thinking returns no text. The same size
/// as the advisor's default `reasoning_max_tokens`.
const REASONING_ALLOWANCE: u32 = 16_384;

/// The reason reported when `compaction_model` is unset.
pub const NO_MODEL: &str = "no web_search.compaction_model is configured";

/// One summarisation request.
#[derive(Debug, Clone, Copy)]
pub struct CompactRequest<'a> {
    /// The text to shorten.
    pub content: &'a str,
    /// The hard limit, in characters, the result must fit within.
    pub limit: usize,
    /// 1-based pass number; later passes are asked to be more aggressive.
    pub pass: u8,
}

/// Something that can shorten text. A trait so the orchestration can be
/// tested without an upstream.
pub trait Compactor {
    /// Return a shortened version of `request.content`, or why that failed.
    fn compact(
        &self,
        request: CompactRequest<'_>,
    ) -> impl Future<Output = Result<String, String>> + Send;
}

/// What became of a page's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fitted {
    /// Already within the limit.
    Unchanged(String),
    /// Summarised by the model to within the limit.
    Compacted { text: String, original_chars: usize },
    /// Cut, not summarised.
    Truncated {
        text: String,
        original_chars: usize,
        limit: usize,
        reason: String,
    },
}

impl Fitted {
    /// The text to hand to the model.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Unchanged(text) | Self::Compacted { text, .. } | Self::Truncated { text, .. } => {
                text
            }
        }
    }

    /// A note the reader of the text must see, if the text is not verbatim.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        match self {
            Self::Unchanged(_) => None,
            Self::Compacted { original_chars, .. } => Some(format!(
                "[Note: this page was summarised from {original_chars} characters; details may be omitted.]"
            )),
            Self::Truncated {
                original_chars,
                limit,
                reason,
                ..
            } => Some(format!(
                "[Note: this page was NOT summarised. Its {original_chars} characters were cut to \
                 the first {limit} because {reason}. The rest of the page is missing.]"
            )),
        }
    }
}

/// Bring `text` within `limit` characters.
pub async fn fit<C: Compactor + Sync>(text: String, limit: usize, compactor: &C) -> Fitted {
    let original_chars = text.chars().count();
    if original_chars <= limit {
        return Fitted::Unchanged(text);
    }

    let mut current = text;
    let mut previous_chars = original_chars;
    let mut failure: Option<String> = None;

    for pass in 1..=MAX_PASSES {
        let request = CompactRequest {
            content: &current,
            limit,
            pass,
        };
        match compactor.compact(request).await {
            Ok(output) => {
                let output = output.trim().to_owned();
                let chars = output.chars().count();
                if chars == 0 {
                    failure = Some("the model returned no text".to_owned());
                    break;
                }
                if chars <= limit {
                    return Fitted::Compacted {
                        text: output,
                        original_chars,
                    };
                }
                if chars >= previous_chars {
                    // Not converging: keep the shorter text we already have.
                    break;
                }
                current = output;
                previous_chars = chars;
            }
            Err(reason) => {
                failure = Some(format!("summarising it failed ({reason})"));
                break;
            }
        }
    }

    let reason = failure
        .unwrap_or_else(|| "the model could not bring the summary under the limit".to_owned());
    // Truncating without a model is the configured behaviour; failing to
    // summarise with one is something the operator needs to hear about.
    if !reason.contains(NO_MODEL) {
        tracing::warn!(
            reason = %reason,
            original_chars,
            limit,
            "web_search compaction failed; truncating the page instead"
        );
    }
    Fitted::Truncated {
        text: truncate_chars(&current, limit),
        original_chars,
        limit,
        reason,
    }
}

/// Cut `text` to at most `limit` characters, preferring the last paragraph
/// break in the second half of the budget.
#[must_use]
pub fn truncate_chars(text: &str, limit: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(limit) else {
        return text.to_owned();
    };
    let head = &text[..end];
    match head.rfind("\n\n") {
        Some(cut) if head[..cut].chars().count() >= limit / 2 => head[..cut].trim_end().to_owned(),
        _ => head.to_owned(),
    }
}

/// The instructions for one summarisation pass.
#[must_use]
pub fn build_prompt(request: CompactRequest<'_>) -> String {
    let original = request.content.chars().count();
    // Aim a little under the hard limit so a summary that overshoots its
    // target still lands inside it; tighten on every retry.
    let scale = match request.pass {
        0 | 1 => 0.95,
        2 => 0.8,
        _ => 0.65,
    };
    let target = ((request.limit as f64) * scale) as usize;
    let minimum = target * 85 / 100;
    let retry_note = if request.pass > 1 {
        format!(
            "\nThis is pass {} of {MAX_PASSES}: be more aggressive about removing boilerplate, \
             repetition and non-essential exposition.\n",
            request.pass
        )
    } else {
        String::new()
    };
    format!(
        "You are a content compaction assistant. Compact the web page text below. The result \
         MUST be between {minimum} and {target} characters long.\n\n\
         Preserve verbatim: code, tables and structured data, URLs, quotations, lists.\n\
         Summarise: descriptive paragraphs, introductions and conclusions, redundant \
         explanation, boilerplate.\n\
         Always keep the original meaning and key facts, stay coherent, and fill most of the \
         allowed length unless the source is already short.\n\n\
         Original length: {original} characters.{retry_note}\n\
         Reply with the compacted text only, no commentary.\n\n\
         --- PAGE TEXT ---\n{content}",
        content = request.content,
    )
}

/// The real [`Compactor`]: one side call to the upstream per pass.
///
/// Like the advisor, it uses an EXPLICIT upstream model name (no alias
/// resolution, no `default_model` fallback) and never fails the client.
pub struct UpstreamCompactor {
    state: AppState,
    url: String,
    headers: axum::http::HeaderMap,
    model: Option<String>,
}

impl UpstreamCompactor {
    #[must_use]
    pub fn new(
        state: AppState,
        url: String,
        headers: axum::http::HeaderMap,
        cfg: &WebSearchConfig,
    ) -> Self {
        Self {
            state,
            url,
            headers,
            model: cfg.compaction_model.clone(),
        }
    }
}

impl Compactor for UpstreamCompactor {
    async fn compact(&self, request: CompactRequest<'_>) -> Result<String, String> {
        let model = self.model.as_deref().ok_or_else(|| NO_MODEL.to_owned())?;
        let outbound = compaction_request(model, request);
        let timeout = COMPACTION_TIMEOUT.min(self.state.config.request_timeout);
        let resp = send_upstream_once(
            &self.state,
            &self.url,
            &self.headers,
            &outbound,
            Some(timeout),
        )
        .await
        .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body: String = body.chars().take(200).collect();
            return Err(format!("upstream returned {status}: {body}"));
        }
        let parsed = parse_upstream_body(resp).await.map_err(|e| e.to_string())?;
        text_of(&parsed)
    }
}

/// The tool-free request for one pass.
fn compaction_request(model: &str, request: CompactRequest<'_>) -> ResponsesRequest {
    // Half a token per allowed output character is generous for any
    // tokenizer; thinking gets its own allowance.
    let max_output_tokens = u32::try_from(request.limit / 2)
        .unwrap_or(u32::MAX)
        .max(512)
        .saturating_add(REASONING_ALLOWANCE);
    ResponsesRequest {
        model: model.to_owned(),
        input: Input::Items(vec![InputItem::Message {
            role: "user".to_owned(),
            content: vec![InputContentPart::InputText {
                text: build_prompt(request),
                prompt_cache_breakpoint: None,
            }],
        }]),
        instructions: Some(
            "You compact web page text while preserving its valuable information.".to_owned(),
        ),
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(max_output_tokens),
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: false,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    }
}

/// The first non-empty output text of a completed response.
fn text_of(resp: &ResponsesResponse) -> Result<String, String> {
    if resp.status != "completed" {
        return Err(format!("the response status was {}", resp.status));
    }
    resp.output
        .iter()
        .find_map(|item| match item {
            OutputItem::Message { content, .. } => content.iter().find_map(|part| match part {
                OutputContentPart::OutputText { text, .. } if !text.trim().is_empty() => {
                    Some(text.trim().to_owned())
                }
                _ => None,
            }),
            _ => None,
        })
        .ok_or_else(|| "the model returned no text".to_owned())
}

#[cfg(test)]
mod tests;
