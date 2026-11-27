use super::*;

/// The default cap (60 s): large enough that the backoff's own behaviour
/// (base, doubling, `BACKOFF_CAP`, the `Retry-After` floor) shows through
/// unchanged, which is the point of the default.
const TEST_CAP: Duration = Duration::from_secs(60);

#[test]
fn backoff_doubles_from_the_base_and_stops_at_the_cap() {
    // Full jitter value (1.0) gives the whole wait.
    assert_eq!(backoff(0, None, 1.0, TEST_CAP), Duration::from_millis(500));
    assert_eq!(backoff(1, None, 1.0, TEST_CAP), Duration::from_millis(1000));
    assert_eq!(backoff(2, None, 1.0, TEST_CAP), Duration::from_millis(2000));
    assert_eq!(backoff(4, None, 1.0, TEST_CAP), Duration::from_secs(8));
    // Far past the cap, and past where a shift would overflow.
    assert_eq!(backoff(10, None, 1.0, TEST_CAP), BACKOFF_CAP);
    assert_eq!(backoff(40, None, 1.0, TEST_CAP), BACKOFF_CAP);
}

#[test]
fn jitter_scales_the_wait_to_between_half_and_all_of_it() {
    assert_eq!(backoff(1, None, 0.0, TEST_CAP), Duration::from_millis(500));
    assert_eq!(backoff(1, None, 0.5, TEST_CAP), Duration::from_millis(750));
    // Out-of-range jitter is clamped, not trusted.
    assert_eq!(backoff(1, None, 9.0, TEST_CAP), Duration::from_millis(1000));
    assert_eq!(backoff(1, None, -3.0, TEST_CAP), Duration::from_millis(500));
}

#[test]
fn the_clock_jitter_stays_in_range() {
    for _ in 0..50 {
        let j = jitter_unit();
        assert!((0.0..1.0).contains(&j), "{j}");
    }
}

#[test]
fn an_upstream_retry_after_is_a_floor_not_a_ceiling() {
    let asked = Duration::from_secs(20);
    assert_eq!(backoff(0, Some(asked), 1.0, TEST_CAP), asked);
    // A shorter ask does not shorten the backoff.
    assert_eq!(
        backoff(2, Some(Duration::from_millis(10)), 1.0, TEST_CAP),
        Duration::from_millis(2000)
    );
}

#[test]
fn no_wait_exceeds_the_configured_max_retry_wait() {
    // A wait below the cap passes through untouched (the default's behaviour).
    assert_eq!(
        backoff(0, None, 1.0, Duration::from_secs(5)),
        Duration::from_millis(500)
    );
    // The exponential part is clamped: attempt 4 would be 8 s.
    assert_eq!(
        backoff(4, None, 1.0, Duration::from_secs(3)),
        Duration::from_secs(3)
    );
    // A `Retry-After` the operator's cap is shorter than is clamped too:
    // the floor never outranks the ceiling.
    assert_eq!(
        backoff(
            0,
            Some(Duration::from_secs(30)),
            1.0,
            Duration::from_secs(5)
        ),
        Duration::from_secs(5)
    );
    // `0` would make the proxy hammer a rate-limited upstream, so the
    // resolver refuses it before it reaches the gate; a zero cap here
    // would be a zero wait, which no caller can build.
    assert_ne!(
        backoff(0, None, 1.0, Duration::from_secs(30)),
        Duration::ZERO
    );
}

#[test]
fn no_limit_means_no_slots() {
    assert!(UpstreamGate::new(0, 3, TEST_CAP).slots.is_none());
    assert!(UpstreamGate::new(2, 3, TEST_CAP).slots.is_some());
}

#[tokio::test]
async fn an_unlimited_gate_never_waits() {
    let gate = UpstreamGate::new(0, 0, TEST_CAP);
    for _ in 0..100 {
        assert!(gate.acquire().await.is_none());
    }
}

#[tokio::test]
async fn a_full_gate_queues_until_a_slot_frees() {
    let gate = UpstreamGate::new(1, 0, TEST_CAP);
    let held = gate.acquire().await.expect("a slot");

    let waiting = tokio::time::timeout(Duration::from_millis(50), gate.acquire()).await;
    assert!(waiting.is_err(), "the second request must wait");

    drop(held);
    let next = tokio::time::timeout(Duration::from_millis(500), gate.acquire()).await;
    assert!(next.is_ok(), "a freed slot lets the queued request in");
}

#[tokio::test]
async fn clones_share_the_same_slots() {
    let gate = UpstreamGate::new(1, 0, TEST_CAP);
    let other = gate.clone();
    let _held = gate.acquire().await.expect("a slot");
    let waiting = tokio::time::timeout(Duration::from_millis(50), other.acquire()).await;
    assert!(waiting.is_err());
}
