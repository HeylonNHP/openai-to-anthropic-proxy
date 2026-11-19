use super::*;
use crate::responses::OutputContentPart;
use serde_json::json;

fn call_item(name: &str, call_id: &str, args: &str) -> OutputItem {
    OutputItem::FunctionCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: args.to_owned(),
        id: Some(format!("fc_{call_id}")),
        status: None,
    }
}

fn response(output: Vec<OutputItem>) -> ResponsesResponse {
    serde_json::from_value(json!({
        "id": "resp_1", "object": "response", "created_at": 1,
        "status": "completed", "model": "m", "output": [],
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
    }))
    .map(|mut r: ResponsesResponse| {
        r.output = output;
        r
    })
    .unwrap()
}

#[test]
fn every_call_to_a_known_tool_is_found_in_order_with_its_tool() {
    let output = vec![
        call_item("tool_b", "c1", "{}"),
        call_item("Bash", "c2", r#"{"command":"ls"}"#),
        call_item("tool_a", "c3", r#"{"x":1}"#),
        call_item("tool_b", "c4", "{}"),
    ];
    let calls = find_calls(&output, &["tool_a", "tool_b"]);
    let found: Vec<(usize, &str)> = calls
        .iter()
        .map(|(i, c)| (*i, c.call_id.as_str()))
        .collect();
    assert_eq!(found, vec![(1, "c1"), (0, "c3"), (1, "c4")]);
    assert_eq!(calls[1].1.arguments, r#"{"x":1}"#);
    assert_eq!(calls[1].1.name, "tool_a");
}

#[test]
fn a_call_to_an_unknown_function_is_not_ours() {
    // The client's own tools, and the upstream's built-in `web_search`, are
    // never served by the loop.
    let output = vec![
        call_item("web_search", "c1", "{}"),
        call_item("Bash", "c2", "{}"),
    ];
    assert!(find_calls(&output, &["proxy_web_search"]).is_empty());
}

#[test]
fn no_calls_when_there_are_no_function_calls() {
    let message = OutputItem::Message {
        role: "assistant".to_owned(),
        content: vec![OutputContentPart::OutputText {
            text: "hi".to_owned(),
            annotations: Vec::new(),
        }],
        id: None,
        status: None,
    };
    assert!(find_calls(&[message], &["t"]).is_empty());
}

#[test]
fn intermediate_blocks_drop_only_the_internal_calls() {
    let resp = response(vec![
        call_item("tool_a", "c1", "{}"),
        call_item("Bash", "c2", r#"{"command":"ls"}"#),
    ]);
    let blocks = intermediate_blocks(&resp, &ToolSchemaRegistry::new(), &["tool_a"]);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], ResponseContentBlock::ToolUse { name, .. } if name == "Bash"));
}

#[test]
fn the_kept_response_loses_its_output_but_keeps_its_metadata() {
    let resp = response(vec![call_item("tool_a", "c1", "{}")]);
    let kept = without_output(&resp);
    assert!(kept.output.is_empty());
    assert_eq!(kept.id, resp.id);
    assert_eq!(kept.usage.as_ref().map(|u| u.output_tokens), Some(4));
}

#[test]
fn an_output_is_injected_with_its_call_so_nothing_dangles() {
    let mut input = Input::Items(Vec::new());
    let call = Call {
        call_id: "c1".to_owned(),
        name: "tool_a".to_owned(),
        arguments: r#"{"x":1}"#.to_owned(),
    };
    inject(&mut input, &call, "the output");

    let Input::Items(items) = input else {
        panic!("items")
    };
    assert_eq!(items.len(), 2);
    assert!(
        matches!(&items[0], InputItem::FunctionCall { call_id, name, arguments }
        if call_id == "c1" && name == "tool_a" && arguments == r#"{"x":1}"#)
    );
    assert!(matches!(&items[1], InputItem::FunctionCallOutput { call_id, .. } if call_id == "c1"));
}

#[test]
fn injecting_into_a_plain_text_input_changes_nothing() {
    let mut input = Input::Text("hello".to_owned());
    let call = Call {
        call_id: "c".to_owned(),
        name: "t".to_owned(),
        arguments: "{}".to_owned(),
    };
    inject(&mut input, &call, "out");
    assert!(matches!(input, Input::Text(t) if t == "hello"));
}

/// A tool that only names itself and can be exhausted.
struct Named(&'static str, bool);

impl ServerTool for Named {
    fn executor_name(&self) -> &'static str {
        self.0
    }

    fn serve<'a>(
        &'a mut self,
        _: &'a Attempt,
        _: &'a Call,
    ) -> futures_util::future::BoxFuture<'a, crate::tools::server_loop::handler::Served> {
        Box::pin(async {
            crate::tools::server_loop::handler::Served {
                blocks: Vec::new(),
                output: String::new(),
                summary: crate::tools::server_loop::summary::CallSummary::new(
                    self.0,
                    "subject",
                    crate::tui::format::OUTCOME_OK,
                ),
            }
        })
    }

    fn exhausted(&self) -> bool {
        self.1
    }
}

fn boxed(items: Vec<Named>) -> Vec<Box<dyn ServerTool>> {
    items
        .into_iter()
        .map(|t| Box::new(t) as Box<dyn ServerTool>)
        .collect()
}

#[test]
fn a_terminal_response_loses_only_the_calls_to_exhausted_tools() {
    let mut resp = response(vec![
        call_item("capped", "c1", "{}"),
        call_item("Bash", "c2", "{}"),
        call_item("live", "c3", "{}"),
    ]);
    strip_exhausted(
        &mut resp,
        &boxed(vec![Named("capped", true), Named("live", false)]),
    );

    let left: Vec<&str> = resp
        .output
        .iter()
        .filter_map(|i| match i {
            OutputItem::FunctionCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(left, ["Bash", "live"]);
}

#[test]
fn stripping_is_idempotent_and_a_no_op_with_nothing_exhausted() {
    let mut resp = response(vec![call_item("live", "c1", "{}")]);
    let tools = boxed(vec![Named("live", false)]);
    strip_exhausted(&mut resp, &tools);
    strip_exhausted(&mut resp, &tools);
    assert_eq!(resp.output.len(), 1);
}

// ── the round limit ──

fn policy(max_rounds: usize, pause_turn: bool) -> RoundPolicy {
    RoundPolicy {
        max_rounds,
        pause_turn,
    }
}

#[test]
fn rounds_below_the_limit_just_continue() {
    for pause in [false, true] {
        for served in 1..3 {
            assert_eq!(policy(3, pause).after_round(served), AfterRound::Continue);
        }
    }
}

#[test]
fn the_limit_asks_for_an_answer_by_default() {
    assert_eq!(policy(3, false).after_round(3), AfterRound::AnswerNext);
    assert_eq!(policy(3, false).after_round(4), AfterRound::AnswerNext);
}

#[test]
fn the_limit_pauses_when_pause_turn_is_on() {
    assert_eq!(policy(3, true).after_round(3), AfterRound::Pause);
}

#[test]
fn a_limit_of_one_acts_after_the_first_round() {
    assert_eq!(policy(1, true).after_round(1), AfterRound::Pause);
    assert_eq!(policy(1, false).after_round(1), AfterRound::AnswerNext);
}

#[test]
fn a_zero_limit_is_raised_to_one_so_a_turn_can_never_do_no_work() {
    let cfg = crate::config::ServerToolsConfig {
        max_rounds: Some(0),
        pause_turn: false,
    };
    assert_eq!(RoundPolicy::from_config(&cfg).max_rounds, 1);
}

#[test]
fn no_limit_is_a_limit_nothing_reaches() {
    let cfg = crate::config::ServerToolsConfig {
        max_rounds: None,
        pause_turn: true,
    };
    let p = RoundPolicy::from_config(&cfg);
    assert_eq!(p.after_round(1_000_000), AfterRound::Continue);
}

#[test]
fn the_config_is_carried_over() {
    let cfg = crate::config::ServerToolsConfig {
        max_rounds: Some(7),
        pause_turn: true,
    };
    assert_eq!(RoundPolicy::from_config(&cfg), policy(7, true));
}

#[test]
fn every_internal_call_is_stripped_from_a_refused_final_response() {
    let mut resp = response(vec![
        call_item("tool_a", "c1", "{}"),
        call_item("Bash", "c2", "{}"),
        call_item("tool_b", "c3", "{}"),
    ]);
    strip_calls(&mut resp, &["tool_a", "tool_b"]);
    assert_eq!(resp.output.len(), 1);
    assert!(matches!(&resp.output[0], OutputItem::FunctionCall { name, .. } if name == "Bash"));
}
