use super::*;

#[test]
fn resize_is_distinct_from_poll_wakeup() {
    assert_ne!(KeyRead::Resize, KeyRead::None);
}
