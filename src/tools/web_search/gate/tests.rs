use super::*;

const GAP: Duration = Duration::from_millis(1500);

#[tokio::test(start_paused = true)]
async fn searches_are_spaced_by_the_minimum_gap() {
    let gate = SearchGate::default();
    let start = Instant::now();
    drop(gate.acquire(GAP).await);
    // The first search is not delayed.
    assert_eq!(start.elapsed(), Duration::ZERO);
    drop(gate.acquire(GAP).await);
    assert_eq!(start.elapsed(), GAP);
    drop(gate.acquire(GAP).await);
    assert_eq!(start.elapsed(), GAP * 2);
}

#[tokio::test(start_paused = true)]
async fn the_gap_counts_from_when_the_previous_search_finished() {
    let gate = SearchGate::default();
    let slot = gate.acquire(GAP).await;
    tokio::time::sleep(Duration::from_secs(5)).await; // a slow search
    drop(slot);
    let after = Instant::now();
    drop(gate.acquire(GAP).await);
    assert_eq!(after.elapsed(), GAP);
}

#[tokio::test(start_paused = true)]
async fn concurrent_searches_run_one_at_a_time() {
    let gate = Arc::new(SearchGate::default());
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let gate = Arc::clone(&gate);
        tasks.push(tokio::spawn(async move {
            let slot = gate.acquire(GAP).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(slot);
            start.elapsed()
        }));
    }
    let mut finished = Vec::new();
    for t in tasks {
        finished.push(t.await.unwrap());
    }
    finished.sort();
    // 1 s each, 1.5 s apart: 1.0, 3.5, 6.0.
    assert_eq!(
        finished,
        [
            Duration::from_millis(1000),
            Duration::from_millis(3500),
            Duration::from_millis(6000)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_repeat_block_reports_time_since_the_previous_one() {
    let gate = SearchGate::default();
    let mut slot = gate.acquire(GAP).await;
    assert_eq!(slot.blocked(), None, "the first block has no predecessor");
    drop(slot);
    tokio::time::sleep(Duration::from_secs(40)).await;
    let mut slot = gate.acquire(GAP).await;
    let since = slot.blocked().expect("a previous block exists");
    assert_eq!(since.as_secs(), 40);
    drop(slot);
}

#[tokio::test(start_paused = true)]
async fn a_success_resets_the_record() {
    let gate = SearchGate::default();
    let mut slot = gate.acquire(GAP).await;
    slot.blocked();
    drop(slot);
    let mut slot = gate.acquire(GAP).await;
    slot.succeeded();
    drop(slot);
    let mut slot = gate.acquire(GAP).await;
    assert_eq!(slot.blocked(), None);
    drop(slot);
}

#[tokio::test]
async fn gates_are_shared_per_endpoint() {
    assert!(Arc::ptr_eq(
        &gate_for("https://a.test/"),
        &gate_for("https://a.test/")
    ));
    assert!(!Arc::ptr_eq(
        &gate_for("https://a.test/"),
        &gate_for("https://b.test/")
    ));
}

#[test]
fn the_first_block_tells_the_model_how_long_to_wait() {
    let text = blocked_message(
        "the search engine returned a bot challenge (CAPTCHA)",
        None,
        10,
    );
    assert_eq!(
        text,
        "the search engine returned a bot challenge (CAPTCHA). \
         Wait about 10 minutes before searching again."
    );
}

#[test]
fn a_repeat_block_reports_seconds_and_minutes() {
    let text = blocked_message(
        "the search engine returned a bot challenge (CAPTCHA)",
        Some(Duration::from_secs(83)),
        10,
    );
    assert!(text.contains("again."), "{text}");
    assert!(text.contains("83 seconds (1.4 minutes) ago"), "{text}");
    assert!(
        text.ends_with("Wait about 10 minutes before searching again."),
        "{text}"
    );
    assert!(!text.to_lowercase().contains("never"), "{text}");
}

#[test]
fn singular_units() {
    assert!(
        blocked_message("x", Some(Duration::from_secs(1)), 1)
            .contains("1 second (0.0 minutes) ago")
    );
    assert!(blocked_message("x", None, 1).contains("1 minute before"));
}
