//! Runtime, hot-reloadable model mappings.
//!
//! The proxy holds a [`MappingsStore`] in shared state. Each request takes
//! a *snapshot* of the current mappings at the start of the handler, so a
//! change applied mid-request never tears the snapshot — the request
//! finishes with the mappings it started with.
//!
//! Implementation notes:
//!
//! - We use [`arc_swap::ArcSwap`] rather than `RwLock<Arc<...>>` because
//!   readers (one per inbound request) vastly outnumber writers (one per
//!   TUI edit). ArcSwap gives lock-free reads and wait-free writes; a
//!   request takes a snapshot for ~tens of milliseconds, while a writer
//!   may not run again for hours.
//! - The store only carries the parts of the configuration that can
//!   change at runtime: `model_aliases.map` and `model_aliases.default_model`.
//!   Static settings (upstream URL, API key, listen address) stay in
//!   `Config` and require a process restart.
//! - "Unsaved" is tracked at the entry level: a snapshot of the file on
//!   disk is kept alongside the live snapshot so the TUI can render
//!   `*` markers on rows that diverge from disk.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// The runtime-mutable portion of `Config::model_aliases`.
///
/// Kept as a tiny POD struct so it can be `Clone + Send + Sync` and
/// sit inside an `Arc` cheaply. The `BTreeMap` is small (single-digit
/// entries in practice), so cloning it on swap is fine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeMappings {
    /// Inbound model name → upstream model name.
    /// Mirrors `Config::model_aliases.map`.
    #[serde(default)]
    pub map: BTreeMap<String, String>,
    /// Default upstream model. Mirrors `Config::model_aliases.default_model`.
    #[serde(default)]
    pub default_model: Option<String>,
}

impl RuntimeMappings {
    /// Construct from the static `Config::model_aliases` view.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            map: config.model_aliases.map.clone(),
            default_model: config.model_aliases.default_model.clone(),
        }
    }

    /// Resolve an inbound model to the upstream model name, exactly the
    /// way `Config::upstream_model_for` does it. The duplication is
    /// deliberate: `MappingsSnapshot` carries no reference back to
    /// `Config` so it can be used from the request hot path without
    /// touching any locks.
    pub fn resolve(&self, model: &str) -> String {
        self.map
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_owned())
    }

    /// Default upstream model, if configured.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }
}

/// A point-in-time view of the live mappings plus the on-disk mappings.
///
/// Cheap to clone (`Arc` bump). The TUI uses this to render the
/// `*` marker on rows that diverge from disk.
#[derive(Debug, Clone)]
pub struct MappingsSnapshot {
    /// What's currently in effect for the proxy.
    pub live: Arc<RuntimeMappings>,
    /// What was last loaded from or saved to `proxy.json`.
    pub on_disk: Arc<RuntimeMappings>,
}

impl MappingsSnapshot {
    /// True when the live mappings differ from the on-disk mappings.
    pub fn is_dirty(&self) -> bool {
        self.live.map != self.on_disk.map
            || self.live.default_model != self.on_disk.default_model
    }

    /// True when the given inbound model's *value* in `live` differs
    /// from its value in `on_disk`. Used for the `*` row marker.
    pub fn inbound_is_dirty(&self, inbound: &str) -> bool {
        self.live.map.get(inbound) != self.on_disk.map.get(inbound)
    }
}

/// Lock-free, Arc-clone-cheap store of the current `RuntimeMappings`.
///
/// `load()` returns a guard that pins the current snapshot; cloning the
/// guard yields an owned `Arc<RuntimeMappings>`. Writers call
/// `store_live()` / `store_on_disk()` to atomically swap the inner Arc;
/// readers always see a consistent snapshot.
pub struct MappingsStore {
    live: ArcSwap<RuntimeMappings>,
    on_disk: ArcSwap<RuntimeMappings>,
}

impl MappingsStore {
    /// Build a store from an explicit `Arc<RuntimeMappings>`. The
    /// `on_disk` side starts equal to the live side, so the TUI shows
    /// no `*` markers until the operator edits and saves.
    pub fn seeded(seed: Arc<RuntimeMappings>) -> Self {
        Self::from_seed(seed)
    }

    /// Build a store from an explicit seed (internal).
    fn from_seed(seed: Arc<RuntimeMappings>) -> Self {
        Self {
            live: ArcSwap::from_pointee(RuntimeMappings::default()),
            on_disk: ArcSwap::from_pointee(RuntimeMappings::default()),
        }
        .with_seed(seed)
    }

    /// Build a store seeded from the static `Config`.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::from_seed(Arc::new(RuntimeMappings::from_config(config)))
    }

    fn with_seed(self, seed: Arc<RuntimeMappings>) -> Self {
        self.live.store(Arc::clone(&seed));
        self.on_disk.store(seed);
        self
    }

    /// Pin the current live snapshot. Cheap (one atomic load + refcount
    /// bump on first access per thread). Use this from request handlers.
    pub fn load_live(&self) -> arc_swap::Guard<Arc<RuntimeMappings>> {
        self.live.load()
    }

    /// Pin the current on-disk snapshot.
    pub fn load_on_disk(&self) -> arc_swap::Guard<Arc<RuntimeMappings>> {
        self.on_disk.load()
    }

    /// Build a `MappingsSnapshot` (live + on_disk) in one call. Used by
    /// the TUI for rendering.
    pub fn snapshot(&self) -> MappingsSnapshot {
        MappingsSnapshot {
            live: self.live.load_full(),
            on_disk: self.on_disk.load_full(),
        }
    }

    /// Replace the live mappings. New requests pick this up immediately;
    /// in-flight requests keep the snapshot they took at handler entry.
    pub fn set_live(&self, mappings: RuntimeMappings) {
        self.live.store(Arc::new(mappings));
    }

    /// Mark the current live mappings as the new on-disk state. Call
    /// after a successful `proxy.json` save.
    pub fn mark_saved(&self) {
        let current = self.live.load_full();
        self.on_disk.store(current);
    }

    /// Build a store from an explicit pair. Used by tests.
    #[cfg(test)]
    pub fn from_parts(live: RuntimeMappings, on_disk: RuntimeMappings) -> Self {
        Self {
            live: ArcSwap::from_pointee(live),
            on_disk: ArcSwap::from_pointee(on_disk),
        }
    }
}

#[cfg(test)]
mod tests {
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
        let store = MappingsStore::from_parts(
            rm(&[("a", "x")], Some("fallback")),
            rm(&[], None),
        );
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
}
