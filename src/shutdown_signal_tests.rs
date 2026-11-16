//! Regression tests for the TUI-exit-triggered shutdown mechanism.
//!
//! The bug: pressing `q` in the TUI exited the TUI but left the
//! axum server running, because the only thing that resolved
//! `shutdown_signal()` was `signal::ctrl_c()`. The user had to
//! press Ctrl-C a *second* time to actually stop the server.
//!
//! The fix wires `shutdown_signal()` to a `watch::Receiver`
//! alongside the OS signal handlers; dropping the sender (which
//! we do immediately after the TUI's `run()` returns) resolves
//! the receiver and triggers the graceful-shutdown path.

use std::time::Duration;

/// Dropping the sender must cause `shutdown_signal`'s
/// `watch::Receiver::changed()` branch to resolve. This pins
/// the primitive we chose: a `watch::channel` whose sender we
/// hold in `main()` and drop after the TUI exits. Tokio's
/// `watch::Receiver::changed()` returns `Err(RecvError)` when
/// the sender is dropped — the `_` pattern in `select!`
/// matches both `Ok` and `Err`, so the branch fires and
/// triggers graceful shutdown.
#[tokio::test]
async fn watch_changed_resolves_when_sender_dropped() {
    let (tx, mut rx) = tokio::sync::watch::channel(());
    drop(tx);
    let result = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
    match result {
        Ok(Err(_)) => {} // expected: Err when sender is dropped
        Ok(Ok(())) => panic!("watch::changed() resolved to Ok(()) without a value change"),
        Err(_) => panic!(
            "watch::changed() did not resolve within 200ms — sender drop did not wake receiver"
        ),
    }
}

/// The actual mechanism: a `tokio::select!` racing a
/// `watch::Receiver::changed()` against `tokio::time::sleep`
/// must pick the receiver branch when the sender is dropped.
/// This is the shape used in `shutdown_signal`.
#[tokio::test]
async fn select_picks_watch_branch_when_sender_dropped() {
    let (tx, mut rx) = tokio::sync::watch::channel(());
    drop(tx);
    let picked = tokio::select! {
        _ = rx.changed() => "watch",
        _ = tokio::time::sleep(Duration::from_secs(10)) => "sleep",
    };
    assert_eq!(
        picked, "watch",
        "select! must pick the dropped-watch branch"
    );
}
