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

fn replay(mode: ReasoningReplayMode) -> ReasoningReplay {
    ReasoningReplay {
        mode,
        max_chars: 0,
        replayed_chars: std::cell::Cell::new(0),
    }
}

fn message_item(text: &str) -> OutputItem {
    OutputItem::Message {
        id: Some("msg_1".to_owned()),
        status: None,
        role: "assistant".to_owned(),
        content: vec![OutputContentPart::OutputText {
            text: text.to_owned(),
            annotations: Vec::new(),
        }],
    }
}

fn reasoning_item(id: &str, summary: &str, encrypted: &str) -> OutputItem {
    serde_json::from_value(json!({
        "type": "reasoning",
        "id": id,
        "summary": [{"type": "summary_text", "text": summary}],
        "encrypted_content": encrypted,
    }))
    .unwrap()
}

/// `type` of each item, plus the text / call id that identifies it.
fn kinds(items: &[InputItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            InputItem::Message { role, content } => {
                let text: String = content
                    .iter()
                    .map(|p| match p {
                        InputContentPart::OutputText { text } => text.clone(),
                        _ => String::new(),
                    })
                    .collect();
                format!("{role}:{text}")
            }
            InputItem::FunctionCall { call_id, .. } => format!("call:{call_id}"),
            InputItem::FunctionCallOutput { call_id, .. } => format!("output:{call_id}"),
            InputItem::Reasoning { id, .. } => {
                format!("reasoning:{}", id.clone().unwrap_or_default())
            }
            InputItem::ItemReference { id } => format!("ref:{id}"),
        })
        .collect()
}

fn items_of(input: Input) -> Vec<InputItem> {
    match input {
        Input::Items(items) => items,
        Input::Text(_) => panic!("items"),
    }
}

#[test]
fn a_round_is_replayed_with_its_text_then_its_calls_then_their_outputs() {
    let resp = response(vec![
        message_item("Let me look that up."),
        call_item("tool_a", "c1", r#"{"x":1}"#),
        call_item("tool_a", "c2", "{}"),
    ]);
    let mut input = Input::Items(Vec::new());
    let answered = vec![
        ("c1".to_owned(), "one".to_owned()),
        ("c2".to_owned(), "two".to_owned()),
    ];
    continue_with(
        &mut input,
        &resp,
        &answered,
        &replay(ReasoningReplayMode::Text),
    );

    let items = items_of(input);
    assert_eq!(
        kinds(&items),
        [
            "assistant:Let me look that up.",
            "call:c1",
            "call:c2",
            "output:c1",
            "output:c2"
        ]
    );
    assert!(
        matches!(&items[1], InputItem::FunctionCall { name, arguments, .. }
        if name == "tool_a" && arguments == r#"{"x":1}"#)
    );
    assert!(
        matches!(&items[3], InputItem::FunctionCallOutput { output, .. }
        if serde_json::to_value(output).unwrap() == json!("one"))
    );
}

#[test]
fn continuing_a_plain_text_input_changes_nothing() {
    let resp = response(vec![call_item("t", "c", "{}")]);
    let mut input = Input::Text("hello".to_owned());
    continue_with(
        &mut input,
        &resp,
        &[("c".to_owned(), "out".to_owned())],
        &replay(ReasoningReplayMode::Text),
    );
    assert!(matches!(input, Input::Text(t) if t == "hello"));
}

#[test]
fn an_unanswered_call_is_not_replayed() {
    // `client_tool` is a sibling CLIENT tool call (the client runs it after
    // the turn); `c_dropped` belongs to an exhausted tool. Neither gets an
    // output, so neither may appear in the input.
    let resp = response(vec![
        call_item("client_tool", "c_client", "{}"),
        call_item("tool_a", "c1", "{}"),
        call_item("tool_b", "c_dropped", "{}"),
    ]);
    let items = replay_round(&resp, &["c1"], &replay(ReasoningReplayMode::Text));
    assert_eq!(kinds(&items), ["call:c1"]);
}

#[test]
fn native_reasoning_is_replayed_as_the_item_itself_before_what_it_preceded() {
    let resp = response(vec![
        reasoning_item("rs_1", "I should search.", "ENC"),
        call_item("tool_a", "c1", "{}"),
    ]);
    let mode = replay(ReasoningReplayMode::Native);
    let items = replay_round(&resp, &["c1"], &mode);
    assert_eq!(kinds(&items), ["reasoning:rs_1", "call:c1"]);
    let InputItem::Reasoning {
        content,
        summary,
        encrypted_content,
        ..
    } = &items[0]
    else {
        panic!("reasoning first")
    };
    assert_eq!(summary[0].text, "I should search.");
    assert_eq!(content[0].text, "I should search.");
    assert_eq!(encrypted_content.as_deref(), Some("ENC"));
    assert_eq!(mode.replayed_chars.get(), 16);
}

#[test]
fn native_reasoning_without_encrypted_content_omits_it() {
    let resp = response(vec![reasoning_item("rs_1", "", ""), message_item("hi")]);
    let items = replay_round(&resp, &[], &replay(ReasoningReplayMode::Native));
    assert_eq!(kinds(&items), ["reasoning:rs_1", "assistant:hi"]);
    assert!(matches!(
        &items[0],
        InputItem::Reasoning {
            encrypted_content: None,
            ..
        }
    ));
}

#[test]
fn text_reasoning_is_folded_into_the_assistant_message() {
    let resp = response(vec![
        reasoning_item("rs_1", "Plan it.", "ENC"),
        message_item("Searching now."),
        reasoning_item("rs_2", "Then fetch.", "ENC"),
        call_item("tool_a", "c1", "{}"),
    ]);
    let items = replay_round(&resp, &["c1"], &replay(ReasoningReplayMode::Text));
    assert_eq!(
        kinds(&items),
        [
            "assistant:<thinking>\nPlan it.\n</thinking>\n\nSearching now.",
            "assistant:<thinking>\nThen fetch.\n</thinking>\n\n",
            "call:c1",
        ]
    );
}

#[test]
fn reasoning_is_dropped_when_replay_is_off() {
    let resp = response(vec![
        reasoning_item("rs_1", "Plan it.", "ENC"),
        message_item("Searching now."),
        call_item("tool_a", "c1", "{}"),
    ]);
    let items = replay_round(&resp, &["c1"], &replay(ReasoningReplayMode::Off));
    assert_eq!(kinds(&items), ["assistant:Searching now.", "call:c1"]);
}

#[test]
fn reasoning_whose_following_item_is_dropped_is_dropped_too() {
    // The upstream rejects a reasoning item without its following item, so
    // reasoning before an unanswered call goes with it, in every mode.
    let resp = response(vec![
        reasoning_item("rs_1", "Call the client tool.", "ENC"),
        call_item("client_tool", "c_client", "{}"),
        message_item("Done."),
    ]);
    for mode in [ReasoningReplayMode::Native, ReasoningReplayMode::Text] {
        let items = replay_round(&resp, &[], &replay(mode));
        assert_eq!(kinds(&items), ["assistant:Done."], "{mode:?}");
    }
}

#[test]
fn replayed_reasoning_respects_max_chars() {
    let resp = response(vec![
        reasoning_item("rs_1", "abcdefghij", ""),
        message_item("x"),
    ]);
    let mode = ReasoningReplay {
        max_chars: 4,
        ..replay(ReasoningReplayMode::Text)
    };
    let items = replay_round(&resp, &[], &mode);
    assert_eq!(
        kinds(&items),
        ["assistant:<thinking>\nabcd\n</thinking>\n\nx"]
    );
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

fn usage(input: u32, output: u32, cached: Option<u32>, reasoning: Option<u32>) -> ResponsesUsage {
    ResponsesUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        input_tokens_details: cached.map(|c| crate::responses::InputTokensDetails {
            cached_tokens: c,
            cache_write_tokens: 1,
        }),
        output_tokens_details: reasoning.map(|r| crate::responses::OutputTokensDetails {
            reasoning_tokens: r,
        }),
    }
}

#[test]
fn the_turn_usage_is_the_sum_of_every_run() {
    let mut total = None;
    add_usage(&mut total, Some(&usage(100, 10, None, None)));
    add_usage(&mut total, None);
    add_usage(&mut total, Some(&usage(250, 20, Some(80), Some(5))));
    add_usage(&mut total, Some(&usage(300, 30, Some(200), None)));
    let total = total.expect("three runs reported usage");
    assert_eq!(total.input_tokens, 650);
    assert_eq!(total.output_tokens, 60);
    assert_eq!(total.total_tokens, 710);
    let details = total.input_tokens_details.expect("cache details summed");
    assert_eq!(details.cached_tokens, 280);
    assert_eq!(details.cache_write_tokens, 2);
    assert_eq!(
        total
            .output_tokens_details
            .expect("reasoning")
            .reasoning_tokens,
        5
    );
}

#[test]
fn a_turn_with_no_reported_usage_has_none() {
    let mut total = None;
    add_usage(&mut total, None);
    assert!(total.is_none());
}
