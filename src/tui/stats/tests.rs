use super::*;

fn usage(input: u32, output: u32) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 4,
        reasoning_tokens: 5,
    }
}

#[test]
fn aggregates_by_model() {
    let store = SessionStatsStore::new();
    store.record("claude-sonnet", usage(10, 20));
    store.record("claude-sonnet", usage(2, 3));
    store.record("claude-opus", usage(7, 8));

    let totals = store.snapshot();
    assert_eq!(totals["claude-sonnet"].requests, 2);
    assert_eq!(totals["claude-sonnet"].input_tokens, 12);
    assert_eq!(totals["claude-sonnet"].output_tokens, 23);
    assert_eq!(totals["claude-sonnet"].cache_read_input_tokens, 6);
    assert_eq!(totals["claude-sonnet"].reasoning_tokens, 10);
    assert_eq!(totals["claude-opus"].requests, 1);
}

#[test]
fn fallback_can_be_attributed_to_actual_model() {
    let store = SessionStatsStore::new();
    // The request's original inbound name is intentionally not recorded:
    // the retry's actual fallback model owns this completion.
    store.record_actual("gpt-fallback", usage(11, 12));
    let snapshot = store.snapshot_sections();
    assert!(!snapshot.inbound.contains_key("claude-unknown"));
    assert_eq!(snapshot.actual["gpt-fallback"].output_tokens, 12);
}
