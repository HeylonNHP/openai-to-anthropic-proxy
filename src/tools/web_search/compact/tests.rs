use super::*;
use std::sync::Mutex;

/// A compactor that replays scripted outputs and records what it was asked.
struct Scripted {
    outputs: Mutex<Vec<Result<String, String>>>,
    seen: Mutex<Vec<(usize, u8)>>,
}

impl Scripted {
    fn new(outputs: Vec<Result<String, String>>) -> Self {
        Self {
            outputs: Mutex::new(outputs),
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Compactor for Scripted {
    async fn compact(&self, request: CompactRequest<'_>) -> Result<String, String> {
        self.seen
            .lock()
            .unwrap()
            .push((request.content.chars().count(), request.pass));
        self.outputs.lock().unwrap().remove(0)
    }
}

#[tokio::test]
async fn short_text_is_untouched_and_never_calls_the_model() {
    let c = Scripted::new(vec![]);
    let fitted = fit("hello".to_owned(), 10, &c).await;
    assert_eq!(fitted, Fitted::Unchanged("hello".into()));
    assert!(fitted.note().is_none());
    assert!(c.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_summary_within_the_limit_is_used() {
    let c = Scripted::new(vec![Ok("  short  ".into())]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    assert_eq!(
        fitted,
        Fitted::Compacted {
            text: "short".into(),
            original_chars: 100
        }
    );
    assert!(fitted.note().unwrap().contains("summarised from 100"));
}

#[tokio::test]
async fn an_oversized_summary_triggers_another_pass() {
    let c = Scripted::new(vec![Ok("y".repeat(60)), Ok("z".repeat(15))]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    assert!(matches!(fitted, Fitted::Compacted { .. }));
    // The second pass summarises the FIRST pass's output.
    assert_eq!(*c.seen.lock().unwrap(), vec![(100, 1), (60, 2)]);
}

#[tokio::test]
async fn a_summary_that_does_not_shrink_stops_and_truncates() {
    let c = Scripted::new(vec![Ok("y".repeat(100))]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    let Fitted::Truncated { text, reason, .. } = fitted else {
        panic!("expected truncation");
    };
    assert_eq!(text.chars().count(), 20);
    assert!(text.starts_with('x'), "the shorter (original) text is kept");
    assert!(reason.contains("under the limit"));
}

#[tokio::test]
async fn gives_up_after_the_pass_limit() {
    let c = Scripted::new(vec![
        Ok("a".repeat(90)),
        Ok("a".repeat(80)),
        Ok("a".repeat(70)),
    ]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    assert!(matches!(fitted, Fitted::Truncated { .. }));
    assert_eq!(c.seen.lock().unwrap().len(), usize::from(MAX_PASSES));
}

#[tokio::test]
async fn a_failing_model_falls_back_to_truncation_with_a_note() {
    let c = Scripted::new(vec![Err("boom".into())]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    let note = fitted.note().expect("truncation must be disclosed");
    assert!(note.contains("NOT summarised"));
    assert!(note.contains("boom"));
    assert!(note.contains("cut to the first 20"));
    assert_eq!(fitted.text().chars().count(), 20);
}

#[tokio::test]
async fn an_empty_summary_is_a_failure() {
    let c = Scripted::new(vec![Ok("   ".into())]);
    let fitted = fit("x".repeat(100), 20, &c).await;
    assert!(matches!(fitted, Fitted::Truncated { .. }));
}

#[test]
fn truncation_prefers_a_paragraph_boundary() {
    let text = "aaaa\n\nbbbb\n\ncccccccccc";
    assert_eq!(truncate_chars(text, 14), "aaaa\n\nbbbb");
}

#[test]
fn truncation_ignores_a_boundary_too_early_in_the_text() {
    let text = format!("a\n\n{}", "b".repeat(50));
    assert_eq!(truncate_chars(&text, 20).chars().count(), 20);
}

#[test]
fn truncation_is_char_safe() {
    let text = "é".repeat(30);
    assert_eq!(truncate_chars(&text, 7), "é".repeat(7));
    assert_eq!(truncate_chars("short", 100), "short");
}

#[test]
fn the_prompt_tightens_on_later_passes_and_carries_the_content() {
    let req = |pass| CompactRequest {
        content: "PAGE BODY",
        limit: 1000,
        pass,
    };
    let first = build_prompt(req(1));
    let third = build_prompt(req(3));
    assert!(first.contains("PAGE BODY"));
    assert!(first.contains("between 807 and 950"));
    assert!(!first.contains("pass 1 of"));
    assert!(third.contains("between 552 and 650"));
    assert!(third.contains("pass 3 of 3"));
}

#[test]
fn the_request_is_tool_free_and_buffered() {
    let r = compaction_request(
        "cheap-model",
        CompactRequest {
            content: "x",
            limit: 5000,
            pass: 1,
        },
    );
    assert_eq!(r.model, "cheap-model");
    assert!(r.tools.is_none());
    assert!(!r.stream);
    // 2500 for the summary, plus the reasoning allowance.
    assert_eq!(r.max_output_tokens, Some(2500 + 16_384));
}
