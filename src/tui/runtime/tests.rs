use super::*;

fn rm(map: &[(&str, &str)], default: Option<&str>) -> RuntimeMappings {
    RuntimeMappings {
        map: map
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        default_model: default.map(str::to_owned),
    }
}

#[test]
fn resolve_uses_alias_map() {
    let m = rm(&[("claude-opus-4-8", "gpt-5.6-luna")], None);
    assert_eq!(m.resolve("claude-opus-4-8"), "gpt-5.6-luna");
}

#[test]
fn resolve_passes_through_when_unmapped() {
    let m = rm(&[], None);
    assert_eq!(m.resolve("claude-opus-4-8"), "claude-opus-4-8");
}

#[test]
fn snapshot_dirty_marks_unsaved_changes() {
    let store = MappingsStore::from_parts(rm(&[("a", "x")], Some("fallback")), rm(&[], None));
    let s = store.snapshot();
    assert!(s.is_dirty());
    assert!(s.inbound_is_dirty("a"));
    assert!(!s.inbound_is_dirty("b"));
}

#[test]
fn snapshot_clean_after_mark_saved() {
    let store = MappingsStore::from_parts(rm(&[("a", "x")], None), rm(&[], None));
    store.mark_saved();
    assert!(!store.snapshot().is_dirty());
}

#[test]
fn set_live_does_not_touch_on_disk() {
    let store = MappingsStore::from_parts(rm(&[], None), rm(&[], None));
    store.set_live(rm(&[("a", "x")], None));
    let s = store.snapshot();
    assert!(s.is_dirty());
    // The on-disk side stays empty; the inbound is dirty because
    // the live side has it.
    assert!(s.inbound_is_dirty("a"));
}
