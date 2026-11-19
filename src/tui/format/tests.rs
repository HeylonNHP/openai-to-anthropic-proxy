use super::*;

#[test]
fn token_counts_use_compact_human_readable_units() {
    assert_eq!(format_token_count(0), "0");
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(1_000), "1.00k");
    assert_eq!(format_token_count(1_234), "1.23k");
    assert_eq!(format_token_count(999_995), "1.00m");
    assert_eq!(format_token_count(1_000_000), "1.00m");
    assert_eq!(format_token_count(1_000_000_000), "1.00b");
    assert_eq!(format_token_count(1_000_000_000_000), "1.00t");
    assert_eq!(format_token_count(1_000_000_000_000_000), "1.00q");
}

#[test]
fn outcome_words_are_distinct() {
    let all = [OUTCOME_OK, OUTCOME_ERROR, OUTCOME_REFUSED, OUTCOME_PARTIAL];
    for (i, word) in all.iter().enumerate() {
        for other in &all[i + 1..] {
            assert_ne!(word, other);
        }
    }
}
