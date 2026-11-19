use super::*;
use crate::anthropic::{ServerToolUseUsage, Usage};
use crate::tools::server_loop::handler::{Call, Served};
use crate::tools::server_loop::summary::CallSummary;
use crate::tui::format::OUTCOME_OK;
use futures_util::future::BoxFuture;
use serde_json::json;

/// A tool that only names itself and adds fetch usage.
struct Tool(&'static str, u32);

impl ServerTool for Tool {
    fn executor_name(&self) -> &'static str {
        self.0
    }

    fn serve<'a>(&'a mut self, _: &'a Attempt, _: &'a Call) -> BoxFuture<'a, Served> {
        Box::pin(async {
            Served {
                blocks: Vec::new(),
                output: String::new(),
                summary: CallSummary::new(self.0, "subject", OUTCOME_OK),
            }
        })
    }

    fn contribute_usage(&self, usage: &mut Usage) {
        if self.1 > 0 {
            usage
                .server_tool_use
                .get_or_insert_with(ServerToolUseUsage::default)
                .web_fetch_requests += self.1;
        }
    }
}

fn tools(items: Vec<Tool>) -> Vec<Box<dyn ServerTool>> {
    items
        .into_iter()
        .map(|t| Box::new(t) as Box<dyn ServerTool>)
        .collect()
}

fn message(content: Vec<ResponseContentBlock>, stop: Option<StopReason>) -> Message {
    Message {
        id: "msg_1".to_owned(),
        r#type: "message",
        role: "assistant",
        content,
        model: "m".to_owned(),
        stop_reason: stop,
        stop_sequence: None,
        usage: Usage::default(),
    }
}

fn text(t: &str) -> ResponseContentBlock {
    ResponseContentBlock::Text { text: t.to_owned() }
}

fn tool_use(name: &str) -> ResponseContentBlock {
    ResponseContentBlock::ToolUse {
        id: format!("toolu_{name}"),
        name: name.to_owned(),
        input: json!({}),
    }
}

fn raw(kind: &str) -> ResponseContentBlock {
    ResponseContentBlock::Raw(json!({"type": kind}))
}

#[test]
fn the_prefix_goes_first_and_internal_calls_are_hidden() {
    let mut anth = message(
        vec![tool_use("tool_a"), tool_use("tool_b"), text("answer")],
        Some(StopReason::EndTurn),
    );
    assemble(
        &mut anth,
        vec![raw("server_tool_use"), raw("some_result")],
        &tools(vec![Tool("tool_a", 0), Tool("tool_b", 0)]),
        false,
    );

    let kinds: Vec<String> = anth
        .content
        .iter()
        .map(|b| {
            serde_json::to_value(b).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["server_tool_use", "some_result", "text"]);
}

#[test]
fn a_clients_own_tool_use_stays_and_sets_tool_use() {
    let mut anth = message(
        vec![tool_use("tool_a"), tool_use("Bash")],
        Some(StopReason::EndTurn),
    );
    assemble(
        &mut anth,
        Vec::new(),
        &tools(vec![Tool("tool_a", 0)]),
        false,
    );
    assert_eq!(anth.content.len(), 1);
    assert!(
        matches!(&anth.content[0], ResponseContentBlock::ToolUse { name, .. } if name == "Bash")
    );
    assert_eq!(anth.stop_reason, Some(StopReason::ToolUse));
}

#[test]
fn a_terminal_stop_reason_is_preserved() {
    for stop in [
        StopReason::MaxTokens,
        StopReason::Refusal,
        StopReason::StopSequence,
    ] {
        let mut anth = message(vec![text("x")], Some(stop));
        assemble(&mut anth, Vec::new(), &tools(vec![Tool("t", 0)]), false);
        assert_eq!(anth.stop_reason, Some(stop));
    }
}

#[test]
fn an_absent_or_paused_stop_reason_becomes_end_turn() {
    for stop in [None, Some(StopReason::PauseTurn), Some(StopReason::ToolUse)] {
        let mut anth = message(vec![text("x")], stop);
        assemble(&mut anth, Vec::new(), &tools(vec![Tool("t", 0)]), false);
        assert_eq!(anth.stop_reason, Some(StopReason::EndTurn));
    }
}

#[test]
fn every_tool_adds_its_usage() {
    let mut anth = message(vec![text("x")], Some(StopReason::EndTurn));
    assemble(
        &mut anth,
        Vec::new(),
        &tools(vec![Tool("a", 2), Tool("b", 1), Tool("c", 0)]),
        false,
    );
    let used = anth.usage.server_tool_use.expect("usage");
    assert_eq!(used.web_fetch_requests, 3);
}

#[test]
fn no_tool_usage_leaves_the_field_absent() {
    let mut anth = message(vec![text("x")], Some(StopReason::EndTurn));
    assemble(&mut anth, Vec::new(), &tools(vec![Tool("a", 0)]), false);
    assert!(anth.usage.server_tool_use.is_none());
}

#[test]
fn a_paused_turn_ends_with_pause_turn() {
    let mut anth = message(vec![text("x")], Some(StopReason::EndTurn));
    assemble(
        &mut anth,
        vec![raw("server_tool_use")],
        &tools(vec![Tool("t", 0)]),
        true,
    );
    assert_eq!(anth.stop_reason, Some(StopReason::PauseTurn));
}

#[test]
fn a_pending_client_tool_wins_over_a_pause() {
    // The client has a tool to run, and sending its result resumes the turn.
    let mut anth = message(vec![tool_use("Bash")], Some(StopReason::EndTurn));
    assemble(&mut anth, Vec::new(), &tools(vec![Tool("t", 0)]), true);
    assert_eq!(anth.stop_reason, Some(StopReason::ToolUse));
}

#[test]
fn a_turn_that_is_not_paused_never_says_pause_turn() {
    let mut anth = message(vec![text("x")], Some(StopReason::PauseTurn));
    assemble(&mut anth, Vec::new(), &tools(vec![Tool("t", 0)]), false);
    assert_eq!(anth.stop_reason, Some(StopReason::EndTurn));
}
